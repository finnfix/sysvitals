//! Build script for app-win: embeds the application icon into the PE
//! resource section so the exe shows the SysVitals icon in Explorer,
//! taskbar, and window chrome.

fn main() {
    // Only embed resources when targeting Windows (avoids breakage if
    // someone runs cargo check on a non-Windows host with -p app-win).
    if std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default() != "windows" {
        return;
    }

    let mut res = winres::WindowsResource::new();
    res.set_icon("assets/icon.ico");
    res.set("ProductName", "SysVitals");
    res.set("FileDescription", "SysVitals — lightweight system monitor");
    res.set("LegalCopyright", "MIT License");
    if let Err(e) = res.compile() {
        // Non-fatal: the binary still works, just without an icon.
        println!("cargo:warning=Failed to embed resources: {e}");
    }
}
