//! Platform clipboard, abstracted so the rest of the app is platform-agnostic.
//!
//! - Linux/Wayland: `smithay-clipboard`, driven from our own `wl_display`/seat (core `wl_data_device`),
//!   so it works on KWin and any Wayland compositor without XWayland or data-control protocols.
//!   It also gives us the primary selection (middle-click paste).
//! - Linux/X11: `arboard`, including primary selection for middle-click paste.
//! - macOS/Windows: the system clipboard via `arboard`. There is no primary selection there, so
//!   those calls are no-ops.
//!
//! Methods take `&self` (the arboard backend uses interior mutability) so call sites don't care
//! which platform they're on.

use winit::window::Window;

pub struct Clipboard(Backend);

enum Backend {
    #[cfg(target_os = "linux")]
    Wayland(smithay_clipboard::Clipboard),
    #[cfg(any(target_os = "linux", windows, target_os = "macos"))]
    Arboard(std::cell::RefCell<arboard::Clipboard>),
    // Keeps the type inhabited (and matches exhaustive) on platforms with no backend.
    #[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
    Disabled,
}

impl Clipboard {
    /// Create the clipboard for the window's display backend, or `None` if unavailable.
    #[allow(unused_variables)]
    pub fn new(window: &Window) -> Option<Self> {
        #[cfg(target_os = "linux")]
        {
            use raw_window_handle::{HasDisplayHandle, RawDisplayHandle};
            match window.display_handle().ok()?.as_raw() {
                RawDisplayHandle::Wayland(h) => {
                    // The app drops its clipboard before tearing down the window/connection.
                    let cb = unsafe { smithay_clipboard::Clipboard::new(h.display.as_ptr()) };
                    Some(Self(Backend::Wayland(cb)))
                }
                RawDisplayHandle::Xlib(_) | RawDisplayHandle::Xcb(_) => arboard::Clipboard::new()
                    .ok()
                    .map(|c| Self(Backend::Arboard(std::cell::RefCell::new(c)))),
                _ => None,
            }
        }
        #[cfg(any(windows, target_os = "macos"))]
        {
            arboard::Clipboard::new()
                .ok()
                .map(|c| Self(Backend::Arboard(std::cell::RefCell::new(c))))
        }
        #[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
        {
            None
        }
    }

    /// Set the system clipboard.
    pub fn store(&self, text: String) {
        match &self.0 {
            #[cfg(target_os = "linux")]
            Backend::Wayland(c) => c.store(text),
            #[cfg(any(target_os = "linux", windows, target_os = "macos"))]
            Backend::Arboard(c) => {
                let _ = c.borrow_mut().set_text(text);
            }
            #[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
            Backend::Disabled => {}
        }
    }

    /// Read the system clipboard.
    pub fn load(&self) -> Option<String> {
        match &self.0 {
            #[cfg(target_os = "linux")]
            Backend::Wayland(c) => c.load().ok(),
            #[cfg(any(target_os = "linux", windows, target_os = "macos"))]
            Backend::Arboard(c) => c.borrow_mut().get_text().ok(),
            #[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
            Backend::Disabled => None,
        }
    }

    /// Set the primary selection (Linux middle-click source). No-op where unsupported.
    pub fn store_primary(&self, text: String) {
        match &self.0 {
            #[cfg(target_os = "linux")]
            Backend::Wayland(c) => c.store_primary(text),
            #[cfg(target_os = "linux")]
            Backend::Arboard(c) => {
                use arboard::{LinuxClipboardKind, SetExtLinux};
                let _ = c
                    .borrow_mut()
                    .set()
                    .clipboard(LinuxClipboardKind::Primary)
                    .text(text);
            }
            #[cfg(any(windows, target_os = "macos"))]
            Backend::Arboard(_) => {
                let _ = text;
            }
            #[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
            Backend::Disabled => {}
        }
    }

    /// Read the primary selection. `None` where unsupported.
    pub fn load_primary(&self) -> Option<String> {
        match &self.0 {
            #[cfg(target_os = "linux")]
            Backend::Wayland(c) => c.load_primary().ok(),
            #[cfg(target_os = "linux")]
            Backend::Arboard(c) => {
                use arboard::{GetExtLinux, LinuxClipboardKind};
                c.borrow_mut()
                    .get()
                    .clipboard(LinuxClipboardKind::Primary)
                    .text()
                    .ok()
            }
            #[cfg(any(windows, target_os = "macos"))]
            Backend::Arboard(_) => None,
            #[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
            Backend::Disabled => None,
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use winit::application::ApplicationHandler;
    use winit::event::WindowEvent;
    use winit::event_loop::{ActiveEventLoop, EventLoop};
    use winit::platform::x11::EventLoopBuilderExtX11;
    use winit::window::WindowId;

    struct X11ClipboardTest;

    impl ApplicationHandler for X11ClipboardTest {
        fn resumed(&mut self, event_loop: &ActiveEventLoop) {
            let window = event_loop
                .create_window(Window::default_attributes().with_visible(false))
                .unwrap();
            let clipboard = Clipboard::new(&window).expect("X11 clipboard backend");
            assert!(matches!(clipboard.0, Backend::Arboard(_)));
            let reader = Clipboard::new(&window).unwrap();
            let text = "clipboard: \u{03bb}\n".repeat(10_000);
            clipboard.store(text.clone());
            clipboard.store_primary("primary selection".into());
            assert_eq!(reader.load().as_deref(), Some(text.as_str()));
            assert_eq!(reader.load_primary().as_deref(), Some("primary selection"));
            clipboard.store("replacement".into());
            assert_eq!(reader.load().as_deref(), Some("replacement"));
            assert_eq!(reader.load_primary().as_deref(), Some("primary selection"));
            event_loop.exit();
        }

        fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}
    }

    #[test]
    #[ignore = "requires an isolated X11 server; modifies its clipboard and primary selection"]
    fn x11_clipboard_and_primary_are_independent() {
        let event_loop = EventLoop::builder()
            .with_x11()
            .with_any_thread(true)
            .build()
            .unwrap();
        event_loop.run_app(&mut X11ClipboardTest).unwrap();
    }
}
