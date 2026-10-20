//! Build script for app-settings: embeds the application icon into the PE
//! resource section so the settings window shows the SysVitals icon in its
//! title bar and taskbar entry on Windows.

fn main() {
    // Only embed resources when targeting Windows (avoids breakage on
    // non-Windows hosts running cargo check on this crate).
    if std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default() != "windows" {
        return;
    }

    let mut res = winres::WindowsResource::new();
    res.set_icon("../app-win/assets/icon.ico");
    res.set("ProductName", "SysVitals");
    res.set("FileDescription", "SysVitals Settings");
    res.set("LegalCopyright", "MIT License");
    if let Err(e) = res.compile() {
        // Non-fatal: the binary still works, just without an icon.
        println!("cargo:warning=Failed to embed resources: {e}");
    }
}
