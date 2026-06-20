//! Build script: on Windows, embed the app icon into the executable as a resource so Explorer,
//! the taskbar, and the default window icon all show it. No-op on other hosts.

fn main() {
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        if let Err(e) = res.compile() {
            println!("cargo:warning=failed to embed Windows icon: {e}");
        }
    }
}
