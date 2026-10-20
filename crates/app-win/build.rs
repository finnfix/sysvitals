//! Build script for app-win: embeds the application icon into the PE
//! resource section so the exe shows the SysVitals icon in Explorer,
//! taskbar, and window chrome.

fn main() {
    #[cfg(windows)]
    embed_resources();
}

/// Compile-time gate: `winres` is a Windows-only build-dependency, so the
/// `winres::` path must not exist on other hosts at all. A runtime
/// `CARGO_CFG_TARGET_OS` check is too late — the path is resolved before
/// `main` runs, so `cargo check`/build on a non-Windows host fails with E0433.
#[cfg(windows)]
fn embed_resources() {
    // The runtime check still matters for a Windows host cross-compiling to
    // a non-Windows target: `winres` is linked there, but embedding PE
    // resources would break the foreign binary.
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
