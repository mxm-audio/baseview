use crate::dpi::{PhysicalSize, Size};
use crate::platform::win::dpi::DpiScalingStrategy;
use crate::platform::win::keyboard::KeyboardState;
use crate::platform::PlatformHandle;
use crate::utils::SizingStrategy;
use crate::window::WindowInitializer;
use crate::wrappers::win32::cursor::SystemCursor;
use crate::wrappers::win32::h_instance::HInstance;
use crate::wrappers::win32::window::HWnd;
use crate::wrappers::win32::{Dpi, DpiAwarenessGuard, ExtendedUser32, LibraryModule};
use crate::{EventStatus, MouseCursor, WindowSize};
use raw_window_handle::{DisplayHandle, Win32WindowHandle};
use std::cell::{Cell, Ref, RefCell};
use std::num::NonZeroIsize;
use std::rc::Rc;
use windows_sys::Win32::Foundation::{LPARAM, WPARAM};
use windows_sys::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_CHAR, WM_KEYDOWN, WM_KEYUP};

/// All data associated with the window.
pub(crate) struct WindowState {
    /// The HWND belonging to this window.
    pub hwnd: HWnd,
    pub keyboard_state: RefCell<KeyboardState>,
    // MXM PATCH (*Keys the editor ignores go to the host*)
    pub keys_given_back: RefCell<KeysGivenBack>,
    pub mouse_button_counter: Cell<usize>,
    pub mouse_was_outside_window: Cell<bool>,
    pub cursor_icon: Cell<MouseCursor>,

    pub user32: LibraryModule<ExtendedUser32>,
    pub shared: Rc<WindowSharedState>,

    #[cfg(feature = "opengl")]
    pub gl_context: std::cell::OnceCell<super::gl::GlContext>,
}

impl WindowState {
    pub fn new(
        hwnd: HWnd, user32: LibraryModule<ExtendedUser32>, shared: Rc<WindowSharedState>,
    ) -> Self {
        Self {
            hwnd,
            keyboard_state: RefCell::new(KeyboardState::new()),
            keys_given_back: RefCell::default(),
            mouse_button_counter: Cell::new(0),
            mouse_was_outside_window: true.into(),
            cursor_icon: Cell::new(MouseCursor::Default),
            user32,
            shared,

            #[cfg(feature = "opengl")]
            gl_context: std::cell::OnceCell::new(),
        }
    }

    /// Returns the current size of this window.
    pub fn size(&self) -> WindowSize {
        self.shared.size()
    }

    /// Returns the current scale factor of this window.
    pub fn scale_factor(&self) -> f64 {
        self.shared.scale_factor()
    }

    pub(crate) fn keyboard_state(&self) -> Ref<'_, KeyboardState> {
        self.keyboard_state.borrow()
    }

    pub fn request_close(&self) {
        unsafe {
            PostMessageW(
                self.hwnd.as_raw(),
                crate::platform::win::window::BV_WINDOW_MUST_CLOSE,
                0,
                0,
            );
        }
    }

    pub fn has_focus(&self) -> bool {
        HWnd::get_focused_window() == self.hwnd.as_raw()
    }

    pub fn focus(&self) -> Result<(), super::PlatformError> {
        self.hwnd.set_focus()?;
        Ok(())
    }

    /// MXM PATCH (*Transient for, and the keys given back*): see [`HWnd::focus_owner`], posted
    /// as `request_close` is. Carried out at once, inside a frame or an event, it would activate
    /// the owner on this thread there and then, and this window would hear it lost the focus while
    /// its handler is still busy: egui-baseview's state is borrowed, and the panic aborts the host.
    pub fn focus_owner(&self) -> Result<(), super::PlatformError> {
        // SAFETY: This type guarantees the HWND is safe to use.
        unsafe {
            PostMessageW(self.hwnd.as_raw(), crate::platform::win::window::BV_FOCUS_OWNER, 0, 0);
        }
        Ok(())
    }

    pub fn resize(&self, size: Size) -> Result<(), super::PlatformError> {
        // `self.window_info` will be modified in response to the `WM_SIZE` event that
        // follows the `SetWindowPos()` call
        let dpi = self.shared.current_dpi.get();
        let new_size = size.to_physical(self.shared.scale_factor());

        let ctx = DpiAwarenessGuard::new(&self.user32, self.shared.dpi_scaling_strategy.get())?;

        self.hwnd.resize_and_activate(new_size, dpi, &ctx)?;
        Ok(())
    }

    pub fn set_mouse_cursor(&self, mouse_cursor: MouseCursor) -> Result<(), super::PlatformError> {
        self.cursor_icon.set(mouse_cursor);
        if let Ok(cursor) = SystemCursor::load(mouse_cursor) {
            cursor.set()
        }

        Ok(())
    }

    #[cfg(feature = "opengl")]
    pub fn gl_context(&self) -> Option<crate::gl::GlContext> {
        Some(crate::gl::GlContext::new(Rc::clone(self.gl_context.get()?)))
    }

    pub fn window_handle(&self) -> Option<raw_window_handle::WindowHandle<'_>> {
        let Some(hwnd) = NonZeroIsize::new(self.hwnd.as_raw() as _) else { unreachable!() };
        let mut handle = Win32WindowHandle::new(hwnd);
        handle.hinstance = Some(HInstance::get_from_dll().addr());

        Some(unsafe { raw_window_handle::WindowHandle::borrow_raw(handle.into()) })
    }

    pub fn display_handle(&self) -> DisplayHandle<'_> {
        DisplayHandle::windows()
    }

    pub fn platform_handle(&self) -> PlatformHandle {
        let Some(hwnd) = NonZeroIsize::new(self.hwnd.as_raw() as _) else { unreachable!() };
        PlatformHandle { hwnd }
    }
}

