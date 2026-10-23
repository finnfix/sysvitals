//! Windows display layer: a topmost layered overlay parked over the taskbar.
//!
//! Why an overlay and not a `SetParent` child of `Shell_TrayWnd`: on current
//! Windows 11 builds the taskbar's own content is composited by a XAML Islands
//! child window whose visual sits above every GDI child of the tray, so an
//! embedded child window can never render crisply (verified empirically with
//! calibration windows: an embedded opaque child is blurred and tinted by the
//! taskbar acrylic, while an identical topmost layered popup is pixel-crisp).
//! A topmost layered popup aligned to the taskbar geometry gives the same
//! visual result through a mechanism the compositor actually honors.
//!
//! Consequences handled here:
//! - The overlay does not share the taskbar's z-order or visibility, so each
//!   tick re-checks whether the taskbar is present (auto-hide). The indicator
//!   hides only when the taskbar itself leaves the screen; no foreground-window
//!   heuristics are involved.
//! - Nothing is embedded, so there is no explorer-side state to roll back on
//!   exit; destroying the window is the whole teardown.
//!
//! Settings architecture (post-refactor):
//! The settings UI lives in a separate process (`SysVitalsSettings.exe`,
//! built with GPUI). This overlay process does NOT host any settings dialog.
//! Instead it watches the shared config file for changes: a 500 ms poll timer
//! compares the file's last-modified timestamp and, on change, reloads the
//! config and re-renders. The context menu "Settings" item simply spawns the
//! settings executable from the same directory. This decoupling keeps the
//! overlay process minimal (no UI toolkit, no modal loops) and lets the
//! settings app evolve independently.
//!
//! Typography mirrors the taskbar clock: Segoe UI at the system caption size,
//! semibold weight, and pure white / pure black glyphs per theme so the
//! indicator reads as part of the taskbar rather than a foreign widget.
//! Rendering is GDI text; the layout strings come from the pure core crate so
//! the single-row / dual-row quadrant rules stay unit-tested there. Glyphs are
//! drawn at 2x supersampled resolution and box-filtered down for smooth edges
//! without ClearType (which corrupts per-pixel-alpha compositing).

#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
mod win {
    use std::cell::RefCell;
    use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
    use std::sync::{OnceLock, RwLock};
    use std::time::{SystemTime, UNIX_EPOCH};

