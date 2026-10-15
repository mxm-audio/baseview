use crate::dpi::Size;
use crate::platform::macos::view::BaseviewView;
use crate::platform::Result;
use crate::platform::{PlatformHandle, WindowSharedState};
use crate::wrappers::appkit::{View, ViewRef};
use crate::*;
use dispatch2::{DispatchQueue, MainThreadBound};
use objc2::rc::{autoreleasepool, Weak};
use objc2::runtime::NSObjectProtocol;
use objc2::{MainThreadMarker, Message};
use raw_window_handle::DisplayHandle;
use std::rc::Rc;

#[derive(Clone)]
pub struct WindowContext {
    mtm: MainThreadMarker,
    view: Weak<View<BaseviewView>>,
    state: Rc<WindowSharedState>,
}

impl WindowContext {
    pub(crate) fn new(view: ViewRef<'_, BaseviewView>) -> Self {
        Self {
            view: Weak::from_retained(&view.view.retain()),
            state: Rc::clone(&view.state),
            mtm: view.mtm,
        }
    }

    pub fn request_close(&self) {
        let Some(view) = self.view.load() else { return };
        let Some(view) = view.inner_ref() else { return };

        autoreleasepool(|_| {
            BaseviewView::close(view, false);
        });
    }

    pub fn has_focus(&self) -> bool {
        let Some(view) = self.view.load() else { return false };
        let Some(window) = view.window() else {
            return false;
        };

        if !window.isKeyWindow() {
            return false;
        }

        let Some(first_responder) = window.firstResponder() else {
            return false;
        };

        view.isEqual(Some(&*first_responder))
    }

    pub fn focus(&self) -> Result<()> {
        let Some(view) = self.view.load() else { return Ok(()) };

        autoreleasepool(|_| {
            if let Some(window) = view.window() {
                window.makeFirstResponder(Some(&view));
            }
        });

        Ok(())
    }

    /// MXM PATCH (*Transient for, and the keys given back*): gives the keyboard back to the window
    /// this one belongs to: the window a floating one is a child of is made key; embedded, the view
    /// it is in becomes the first responder. On the main queue, after the handler's call: done at
    /// once, inside a frame or an event, this window would resign and its handler hear of it while
    /// still busy, as on Windows.
    pub fn focus_owner(&self) -> Result<()> {
        let view = MainThreadBound::new(self.view.clone(), self.mtm);

        DispatchQueue::main().exec_async(move || {
            let Some(mtm) = MainThreadMarker::new() else { return };
            let Some(view) = view.get(mtm).load() else { return };

            autoreleasepool(|_| {
                let Some(window) = view.window() else { return };
                if let Some(parent) = window.parentWindow() {
                    parent.makeKeyAndOrderFront(None);
                // SAFETY: on the main thread; the superview comes back retained.
                } else if let Some(superview) = unsafe { view.superview() } {
                    window.makeFirstResponder(Some(&superview));
                }
            });
        });

        Ok(())
    }

    pub fn resize(&self, size: Size) -> Result<()> {
        if self.state.closed.get() {
            return Ok(());
        }

        let Some(view) = self.view.load() else { return Ok(()) };
        let Some(view) = view.inner_ref() else { return Ok(()) };

        let size = self.state.sizing_strategy.adjust_size(size, self.size()).logical;

        autoreleasepool(|_| {
            BaseviewView::resize(view, size, true, false);
        });

        Ok(())
    }

    pub fn set_mouse_cursor(&self, cursor: MouseCursor) -> Result<()> {
        let Some(view) = self.view.load() else { return Ok(()) };
        let Some(view) = view.inner_ref() else { return Ok(()) };

        autoreleasepool(|_| {
            view.inner.cursor_manager.set_cursor(cursor);
        });

        Ok(())
    }

    pub fn size(&self) -> WindowSize {
        WindowSize::from_logical(self.state.size.get(), self.state.scale_factor.get())
    }

    pub fn scale_factor(&self) -> f64 {
        self.state.scale_factor.get()
    }

    #[cfg(feature = "opengl")]
    pub fn gl_context(&self) -> Option<crate::gl::GlContext> {
        autoreleasepool(|_| {
            Some(crate::gl::GlContext::new(self.view.load()?.inner()?.gl_context.get()?.clone()))
        })
    }

    pub fn window_handle(&self) -> Option<raw_window_handle::WindowHandle<'_>> {
        View::window_handle_from_weak(&self.view)
    }

    pub fn display_handle(&self) -> DisplayHandle<'_> {
        DisplayHandle::appkit()
    }

    pub fn platform_handle(&self) -> PlatformHandle {
        PlatformHandle { inner: MainThreadBound::new(self.view.clone(), self.mtm) }
    }
}
