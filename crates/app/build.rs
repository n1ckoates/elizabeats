//! On Windows, embeds the app icon into `elizabeats.exe`, so the taskbar,
//! Start menu and installer shortcuts show it. Nothing to do elsewhere: the
//! macOS bundle and Linux package carry their own icon files.

fn main() {
    #[cfg(windows)]
    {
        println!("cargo::rerun-if-changed=../../assets/icon.ico");
        let mut resource = winresource::WindowsResource::new();
        resource.set_icon("../../assets/icon.ico");
        if let Err(error) = resource.compile() {
            println!("cargo::warning=could not embed the app icon: {error}");
        }
    }
}