    use sysvitals_core::format::FormattedRow;
    use sysvitals_core::{
        Config, NetSelection, Sampler, Theme, format_frame, format_frame_vertical, t,
    };
    use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
    use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_TIMEOUT};
    use windows::Win32::Graphics::Gdi::{
        AC_SRC_ALPHA, AC_SRC_OVER, ANTIALIASED_QUALITY, BI_RGB, BITMAPINFO, BITMAPINFOHEADER,
        BLENDFUNCTION, CLIP_DEFAULT_PRECIS, CreateCompatibleDC, CreateDIBSection, CreateFontW,
        DEFAULT_CHARSET, DEFAULT_PITCH, DIB_RGB_COLORS, DT_CALCRECT, DT_CENTER, DT_LEFT,
        DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, DeleteDC, DeleteObject, DrawTextW, FF_SWISS,
        FW_REGULAR, FW_SEMIBOLD, GetDC, HFONT, OUT_DEFAULT_PRECIS, ReleaseDC, SelectObject,
        SetBkMode, SetTextColor, TRANSPARENT,
    };
    use windows::Win32::System::Diagnostics::Debug::{
        EXCEPTION_POINTERS, SetUnhandledExceptionFilter,
    };
    use windows::Win32::System::LibraryLoader::{
        GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
        GetModuleFileNameW, GetModuleHandleExW, GetModuleHandleW,
    };
    use windows::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_READ, REG_VALUE_TYPE, RegCloseKey, RegOpenKeyExW,
        RegQueryValueExW,
    };
    use windows::Win32::System::SystemInformation::GetLocalTime;
    use windows::Win32::System::Threading::CreateMutexW;
    use windows::Win32::System::Threading::TerminateProcess;
    use windows::Win32::System::Threading::WaitForSingleObject;
    use windows::Win32::UI::HiDpi::{
        DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForSystem, GetDpiForWindow,
        SetProcessDpiAwarenessContext,
    };
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent,
    };
    use windows::Win32::UI::Shell::{
        ABE_LEFT, ABE_RIGHT, ABM_GETTASKBARPOS, APPBARDATA, SEE_MASK_NOCLOSEPROCESS,
        SHAppBarMessage, SHELLEXECUTEINFOW, ShellExecuteExW, ShellExecuteW,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        AppendMenuW, CS_HREDRAW, CS_VREDRAW, CreatePopupMenu, CreateWindowExW, DefWindowProcW,
        DestroyMenu, DestroyWindow, DispatchMessageW, FindWindowExW, FindWindowW, GWL_EXSTYLE,
        GetCursorPos, GetMessageW, GetSystemMetrics, GetWindowLongPtrW, GetWindowRect,
        HWND_TOPMOST, IsWindowVisible, KillTimer, MF_STRING, MSG, MoveWindow, NONCLIENTMETRICSW,
        PostMessageW, PostQuitMessage, RegisterClassExW, SM_CXSCREEN, SM_CYSCREEN,
        SPI_GETNONCLIENTMETRICS, SW_HIDE, SW_SHOWNOACTIVATE, SW_SHOWNORMAL, SWP_NOACTIVATE,
        SWP_NOMOVE, SWP_NOSIZE, SetForegroundWindow, SetTimer, SetWindowPos, ShowWindow,
        SystemParametersInfoW, TPM_LEFTALIGN, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu,
        TranslateMessage, ULW_ALPHA, UpdateLayeredWindow, WM_DESTROY, WM_DISPLAYCHANGE,
        WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSELEAVE, WM_MOUSEMOVE, WM_NULL, WM_RBUTTONUP,
        WM_SETTINGCHANGE, WM_TIMER, WNDCLASSEXW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
        WS_EX_TOPMOST, WS_POPUP,
    };

    // ------------------------------------------------------------------ consts

    /// Gap between the widget's edge and its anchor (tray left / taskbar left).
    const TRAY_GAP_PX: i32 = 0;
    const TIMER_SAMPLE: usize = 0x515;
    const TIMER_MOVE: usize = 0x516;
    /// Timer id for the config-file change poller.
    const TIMER_CONFIG: usize = 0x517;
    const MOVE_MS: u32 = 1_000;
    /// How often (ms) we poll the config file's mtime for external changes.
    const CONFIG_POLL_MS: u32 = 500;
    /// Custom message posted to the overlay to apply a config change. The
    /// overlay re-samples, resizes and re-renders in its own wndproc at top
    /// level, which is always safe (no reentrant rendering).
    const WM_APP_APPLY: u32 = 0x8000 + 1;
    /// Horizontal padding around the measured text.
    const PAD_PX: i32 = 6;
    /// Extra width slack added when sizing the overlay. Sizing measures text at
    /// 1x with the overlay font, but rendering draws at 2x and downsamples; the
    /// two rounded widths do not match exactly, so without slack the last
    /// characters get clipped.
    const WIDTH_SLACK_PX: i32 = 8;
    /// Font size for the vertical (left/right) taskbar layout, as a fraction
    /// of the system UI size expressed in hundredths. On a vertical taskbar
    /// the bar's *thickness* — not its length — is the width budget: a typical
    /// bar is ~48 logical px wide, so the system size would clip; the stacked
    /// label/value column plus this reduction keeps every line inside the bar.
    /// Horizontal taskbars are limited by height, not width, so they keep the
    /// system size untouched.
    ///
    /// 0.75× (was 0.7×): matches the macOS dual-row scale factor exactly so
    /// the two platforms shrink the same system font by the same amount.
    const VERTICAL_FONT_HUNDREDTHS: i32 = 75;
    /// Horizontal padding for the vertical layout: the thickness is the binding
    /// constraint, so the text keeps a tighter margin than on horizontal bars.
    const VERTICAL_PAD_PX: i32 = 3;
    /// Width slack for the vertical layout (same rounding compensation as
    /// `WIDTH_SLACK_PX`, trimmed because every pixel counts against the bar).
    const VERTICAL_WIDTH_SLACK_PX: i32 = 4;
    const THEME_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize";
    const THEME_VALUE: &str = "SystemUsesLightTheme";
    const TASKBAR_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\Advanced";
    const TASKBAR_VALUE: &str = "TaskbarAl";

    const IDM_SETTINGS: usize = 901;
    const IDM_QUIT: usize = 902;
    const IDM_RESTART: usize = 903;
    /// Right-click menu entry that launches Task Manager. This used to be the
    /// double-click action on the indicator; moving it here is what lets the
    /// left click fire the trend chart immediately (see `WM_LBUTTONUP`).
    const IDM_TASKMGR: usize = 904;

    // ----------------------------------------------------------------- statics

    static WIN_W: AtomicI32 = AtomicI32::new(0);
    static WIN_H: AtomicI32 = AtomicI32::new(0);
    /// Whether we already attempted to open the history store. A failed open
    /// is logged once and never retried, so a broken data dir does not spam
    /// the log on every tick.
    static STORE_ATTEMPTED: AtomicBool = AtomicBool::new(false);
    /// Cached `SystemUsesLightTheme` (1 = light taskbar). Refreshed on
    /// `WM_SETTINGCHANGE` so a live theme switch repaints correctly.
    static LIGHT_THEME: AtomicU32 = AtomicU32::new(0);
    /// Glyph weight follows the taskbar theme, like the clock: semibold on
    /// the dark taskbar, regular on the light one.
    static TEXT_WEIGHT: AtomicI32 = AtomicI32::new(600);
    static CONFIG: OnceLock<RwLock<Config>> = OnceLock::new();
    /// Track whether the taskbar is currently vertical so we can reset the
    /// grow-only width when orientation flips.
    static TASKBAR_VERTICAL: AtomicBool = AtomicBool::new(false);
    /// Mouse hover over the indicator rectangle (requirement: a tray-like
    /// focus background). Set on the first `WM_MOUSEMOVE` after a leave,
    /// cleared on `WM_MOUSELEAVE`; drives the pill in `render_layered`.
    static HOVER: AtomicBool = AtomicBool::new(false);
    /// Left button currently pressed on the indicator (the "clicked" tint).
    /// Set on `WM_LBUTTONDOWN`, cleared on `WM_LBUTTONUP` / `WM_RBUTTONUP`.
    static PRESSED: AtomicBool = AtomicBool::new(false);
    /// Whether a `TrackMouseEvent` leave request is already armed, so each
    /// `WM_MOUSEMOVE` does not re-register one. Reset when `WM_MOUSELEAVE`
    /// fires.
    static TRACK_ARMED: AtomicBool = AtomicBool::new(false);
    /// Last observed mtime (seconds since UNIX epoch) of the config file.
    /// Compared on each CONFIG_POLL_MS tick to detect external edits.
    static CONFIG_MTIME: AtomicI32 = AtomicI32::new(0);

    thread_local! {
        static SAMPLER: RefCell<Option<Sampler>> = const { RefCell::new(None) };
        static LAST_ROW: RefCell<Option<FormattedRow>> = const { RefCell::new(None) };
        // History store. Lazily opened on the first sample so a missing
        // data dir never blocks the indicator itself.
        static STORE: RefCell<Option<sysvitals_core::Store>> = const { RefCell::new(None) };
        static TICK_COUNT: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
        // Child process handles, kept so we can detect a still-running
        // instance and avoid stacking duplicate windows.
        static SETTINGS_PROCESS: RefCell<Option<HANDLE>> = const { RefCell::new(None) };
        static HISTORY_PROCESS: RefCell<Option<HANDLE>> = const { RefCell::new(None) };
    }

    // ------------------------------------------------------------ config access

    fn config() -> Config {
        CONFIG
            .get_or_init(|| {
                // A missing or broken config file must not stop the indicator:
                // fall back to defaults and keep running.
                let loaded = sysvitals_core::config::config_path()
                    .and_then(|p| Config::load(&p).map(|(c, _)| c))
                    .unwrap_or_default();
                RwLock::new(loaded)
            })
            .read()
            .expect("config lock")
            .clone()
    }

    fn store_config(new: Config) {
        if let Some(slot) = CONFIG.get() {
            *slot.write().expect("config lock") = new;
        }
    }

    // ----------------------------------------------------------------- logging

    /// Current local date/time strings from the OS clock.
    fn local_now() -> (String, String) {
        let st = unsafe { GetLocalTime() };
        (
            format!("{:04}-{:02}-{:02}", st.wYear, st.wMonth, st.wDay),
            format!("{:02}:{:02}:{:02}", st.wHour, st.wMinute, st.wSecond),
        )
    }

    /// Append one line to today's log file under the data dir. Logging must
    /// never break the indicator, so failures are silently ignored.
    fn log_event(level: &str, message: &str) {
        let Ok(dir) = sysvitals_core::config::data_dir() else {
            return;
        };
        let (date, time) = local_now();
        sysvitals_core::logger::log_line(&dir, &date, &time, level, message);
    }

    /// Remove log files older than the retention window. Run once at startup.
    fn prune_old_logs() {
        let Ok(dir) = sysvitals_core::config::data_dir() else {
            return;
        };
        let (date, _) = local_now();
        sysvitals_core::logger::prune_logs(
            &dir,
            &date,
            sysvitals_core::logger::DEFAULT_RETENTION_DAYS,
        );
    }

    // ------------------------------------------------------------------ helpers

    fn utf16(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Read a DWORD from HKCU; `None` when the key or value is absent.
    unsafe fn hkcu_dword(key: &str, value: &str) -> Option<u32> {
        let key = utf16(key);
        let value = utf16(value);
        let mut hkey = HKEY::default();
        if RegOpenKeyExW(
            HKEY_CURRENT_USER,
            windows::core::PCWSTR(key.as_ptr()),
            None,
            KEY_READ,
            &mut hkey,
        )
        .is_err()
        {
            return None;
        }
        let mut data = [0u8; 4];
        let mut size = 4u32;
        let mut kind = REG_VALUE_TYPE::default();
        let queried = RegQueryValueExW(
            hkey,
            windows::core::PCWSTR(value.as_ptr()),
            None,
            Some(&mut kind),
            Some(data.as_mut_ptr()),
            Some(&mut size),
        );
        let _ = RegCloseKey(hkey);
        if queried.is_ok() && size >= 4 {
            Some(u32::from_le_bytes(data))
        } else {
            None
        }
    }

    unsafe fn refresh_theme() {
        LIGHT_THEME.store(
            hkcu_dword(THEME_KEY, THEME_VALUE).unwrap_or(0).min(1),
            Ordering::Relaxed,
        );
        let weight = if LIGHT_THEME.load(Ordering::Relaxed) == 1 {
            FW_REGULAR.0 as i32
        } else {
            FW_SEMIBOLD.0 as i32
        };
        TEXT_WEIGHT.store(weight, Ordering::Relaxed);
    }

    /// Win11 with centered taskbar icons: the only case where the far-left
    /// anchor is offered.
    unsafe fn win11_centered_taskbar() -> bool {
        hkcu_dword(TASKBAR_KEY, TASKBAR_VALUE) == Some(1)
    }

    /// Whether the effective theme is light (config override wins over the
    /// system flag).
    fn theme_is_light() -> bool {
        match config().display.theme {
            Theme::Auto => LIGHT_THEME.load(Ordering::Relaxed) == 1,
            Theme::Light => true,
            Theme::Dark => false,
        }
    }

    // ------------------------------------------------------------------- fonts

    /// System UI face + pixel height, cached on first use. Read from
    /// `NONCLIENTMETRICS.lfMessageFont` so the indicator's font follows the
    /// system setting (face and size) instead of a hardcoded Segoe UI 12px.
    /// Font changes only take effect after re-login anyway, so caching is fine.
    static UI_FONT: OnceLock<(i32, Vec<u16>)> = OnceLock::new();

    fn ui_font() -> (i32, Vec<u16>) {
        UI_FONT
            .get_or_init(|| unsafe {
                let fallback = || (-(12 * GetDpiForSystem() as i32 / 96), utf16("Segoe UI"));
                let mut nc: NONCLIENTMETRICSW = std::mem::zeroed();
                nc.cbSize = std::mem::size_of::<NONCLIENTMETRICSW>() as u32;
                let ok = SystemParametersInfoW(
                    SPI_GETNONCLIENTMETRICS,
                    std::mem::size_of::<NONCLIENTMETRICSW>() as u32,
                    Some(std::ptr::from_mut(&mut nc).cast::<std::ffi::c_void>()),
                    windows::Win32::UI::WindowsAndMessaging::SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
                )
                .is_ok();
                if !ok {
                    return fallback();
                }
                let lf = &nc.lfMessageFont;
                // lfHeight is negative (character height) in practice, but be
                // explicit: normalize to a negative em height so CreateFontW
                // sizes glyphs exactly like the system does.
                let height = match lf.lfHeight {
                    h if h < 0 => h,
                    h if h > 0 => -h,
                    _ => fallback().0,
                };
                let mut face: Vec<u16> = lf
                    .lfFaceName
                    .iter()
                    .copied()
                    .take_while(|&c| c != 0)
                    .collect();
                if face.is_empty() {
                    face = utf16("Segoe UI");
                } else {
                    face.push(0);
                }
                (height, face)
            })
            .clone()
    }

    /// Effective glyph height in pixels for the current taskbar orientation:
    /// the system UI size on a horizontal bar, reduced by
    /// `VERTICAL_FONT_HUNDREDTHS` on a vertical (left/right) bar where the
    /// bar's thickness — not its length — is the width budget.
    fn effective_font_height() -> i32 {
        let (height, _) = ui_font();
        if TASKBAR_VERTICAL.load(Ordering::Relaxed) {
            // `height` is a negative character height; clamp the reduction so a
            // tiny system font can never produce 0 (which CreateFontW would
            // silently interpret as "default font").
            (height * VERTICAL_FONT_HUNDREDTHS / 100).min(-1)
        } else {
            height
        }
    }

    /// Horizontal padding for the current orientation.
    fn pad_px() -> i32 {
        if TASKBAR_VERTICAL.load(Ordering::Relaxed) {
            VERTICAL_PAD_PX
        } else {
            PAD_PX
        }
    }

    /// Width slack for the current orientation.
    fn width_slack_px() -> i32 {
        if TASKBAR_VERTICAL.load(Ordering::Relaxed) {
            VERTICAL_WIDTH_SLACK_PX
        } else {
            WIDTH_SLACK_PX
        }
    }

    /// Overlay font: the system UI face at the system UI size, grayscale AA.
    /// Antialiased (not ClearType) because the glyphs are composited
    /// per-pixel-alpha over the taskbar.
    unsafe fn make_font() -> HFONT {
        let (_, face) = ui_font();
        CreateFontW(
            effective_font_height(),
            0,
            0,
            0,
            TEXT_WEIGHT.load(Ordering::Relaxed),
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            ANTIALIASED_QUALITY,
            (DEFAULT_PITCH.0 | FF_SWISS.0) as u32,
            windows::core::PCWSTR(face.as_ptr()),
        )
    }

    /// Overlay font at an integer supersample multiplier. Used to render at a
    /// supersampled scale (2x) before downsampling.
    unsafe fn make_font_at(scale: i32) -> HFONT {
        let (_, face) = ui_font();
        CreateFontW(
            effective_font_height() * scale.max(1),
            0,
            0,
            0,
            TEXT_WEIGHT.load(Ordering::Relaxed),
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            ANTIALIASED_QUALITY,
            (DEFAULT_PITCH.0 | FF_SWISS.0) as u32,
            windows::core::PCWSTR(face.as_ptr()),
        )
    }

    // ------------------------------------------------------- taskbar geometry

    /// Taskbar geometry: rect in screen coordinates plus orientation.
    struct TaskbarInfo {
        rect: RECT,
        vertical: bool,
        /// True when a vertical bar is docked to the *left* screen edge. The
        /// placement code uses it to pick which edge the indicator must never
        /// run past when its column is wider than the bar's thickness.
        left_edge: bool,
    }

    /// Taskbar geometry, or `None` when the taskbar is not currently on screen
    /// (auto-hidden, or shell not ready).
    unsafe fn taskbar_info() -> Option<TaskbarInfo> {
        let mut abd = APPBARDATA {
            cbSize: std::mem::size_of::<APPBARDATA>() as u32,
            ..Default::default()
        };
        if SHAppBarMessage(ABM_GETTASKBARPOS, &mut abd) == 0 {
            return None;
        }
        let r = abd.rc;
        let screen_h = GetSystemMetrics(SM_CYSCREEN);
        let screen_w = GetSystemMetrics(SM_CXSCREEN);
        // An auto-hidden taskbar reports a rect parked off-screen.
        let on_screen = r.bottom > 0 && r.top < screen_h && r.right > 0 && r.left < screen_w;
        if !on_screen {
            return None;
        }
        let vertical = abd.uEdge == ABE_LEFT || abd.uEdge == ABE_RIGHT;
        Some(TaskbarInfo {
            rect: r,
            vertical,
            left_edge: abd.uEdge == ABE_LEFT,
        })
    }

    /// How many times the topmost band was found lost between two ticks.
    /// Logged on the first loss and every 60th so a hostile z-order partner
    /// cannot spam the log.
    static TOPMOST_LOST_COUNT: AtomicU32 = AtomicU32::new(0);
    /// Set while a context menu is being tracked. `TrackPopupMenu` runs a
    /// modal loop that still dispatches our `WM_TIMER`, and the menu lives
    /// in the topmost band — re-asserting the indicator's band during tracking
    /// would lift the tiny overlay back above the open menu. Skip those ticks.
    static MENU_TRACKING: AtomicBool = AtomicBool::new(false);
    /// Re-entrancy guard for `launch_child`. The single-instance check reads
    /// the process handle slot, but `ShellExecuteExW` only writes it *after*
    /// the launch completes — and the Shell execute path can pump messages
    /// internally (shell extensions, auto-approval). A fast double click on
    /// the indicator can therefore dispatch the second `WM_LBUTTONUP` while
    /// the first launch is still in flight, find the slot empty, and spawn a
    /// duplicate child (two trend popups stacked at the same anchor, only
    /// one of them ever activated). This flag spans the whole launch call:
    /// the second click sees "launching" and no-ops.
    static LAUNCHING: AtomicBool = AtomicBool::new(false);

    /// Keep the overlay at the very top of the topmost band, every tick.
    ///
    /// Why: the topmost band is not an exclusive seat — shell flyouts (Start
    /// Menu, Task View, Action Center and the tray popups) and third-party
    /// always-on-top overlays are topmost too, and asserting their own band
    /// position re-sorts everyone else inside it. Field diagnosis on a live
    /// install caught the indicator with its `WS_EX_TOPMOST` style bit gone
    /// entirely and the window re-banded below a dozen ordinary windows; even
    /// when the bit survives, a flyout that asserts after us sits on top. A
    /// single startup assertion cannot hold, so the tick re-asserts the band
    /// position. `HWND_TOPMOST` with `SWP_NOMOVE | SWP_NOSIZE |
    /// SWP_NOACTIVATE` only re-inserts the window at the top of the band: no
    /// repaint, no move, no focus steal.
    ///
    /// Diagnostic: count how often the bit was found lost before re-assertion
    /// (first loss, then every 60th, so a hostile z-order partner cannot spam
    /// the log).
    ///
    /// Skip while our Settings/History child process is alive: the trend
    /// popup is meant to sit above the indicator, and lifting the band
    /// position mid-flight would fight it. The tick immediately after the
    /// popup exits (its process handle reaped by `child_alive`) re-asserts,
    /// so closing the popup also brings the indicator back without needing a
    /// click elsewhere — the recovery the user asked for.
    ///
    /// Skip while the right-click menu is tracking: `TrackPopupMenu` runs a
    /// modal loop that still dispatches our WM_TIMER, and the menu itself
    /// lives in the topmost band — re-asserting now would lift the indicator
    /// above the menu the user is reading.
    unsafe fn maintain_topmost(hwnd: HWND) {
        if MENU_TRACKING.load(Ordering::Relaxed) {
            return;
        }
        let child_running = SETTINGS_PROCESS.with(|slot| child_alive(slot))
            || HISTORY_PROCESS.with(|slot| child_alive(slot));
        if child_running {
            return;
        }
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        if (ex & WS_EX_TOPMOST.0 as isize) == 0 {
            let n = TOPMOST_LOST_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
            if n == 1 || n % 60 == 0 {
                log_event(
                    "INFO",
                    &format!("topmost band lost before re-assertion (count={n})"),
                );
            }
        }
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }

    /// Move-only placement. Visibility follows the taskbar alone: as long as
    /// the taskbar is on screen the indicator stays visible. It hides only
    /// when the taskbar itself is gone (auto-hide or a true fullscreen app
    /// pushing it off screen). No foreground-window heuristics involved.
    unsafe fn reposition(hwnd: HWND) {
        let Some(info) = taskbar_info() else {
            if IsWindowVisible(hwnd).as_bool() {
                log_event(
                    "INFO",
                    "overlay hidden: taskbar not on screen (auto-hide or fullscreen app)",
                );
            }
            let _ = ShowWindow(hwnd, SW_HIDE);
            return;
        };
        // Taskbar is on screen: keep the indicator in the topmost band (it can
        // be re-banded below shell flyouts between ticks; see maintain_topmost).
        maintain_topmost(hwnd);
        let bar = info.rect;

        // Track orientation changes: reset grow-only width when the taskbar
        // flips between horizontal and vertical.
        let was_vertical = TASKBAR_VERTICAL.swap(info.vertical, Ordering::Relaxed);
        if was_vertical != info.vertical {
            log_event(
                "INFO",
                &format!("taskbar orientation changed: vertical={}", info.vertical),
            );
            WIN_W.store(0, Ordering::Relaxed);
            sample_now(hwnd);
        }

        let cfg = config();
        let w = WIN_W.load(Ordering::Relaxed);
        let h = WIN_H.load(Ordering::Relaxed);

        let (x, y) = if info.vertical {
            // Vertical taskbar: horizontally centered within the bar thickness,
            // anchored top or bottom. When the stacked column is wider than the
            // bar's thickness, centering would push the window past the screen
            // edge it is docked to and clip it. Clamp so the overflow goes into
            // the desktop instead: keep the left edge on-screen for a
            // left-docked bar, the right edge on-screen for a right-docked bar.
            let center_x = bar.left + (bar.right - bar.left - w) / 2;
            let screen_w = GetSystemMetrics(SM_CXSCREEN);
            let x = if info.left_edge {
                center_x.max(0)
            } else {
                center_x.min(screen_w - w).max(0)
            };
            // On a vertical bar `left_margin_px` measures from the bar's TOP
            // edge (the user-facing knob of this value on left/right taskbars;
            // no anchor gate — the far-left anchor concept does not exist
            // here). The indicator is pinned near the far end by default, so
            // the same value also has to be honored when re-anchoring the bar
            // away from the tray side; see `else` branch for the horizontal
            // far-left gate.
            let y = bar.top + cfg.windows.left_margin_px as i32;
            (x, y)
        } else {
            // Horizontal taskbar (bottom or top).
            let left_anchor =
                cfg.windows.anchor == sysvitals_core::Anchor::Left && win11_centered_taskbar();
            let x = if left_anchor {
                bar.left + cfg.windows.left_margin_px as i32
            } else {
                let cls = utf16("Shell_TrayWnd");
                let Ok(tray) = FindWindowW(windows::core::PCWSTR(cls.as_ptr()), None) else {
                    return;
                };
                let notify_cls = utf16("TrayNotifyWnd");
                let notify = FindWindowExW(
                    Some(tray),
                    None,
                    windows::core::PCWSTR(notify_cls.as_ptr()),
                    None,
                )
                .unwrap_or_else(|_| HWND::default());
                let anchor_x = if notify.is_invalid() {
                    bar.right
                } else {
                    let mut nr = RECT::default();
                    if GetWindowRect(notify, &mut nr).is_err() {
                        bar.right
                    } else {
                        nr.left
                    }
                };
                anchor_x - w - TRAY_GAP_PX
            };
            let y = bar.top + (bar.bottom - bar.top - h) / 2;
            (x, y)
        };

        if info.vertical {
            if y < 0 || y + h > bar.bottom {
                return;
            }
        } else if x < 0 || y < 0 || x + w > bar.right {
            return; // would leave the taskbar; keep last position
        }
        let _ = MoveWindow(hwnd, x, y, w, h, true);
        // Only re-show when actually hidden (e.g. returning from auto-hide).
        if !IsWindowVisible(hwnd).as_bool() {
            log_event("INFO", "overlay re-shown after being hidden");
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        }
    }

    // ------------------------------------------------------- measurement/render

    /// Geometry of one frame measured with the font currently selected into
    /// `hdc`.
    struct FrameMetrics {
        line_h: i32,
        want_w: i32,
        right_x: Option<i32>,
        multi_line: bool,
        item_widths: Option<Vec<i32>>,
        /// Vertical layout only: height of each line slot in pixels, in line
        /// order. Blank separator lines take `item_spacing` (the same gap the
        /// horizontal single-row layout puts between metrics) instead of a
        /// full text line, so both layouts use one consistent block gap and
        /// the column stays short. Empty for the other layouts, where every
        /// line slot has the uniform `line_h`.
        line_heights: Vec<i32>,
    }

    /// Whether a blank line is the vertical layout's inter-block separator
    /// rather than content. The vertical frame is the only multi-line,
    /// single-column frame whose lines can be empty (the stacked builder
    /// inserts one blank line between metric blocks).
    fn is_vertical_separator(line: &sysvitals_core::Line) -> bool {
        line.left.is_empty() && line.right.is_empty()
    }

    unsafe fn measure_frame(
        hdc: windows::Win32::Graphics::Gdi::HDC,
        row: &FormattedRow,
        spacing_px: i32,
        vertical: bool,
    ) -> FrameMetrics {
        let multi_line = row.lines.len() > 1;
        let has_right_col = row.lines.iter().any(|l| !l.right.is_empty());
        let mut line_h = 0i32;
        let mut text_w = |text: &str| -> i32 {
            let mut rc = RECT::default();
            let mut buf = utf16(text);
            DrawTextW(
                hdc,
                &mut buf,
                &mut rc,
                DT_CALCRECT | DT_SINGLELINE | DT_NOPREFIX,
            );
            line_h = line_h.max(rc.bottom - rc.top);
            rc.right - rc.left
        };

        // Single-row with individual items: lay out with a pixel gap.
        if !row.items.is_empty() && !multi_line {
            let mut item_widths = Vec::with_capacity(row.items.len());
            let mut total_w = 0i32;
            for (i, item) in row.items.iter().enumerate() {
                let w = text_w(item);
                item_widths.push(w);
                total_w += w;
                if i + 1 < row.items.len() {
                    total_w += spacing_px;
                }
            }
            return FrameMetrics {
                line_h,
                want_w: total_w,
                right_x: None,
                multi_line: false,
                item_widths: Some(item_widths),
                line_heights: Vec::new(),
            };
        }

        if !multi_line || !has_right_col {
            let mut want_w = 0i32;
            for line in &row.lines {
                want_w = want_w.max(text_w(&row_text(line)));
            }
            // Vertical layout: separator lines shrink to the configured block
            // gap so it matches the horizontal single-row metric gap; content
            // lines keep the measured text height + breathing room.
            let line_heights: Vec<i32> = if vertical {
                row.lines
                    .iter()
                    .map(|l| {
                        if is_vertical_separator(l) {
                            spacing_px.max(1)
                        } else {
                            line_h + 2
                        }
                    })
                    .collect()
            } else {
                Vec::new()
            };
            return FrameMetrics {
                line_h,
                want_w,
                right_x: None,
                multi_line,
                item_widths: None,
                line_heights,
            };
        }
        let mut left_col_w = 0i32;
        let mut right_col_w = 0i32;
        for line in &row.lines {
            left_col_w = left_col_w.max(text_w(&line.left));
            if !line.right.is_empty() {
                right_col_w = right_col_w.max(text_w(&line.right));
            }
        }
        let right_x = left_col_w + spacing_px;
        FrameMetrics {
            line_h,
            want_w: right_x + right_col_w,
            right_x: Some(right_x),
            multi_line,
            item_widths: None,
            line_heights: Vec::new(),
        }
    }

    /// Measure the formatted rows and grow the window to fit. Width only
    /// ever grows; height follows the row count, and in the vertical layout
    /// the per-line slot heights from [`measure_frame`] (separator lines
    /// sized to `item_spacing`, not a full text line).
    unsafe fn resize_to_content(hwnd: HWND, row: &FormattedRow) {
        let cfg = config();
        let spacing_px = cfg.display.item_spacing as i32;
        let vertical = TASKBAR_VERTICAL.load(Ordering::Relaxed);
        let hdc = GetDC(Some(hwnd));
        let font = make_font();
        let old = SelectObject(hdc, font.into());
        let m = measure_frame(hdc, row, spacing_px, vertical);
        SelectObject(hdc, old);
        let _ = DeleteObject(font.into());
        let _ = ReleaseDC(Some(hwnd), hdc);
        if m.line_h == 0 {
            return;
        }
        let want_w = m.want_w + pad_px() * 2 + width_slack_px();
        let want_h = if m.line_heights.is_empty() {
            (m.line_h + 2) * row.lines.len() as i32 + 2
        } else {
            m.line_heights.iter().sum::<i32>() + 2
        };
        WIN_W.fetch_max(want_w, Ordering::Relaxed);
        WIN_H.store(want_h, Ordering::Relaxed);
        let w = WIN_W.load(Ordering::Relaxed);
        let mut rc = RECT::default();
        if GetWindowRect(hwnd, &mut rc).is_ok() {
            let _ = MoveWindow(hwnd, rc.left, rc.top, w, want_h, true);
        }
    }

    fn row_text(line: &sysvitals_core::Line) -> String {
        if line.right.is_empty() {
            line.left.clone()
        } else {
            format!("{}  {}", line.left, line.right)
        }
    }

    unsafe fn sample_now(hwnd: HWND) {
        let cfg = config();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let snapshot = SAMPLER.with(|s| {
            let mut guard = s.borrow_mut();
            let sampler = guard
                .get_or_insert_with(|| Sampler::new(NetSelection::Auto, cfg.sample.cpu_smoothing));
            sampler.sample(now)
        });
        record_history(&cfg, now, &snapshot);
        let row = if TASKBAR_VERTICAL.load(Ordering::Relaxed) {
            format_frame_vertical(&snapshot, &cfg)
        } else {
            format_frame(&snapshot, &cfg)
        };
        resize_to_content(hwnd, &row);
        LAST_ROW.with(|l| *l.borrow_mut() = Some(row));
        render_layered(hwnd);
    }

    /// Insert the current CPU/MEM sample into the history database and prune
    /// old rows periodically.
    fn record_history(cfg: &Config, now: i64, snapshot: &sysvitals_core::Snapshot) {
        if !cfg.history.enabled {
            return;
        }
        STORE.with(|s| {
            let mut guard = s.borrow_mut();
            if guard.is_none() && !STORE_ATTEMPTED.swap(true, Ordering::Relaxed) {
                if let Ok(path) = sysvitals_core::config::history_path() {
                    if let Some(parent) = path.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    match sysvitals_core::Store::open(&path) {
                        Ok(store) => {
                            log_event("INFO", &format!("history store opened: {}", path.display()));
                            *guard = Some(store);
                        }
                        Err(e) => {
                            log_event(
                                "WARN",
                                &format!(
                                    "history store failed to open ({}): {}",
                                    path.display(),
                                    e
                                ),
                            );
                            *guard = None;
                        }
                    }
                }
            }
            if let Some(store) = guard.as_ref() {
                let _ = store.insert(now, snapshot.cpu_pct, snapshot.mem_pct);
                let tick = TICK_COUNT.with(|c| {
                    let v = c.get() + 1;
                    c.set(v);
                    v
                });
                if tick % 300 == 0 {
                    let _ = store.prune(now, cfg.history.retention_days);
                }
            }
        });
    }

    /// Render the cached row into a 32bpp premultiplied-alpha DIB and blit it
    /// onto the layered window with `UpdateLayeredWindow`. Glyphs are rendered
    /// at 2x supersampled resolution and box-filtered down.
    unsafe fn render_layered(hwnd: HWND) {
        let mut win_rc = RECT::default();
        if GetWindowRect(hwnd, &mut win_rc).is_err() {
            return;
        }
        let w = win_rc.right - win_rc.left;
        let h = win_rc.bottom - win_rc.top;
        if w <= 0 || h <= 0 {
            return;
        }
        let Some(row) = LAST_ROW.with(|l| l.borrow().clone()) else {
            return;
        };

        let screen_dc = GetDC(None);
        let mem_dc = CreateCompatibleDC(Some(screen_dc));

        // --- Scratch surface at 2x supersampled scale ---------------------
        const SS: i32 = 2;
        let sw = w * SS;
        let sh = h * SS;
        let mut ss_bmi = BITMAPINFO::default();
        ss_bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        ss_bmi.bmiHeader.biWidth = sw;
        ss_bmi.bmiHeader.biHeight = -sh;
        ss_bmi.bmiHeader.biPlanes = 1;
        ss_bmi.bmiHeader.biBitCount = 32;
        ss_bmi.bmiHeader.biCompression = BI_RGB.0;
        let mut ss_bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let Ok(ss_bmp) =
            CreateDIBSection(Some(mem_dc), &ss_bmi, DIB_RGB_COLORS, &mut ss_bits, None, 0)
        else {
            let _ = DeleteDC(mem_dc);
            let _ = ReleaseDC(None, screen_dc);
            return;
        };
        let old_ss_bmp = SelectObject(mem_dc, ss_bmp.into());
        let ss_font = make_font_at(SS);
        let old_ss_font = SelectObject(mem_dc, ss_font.into());
        SetBkMode(mem_dc, TRANSPARENT);
        SetTextColor(mem_dc, COLORREF(0x00FF_FFFF));

        // Clear the scratch surface to black (coverage 0).
        let ss_px = ss_bits as *mut u32;
        let ss_n = (sw * sh) as usize;
        for i in 0..ss_n {
            *ss_px.add(i) = 0;
        }

        let spacing_px = config().display.item_spacing as i32;
        // Padding follows the orientation so the vertical layout can use its
        // tighter margin; measurement and rendering must agree on it.
        let pad = pad_px() * SS;
        let vertical = TASKBAR_VERTICAL.load(Ordering::Relaxed);
        let fm = measure_frame(mem_dc, &row, spacing_px * SS, vertical);
        // Line slot tops in supersampled pixels. Uniform division works for
        // the horizontal layouts; the vertical layout has per-line heights
        // (separator lines sized to `item_spacing`, content lines sized to
        // the text height + breathing), so tops accumulate from that table.
        let ss_line_h = sh / row.lines.len().max(1) as i32;
        let mut tops: Vec<(i32, i32)> = Vec::with_capacity(row.lines.len());
        if fm.line_heights.is_empty() {
            for i in 0..row.lines.len() {
                tops.push((i as i32 * ss_line_h, (i as i32 + 1) * ss_line_h));
            }
        } else {
            let mut acc = 0i32;
            for h in &fm.line_heights {
                tops.push((acc, acc + h));
                acc += h;
            }
        }
        for (i, line) in row.lines.iter().enumerate() {
            let (top, bottom) = tops.get(i).copied().unwrap_or((0, sh));
            if let Some(ref item_widths) = fm.item_widths {
                // Single-row pixel-spacing layout.
                let gap = spacing_px * SS;
                let total_w: i32 = item_widths.iter().sum::<i32>()
                    + gap * (row.items.len().saturating_sub(1) as i32);
                let mut x = (sw - total_w) / 2;
                for (j, item) in row.items.iter().enumerate() {
                    let mut rc = RECT {
                        left: x,
                        top,
                        right: x + item_widths[j],
                        bottom,
                    };
                    let mut buf = utf16(item);
                    DrawTextW(
                        mem_dc,
                        &mut buf,
                        &mut rc,
                        DT_SINGLELINE | DT_LEFT | DT_VCENTER | DT_NOPREFIX,
                    );
                    x += item_widths[j] + gap;
                }
            } else if let Some(right_x) = fm.right_x {
                // Dual-row: left column left-aligned, right column at offset.
                let mut left_rc = RECT {
                    left: pad,
                    top,
                    right: pad + right_x,
                    bottom,
                };
                let mut lbuf = utf16(&line.left);
                DrawTextW(
                    mem_dc,
                    &mut lbuf,
                    &mut left_rc,
                    DT_SINGLELINE | DT_LEFT | DT_VCENTER | DT_NOPREFIX,
                );
                if !line.right.is_empty() {
                    let mut right_rc = RECT {
                        left: pad + right_x,
                        top,
                        right: sw - pad,
                        bottom,
                    };
                    let mut rbuf = utf16(&line.right);
                    DrawTextW(
                        mem_dc,
                        &mut rbuf,
                        &mut right_rc,
                        DT_SINGLELINE | DT_LEFT | DT_VCENTER | DT_NOPREFIX,
                    );
                }
            } else if fm.multi_line {
                // Multi-line single column: left-aligned.
                let text = row_text(line);
                let mut line_rc = RECT {
                    left: pad,
                    top,
                    right: sw - pad,
                    bottom,
                };
                let mut buf = utf16(&text);
                DrawTextW(
                    mem_dc,
                    &mut buf,
                    &mut line_rc,
                    DT_SINGLELINE | DT_LEFT | DT_VCENTER | DT_NOPREFIX,
                );
            } else {
                // Single row: centered.
                let text = row_text(line);
                let mut line_rc = RECT {
                    left: 0,
                    top,
                    right: sw,
                    bottom,
                };
                let mut buf = utf16(&text);
                DrawTextW(
                    mem_dc,
                    &mut buf,
                    &mut line_rc,
                    DT_SINGLELINE | DT_CENTER | DT_VCENTER | DT_NOPREFIX,
                );
            }
        }

        SelectObject(mem_dc, old_ss_font);
        let _ = DeleteObject(ss_font.into());

        // --- Final surface at window scale, downsampled 2x2 ---------------
        let mut bmi = BITMAPINFO::default();
        bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        bmi.bmiHeader.biWidth = w;
        bmi.bmiHeader.biHeight = -h;
        bmi.bmiHeader.biPlanes = 1;
        bmi.bmiHeader.biBitCount = 32;
        bmi.bmiHeader.biCompression = BI_RGB.0;
        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let Ok(bmp) = CreateDIBSection(Some(mem_dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0)
        else {
            SelectObject(mem_dc, old_ss_bmp);
            let _ = DeleteObject(ss_bmp.into());
            let _ = DeleteDC(mem_dc);
            let _ = ReleaseDC(None, screen_dc);
            return;
        };

        let (tr, tg, tb) = if theme_is_light() {
            (0u32, 0u32, 0u32)
        } else {
            (255u32, 255u32, 255u32)
        };
        let px = bits as *mut u32;
        // Tray-style focus pill (hover / pressed background): while the
        // pointer is over the indicator, or a button is held down on it, a
        // rounded rect in the glyph color at a low alpha paints under the
        // text, imitating how the shell tints its own tray buttons. Idle
        // (`pill_a == 0`) the composition is byte-for-byte the previous
        // output: no pill pixels, glyphs as before.
        let pill_a: u32 = {
            // Pressed and "popup open" share the stronger tint; hover gets
            // the lighter one; idle (0) composites exactly as before.
            let popup_open = HISTORY_PROCESS.with(|slot| {
                slot.borrow().as_ref().is_some_and(|h| {
                    !h.is_invalid() && {
                        // Read-only liveness probe: no handle close, no slot
                        // clear. `close_history` and the launch path keep owning
                        // the reaping; a popup that closed itself simply leaves a
                        // signaled handle, which reads as "not alive" here.
                        unsafe { WaitForSingleObject(*h, 0) == WAIT_TIMEOUT }
                    }
                })
            });
            if PRESSED.load(Ordering::Relaxed) || popup_open {
                64
            } else if HOVER.load(Ordering::Relaxed) {
                32
            } else {
                0
            }
        };
        // Pill geometry in window coordinates: inset so the highlight does
        // not touch the window edge, radius capped by the short axis (and by
        // 8px) so it reads as a chip, not a circle.
        let inset = 2i32.min(w / 4).min(h / 4).max(0);
        let pw = (w - 2 * inset).max(0);
        let ph = (h - 2 * inset).max(0);
        let radius = (pw.min(ph) / 2).min(8);
        // Half-extents of the pill's straight-sided core; the rounded-rect
        // SDF below measures distance against them plus `radius`.
        let hx = (pw - 2 * radius).max(0) as f32 / 2.0;
        let hy = (ph - 2 * radius).max(0) as f32 / 2.0;
        let pcx = inset as f32 + pw as f32 / 2.0;
        let pcy = inset as f32 + ph as f32 / 2.0;
        for y in 0..h {
            for x in 0..w {
                let mut acc = 0u32;
                for dy in 0..SS {
                    for dx in 0..SS {
                        let sx = x * SS + dx;
                        let sy = y * SS + dy;
                        acc += *ss_px.add((sy * sw + sx) as usize) & 0xFF;
                    }
                }
                let cov = acc / (SS * SS) as u32;
                // Signed distance to the rounded rect (<0 inside), feathered
                // over one pixel so the pill edge is anti-aliased even
                // though this pass runs at 1x.
                let ddx = (((x as f32 + 0.5) - pcx).abs() - hx).max(0.0);
                let ddy = (((y as f32 + 0.5) - pcy).abs() - hy).max(0.0);
                let dist = (ddx * ddx + ddy * ddy).sqrt() - radius as f32;
                let inside = (1.0 - dist.max(0.0)).clamp(0.0, 1.0);
                let pa = (pill_a as f32 * inside) as u32;
                if cov == 0 && pa == 0 {
                    // Background: minimal (but non-zero) alpha so the whole
                    // rectangle stays hit-testable yet invisible over the
                    // taskbar.
                    *px.add((y * w + x) as usize) = 0x0100_0000;
                } else {
                    // Source-over with the glyph on top of the pill; channels
                    // stay premultiplied for `AC_SRC_ALPHA` (both layers share
                    // the glyph color, so one blend factor per channel).
                    let a = cov + pa * (255 - cov) / 255;
                    let mix = |c: u32| (c * cov / 255) + (c * pa / 255) * (255 - cov) / 255;
                    *px.add((y * w + x) as usize) =
                        (a << 24) | (mix(tr) << 16) | (mix(tg) << 8) | mix(tb);
                }
            }
        }

        SelectObject(mem_dc, old_ss_bmp);
        let _ = DeleteObject(ss_bmp.into());

        let old_bmp = SelectObject(mem_dc, bmp.into());
        let src_pt = POINT { x: 0, y: 0 };
        let size = SIZE { cx: w, cy: h };
        let dst_pt = POINT {
            x: win_rc.left,
            y: win_rc.top,
        };
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        let _ = UpdateLayeredWindow(
            hwnd,
            None,
            Some(&dst_pt),
            Some(&size),
            Some(mem_dc),
            Some(&src_pt),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        );

        SelectObject(mem_dc, old_bmp);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(mem_dc);
        let _ = ReleaseDC(None, screen_dc);
    }

    // ------------------------------------------------- config file watching

    /// Read the config file's last-modified time as seconds since UNIX epoch.
    /// Returns 0 when the file does not exist or cannot be read.
    fn config_mtime() -> i32 {
        let Ok(path) = sysvitals_core::config::config_path() else {
            return 0;
        };
        let Ok(meta) = std::fs::metadata(&path) else {
            return 0;
        };
        let Ok(modified) = meta.modified() else {
            return 0;
        };
        modified
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i32)
            .unwrap_or(0)
    }

    /// Called on each TIMER_CONFIG tick. Compares the config file's mtime
    /// against the cached value; on change, reloads the config, re-seeds the
    /// sample timer (interval may have changed), and posts WM_APP_APPLY.
    fn poll_config_change(hwnd: HWND) {
        let current = config_mtime();
        let previous = CONFIG_MTIME.load(Ordering::Relaxed);
        if current == previous {
            return;
        }
        CONFIG_MTIME.store(current, Ordering::Relaxed);
        // Reload from disk.
        let loaded = sysvitals_core::config::config_path()
            .and_then(|p| Config::load(&p).map(|(c, _)| c))
            .unwrap_or_default();
        store_config(loaded.clone());
        log_event(
            "INFO",
            &format!(
                "config file change detected: interval={}ms language={:?} theme={:?} \
                 dual_row={} show_resources={} show_network={} item_spacing={}px \
                 anchor={:?} left_margin={}px",
                loaded.sample.interval_ms,
                loaded.display.language,
                loaded.display.theme,
                loaded.display.dual_row,
                loaded.display.show_resources,
                loaded.display.show_network,
                loaded.display.item_spacing,
                loaded.windows.anchor,
                loaded.windows.left_margin_px,
            ),
        );
        // Re-seed the sample timer in case the interval changed.
        unsafe {
            let _ = KillTimer(Some(hwnd), TIMER_SAMPLE);
            SetTimer(
                Some(hwnd),
                TIMER_SAMPLE,
                loaded.sample.interval_ms.clamp(500, 5000) as u32,
                None,
            );
        }
        // Post the apply message so the overlay re-renders at top level.
        let _ = unsafe { PostMessageW(Some(hwnd), WM_APP_APPLY, WPARAM(0), LPARAM(0)) };
    }

    // ------------------------------------------------------- child process launch

    /// Whether the child process behind `slot` is still running. Reaps the
    /// handle when the process has exited.
    fn child_alive(slot: &std::cell::RefCell<Option<HANDLE>>) -> bool {
        let mut borrow = slot.borrow_mut();
        let Some(handle) = borrow.as_ref() else {
            return false;
        };
        if handle.is_invalid() {
            return false;
        }
        let wait = unsafe { WaitForSingleObject(*handle, 0) };
        if wait == WAIT_TIMEOUT {
            true
        } else {
            let _ = unsafe { CloseHandle(*handle) };
            *borrow = None;
            false
        }
    }

    /// Terminate the history popup's process if it is still running: the
    /// click-to-toggle half of `WM_LBUTTONUP`. The child's handle is closed and
    /// the slot cleared either way (an already-exited child is reaped), so the
    /// next click opens a fresh popup. Returns `true` only when a live popup
    /// was terminated — `false` means nothing was running and the caller
    /// should open a new one.
    ///
    /// This is a hard process kill rather than a polite "please close": with
    /// the overlay's `WS_EX_NOACTIVATE`, a click on the indicator leaves the
    /// popup foreground and alive, so `close_history` is the only exit path
    /// for the toggle click (blur-close still owns every other click-away).
    /// Terminating the handle is deterministic and idempotent (already-dead
    /// handles are reaped).
    fn close_history() -> bool {
        HISTORY_PROCESS.with(|slot| {
            let mut borrow = slot.borrow_mut();
            let Some(handle) = borrow.as_ref() else {
                return false;
            };
            if handle.is_invalid() {
                *borrow = None;
                return false;
            }
            // Already exited: reap quietly, treat as "nothing to close".
            if unsafe { WaitForSingleObject(*handle, 0) } != WAIT_TIMEOUT {
                let _ = unsafe { CloseHandle(*handle) };
                *borrow = None;
                return false;
            }
            let _ = unsafe { TerminateProcess(*handle, 0) };
            let _ = unsafe { CloseHandle(*handle) };
            *borrow = None;
            log_event("INFO", "history popup closed by indicator click");
            true
        })
    }

    /// Launch a sibling executable from the current exe directory, keeping one
    /// live instance per target: if a previously spawned process is still
    /// running, this is a no-op. Extra argv (e.g. the `--anchor` geometry) is
    /// passed through verbatim.
    ///
    /// The liveness check reads the process handle slot, but that slot is only
    /// written *after* `ShellExecuteExW` returns — and Shell execute can pump
    /// messages on its own thread-reentrant path, so a second click landing
    /// mid-launch would see an empty slot and spawn a duplicate. The `LAUNCHING`
    /// flag spans the whole launch (checked + set before, cleared after) so the
    /// overlapping click no-ops instead. See the `LAUNCHING` note above.
    fn launch_child(
        exe_name: &str,
        args: Option<&str>,
        slot: &std::cell::RefCell<Option<HANDLE>>,
    ) -> bool {
        if child_alive(slot) {
            log_event(
                "INFO",
                &format!("launch_child {exe_name}: already running, skipped"),
            );
            return false;
        }
        if LAUNCHING.swap(true, Ordering::SeqCst) {
            log_event(
                "INFO",
                &format!("launch_child {exe_name}: launch already in flight, skipped"),
            );
            return false;
        }
        let result = launch_child_inner(exe_name, args, slot);
        LAUNCHING.store(false, Ordering::SeqCst);
        result
    }

    fn launch_child_inner(
        exe_name: &str,
        args: Option<&str>,
        slot: &std::cell::RefCell<Option<HANDLE>>,
    ) -> bool {
        let Some(dir) = current_exe_dir() else {
            log_event(
                "WARN",
                &format!("launch_child: cannot determine exe directory: {exe_name}"),
            );
            return false;
        };
        let exe_path = format!("{dir}\\{exe_name}");
        if !std::path::Path::new(&exe_path).exists() {
            log_event("WARN", &format!("launch_child: not found: {exe_path}"));
            return false;
        }
        log_event(
            "INFO",
            &format!(
                "launching {exe_name}{}",
                args.map(|a| format!(" {a}")).unwrap_or_default()
            ),
        );
        let path_w = utf16(&exe_path);
        let verb = utf16("open");
        let args_w = args.map(utf16);
        let mut sei = SHELLEXECUTEINFOW {
            cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
            fMask: SEE_MASK_NOCLOSEPROCESS,
            lpVerb: windows::core::PCWSTR(verb.as_ptr()),
            lpFile: windows::core::PCWSTR(path_w.as_ptr()),
            lpParameters: args_w
                .as_ref()
                .map(|a| windows::core::PCWSTR(a.as_ptr()))
                .unwrap_or(windows::core::PCWSTR::null()),
            nShow: SW_SHOWNORMAL.0,
            ..Default::default()
        };
        if unsafe { ShellExecuteExW(&mut sei) }.is_err() {
            log_event(
                "WARN",
                &format!("launch_child: ShellExecuteExW failed for {exe_name}"),
            );
            return false;
        }
        *slot.borrow_mut() = Some(sei.hProcess);
        true
    }

    /// Launch the external settings process (`SysVitalsSettings.exe`).
    fn launch_settings() {
        // `thread_local!` statics expose a `LocalKey`; the cell itself is
        // reached through `with`, not by borrowing the static.
        SETTINGS_PROCESS.with(|slot| launch_child("SysVitalsSettings.exe", None, slot));
    }

    /// Launch the external history chart process (`SysVitalsHistory.exe`),
    /// passing the indicator's screen rectangle so the popup can anchor to it.
    fn launch_history(hwnd: HWND) {
        let mut rect = RECT::default();
        let args = match unsafe { GetWindowRect(hwnd, &mut rect) } {
            Ok(()) => {
                // `GetWindowRect` reports physical pixels; the child process
                // lays its window out in GPUI's global logical pixel space
                // (top-left origin, y down) — the same units app-mac emits.
                // Scale by this window's own monitor DPI.
                let dpi = unsafe { GetDpiForWindow(hwnd) };
                let scale = if dpi == 0 { 1.0 } else { dpi as f32 / 96.0 };
                Some(format!(
                    "--anchor {:.1} {:.1} {:.1} {:.1}",
                    rect.left as f32 / scale,
                    rect.top as f32 / scale,
                    rect.right as f32 / scale,
                    rect.bottom as f32 / scale
                ))
            }
            Err(_) => None,
        };
        HISTORY_PROCESS
            .with(move |slot| launch_child("SysVitalsHistory.exe", args.as_deref(), slot));
    }

    /// Best-effort: return the directory containing the current executable.
    fn current_exe_dir() -> Option<String> {
        let mut buf = [0u16; 1024];
        let n = unsafe { GetModuleFileNameW(None, &mut buf) };
        if n == 0 {
            return None;
        }
        let path = String::from_utf16_lossy(&buf[..n as usize]);
        path.rfind(['\\', '/']).map(|i| path[..i].to_string())
    }

    /// Restart the overlay: schedule a delayed relaunch of the current exe
    /// (after the single-instance mutex is released on exit), then exit.
    fn restart_self(hwnd: HWND) {
        let mut buf = [0u16; 1024];
        let n = unsafe { GetModuleFileNameW(None, &mut buf) };
        if n == 0 {
            log_event("WARN", "restart_self: cannot determine exe path");
            return;
        }
        let exe = String::from_utf16_lossy(&buf[..n as usize]);
        // Delay 2 seconds so this process exits and releases the mutex first.
        let cmd = format!("/c timeout /t 2 /nobreak >nul & start \"\" \"{}\"", exe);
        let cmd_w = utf16(&cmd);
        let verb = utf16("open");
        let target = utf16("cmd.exe");
        let params = windows::core::PCWSTR(cmd_w.as_ptr());
        log_event("INFO", "restart_self: scheduling relaunch and exiting");
        unsafe {
            let _ = ShellExecuteW(
                None,
                windows::core::PCWSTR(verb.as_ptr()),
                windows::core::PCWSTR(target.as_ptr()),
                params,
                windows::core::PCWSTR::null(),
                SW_HIDE,
            );
        }
        // Exit the current instance.
        unsafe {
            let _ = KillTimer(Some(hwnd), TIMER_SAMPLE);
            let _ = KillTimer(Some(hwnd), TIMER_MOVE);
            let _ = KillTimer(Some(hwnd), TIMER_CONFIG);
            let _ = DestroyWindow(hwnd);
        }
    }

    // -------------------------------------------------------------------- menu

    /// Launch the system resource viewer (Task Manager). This action used to
    /// live on a double-click of the indicator; putting it in the context
    /// menu is what lets the left click open the trend chart immediately,
    /// without waiting for a possible second press.
    unsafe fn launch_taskmgr() {
        let verb = utf16("open");
        let target = utf16("taskmgr.exe");
        let _ = ShellExecuteW(
            None,
            windows::core::PCWSTR(verb.as_ptr()),
            windows::core::PCWSTR(target.as_ptr()),
            windows::core::PCWSTR::null(),
            windows::core::PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }

    unsafe fn show_context_menu(hwnd: HWND) {
        let Ok(menu) = CreatePopupMenu() else {
            return;
        };
        let lang = config().display.language;
        // Order: system resource viewer first (most actionable when the
        // indicator is in the way), then Settings, Restart, Quit. Task
        // Manager sits at the top because opening it is the reason the
        // indicator used to double-click — moving it here is what freed the
        // left click to open the chart instantly.
        let taskmgr = utf16(t(lang, "menu.task_manager"));
        let settings = utf16(t(lang, "menu.settings"));
        let restart = utf16(t(lang, "menu.restart"));
        let quit = utf16(t(lang, "menu.quit"));
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            IDM_TASKMGR,
            windows::core::PCWSTR(taskmgr.as_ptr()),
        );
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            IDM_SETTINGS,
            windows::core::PCWSTR(settings.as_ptr()),
        );
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            IDM_RESTART,
            windows::core::PCWSTR(restart.as_ptr()),
        );
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            IDM_QUIT,
            windows::core::PCWSTR(quit.as_ptr()),
        );
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        // The overlay is created `WS_EX_NOACTIVATE` (so a left click on the
        // indicator never steals foreground from the trend-chart popup — see
        // the toggle note in `close_history`). That same flag means a right
        // click no longer naturally foregrounds the owner window, and
        // `TrackPopupMenu` only auto-dismisses on a click-away when its owner
        // is foreground. Restore that with the documented tray-menu pattern:
        // foreground the owner first, then post a `WM_NULL` afterward so the
        // menu reliably disappears (the post is the standard workaround for
        // the menu lingering when it should not).
        let _ = SetForegroundWindow(hwnd);
        // Keep maintain_topmost from lifting the overlay above the menu while
        // it is open (its modal loop still dispatches our WM_TIMER).
        MENU_TRACKING.store(true, Ordering::Relaxed);
        let choice = TrackPopupMenu(
            menu,
            TPM_LEFTALIGN | TPM_RIGHTBUTTON | TPM_RETURNCMD,
            pt.x,
            pt.y,
            None,
            hwnd,
            None,
        )
        .0 as u32;
        MENU_TRACKING.store(false, Ordering::Relaxed);
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
        match choice {
            id if id == IDM_TASKMGR as u32 => launch_taskmgr(),
            id if id == IDM_SETTINGS as u32 => launch_settings(),
            id if id == IDM_RESTART as u32 => restart_self(hwnd),
            id if id == IDM_QUIT as u32 => {
                let _ = KillTimer(Some(hwnd), TIMER_SAMPLE);
                let _ = KillTimer(Some(hwnd), TIMER_MOVE);
                let _ = KillTimer(Some(hwnd), TIMER_CONFIG);
                let _ = DestroyWindow(hwnd);
            }
            _ => {}
        }
    }

    // ----------------------------------------------------------------- wndproc

    unsafe extern "system" fn wndproc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match msg {
            WM_TIMER => {
                match wparam.0 {
                    id if id == TIMER_SAMPLE => sample_now(hwnd),
                    id if id == TIMER_MOVE => reposition(hwnd),
                    id if id == TIMER_CONFIG => poll_config_change(hwnd),
                    _ => {}
                }
                LRESULT(0)
            }
            WM_APP_APPLY => {
                // Config changed (from the file watcher): reposition and redraw
                // at top level. Reset grow-only width so spacing/metric changes
                // to a narrower layout take effect.
                log_event(
                    "INFO",
                    "WM_APP_APPLY received: repositioning and re-rendering",
                );
                WIN_W.store(0, Ordering::Relaxed);
                reposition(hwnd);
                sample_now(hwnd);
                LRESULT(0)
            }
            WM_SETTINGCHANGE => {
                // Theme flips arrive here; re-read the flag and repaint.
                let old_light = LIGHT_THEME.load(Ordering::Relaxed);
                refresh_theme();
                let new_light = LIGHT_THEME.load(Ordering::Relaxed);
                if old_light != new_light {
                    log_event("INFO", &format!("theme changed: light={}", new_light == 1));
                }
                render_layered(hwnd);
                LRESULT(0)
            }
            WM_DISPLAYCHANGE => {
                log_event("INFO", "WM_DISPLAYCHANGE: display configuration changed");
                reposition(hwnd);
                LRESULT(0)
            }
            WM_LBUTTONDOWN => {
                PRESSED.store(true, Ordering::Relaxed);
                render_layered(hwnd);
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                // Toggle the trend chart. A live popup (child still running)
                // is closed; otherwise a new one opens immediately. There is
                // no deferral: the class style dropped `CS_DBLCLKS`, so
                // Windows does not synthesize a `WM_LBUTTONDBLCLK`, and Task
                // Manager lives in the right-click menu. Any left click is a
                // left click.
                let was_pressed = PRESSED.swap(false, Ordering::Relaxed);
                if !close_history() {
                    launch_history(hwnd);
                }
                // Paint off the pressed pill even when the popup just opened
                // (the overlay stays click-responsive, so this tick is ours).
                if was_pressed {
                    render_layered(hwnd);
                }
                LRESULT(0)
            }
            WM_RBUTTONUP => {
                PRESSED.store(false, Ordering::Relaxed);
                show_context_menu(hwnd);
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                // Arm one leave-tracking request per enter; the flag keeps
                // subsequent moves inside the rect from re-posting.
                if !HOVER.swap(true, Ordering::Relaxed)
                    && !TRACK_ARMED.swap(true, Ordering::Relaxed)
                    // SAFETY: the hwnd is live; TME_LEAVE only requests the
                    // one-shot WM_MOUSELEAVE.
                    && (unsafe {
                        TrackMouseEvent(&mut TRACKMOUSEEVENT {
                            cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                            dwFlags: TME_LEAVE,
                            hwndTrack: hwnd,
                            dwHoverTime: 0,
                        })
                    })
                    .is_ok()
                {
                    render_layered(hwnd);
                }
                LRESULT(0)
            }
            WM_MOUSELEAVE => {
                TRACK_ARMED.store(false, Ordering::Relaxed);
                PRESSED.store(false, Ordering::Relaxed);
                if HOVER.swap(false, Ordering::Relaxed) {
                    render_layered(hwnd);
                }
                LRESULT(0)
            }
            WM_DESTROY => {
                log_event("INFO", "SysVitals exiting");
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }

    // ------------------------------------------------------------ crash hooks

    /// Install crash diagnostics: a Rust panic hook and a Win32 unhandled
    /// exception filter. Both write the crash reason to the log file so a
    /// silent process death becomes diagnosable.
    fn install_crash_hooks() {
        std::panic::set_hook(Box::new(|info| {
            let msg = if let Some(s) = info.payload().downcast_ref::<&str>() {
                s.to_string()
            } else if let Some(s) = info.payload().downcast_ref::<String>() {
                s.clone()
            } else {
                "unknown panic".to_string()
            };
            let loc = info
                .location()
                .map(|l| format!("{}:{}", l.file(), l.line()))
                .unwrap_or_else(|| "unknown location".to_string());
            log_event("FATAL", &format!("panic at {loc}: {msg}"));
        }));
        unsafe {
            SetUnhandledExceptionFilter(Some(unhandled_exception_filter));
        }
    }

    unsafe extern "system" fn unhandled_exception_filter(info: *const EXCEPTION_POINTERS) -> i32 {
        let mut code = 0u32;
        let mut addr = 0usize;
        if !info.is_null() && !(*info).ExceptionRecord.is_null() {
            let rec = &*(*info).ExceptionRecord;
            code = rec.ExceptionCode.0 as u32;
            addr = rec.ExceptionAddress as usize;
        }
        let module = module_name_for_address(addr as *mut std::ffi::c_void);
        log_event(
            "FATAL",
            &format!(
                "unhandled exception: 0x{code:08X} at 0x{addr:016X} in {module} — process will terminate"
            ),
        );
        // EXCEPTION_EXECUTE_HANDLER: let the OS terminate after our logging.
        1
    }

    /// Best-effort: return the module file name that contains `addr`.
    unsafe fn module_name_for_address(addr: *mut std::ffi::c_void) -> String {
        let mut hmod = windows::Win32::Foundation::HMODULE::default();
        let ok = GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            windows::core::PCWSTR(addr as *const u16),
            &mut hmod,
        )
        .is_ok();
        if !ok {
            return "<unknown module>".to_string();
        }
        let mut buf = [0u16; 512];
        let n = GetModuleFileNameW(Some(hmod), &mut buf);
        if n == 0 {
            return "<unknown module>".to_string();
        }
        let path = String::from_utf16_lossy(&buf[..n as usize]);
        path.rsplit(['\\', '/'])
            .next()
            .unwrap_or("<unknown module>")
            .to_string()
    }

    // --------------------------------------------------------------------- run

    pub fn run() {
        unsafe {
            // Crash diagnostics first.
            install_crash_hooks();

            // Single-instance guard.
            let mutex_name = utf16("Local\\SysVitals_SingleInstance");
            let _mutex = match CreateMutexW(None, false, windows::core::PCWSTR(mutex_name.as_ptr()))
            {
                Ok(h) => {
                    if windows::Win32::Foundation::GetLastError()
                        == windows::Win32::Foundation::ERROR_ALREADY_EXISTS
                    {
                        return;
                    }
                    h
                }
                Err(_) => return,
            };

            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            refresh_theme();
            let cfg = config();
            let interval_ms = cfg.sample.interval_ms.clamp(500, 5000) as u32;

            // Seed the config mtime so the first poll does not false-trigger.
            CONFIG_MTIME.store(config_mtime(), Ordering::Relaxed);

            // Daily-file logging: prune stale logs once at startup.
            prune_old_logs();
            log_event("INFO", "SysVitals starting");
            log_event(
                "INFO",
                &format!(
                    "config loaded: interval={}ms language={:?} theme={:?} dual_row={} \
                     show_resources={} show_network={} item_spacing={}px anchor={:?} \
                     left_margin={}px",
                    cfg.sample.interval_ms,
                    cfg.display.language,
                    cfg.display.theme,
                    cfg.display.dual_row,
                    cfg.display.show_resources,
                    cfg.display.show_network,
                    cfg.display.item_spacing,
                    cfg.windows.anchor,
                    cfg.windows.left_margin_px,
                ),
            );
            let centered = win11_centered_taskbar();
            log_event(
                "INFO",
                &format!(
                    "environment: win11_centered_taskbar={} dpi={}",
                    centered,
                    GetDpiForSystem()
                ),
            );

            let hmodule = GetModuleHandleW(None).expect("module handle");
            let class_name = utf16("SysVitalsOverlay");
            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                // No `CS_DBLCLKS`: the indicator's left click now opens the
                // trend chart immediately, and the system resource viewer
                // (Task Manager) lives in the right-click menu. Keeping
                // `CS_DBLCLKS` would let Windows synthesize a
                // `WM_LBUTTONDBLCLK` after the trailing `WM_LBUTTONUP`,
                // which we no longer handle; dropping it here makes the
                // click pipeline honestly single-click only.
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(wndproc),
                hInstance: hmodule.into(),
                hbrBackground: windows::Win32::Graphics::Gdi::HBRUSH::default(),
                lpszClassName: windows::core::PCWSTR(class_name.as_ptr()),
                ..Default::default()
            };
            if RegisterClassExW(&wc) == 0 {
                return;
            }

            let title = utf16("SysVitals");
            // `WS_EX_NOACTIVATE` — the toggle race fix. A left click on the
            // overlay used to pull foreground away from the trend-chart
            // child; the child's blur observer then began closing itself,
            // and the overlay's `close_history` liveness check could catch
            // it mid-exit, see "already dead", and launch a fresh popup
            // instead — the "closes then re-opens" symptom. Not activating
            // on click keeps the child foreground and alive, so
            // `close_history` deterministically terminates it. This is safe
            // because `app-history` claims activation itself on its first
            // frame (`activate_window`, Windows-gated) when it did not
            // inherit foreground from the launcher, so the popup still
            // arms its blur-close for genuine click-aways. Right-click
            // menus keep their click-away dismissal through the standard
            // tray pattern in `show_context_menu`
            // (`SetForegroundWindow` before, `WM_NULL` post after).
            // Behavior on target OSes needs verification on real devices.
            let hwnd = CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                windows::core::PCWSTR(class_name.as_ptr()),
                windows::core::PCWSTR(title.as_ptr()),
                WS_POPUP,
                -32_000,
                -32_000,
                200,
                24,
                None,
                None,
                Some(hmodule.into()),
                None,
            )
            .expect("CreateWindowExW");

            // Topmost so the overlay sits above the taskbar's own content.
            let _ = SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
            // Detect taskbar orientation first, then seed one frame.
            reposition(hwnd);
            sample_now(hwnd);
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            SetTimer(Some(hwnd), TIMER_SAMPLE, interval_ms, None);
            SetTimer(Some(hwnd), TIMER_MOVE, MOVE_MS, None);
            SetTimer(Some(hwnd), TIMER_CONFIG, CONFIG_POLL_MS, None);

            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}

fn main() {
    #[cfg(windows)]
    win::run();
    #[cfg(not(windows))]
    println!("app-win builds its display layer on Windows only.");
}
