//! Embeds the app icon in the executable, so Explorer, the taskbar, and
//! shortcuts show it.

fn main() {
    println!("cargo:rerun-if-changed=../../assets/icon/app-icon.ico");
    // Only Windows executables carry a resource section. The crate is a
    // Windows-only dependency, so the code that uses it must be gated too.
    #[cfg(windows)]
    {
        winresource::WindowsResource::new()
            .set_icon("../../assets/icon/app-icon.ico")
            .compile()
            .expect("the app icon could not be embedded");
    }
}
