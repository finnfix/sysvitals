//! Windows display layer. Stub that compiles on any host so the
//! workspace resolves; the Win32 + Direct2D/DirectWrite taskbar indicator is
//! built only on Windows.
//!
//! Binary name is `sysvitals` (the process/exe name).
//! The pure sampling/format logic it will consume is exercised now via
//! `cargo run -p sysvitals-core --example monitor`.

fn main() {
    println!("app-win: display layer not implemented yet. Build on Windows MSVC after adding Win32 + D2D deps.");
}
