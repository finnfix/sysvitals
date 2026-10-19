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
        format_frame, format_frame_vertical, t, Config, NetSelection, Sampler, Theme,
    };
    use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
    use windows::Win32::Graphics::Gdi::{
        CreateCompatibleDC, CreateDIBSection, CreateFontW, DeleteDC, DeleteObject, DrawTextW,
        GetDC, ReleaseDC, SelectObject, SetBkMode, SetTextColor, AC_SRC_ALPHA, AC_SRC_OVER,
        ANTIALIASED_QUALITY, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION,
        CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DEFAULT_PITCH, DIB_RGB_COLORS, DT_CALCRECT,
        DT_CENTER, DT_LEFT, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, FF_SWISS, FW_REGULAR,
        FW_SEMIBOLD, HFONT, OUT_DEFAULT_PRECIS, TRANSPARENT,
    };
    use windows::Win32::System::Diagnostics::Debug::{
        SetUnhandledExceptionFilter, EXCEPTION_POINTERS,
    };
    use windows::Win32::System::LibraryLoader::{
        GetModuleFileNameW, GetModuleHandleExW, GetModuleHandleW,
        GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
    };
    use windows::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CURRENT_USER, KEY_READ,
        REG_VALUE_TYPE,
    };
    use windows::Win32::System::SystemInformation::GetLocalTime;
    use windows::Win32::System::Threading::CreateMutexW;
    use windows::Win32::UI::HiDpi::{
        GetDpiForSystem, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    };
    use windows::Win32::UI::Shell::{
        SHAppBarMessage, ShellExecuteW, ABE_LEFT, ABE_RIGHT, ABM_GETTASKBARPOS, APPBARDATA,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
        DispatchMessageW, FindWindowExW, FindWindowW, GetCursorPos, GetMessageW, GetSystemMetrics,
        GetWindowRect, IsWindowVisible, KillTimer, MoveWindow, PostMessageW, PostQuitMessage,
        RegisterClassExW, SetTimer, SetWindowPos, ShowWindow, TrackPopupMenu, TranslateMessage,
        UpdateLayeredWindow, CS_DBLCLKS, CS_HREDRAW, CS_VREDRAW, HWND_TOPMOST, MF_STRING, MSG,
        SM_CXSCREEN, SM_CYSCREEN, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SW_HIDE,
        SW_SHOWNOACTIVATE, SW_SHOWNORMAL, TPM_LEFTALIGN, TPM_RETURNCMD, TPM_RIGHTBUTTON, ULW_ALPHA,
        WM_DESTROY, WM_DISPLAYCHANGE, WM_LBUTTONDBLCLK, WM_LBUTTONUP, WM_RBUTTONUP,
        WM_SETTINGCHANGE, WM_TIMER, WNDCLASSEXW, WS_EX_LAYERED, WS_EX_TOOLWINDOW, WS_POPUP,
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
    const THEME_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize";
    const THEME_VALUE: &str = "SystemUsesLightTheme";
    const TASKBAR_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\Advanced";
    const TASKBAR_VALUE: &str = "TaskbarAl";

    const IDM_SETTINGS: usize = 901;
    const IDM_QUIT: usize = 902;

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

    /// Overlay font: Segoe UI at the system caption size, grayscale AA.
    /// Negative height = character height (em box). Antialiased (not ClearType)
    /// because the glyphs are composited per-pixel-alpha over the taskbar.
    unsafe fn make_font() -> HFONT {
        let dpi = GetDpiForSystem();
        let face = utf16("Segoe UI");
        CreateFontW(
            -(12 * dpi as i32 / 96),
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

    /// Overlay font at an explicit pixel height. Used to render at a
    /// supersampled scale (2x) before downsampling.
    unsafe fn make_font_at(height_px: i32) -> HFONT {
        let face = utf16("Segoe UI");
        CreateFontW(
            -height_px,
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
        Some(TaskbarInfo { rect: r, vertical })
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
            // Vertical taskbar: horizontally centered, anchored top or bottom.
            let x = bar.left + (bar.right - bar.left - w) / 2;
            let left_anchor =
                cfg.windows.anchor == sysvitals_core::Anchor::Left && win11_centered_taskbar();
            let y = if left_anchor {
                bar.top + cfg.windows.left_margin_px as i32
            } else {
                bar.bottom - h - TRAY_GAP_PX
            };
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
    }

    unsafe fn measure_frame(
        hdc: windows::Win32::Graphics::Gdi::HDC,
        row: &FormattedRow,
        spacing_px: i32,
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
            };
        }

        if !multi_line || !has_right_col {
            let mut want_w = 0i32;
            for line in &row.lines {
                want_w = want_w.max(text_w(&row_text(line)));
            }
            return FrameMetrics {
                line_h,
                want_w,
                right_x: None,
                multi_line,
                item_widths: None,
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
        }
    }

    /// Measure the formatted rows and grow the window to fit. Width only
    /// ever grows; height follows the row count.
    unsafe fn resize_to_content(hwnd: HWND, row: &FormattedRow) {
        let cfg = config();
        let spacing_px = cfg.display.item_spacing as i32;
        let hdc = GetDC(Some(hwnd));
        let font = make_font();
        let old = SelectObject(hdc, font.into());
        let m = measure_frame(hdc, row, spacing_px);
        SelectObject(hdc, old);
        let _ = DeleteObject(font.into());
        let _ = ReleaseDC(Some(hwnd), hdc);
        if m.line_h == 0 {
            return;
        }
        let line_h = m.line_h + 2;
        let want_w = m.want_w + PAD_PX * 2 + WIDTH_SLACK_PX;
        let want_h = line_h * row.lines.len() as i32 + 2;
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
            let sampler = guard.get_or_insert_with(|| Sampler::new(NetSelection::Auto));
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
        let ss_font = make_font_at(12 * GetDpiForSystem() as i32 / 96 * SS);
        let old_ss_font = SelectObject(mem_dc, ss_font.into());
        SetBkMode(mem_dc, TRANSPARENT);
        SetTextColor(mem_dc, COLORREF(0x00FF_FFFF));

        // Clear the scratch surface to black (coverage 0).
        let ss_px = ss_bits as *mut u32;
        let ss_n = (sw * sh) as usize;
        for i in 0..ss_n {
            *ss_px.add(i) = 0;
        }

        let ss_line_h = sh / row.lines.len().max(1) as i32;
        let spacing_px = config().display.item_spacing as i32;
        let fm = measure_frame(mem_dc, &row, spacing_px * SS);
        for (i, line) in row.lines.iter().enumerate() {
            let top = i as i32 * ss_line_h;
            let bottom = (i as i32 + 1) * ss_line_h;
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
                    left: PAD_PX * SS,
                    top,
                    right: PAD_PX * SS + right_x,
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
                        left: PAD_PX * SS + right_x,
                        top,
                        right: sw - PAD_PX * SS,
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
                    left: PAD_PX * SS,
                    top,
                    right: sw - PAD_PX * SS,
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
        let n = (w * h) as usize;
        for i in 0..n {
            // Background: minimal (but non-zero) alpha so the whole rectangle
            // stays hit-testable yet invisible over the taskbar.
            *px.add(i) = 0x0100_0000;
        }
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
                if cov > 0 {
                    let a = cov;
                    let r = (tr * a) / 255;
                    let g = (tg * a) / 255;
                    let b = (tb * a) / 255;
                    *px.add((y * w + x) as usize) = (a << 24) | (r << 16) | (g << 8) | b;
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

    // -------------------------------------------------------- settings launcher

    /// Launch the external settings process (`SysVitalsSettings.exe`) from the
    /// same directory as the current executable. If the file is missing, logs
    /// a warning and does nothing.
    fn launch_settings() {
        let exe_path = current_exe_dir();
        let Some(dir) = exe_path else {
            log_event("WARN", "launch_settings: cannot determine exe directory");
            return;
        };
        let settings_path = format!("{}\\SysVitalsSettings.exe", dir);
        if !std::path::Path::new(&settings_path).exists() {
            log_event(
                "WARN",
                &format!("launch_settings: not found: {}", settings_path),
            );
            return;
        }
        log_event("INFO", &format!("launching settings: {}", settings_path));
        let path_w = utf16(&settings_path);
        let verb = utf16("open");
        unsafe {
            let _ = ShellExecuteW(
                None,
                windows::core::PCWSTR(verb.as_ptr()),
                windows::core::PCWSTR(path_w.as_ptr()),
                windows::core::PCWSTR::null(),
                windows::core::PCWSTR::null(),
                SW_SHOWNORMAL,
            );
        }
    }

    /// Launch the external history chart process (`SysVitalsHistory.exe`)
    /// from the same directory as the current executable. If the file is
    /// missing, logs a warning and does nothing.
    fn launch_history() {
        let exe_path = current_exe_dir();
        let Some(dir) = exe_path else {
            log_event("WARN", "launch_history: cannot determine exe directory");
            return;
        };
        let history_path = format!("{}\\SysVitalsHistory.exe", dir);
        if !std::path::Path::new(&history_path).exists() {
            log_event(
                "WARN",
                &format!("launch_history: not found: {}", history_path),
            );
            return;
        }
        log_event("INFO", &format!("launching history: {}", history_path));
        let path_w = utf16(&history_path);
        let verb = utf16("open");
        unsafe {
            let _ = ShellExecuteW(
                None,
                windows::core::PCWSTR(verb.as_ptr()),
                windows::core::PCWSTR(path_w.as_ptr()),
                windows::core::PCWSTR::null(),
                windows::core::PCWSTR::null(),
                SW_SHOWNORMAL,
            );
        }
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

    // -------------------------------------------------------------------- menu

    unsafe fn show_context_menu(hwnd: HWND) {
        let Ok(menu) = CreatePopupMenu() else {
            return;
        };
        let lang = config().display.language;
        let settings = utf16(t(lang, "menu.settings"));
        let quit = utf16(t(lang, "menu.quit"));
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            IDM_SETTINGS,
            windows::core::PCWSTR(settings.as_ptr()),
        );
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            IDM_QUIT,
            windows::core::PCWSTR(quit.as_ptr()),
        );
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
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
        let _ = DestroyMenu(menu);
        match choice {
            id if id == IDM_SETTINGS as u32 => launch_settings(),
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
            WM_LBUTTONUP => {
                // Single-click opens the history trend chart popup.
                launch_history();
                LRESULT(0)
            }
            WM_LBUTTONDBLCLK => {
                // Double-click opens Task Manager.
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
                LRESULT(0)
            }
            WM_RBUTTONUP => {
                show_context_menu(hwnd);
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
                style: CS_HREDRAW | CS_VREDRAW | CS_DBLCLKS,
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
            let hwnd = CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TOOLWINDOW,
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
