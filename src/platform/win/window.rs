use windows_core::{ComObject, HSTRING};
use windows_sys::Win32::{
    Foundation::{LPARAM, LRESULT, RECT, WPARAM},
    UI::{Controls::WM_MOUSELEAVE, WindowsAndMessaging::*},
};

use crate::dpi::{PhysicalPosition, PhysicalSize, Size};
use crate::{warn, EventStatus, HandlerError, WindowHandler};
use std::cell::{Cell, OnceCell};
use std::num::{NonZeroU32, NonZeroUsize};
use windows_sys::Win32::Foundation::POINT;

pub(crate) const BV_WINDOW_MUST_CLOSE: u32 = WM_USER + 1;
// MXM PATCH: the frame clock's message (see `FrameClock`).
pub(crate) const BV_FRAME: u32 = WM_USER + 2;
// MXM PATCH (*Transient for, and the keys given back*): `WindowContext::focus_owner`'s request.
pub(crate) const BV_FOCUS_OWNER: u32 = WM_USER + 3;

use super::drop_target::DropTarget;
use super::*;
use crate::handler::WindowHandlerBuilder;
use crate::host::Host;
use crate::platform::win::window_state::{WindowSharedState, WindowState};
use crate::platform::PlatformError;
use crate::utils::SizingStrategy;
use crate::window::WindowInitializer;
use crate::wrappers::win32::cursor::SystemCursor;
use crate::wrappers::win32::window::*;
use crate::wrappers::win32::{
    ole_initialize, run_thread_message_loop_until, Dpi, DpiAwarenessGuard, LibraryModule, Rect,
    WindowStyle,
};
use crate::{Event, MouseButton, MouseEvent, ScrollDelta, WindowEvent, WindowSize};

fn hi_word(wparam: WPARAM) -> u16 {
    ((wparam >> 16) & 0xffff) as u16
}

fn lo_word(lparam: LPARAM) -> u16 {
    (lparam & 0xffff) as u16
}

const WIN_FRAME_TIMER: NonZeroUsize = match NonZeroUsize::new(4242) {
    Some(x) => x,
    None => unreachable!(),
};

// MXM PATCH: a frame clock that a busy host cannot starve.
//
// `WM_TIMER` is the lowest-priority message Windows has: it is only synthesised when the thread's
// queue holds nothing else, a pending `WM_PAINT` included. A plugin's window lives on the host's
// GUI thread, so a host that always has its next frame queued (redrawing back to back while it
// plays, say) starves the window of every frame, and with them of its mouse and keyboard input,
// which egui only reads in a frame. A posted message is retrieved before input, `WM_PAINT` and
// `WM_TIMER`, so a thread that posts one every frame interval is heard whatever the host does.
// At most one is ever waiting (`pending`), so a stalled window gets no backlog.
//
// `WM_TIMER` stays as the fallback, and `MIN_FRAME_INTERVAL` keeps the two from drawing twice
// per interval.
//
// **The clock leaves the thread a gap after every frame** (`FRAME_GAP`), whatever the screen's
// rate. The same priority that lets a posted frame past a busy host lets it past the window's own
// input: any frame that takes longer than the interval found the next one already posted when it
// ended, and the window drew back to back while the host, its own windows and the mouse waited. A
// vsynced present takes one refresh, so every screen under about 66 Hz did it, and a slow GPU
// does it at any rate (MXM Player froze on a 4K, 59 Hz screen, 2026-10-14). So a frame is posted
// only once the interval has passed since the last one started and the gap since it ended, never
// while one is drawn: a frame of up to the interval less the gap keeps the interval's pace, and a
// longer one is followed by the gap, in which input and the host's drawing run.
//
// **The gap is kept where frames are drawn**, on the window's own thread, for both messages
// (`frame_due`): no frame starts within `MIN_FRAME_INTERVAL` of the last one's start or `FRAME_GAP`
// of its end. The clock's reading of the times is only when to wake: it reads two atomics another
// thread writes, so it can see a frame that has just begun as finished, and post early. A frame
// posted early is refused and its claim let go, which costs a wake-up, never the gap. The claim
// itself (`pending`) is held from the post until the frame it asked for has ended.
const FRAME_INTERVAL: std::time::Duration = std::time::Duration::from_millis(15);
const MIN_FRAME_INTERVAL: std::time::Duration = std::time::Duration::from_millis(10);
const FRAME_GAP: std::time::Duration = std::time::Duration::from_millis(5);

/// When the window's last frame started and ended, in nanoseconds from `origin`: shared with its
/// frame clock, which posts the next frame from them.
struct FrameTimes {
    origin: std::time::Instant,
    started: std::sync::atomic::AtomicU64,
    ended: std::sync::atomic::AtomicU64,
}

impl FrameTimes {
    fn new() -> Self {
        Self {
            origin: std::time::Instant::now(),
            started: std::sync::atomic::AtomicU64::new(0),
            ended: std::sync::atomic::AtomicU64::new(0),
        }
    }

    fn now(&self) -> u64 {
        self.origin.elapsed().as_nanos() as u64
    }
}

