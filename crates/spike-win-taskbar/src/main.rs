//! Windows taskbar-embedding spike (throwaway risk gate).
//!
//! Before the formal display layer is built, this spike must prove four
//! things on a real desktop: embedding without flicker, staying clear of the
//! tray, surviving an explorer restart via the `TaskbarCreated` broadcast, and
//! readable text in both themes. The stub compiles on any host; it only does
//! the real Win32 work when built on Windows.

fn main() {
    println!("spike-win-taskbar: spike not implemented yet. Build on Windows MSVC after adding Win32/GDI deps.");
}