pub struct WindowSharedState {
    pub parented: Cell<bool>,
    pub is_alive: Cell<bool>,
    pub current_size: Cell<PhysicalSize<u32>>,
    pub current_dpi: Cell<Dpi>,
    pub fallback_scale_factor: Cell<Option<f64>>,
    pub resize_host_originated: Cell<bool>,
    pub destroy_host_originated: Cell<bool>,
    pub dpi_scaling_strategy: Cell<DpiScalingStrategy>,

    pub user32: LibraryModule<ExtendedUser32>,
    pub sizing_strategy: SizingStrategy,
}

impl WindowSharedState {
    pub fn new(user32: LibraryModule<ExtendedUser32>, init: &WindowInitializer) -> Rc<Self> {
        let state = Self {
            sizing_strategy: SizingStrategy::from_settings(&init.settings),
            user32,

            // Internal state
            is_alive: true.into(),
            resize_host_originated: false.into(),
            destroy_host_originated: false.into(),

            // Overriden by init() below
            parented: false.into(),
            current_dpi: Dpi::default().into(),
            current_size: init.settings.size.to_physical(1.0).into(),
            fallback_scale_factor: init.settings.fallback_scale_factor.into(),
            dpi_scaling_strategy: DpiScalingStrategy::default().into(),
        };

        state.init(init);

        state.into()
    }

    pub fn init(&self, init: &WindowInitializer) {
        let parent = init.settings.parent.as_ref().map(|p| p.inner.handle);
        let strategy = DpiScalingStrategy::get(
            Some(&self.user32),
            parent,
            #[cfg(feature = "opengl")]
            &init.settings,
        );

        if strategy.assume_96_dpi {
            self.current_dpi.set(Dpi::default());
            self.current_size.set(init.settings.size.to_physical(1.0));
            self.fallback_scale_factor.set(Some(1.0));
        } else {
            let parent_dpi = parent.and_then(|p| p.get_dpi(&self.user32));
            let scale_factor = parent_dpi
                .map(|dpi| dpi.scale_factor())
                .or(init.settings.fallback_scale_factor)
                .unwrap_or(1.0);

            self.current_dpi.set(parent_dpi.unwrap_or_default());
            self.current_size.set(init.settings.size.to_physical(scale_factor));
            self.fallback_scale_factor.set(init.settings.fallback_scale_factor);
        }

        self.parented.set(init.settings.parent.is_some() || init.settings.wait_for_parent);
        self.dpi_scaling_strategy.set(strategy);
    }

    pub fn size(&self) -> WindowSize {
        WindowSize::from_physical(self.current_size.get(), self.scale_factor())
    }

    pub fn scale_factor(&self) -> f64 {
        let strategy = self.dpi_scaling_strategy.get();
        if strategy.assume_96_dpi {
            return 1.0;
        }

        if strategy.should_use_host_suggested_scale_factor {
            return self.fallback_scale_factor.get().unwrap_or(1.0);
        }

        self.current_dpi.get().scale_factor()
    }

    pub fn originate_host_resize(&self) -> impl Drop + use<'_> {
        self.resize_host_originated.set(true);
        Guard(&self.resize_host_originated)
    }

    pub fn originate_host_destroy(&self) -> impl Drop + use<'_> {
        self.destroy_host_originated.set(true);
        Guard(&self.destroy_host_originated)
    }
}

struct Guard<'a>(&'a Cell<bool>);
impl<'a> Drop for Guard<'a> {
    fn drop(&mut self) {
        self.0.set(false);
    }
}