/// How long the clock sleeps before it posts a frame, at `now`, after a frame that `started` and
/// `ended` (nanoseconds on one clock): a gap's wait while a frame is drawn (`started > ended`), else
/// whatever is left of the interval since it started and of the gap since it ended.
fn until_due(now: u64, started: u64, ended: u64) -> std::time::Duration {
    use std::time::Duration;
    if started > ended {
        return FRAME_GAP;
    }
    let since_start = Duration::from_nanos(now.saturating_sub(started));
    let since_end = Duration::from_nanos(now.saturating_sub(ended));
    FRAME_INTERVAL.saturating_sub(since_start).max(FRAME_GAP.saturating_sub(since_end))
}

/// Whether a frame may start at `now`, after one that started at `last_start` and ended at
/// `last_end`: on the window's own thread, for the clock's message and `WM_TIMER` alike.
fn frame_due(
    now: std::time::Instant, last_start: Option<std::time::Instant>,
    last_end: Option<std::time::Instant>,
) -> bool {
    let since = |then: Option<std::time::Instant>, wait| {
        then.is_none_or(|then| now.saturating_duration_since(then) >= wait)
    };
    since(last_start, MIN_FRAME_INTERVAL) && since(last_end, FRAME_GAP)
}

struct FrameClock {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl FrameClock {
    fn start(
        hwnd: HWnd, pending: std::sync::Arc<std::sync::atomic::AtomicBool>,
        times: std::sync::Arc<FrameTimes>,
    ) -> Option<Self> {
        use std::sync::atomic::Ordering;

        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        // A window handle is not `Send`; the thread only ever posts to it, which any thread may.
        let raw = hwnd.as_raw() as usize;
        let thread = std::thread::Builder::new()
            .name("baseview frame clock".into())
            .spawn({
                let stop = std::sync::Arc::clone(&stop);
                move || {
                    let mut wait = FRAME_INTERVAL;
                    while !stop.load(Ordering::Acquire) {
                        std::thread::sleep(wait);
                        if stop.load(Ordering::Acquire) {
                            break;
                        }
                        wait = until_due(
                            times.now(),
                            times.started.load(Ordering::Acquire),
                            times.ended.load(Ordering::Acquire),
                        );
                        if !wait.is_zero() {
                            continue;
                        }
                        wait = FRAME_INTERVAL;
                        if pending.swap(true, Ordering::AcqRel) {
                            continue;
                        }
                        // SAFETY: the window outlives this thread: dropping the clock, which the
                        // window does as it is destroyed, stops and joins it first.
                        let posted = unsafe {
                            PostMessageW(
                                raw as windows_sys::Win32::Foundation::HWND,
                                BV_FRAME,
                                0,
                                0,
                            )
                        } != 0;
                        if !posted {
                            pending.store(false, Ordering::Release);
                        }
                    }
                }
            })
            .ok()?;
        Some(Self { stop, thread: Some(thread) })
    }
}

impl Drop for FrameClock {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub struct WindowHandle {
    init: Cell<Option<WindowInitializer>>,
    hwnd: Cell<Option<HWnd>>,
    state: Rc<WindowSharedState>,
}

impl WindowHandle {
    pub fn run_until_closed(self) -> Result<()> {
        self.show()?;

        run_thread_message_loop_until(|| !self.is_open())?;
        Ok(())
    }

    pub fn is_open(&self) -> bool {
        self.state.is_alive.get()
    }

    pub fn is_resizable(&self) -> bool {
        self.state.sizing_strategy.is_resizable()
    }

    pub fn min_size(&self) -> Option<Size> {
        self.state.sizing_strategy.min_size()
    }

    pub fn max_size(&self) -> Option<Size> {
        self.state.sizing_strategy.max_size()
    }

    pub fn size(&self) -> WindowSize {
        self.state.size()
    }

    pub fn resize(&self, new_size: Size) -> Result<()> {
        let new_size = self.state.sizing_strategy.adjust_size(new_size, self.size()).physical;

        if new_size == self.state.current_size.get() {
            return Ok(());
        }

        let hwnd = match self.hwnd.get() {
            Some(hwnd) => hwnd,
            None => {
                self.state.current_size.set(new_size);
                return Ok(());
            }
        };

        let _guard = self.state.originate_host_resize();
        let dpi_ctx =
            DpiAwarenessGuard::new(&self.state.user32, self.state.dpi_scaling_strategy.get())?;
        hwnd.resize_and_activate(new_size, self.state.current_dpi.get(), &dpi_ctx)?;

        if self.state.current_size.get() == new_size {
            Ok(())
        } else {
            Err(PlatformError::UnexpectedResizeResult {
                new_actual_size: self.state.current_size.get(),
            })
        }
    }

    pub fn sizing_strategy(&self) -> SizingStrategy {
        self.state.sizing_strategy
    }

    pub fn suggest_scale_factor(&self, new_host_scale_factor: f64) -> Result<()> {
        let new_dpi = Dpi::from_scale_factor(new_host_scale_factor)
            .ok_or(PlatformError::InvalidScaleFactor)?;

        let current_scale_factor = self.state.scale_factor();
        self.state.fallback_scale_factor.set(Some(new_host_scale_factor));

        let dpi_scaling_strategy = self.state.dpi_scaling_strategy.get();

        if !dpi_scaling_strategy.should_use_host_suggested_scale_factor {
            return Ok(());
        }

        self.state.current_dpi.set(new_dpi);

        let Some(hwnd) = self.hwnd.get() else { return Ok(()) };

        let current_size = self.state.current_size.get();
        let new_size = self
            .state
            .current_size
            .get()
            .to_logical::<f64>(current_scale_factor)
            .to_physical(new_host_scale_factor);

        // This call doesn't meaningfully change the scaling factor, ignore the result
        if current_size == new_size {
            return Ok(());
        }

        let _guard = self.state.originate_host_resize();
        let dpi_ctx = DpiAwarenessGuard::new(&self.state.user32, dpi_scaling_strategy)?;

        hwnd.resize_and_activate(new_size, new_dpi, &dpi_ctx)?;

        if self.state.current_size.get() == new_size {
            Ok(())
        } else {
            Err(PlatformError::UnexpectedResizeResult {
                new_actual_size: self.state.current_size.get(),
            })
        }
    }

