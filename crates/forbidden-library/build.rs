//! Embeds the app icon in the executable, so Explorer, the taskbar, and
//! shortcuts show it.

fn main() {
    println!("cargo:rerun-if-changed=../../assets/icon/app-icon.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        winresource::WindowsResource::new()
            .set_icon("../../assets/icon/app-icon.ico")
            .compile()
            .expect("the app icon could not be embedded");
    }
}