/// MXM PATCH (*Keys the editor ignores go to the host*): which key messages an embedded window
/// gives its parent, as JUCE does (`forwardMessageToParent`) and baseview's macOS view does (an
/// ignored key goes to the next responder). A host that hands its plugin window every key, as
/// Ableton Live 11.3.22 does (measured 2026-10-16), plays or starts its transport with the ones the
/// editor ignores only if they come back. `WM_SYS*` keys are not given: Windows' default handling
/// already takes them to the top-level window.
#[derive(Debug, Default)]
pub(crate) struct KeysGivenBack {
    /// A key-down whose character follows: it is given, or not, with that character.
    pending_down: Option<(u32, WPARAM, LPARAM)>,
    /// Each key that is down, by virtual-key code, and whether its key-down was given: its key-up
    /// goes the same way, whatever the handler says of it.
    down: std::collections::HashMap<WPARAM, bool>,
}

impl KeysGivenBack {
    /// The messages to give the parent for `msg`, which the window's handler answered `status`
    /// (`None`: no event yet).
    pub(crate) fn decide(
        &mut self, msg: u32, wparam: WPARAM, lparam: LPARAM, status: Option<EventStatus>,
    ) -> Vec<(u32, WPARAM, LPARAM)> {
        let ignored = status == Some(EventStatus::Ignored);
        let mut given = Vec::new();
        match msg {
            WM_KEYDOWN => match status {
                None => self.pending_down = Some((msg, wparam, lparam)),
                Some(_) => {
                    self.pending_down = None;
                    self.down.insert(wparam, ignored);
                    if ignored {
                        given.push((msg, wparam, lparam));
                    }
                }
            },
            WM_CHAR if status.is_some() => {
                if let Some(down) = self.pending_down.take() {
                    self.down.insert(down.1, ignored);
                    if ignored {
                        given.push(down);
                    }
                }
                if ignored {
                    given.push((msg, wparam, lparam));
                }
            }
            // Its key-down's way, or, with no key-down seen, its own.
            WM_KEYUP if self.down.remove(&wparam).unwrap_or(ignored) => {
                given.push((msg, wparam, lparam));
            }
            _ => {}
        }
        given
    }
}

#[cfg(test)]
mod keys_given_back_tests {
    use super::*;
    use windows_sys::Win32::UI::WindowsAndMessaging::{WM_SYSKEYDOWN, WM_SYSKEYUP};

    const T: WPARAM = 0x54;
    const UP: WPARAM = 0x26;

    #[test]
    fn an_ignored_key_goes_to_the_parent_down_and_up() {
        let mut keys = KeysGivenBack::default();
        let ignored = Some(EventStatus::Ignored);
        assert_eq!(keys.decide(WM_KEYDOWN, T, 1, ignored), [(WM_KEYDOWN, T, 1)]);
        assert_eq!(keys.decide(WM_KEYUP, T, 2, ignored), [(WM_KEYUP, T, 2)]);
    }

    #[test]
    fn a_captured_key_stays_and_so_does_its_key_up_whatever_it_is_called() {
        let mut keys = KeysGivenBack::default();
        assert!(keys.decide(WM_KEYDOWN, UP, 1, Some(EventStatus::Captured)).is_empty());
        assert!(keys.decide(WM_KEYUP, UP, 2, Some(EventStatus::Ignored)).is_empty());
    }

    /// A letter's key-down waits for its character, then both go, in order, or neither.
    #[test]
    fn a_key_down_with_a_character_is_given_with_it() {
        let mut keys = KeysGivenBack::default();
        assert!(keys.decide(WM_KEYDOWN, T, 1, None).is_empty());
        assert_eq!(
            keys.decide(WM_CHAR, 't' as WPARAM, 1, Some(EventStatus::Ignored)),
            [(WM_KEYDOWN, T, 1), (WM_CHAR, 't' as WPARAM, 1)]
        );
        assert_eq!(keys.decide(WM_KEYUP, T, 2, Some(EventStatus::Ignored)).len(), 1);

        assert!(keys.decide(WM_KEYDOWN, T, 1, None).is_empty());
        assert!(keys.decide(WM_CHAR, 't' as WPARAM, 1, Some(EventStatus::Captured)).is_empty());
        assert!(keys.decide(WM_KEYUP, T, 2, Some(EventStatus::Ignored)).is_empty());
    }

    #[test]
    fn system_keys_are_left_to_windows() {
        let mut keys = KeysGivenBack::default();
        let ignored = Some(EventStatus::Ignored);
        assert!(keys.decide(WM_SYSKEYDOWN, T, 1, ignored).is_empty());
        assert!(keys.decide(WM_SYSKEYUP, T, 2, ignored).is_empty());
    }
}