    pub fn set_parent(&self, new_parent: ParentWindowHandle) -> Result<()> {
        let hwnd = match self.hwnd.get() {
            Some(hwnd) => hwnd,
            None => {
                let Some(mut init) = self.init.take() else { return Ok(()) };
                init.settings.parent = Some(new_parent.into());

                let window = BaseviewWindow::create(Rc::clone(&self.state), init)?;
                self.hwnd.set(Some(window));

                return Ok(());
            }
        };

        // MXM PATCH (*Transient for, and the keys given back*): a window opened floating is made
        // transient for `new_parent` rather than reparented, which panicked here. CLAP's
        // `gui.set_transient` reaches a floating editor's window this way only, through the editor
        // handle's `set_parent`.
        if !self.state.parented.get() {
            hwnd.set_owner(&new_parent.handle);
            return Ok(());
        }

        hwnd.set_parent(&new_parent.handle)?;

        Ok(())
    }

    #[inline]
    pub fn handle_main_thread_callback(&self) {
        // No-op
    }

    pub fn show(&self) -> Result<()> {
        let hwnd = match self.hwnd.get() {
            Some(hwnd) => hwnd,
            None => {
                let Some(init) = self.init.take() else { return Ok(()) };

                let window = BaseviewWindow::create(Rc::clone(&self.state), init)?;
                self.hwnd.set(Some(window));

                return Ok(());
            }
        };

        hwnd.show_and_activate();

        Ok(())
    }

    pub fn hide(&self) -> Result<()> {
        let Some(hwnd) = self.hwnd.get() else { return Ok(()) };
        hwnd.hide();

        Ok(())
    }
}

impl Drop for WindowHandle {
    fn drop(&mut self) {
        if !self.state.is_alive.get() {
            return;
        }

        if let Some(hwnd) = self.hwnd.take() {
            let _guard = self.state.originate_host_destroy();
            if let Err(e) = hwnd.destroy() {
                warn!("Failed to destroy window: {}", e);
            }
        }
    }
}

pub struct BaseviewWindow {
    window_state: Rc<WindowState>,
    shared_state: Rc<WindowSharedState>,
    initial_size: Size,

    handler_builder: Cell<Option<WindowHandlerBuilder>>,
    handler: OnceCell<Box<dyn WindowHandler>>,
    host: Host,

    // Things not directly used, but kept so their Drop impl runs when the window is destroyed
    _keyboard_hook: Cell<Option<hook::KeyboardHookHandle>>,
    _drop_target: Cell<Option<ComObject<DropTarget>>>,

    // MXM PATCH: the frame clock, its one waiting message, and when the last frame was drawn.
    frame_clock: Cell<Option<FrameClock>>,
    frame_pending: std::sync::Arc<std::sync::atomic::AtomicBool>,
    frame_times: std::sync::Arc<FrameTimes>,
    last_frame: Cell<Option<std::time::Instant>>,
    last_frame_end: Cell<Option<std::time::Instant>>,

    #[cfg(feature = "opengl")]
    gl_config: Option<crate::gl::GlConfig>,
}

impl BaseviewWindow {
    pub fn create(shared_state: Rc<WindowSharedState>, init: WindowInitializer) -> Result<HWnd> {
        shared_state.init(&init);

        let style = WindowStyle::from_settings(&init.settings);
        let parent = init.settings.parent.map(|p| p.inner.handle);

        let dpi_ctx =
            DpiAwarenessGuard::new(&shared_state.user32, shared_state.dpi_scaling_strategy.get())?;

        let window_size = shared_state.current_size.get();

        let initializer = {
            let shared_state = Rc::clone(&shared_state);

            move |hwnd: HWnd| {
                let window_state = Rc::new(WindowState::new(
                    hwnd,
                    shared_state.user32.clone(),
                    Rc::clone(&shared_state),
                ));

                // MXM PATCH: the frame clock starts with the window.
                let frame_pending = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                let frame_times = std::sync::Arc::new(FrameTimes::new());
                let frame_clock = FrameClock::start(
                    hwnd,
                    std::sync::Arc::clone(&frame_pending),
                    std::sync::Arc::clone(&frame_times),
                );

                BaseviewWindow {
                    window_state,
                    frame_clock: Cell::new(frame_clock),
                    frame_pending,
                    frame_times,
                    last_frame: Cell::new(None),
                    last_frame_end: Cell::new(None),
                    initial_size: init.settings.size,
                    handler_builder: Cell::new(Some(init.builder)),
                    handler: OnceCell::new(),
                    shared_state,
                    host: init.host,

                    _drop_target: None.into(),
                    _keyboard_hook: None.into(),

                    #[cfg(feature = "opengl")]
                    gl_config: init.settings.gl_config,
                }
            }
        };

        let rect = dpi_ctx.client_area_to_nc_area(window_size.into(), style, None)?;
        let title = HSTRING::from(init.settings.title);
        let window = create_window(&title, style, rect.size(), parent, &dpi_ctx, initializer)?;

        // FIXME: this SetTimer call could be in after_create, but for some reason it changes the ordering
        // for a parent+child window situation, which results in the parent drawing over the child.
        // This timer should be replaced by proper window redrawing/damage/vsync handling, but this
        // would be a breaking change, so we'll do that later.
        // TODO: create a new timer instead of hard-coding a specific ID
        window.set_timer(WIN_FRAME_TIMER, 15)?;

        Ok(window)
    }

