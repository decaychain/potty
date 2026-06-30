//! Platform clipboard, abstracted so the rest of the app is platform-agnostic.
//!
//! - Linux: `smithay-clipboard`, driven from our own `wl_display`/seat (core `wl_data_device`),
//!   so it works on KWin and any Wayland compositor without XWayland or data-control protocols.
//!   It also gives us the primary selection (middle-click paste).
//! - macOS/Windows: the system clipboard via `arboard`. There is no primary selection there, so
//!   those calls are no-ops.
//!
//! Methods take `&self` (the Windows backend uses interior mutability) so call sites don't care
//! which platform they're on.

use winit::window::Window;

pub struct Clipboard(Backend);

enum Backend {
    #[cfg(target_os = "linux")]
    Wayland(smithay_clipboard::Clipboard),
    #[cfg(any(windows, target_os = "macos"))]
    Arboard(std::cell::RefCell<arboard::Clipboard>),
    // Keeps the type inhabited (and matches exhaustive) on platforms with no backend.
    #[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
    Disabled,
}

impl Clipboard {
    /// Create the platform clipboard, or `None` if unavailable (e.g. non-Wayland Linux).
    #[allow(unused_variables)]
    pub fn new(window: &Window) -> Option<Self> {
        #[cfg(target_os = "linux")]
        {
            use raw_window_handle::{HasDisplayHandle, RawDisplayHandle};
            if let Ok(RawDisplayHandle::Wayland(h)) = window.display_handle().map(|h| h.as_raw()) {
                let cb = unsafe { smithay_clipboard::Clipboard::new(h.display.as_ptr()) };
                return Some(Self(Backend::Wayland(cb)));
            }
            None
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
            #[cfg(any(windows, target_os = "macos"))]
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
            #[cfg(any(windows, target_os = "macos"))]
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
            #[cfg(any(windows, target_os = "macos"))]
            Backend::Arboard(_) => None,
            #[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
            Backend::Disabled => None,
        }
    }
}
