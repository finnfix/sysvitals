# SysVitals

A lightweight CPU / memory / network monitor living in your taskbar (Windows) and menu bar (macOS).

SysVitals shows live percentages and transfer rates directly in the system status bar, follows
your system font, DPI and light/dark theme, and opens a small history chart when clicked. It is
built in Rust and aims to stay visually native and negligibly cheap to run 24/7.

## Status

**Early development.** The platform-independent core is implemented and unit-tested:

- `crates/core` — sampling (CPU / memory / network differentials), formatting, configuration and i18n
- `crates/app-win` — Windows taskbar display layer (planned)
- `crates/app-mac` — macOS menu-bar display layer (planned)
- `crates/ui-common` — shared settings window and tray (planned, GPUI)

No tagged or packaged releases yet.

## Planned features

- Taskbar (Windows) / menu bar (macOS) indicator for CPU, memory, upload and download
- Configurable prefixes, single-line or dual-row layout, tray-adjacent or far-left anchoring on Windows
- Binary units with `K/s` and `M/s` rate formatting
- Light / dark theme following the system
- Click to open a CPU/memory history chart, with one-click CSV copy
- English and Chinese UI

## Building the core

Requirements: a stable Rust toolchain (`rust-toolchain.toml` pins it).

```sh
cargo build            # cross-platform crates only (core + ui-common)
cargo test             # core unit tests
cargo run -p sysvitals-core --example monitor   # terminal demo of the sampling loop
```

Platform display crates are isolated from the default build and are compiled explicitly on
their own OS:

```sh
cargo build -p app-win    # Windows, MSVC toolchain
cargo build -p app-mac    # macOS
```

## Design notes

SysVitals keeps all error-prone logic — counter differencing, wrap-around protection, NIC
selection, unit selection, layout — in the pure-Rust `core` crate, where it is unit-tested on
any platform. The status-bar surfaces themselves are drawn by thin, platform-native glue:
a Win32 child window embedded in the taskbar (Windows) and `NSStatusItem` (macOS). Taskbar
embedding is not an officially supported API; the app treats it as best-effort and degrades
to a tray item if the system layout changes.

## License

Released under the MIT License. See [LICENSE](LICENSE).