    fn notify_destroyed_to_host(&self) {
        if self.shared_state.destroy_host_originated.get() {
            return;
        };

        self.host.notify_destroyed()
    }

    fn request_host_resize(&self, new_size: WindowSize) -> core::result::Result<(), HandlerError> {
        if self.shared_state.resize_host_originated.get() {
            return Ok(());
        };

        self.host.request_resize(new_size)
    }

    fn adapt_host_window_to_size(
        &self, previous_size: PhysicalSize<u32>, new_size: WindowSize,
    ) -> core::result::Result<(), ()> {
        if let Err(e) = self.request_host_resize(new_size) {
            warn!("Resize request from Host failed: {}. Reverting to previous size.", e);

            if let Some(handler) = self.handler.get() {
                if let Err(e) = handler.resized(new_size) {
                    warn!("Window Handler failed to resize to previous window size: {}", e);
                }
            };

            self.window_state.shared.current_size.set(previous_size);
            if let Err(e) = self.window_state.resize(previous_size.into()) {
                warn!("Failed to resize back to previous window size: {}", e);
            }

            return Err(());
        }

        Ok(())
    }

    // MXM PATCH: a frame from either clock, when one is due (`frame_due`), decided here on the
    // window's own thread; its start and end published for the clock. None while the window is
    // off screen (`on_screen`).
    fn handle_on_frame_paced(&self) {
        if !on_screen(self.window_state.hwnd.as_raw()) {
            return;
        }
        let now = std::time::Instant::now();
        if !frame_due(now, self.last_frame.get(), self.last_frame_end.get()) {
            return;
        }
        self.last_frame.set(Some(now));
        let times = &self.frame_times;
        times.started.store(times.now(), std::sync::atomic::Ordering::Release);
        self.handle_on_frame();
        self.last_frame_end.set(Some(std::time::Instant::now()));
        times.ended.store(times.now(), std::sync::atomic::Ordering::Release);
    }

    pub(crate) fn handle_on_frame(&self) {
        let Some(handler) = self.handler.get() else { return };

        if let Err(e) = handler.on_frame() {
            warn!("Error while rendering frame: {}", e);
            self.window_state.request_close();
        }
    }

    pub(crate) fn handle_event(&self, event: Event) -> EventStatus {
        let Some(handler) = self.handler.get() else {
            return EventStatus::Ignored;
        };

        handler.on_event(event)
    }
}

impl Drop for BaseviewWindow {
    fn drop(&mut self) {
        self.shared_state.is_alive.set(false);
        self.notify_destroyed_to_host();
    }
}

impl WindowImpl for BaseviewWindow {
    fn non_client_create(&self, window: HWnd) -> std::result::Result<(), PlatformError> {
        if self.shared_state.dpi_scaling_strategy.get().should_enable_nc_dpi_scaling_manually {
            window.enable_non_client_dpi_scaling(&self.shared_state.user32);
        }

        Ok(())
    }

    fn after_create(&self, window: HWnd) -> core::result::Result<(), PlatformError> {
        let window_state = &self.window_state;

        self._keyboard_hook.set(Some(hook::init_keyboard_hook(window.as_raw())));

        // Now we can get the actual dpi of the window.
        let dpi = window_state
            .shared
            .dpi_scaling_strategy
            .get()
            .get_dpi_for_window(window, &self.shared_state.user32);

        if let Some(dpi) = dpi {
            if dpi != window_state.shared.current_dpi.get() {
                window_state.shared.current_dpi.set(dpi);

                // We cannot create a window in "logical" pixels, and we can't DPI-scale to physical pixels because we
                // have no way to know where the window will end up.
                // So, at window creation, we assume a DPI=96 (or parent DPI if available), and if it ends up wrong, we resize the window
                // to the actual logical size the user desired.
                let new_size = self.initial_size.to_physical(dpi.scale_factor());

                // Preemptively update so a synchronous WM_SIZE from SetWindowPos below
                // doesn't also emit Resized.
                let previous_size = window_state.shared.current_size.replace(new_size);
                let guard = DpiAwarenessGuard::new(
                    &window_state.shared.user32,
                    self.shared_state.dpi_scaling_strategy.get(),
                )?;
                window.resize_and_activate(new_size, dpi, &guard)?;

                let _ = self.adapt_host_window_to_size(
                    previous_size,
                    WindowSize::from_physical(new_size, dpi.scale_factor()),
                );
            } else {
                // If the host queried size before it gave us a parent, this is the one it's been using.
                let previous_host_size = self.initial_size.to_physical(1.0);
                let current_size = self.initial_size.to_physical(dpi.scale_factor());

                let _ = self.adapt_host_window_to_size(
                    previous_host_size,
                    WindowSize::from_physical(current_size, dpi.scale_factor()),
                );
            }
        }

        let drop_target = ComObject::new(DropTarget::new(Rc::downgrade(window_state), window));
        self._drop_target.set(Some(drop_target.clone()));

        ole_initialize()?;
        window.register_drag_drop(drop_target.as_interface())?;

        #[cfg(feature = "opengl")]
        if let Some(gl_config) = self.gl_config.clone() {
            let gl_context = gl::GlContextInner::create(window, gl_config)?;

            let Ok(()) = self.window_state.gl_context.set(Rc::new(gl_context)) else {
                unreachable!();
            };
        };

        let handler = {
            let context = crate::WindowContext::new(Rc::clone(&self.window_state));
            let Some(handler_builder) = self.handler_builder.take() else {
                unreachable!();
            };

            handler_builder.build(context)?
        };
        let Ok(()) = self.handler.set(handler) else { unreachable!() };

        Ok(())
    }

    unsafe fn handle_message(
        &self, window: HWnd, msg: u32, wparam: WPARAM, lparam: LPARAM,
    ) -> Option<LRESULT> {
        unsafe { wnd_proc_inner(window, msg, wparam, lparam, self) }
    }

    fn before_destroy(&self, window: HWnd) {
        let _ = window.revoke_drag_drop();
        // MXM PATCH: the frame clock stops, its thread joined, while the handle is still this
        // window's. `WM_DESTROY` comes before Windows frees it, but the window itself may drop only
        // after, when `DestroyWindow` was called from inside one of its own messages
        // (`BV_WINDOW_MUST_CLOSE`), and a clock still running then could post to a handle reused
        // by another window. The join waits at most one of the clock's sleeps.
        drop(self.frame_clock.take());
    }
}

/// Our custom `wnd_proc` handler. If the result contains a value, then this is returned after
/// handling any deferred tasks. otherwise the default window procedure is invoked.
#[allow(clippy::unwrap_used, reason = "Refactor this in a later PR")] // TODO
unsafe fn wnd_proc_inner(
    window: HWnd, msg: u32, wparam: WPARAM, lparam: LPARAM, window_bv: &BaseviewWindow,
) -> Option<LRESULT> {
    let window_state = &window_bv.window_state;
    match msg {
        WM_MOUSEMOVE => {
            if window_state.mouse_was_outside_window.get() {
                // this makes Windows track whether the mouse leaves the window.
                // When the mouse leaves it results in a `WM_MOUSELEAVE` event.
                // Couldn't find a good way to track whether the mouse enters,
                // but if `WM_MOUSEMOVE` happens, the mouse must have entered.
                let _ = window.start_cursor_leave_tracking();
                window_state.mouse_was_outside_window.set(false);

                let enter_event = Event::Mouse(MouseEvent::CursorEntered);
                window_bv.handle_event(enter_event);
            }

            let x = (lparam & 0xFFFF) as i16 as i32;
            let y = ((lparam >> 16) & 0xFFFF) as i16 as i32;

            let move_event = Event::Mouse(MouseEvent::CursorMoved {
                position: PhysicalPosition { x, y }.cast(),
                modifiers: window_state
                    .keyboard_state
                    .borrow()
                    .get_modifiers_from_mouse_wparam(wparam),
            });

            window_bv.handle_event(move_event);
            Some(0)
        }

        WM_MOUSELEAVE => {
            window_bv.handle_event(Event::Mouse(MouseEvent::CursorLeft));

            window_state.mouse_was_outside_window.set(true);
            Some(0)
        }
        WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
            let value = (wparam >> 16) as i16;
            let value = value as i32;
            let value = value as f32 / WHEEL_DELTA as f32;

            let event = Event::Mouse(MouseEvent::WheelScrolled {
                delta: if msg == WM_MOUSEWHEEL {
                    ScrollDelta::Lines { x: 0.0, y: value }
                } else {
                    ScrollDelta::Lines { x: value, y: 0.0 }
                },
                modifiers: window_state
                    .keyboard_state
                    .borrow()
                    .get_modifiers_from_mouse_wparam(wparam),
            });

            window_bv.handle_event(event);
            Some(0)
        }
        WM_LBUTTONDOWN | WM_LBUTTONUP | WM_MBUTTONDOWN | WM_MBUTTONUP | WM_RBUTTONDOWN
        | WM_RBUTTONUP | WM_XBUTTONDOWN | WM_XBUTTONUP => {
            let mut mouse_button_counter = window_state.mouse_button_counter.get();

            fn get_xbutton_wparam(wparam: WPARAM) -> u16 {
                hi_word(wparam)
            }

            const XBUTTON1: u16 = 0x1;
            const XBUTTON2: u16 = 0x2;

            let button = match msg {
                WM_LBUTTONDOWN | WM_LBUTTONUP => Some(MouseButton::Left),
                WM_MBUTTONDOWN | WM_MBUTTONUP => Some(MouseButton::Middle),
                WM_RBUTTONDOWN | WM_RBUTTONUP => Some(MouseButton::Right),
                WM_XBUTTONDOWN | WM_XBUTTONUP => match get_xbutton_wparam(wparam) {
                    XBUTTON1 => Some(MouseButton::Back),
                    XBUTTON2 => Some(MouseButton::Forward),
                    _ => None,
                },
                _ => None,
            };

            if let Some(button) = button {
                let event = match msg {
                    WM_LBUTTONDOWN | WM_MBUTTONDOWN | WM_RBUTTONDOWN | WM_XBUTTONDOWN => {
                        // Capture the mouse cursor on button down
                        mouse_button_counter = mouse_button_counter.saturating_add(1);
                        window.set_capture();
                        MouseEvent::ButtonPressed {
                            button,
                            modifiers: window_state
                                .keyboard_state
                                .borrow()
                                .get_modifiers_from_mouse_wparam(wparam),
                        }
                    }
                    WM_LBUTTONUP | WM_MBUTTONUP | WM_RBUTTONUP | WM_XBUTTONUP => {
                        // Release the mouse cursor capture when all buttons are released
                        mouse_button_counter = mouse_button_counter.saturating_sub(1);
                        if mouse_button_counter == 0 {
                            HWnd::release_capture();
                        }

                        MouseEvent::ButtonReleased {
                            button,
                            modifiers: window_state
                                .keyboard_state
                                .borrow()
                                .get_modifiers_from_mouse_wparam(wparam),
                        }
                    }
                    _ => {
                        unreachable!()
                    }
                };

                window_state.mouse_button_counter.set(mouse_button_counter);
                window_bv.handle_event(Event::Mouse(event));
            }

            None
        }
        WM_CAPTURECHANGED => {
            let window_gaining_capture =
                NonNull::new(lparam as _).map(|p| unsafe { HWnd::from_raw(p) });

            if window_gaining_capture != Some(window) {
                let previous_counter = window_state.mouse_button_counter.replace(0);
                HWnd::release_capture();

                if previous_counter == 0 {
                    window_bv.handle_event(Event::Mouse(MouseEvent::CursorLeft));
                    window_state.mouse_was_outside_window.set(true);
                }
            }

            Some(0)
        }
        WM_TIMER => {
            if wparam == WIN_FRAME_TIMER.get() {
                // MXM PATCH: paced against the frame clock below.
                window_bv.handle_on_frame_paced()
            }

            Some(0)
        }
        // MXM PATCH: the frame clock's message. Its claim is let go once the frame has ended (or
        // been refused), so the clock cannot post the next while this one is drawn.
        BV_FRAME => {
            window_bv.handle_on_frame_paced();
            window_bv.frame_pending.store(false, std::sync::atomic::Ordering::Release);
            Some(0)
        }
        WM_CLOSE => {
            window_bv.handle_event(Event::Window(WindowEvent::WillClose));

            None
        }
        WM_CHAR | WM_SYSCHAR | WM_KEYDOWN | WM_SYSKEYDOWN | WM_KEYUP | WM_SYSKEYUP
        | WM_INPUTLANGCHANGE => {
            let opt_event = window_state.keyboard_state.borrow_mut().process_message(
                window.as_raw(),
                msg,
                wparam,
                lparam,
            );

            let status = opt_event.map(|event| window_bv.handle_event(Event::Keyboard(event)));
            // MXM PATCH (*Keys the editor ignores go to the host*)
            let given =
                window_state.keys_given_back.borrow_mut().decide(msg, wparam, lparam, status);
            if !given.is_empty() {
                give_keys_to_parent(window.as_raw(), &given);
            }

            if msg != WM_SYSKEYDOWN {
                Some(0)
            } else {
                None
            }
        }
        WM_SETFOCUS => {
            window_bv.handle_event(Event::Window(WindowEvent::Focused));

            None
        }
        WM_KILLFOCUS => {
            window_bv.handle_event(Event::Window(WindowEvent::Unfocused));

            None
        }
        WM_SIZE => {
            let width = (lparam & 0xFFFF) as u16 as u32;
            let height = ((lparam >> 16) & 0xFFFF) as u16 as u32;

            let new_size = PhysicalSize { width, height };
            let current_size = window_state.shared.current_size.get();

            // Only send the event if anything changed
            if current_size == new_size {
                return None;
            }

            let previous = window_state.shared.current_size.replace(new_size);
            let new_size = WindowSize::from_physical(new_size, window_state.shared.scale_factor());

            let handler = window_bv.handler.get()?;
            if let Err(e) = handler.resized(new_size) {
                warn!("Window Handler failed to resize: {}", e);
                window_state.shared.current_size.set(previous);

                if let Err(e) = window_state.resize(previous.into()) {
                    warn!("Failed to resize back to previous window size: {}", e);
                }

                return Some(-1);
            }

            if let Err(e) = window_bv.request_host_resize(new_size) {
                warn!("Resize request from Host failed: {}. Reverting to previous size.", e);

                if let Err(e) = handler.resized(new_size) {
                    warn!("Window Handler failed to resize to previous window size: {}", e);
                }

                window_state.shared.current_size.set(previous);
                if let Err(e) = window_state.resize(previous.into()) {
                    warn!("Failed to resize back to previous window size: {}", e);
                }

                return Some(-1);
            }

            None
        }
        WM_DPICHANGED => {
            let suggested_nc_rect = Rect((lparam as *const RECT).read());
            let Some(dpi) = NonZeroU32::new((wparam & 0xFFFF) as u16 as u32) else {
                return Some(-1);
            };
            let dpi = Dpi(dpi);

            let dpi_ctx = DpiAwarenessGuard::new(
                &window_state.user32,
                window_state.shared.dpi_scaling_strategy.get(),
            )
            .unwrap();
            let style = window.get_style().unwrap();
            let suggested_rect =
                dpi_ctx.nc_area_to_client_area(suggested_nc_rect, style, Some(dpi)).unwrap();

            let new_size = suggested_rect.size();

            let changed = window_state.shared.current_size.get() != new_size
                || window_state.shared.current_dpi.get() != dpi;

            window_state.shared.current_dpi.set(dpi);
            let previous_size = window_state.shared.current_size.replace(new_size);

            // Windows makes us resize the window manually. This however will not send a WM_SIZE event,
            // hence why we are notifying the window handler manually below.
            let _ = window.set_nc_rect(suggested_nc_rect);

            if changed {
                let handler = window_bv.handler.get()?;
                let new_size = WindowSize::from_physical(new_size, dpi.scale_factor());

                if let Err(e) = handler.resized(new_size) {
                    warn!("Window Handler failed to resize: {}", e);
                    window_state.shared.current_size.set(previous_size);

                    if let Err(e) = window_state.resize(previous_size.into()) {
                        warn!("Failed to resize back to previous window size: {}", e);
                    }

                    return Some(-1);
                }

                if let Err(()) = window_bv.adapt_host_window_to_size(previous_size, new_size) {
                    return Some(-1);
                }
            }

            None
        }
        // If WM_SETCURSOR returns `None`, WM_SETCURSOR continues to get handled by the outer window(s),
        // If it returns `Some(1)`, the current window decides what the cursor is
        WM_SETCURSOR => {
            let low_word = lo_word(lparam) as u32;
            let mouse_in_window = low_word == HTCLIENT;
            if mouse_in_window {
                // Here we need to set the cursor back to what the state says, since it can have changed when outside the window
                if let Ok(cursor) = SystemCursor::load(window_state.cursor_icon.get()) {
                    cursor.set()
                }
                Some(1)
            } else {
                // Cursor is being changed by some other window, e.g. when having mouse on the borders to resize it
                None
            }
        }
        WM_GETMINMAXINFO => {
            let sizing = window_state.shared.sizing_strategy;

            // Only implement this message if we actually need to specify a min/max size
            if let (None, None) = (sizing.min_size(), sizing.max_size()) {
                return None;
            }

            let info = lparam as *mut MINMAXINFO;

            let ctx = DpiAwarenessGuard::new(
                &window_state.user32,
                window_state.shared.dpi_scaling_strategy.get(),
            )
            .unwrap();
            let style = window.get_style().unwrap();
            let dpi = window_state.shared.current_dpi.get();

            if let Some(size) = sizing.min_size() {
                let size = size.to_physical(window_state.shared.scale_factor());
                let size = ctx
                    .client_area_to_nc_area(size.into(), style, Some(dpi))
                    .unwrap()
                    .size()
                    .cast();
                let pt = POINT { x: size.width, y: size.height };
                (&raw mut (*info).ptMinTrackSize).write(pt);
            }

            if let Some(size) = sizing.max_size() {
                let size = size.to_physical(window_state.shared.scale_factor());
                let size = ctx
                    .client_area_to_nc_area(size.into(), style, Some(dpi))
                    .unwrap()
                    .size()
                    .cast();
                let pt = POINT { x: size.width, y: size.height };
                (&raw mut (*info).ptMaxTrackSize).write(pt);
            }

            Some(0)
        }
        // NOTE: `WM_NCDESTROY` is handled in the outer function because this deallocates the window
        //        state
        BV_WINDOW_MUST_CLOSE => {
            let _ = window.destroy();
            Some(0)
        }
        // MXM PATCH (*Transient for, and the keys given back*): carried out here, outside the
        // handler's calls (see `WindowState::focus_owner`).
        BV_FOCUS_OWNER => {
            window.focus_owner();
            Some(0)
        }
        _ => None,
    }
}

impl WindowHandle {
    pub fn create_window(init: WindowInitializer) -> Result<WindowHandle> {
        let extended_user_32 = LibraryModule::load()?;

        let shared_state = WindowSharedState::new(extended_user_32, &init);

        if init.settings.wait_for_parent && init.settings.parent.is_none() {
            return Ok(WindowHandle {
                hwnd: None.into(),
                state: shared_state,
                init: Some(init).into(),
            });
        }

        let window = BaseviewWindow::create(Rc::clone(&shared_state), init)?;

        Ok(WindowHandle { hwnd: Some(window).into(), state: shared_state, init: None.into() })
    }
}

pub fn copy_to_clipboard(_data: &str) {
    unimplemented!()
}

/// MXM PATCH: whether `hwnd` can be seen: it and every ancestor visible, and its top-level window
/// not minimised. A host may hide a plugin's window rather than close it: DaVinci Resolve 21.1
/// hides the dialog an editor is embedded in, and six hidden MXM editors drawing there held its
/// GUI thread in OpenGL presents until its own UI froze for seconds (2026-10-27).
fn on_screen(hwnd: windows_sys::Win32::Foundation::HWND) -> bool {
    // SAFETY: plain Win32 queries on a window handle this thread's window procedure was given; a
    // handle gone stale since makes them answer false, nothing more.
    unsafe { IsWindowVisible(hwnd) != 0 && IsIconic(GetAncestor(hwnd, GA_ROOT)) == 0 }
}

// MXM PATCH: the frame clock's pacing, at the rates screens and GPUs give it.
/// MXM PATCH (*Keys the editor ignores go to the host*): posts `messages` to the window `hwnd` is
/// embedded in. A top-level window, floating, has no parent to give them to.
fn give_keys_to_parent(
    hwnd: windows_sys::Win32::Foundation::HWND, messages: &[(u32, WPARAM, LPARAM)],
) {
    // SAFETY: plain Win32 calls on a window handle this thread's window procedure was given; a
    // handle gone stale since makes them fail, nothing more.
    unsafe {
        let parent = GetAncestor(hwnd, GA_PARENT);
        if parent.is_null() || parent == GetDesktopWindow() {
            return;
        }
        for &(msg, wparam, lparam) in messages {
            PostMessageW(parent, msg, wparam, lparam);
        }
    }
}

#[cfg(test)]
mod frame_clock_tests {
    use super::{frame_due, until_due, FRAME_GAP, FRAME_INTERVAL, MIN_FRAME_INTERVAL};
    use std::time::{Duration, Instant};

    const MS: u64 = 1_000_000;

    /// After a frame of `frame_ns` drawn from 100 ms: the start-to-start period until the next is
    /// due, and the gap the thread is left between them.
    fn after_a_frame_of(frame_ns: u64) -> (Duration, Duration) {
        let started = 100 * MS;
        let ended = started.saturating_add(frame_ns);
        let wait = until_due(ended, started, ended);
        (Duration::from_nanos(frame_ns).saturating_add(wait), wait)
    }

    /// Up to the interval less the gap, 10 ms, a frame keeps the interval's pace.
    #[test]
    fn a_fast_frame_keeps_the_intervals_pace() {
        // A vsynced present at 240, 144 and 120 Hz.
        for frame_ns in [4_166_667, 6_944_444, 8_333_333] {
            let (period, gap) = after_a_frame_of(frame_ns);
            assert_eq!(period, FRAME_INTERVAL, "{frame_ns} ns");
            assert!(gap >= FRAME_GAP, "{frame_ns} ns leaves {gap:?}");
        }
    }

    /// **The freeze**: at 60, 59.94, 50 and 30 Hz, or any slow frame, the next frame used to be
    /// posted before this one ended. Now the thread always gets the gap.
    #[test]
    fn a_frame_slower_than_the_interval_leaves_the_thread_its_gap() {
        for frame_ns in [16_666_667, 16_683_350, 20_000_000, 33_333_333, 250 * MS] {
            let (_, gap) = after_a_frame_of(frame_ns);
            assert_eq!(gap, FRAME_GAP, "{frame_ns} ns");
        }
    }

    #[test]
    fn nothing_is_posted_while_a_frame_is_drawn() {
        // Started at 100 ms, the previous one ended at 90 ms: drawing now, however late it is.
        for now in [101 * MS, 130 * MS, 10_000 * MS] {
            assert!(!until_due(now, 100 * MS, 90 * MS).is_zero());
        }
    }

    #[test]
    fn once_due_it_is_posted_at_once() {
        let started = 100 * MS;
        let ended = started + 2 * MS;
        assert!(until_due(started + 15 * MS, started, ended).is_zero());
        // The first frame: nothing drawn yet, so due once the clock has slept its interval.
        assert!(until_due(15 * MS, 0, 0).is_zero());
    }

    /// **No frame starts inside the gap**, whichever message asks: a `WM_TIMER` due the moment a
    /// slow clock frame ends waits the gap out, as a clock frame does.
    #[test]
    fn no_frame_starts_inside_the_gap_after_a_slow_one() {
        let start = Instant::now();
        let end = start + Duration::from_nanos(16_900_000);
        for ms in [0, 1, 4] {
            let now = end + Duration::from_millis(ms);
            assert!(!frame_due(now, Some(start), Some(end)), "{ms} ms after the end");
        }
        assert!(frame_due(end + FRAME_GAP, Some(start), Some(end)));
    }

    /// A fast frame is held to `MIN_FRAME_INTERVAL` from its start, as before the gap existed.
    #[test]
    fn a_fast_frame_waits_out_the_minimum_interval() {
        let start = Instant::now();
        let end = start + Duration::from_millis(2);
        assert!(!frame_due(start + Duration::from_millis(9), Some(start), Some(end)));
        assert!(frame_due(start + MIN_FRAME_INTERVAL, Some(start), Some(end)));
        assert!(frame_due(start, None, None), "the first frame");
    }

    /// **The race the clock cannot win**: it read the times of the frame before (a finished one,
    /// due) while a newer one began and ended, and posted. The window refuses the frame it asked
    /// for, because the newer one ended inside the gap.
    #[test]
    fn a_frame_posted_on_a_stale_reading_is_refused() {
        let ms = |n: u64| n * 1_000_000;
        assert!(until_due(ms(140), ms(100), ms(117)).is_zero(), "the stale reading: due");
        let origin = Instant::now();
        let at = |n: u64| origin + Duration::from_nanos(ms(n));
        // The frame drawn meanwhile: 122 to 139 ms. The posted frame arrives at 140.
        assert!(!frame_due(at(140), Some(at(122)), Some(at(139))));
    }
}
