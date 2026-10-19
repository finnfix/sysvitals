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
//!   tick re-checks whether the taskbar is present (auto-hide) and whether a
//!   fullscreen foreground window should make room for it.
//! - Nothing is embedded, so there is no explorer-side state to roll back on
//!   exit; destroying the window is the whole teardown.
//!
//! Typography mirrors the taskbar clock: Segoe UI at the system caption size,
//! semibold weight, and pure white / pure black glyphs per theme so the
//! indicator reads as part of the taskbar rather than a foreign widget.
//! Rendering is GDI text for now; the layout strings come from the pure core
//! crate so the single-row / dual-row quadrant rules stay unit-tested there.

#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
mod win {
    use std::cell::RefCell;
    use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicUsize, Ordering};
    use std::sync::{OnceLock, RwLock};
    use std::time::{SystemTime, UNIX_EPOCH};

    use sysvitals_core::format::FormattedRow;
    use sysvitals_core::{format_frame, t, Config, Language, NetSelection, Sampler, Theme};
    use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
    use windows::Win32::Graphics::Gdi::{
        CreateCompatibleDC, CreateDIBSection, CreateFontW, CreateSolidBrush, DeleteDC,
        DeleteObject, DrawTextW, GetDC, GetSysColor, ReleaseDC, SelectObject, SetBkMode,
        SetTextColor, AC_SRC_ALPHA, AC_SRC_OVER, ANTIALIASED_QUALITY, BITMAPINFO, BITMAPINFOHEADER,
        BI_RGB, BLENDFUNCTION, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, COLOR_BTNFACE,
        DEFAULT_CHARSET, DEFAULT_PITCH, DIB_RGB_COLORS, DT_CALCRECT, DT_CENTER, DT_END_ELLIPSIS,
        DT_LEFT, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, FF_SWISS, FW_REGULAR, FW_SEMIBOLD, HFONT,
        OUT_DEFAULT_PRECIS, TRANSPARENT,
    };
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CURRENT_USER, KEY_READ,
        REG_VALUE_TYPE,
    };
    use windows::Win32::UI::HiDpi::{
        GetDpiForSystem, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    };
    use windows::Win32::UI::Shell::{
        SHAppBarMessage, ShellExecuteW, ABM_GETTASKBARPOS, APPBARDATA,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
        DispatchMessageW, EnumChildWindows, FindWindowExW, FindWindowW, GetCursorPos,
        GetForegroundWindow, GetMessageW, GetWindowRect, GetWindowTextW, IsWindowVisible,
        KillTimer, MoveWindow, PostQuitMessage, RegisterClassExW, SendMessageW, SetTimer,
        SetWindowTextW, ShowWindow, TrackPopupMenu, TranslateMessage, UpdateLayeredWindow,
        BS_AUTOCHECKBOX, BS_DEFPUSHBUTTON, CBS_DROPDOWNLIST, CS_DBLCLKS, CS_HREDRAW, CS_VREDRAW,
        ES_AUTOHSCROLL, HWND_TOPMOST, MF_STRING, MSG, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
        SW_HIDE, SW_SHOW, SW_SHOWNOACTIVATE, SW_SHOWNORMAL, TPM_LEFTALIGN, TPM_RETURNCMD,
        TPM_RIGHTBUTTON, ULW_ALPHA, WINDOW_STYLE, WM_CLOSE, WM_COMMAND, WM_DESTROY,
        WM_DISPLAYCHANGE, WM_LBUTTONDBLCLK, WM_RBUTTONUP, WM_SETTINGCHANGE, WM_TIMER, WNDCLASSEXW,
        WS_BORDER, WS_CAPTION, WS_CHILD, WS_EX_LAYERED, WS_EX_TOOLWINDOW, WS_POPUP, WS_SYSMENU,
        WS_VISIBLE,
    };

    /// Gap between the widget's edge and its anchor (tray left / taskbar left).
    /// Kept at 0 so the indicator sits as close to the tray as the tray's own
    /// icons do; the internal text padding provides the visual breathing room.
    const TRAY_GAP_PX: i32 = 0;
    const TIMER_SAMPLE: usize = 0x515;
    const TIMER_MOVE: usize = 0x516;
    const MOVE_MS: u32 = 1_000;
    /// Horizontal padding around the measured text. Small so the indicator can
    /// sit tight to the tray (matching tray-icon spacing) while still keeping
    /// a hit-testable margin.
    const PAD_PX: i32 = 6;
    const THEME_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize";
    const THEME_VALUE: &str = "SystemUsesLightTheme";
    const TASKBAR_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\Advanced";
    const TASKBAR_VALUE: &str = "TaskbarAl";

    const IDM_SETTINGS: usize = 901;
    const IDM_QUIT: usize = 902;
    // Dialog command ids (IDOK/IDCANCEL are messagebox results in this crate
    // generation, so the dialog uses its own stable ids).
    const CMD_OK: i32 = 1;
    const CMD_CANCEL: i32 = 2;
    const CMD_APPLY: i32 = 3;
    // Settings dialog control ids.
    const IDC_LANG: i32 = 101;
    const IDC_INTERVAL: i32 = 102;
    const IDC_DUAL: i32 = 103;
    const IDC_ANCHOR: i32 = 104;
    const IDC_THEME: i32 = 105;
    const IDC_SHOW_RES: i32 = 106;
    const IDC_SHOW_NET: i32 = 107;
    const IDC_PREFIX_CPU: i32 = 108;
    const IDC_PREFIX_MEM: i32 = 109;
    const IDC_PREFIX_UP: i32 = 110;
    const IDC_PREFIX_DOWN: i32 = 111;
    const IDC_SPACING: i32 = 112;

    static WIN_W: AtomicI32 = AtomicI32::new(0);
    static WIN_H: AtomicI32 = AtomicI32::new(0);
    /// Cached `SystemUsesLightTheme` (1 = light taskbar). Refreshed on
    /// `WM_SETTINGCHANGE` so a live theme switch repaints correctly.
    static LIGHT_THEME: AtomicU32 = AtomicU32::new(0);
    /// Glyph weight follows the taskbar theme, like the clock: semibold on
    /// the dark taskbar, regular on the light one.
    static TEXT_WEIGHT: AtomicI32 = AtomicI32::new(600);
    static CONFIG: OnceLock<RwLock<Config>> = OnceLock::new();
    static OVERLAY: AtomicUsize = AtomicUsize::new(0);
    static SETTINGS_OPEN: AtomicBool = AtomicBool::new(false);
    static SETTINGS_DONE: AtomicBool = AtomicBool::new(false);

    thread_local! {
        static SAMPLER: RefCell<Option<Sampler>> = const { RefCell::new(None) };
        static LAST_ROW: RefCell<Option<FormattedRow>> = const { RefCell::new(None) };
        static SETTINGS: RefCell<SettingsHandles> = RefCell::new(SettingsHandles::default());
        // History store (R12). Lazily opened on the first sample so a missing
        // data dir never blocks the indicator itself.
        static STORE: RefCell<Option<sysvitals_core::Store>> = const { RefCell::new(None) };
        static TICK_COUNT: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    }

    #[derive(Default, Clone, Copy)]
    struct SettingsHandles {
        dlg: HWND,
        lang: HWND,
        interval: HWND,
        dual: HWND,
        spacing: HWND,
        anchor: HWND,
        theme: HWND,
        show_res: HWND,
        show_net: HWND,
        prefix: [HWND; 4],
        // Text-bearing controls that need re-translation when the language
        // changes via Apply (issue: dialog used to stay in the old language).
        lbl_lang: HWND,
        lbl_interval: HWND,
        lbl_spacing: HWND,
        lbl_anchor: HWND,
        lbl_theme: HWND,
        lbl_prefixes: HWND,
        lbl_prefix: [HWND; 4],
        lbl_hint: HWND,
        btn_apply: HWND,
        btn_ok: HWND,
        btn_cancel: HWND,
    }

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
        // Glyph weight follows the taskbar theme like the clock: semibold on
        // the dark taskbar, regular on the light one.
        let weight = if LIGHT_THEME.load(Ordering::Relaxed) == 1 {
            FW_REGULAR.0 as i32
        } else {
            FW_SEMIBOLD.0 as i32
        };
        TEXT_WEIGHT.store(weight, Ordering::Relaxed);
    }

    /// Win11 with centered taskbar icons: the only case where the far-left
    /// anchor is offered (per product rules; left-aligned or Win10 taskbars
    /// keep the right anchor).
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

    /// Segoe UI at the system caption size with the theme-dependent weight
    /// (semibold on the dark taskbar, regular on the light one, matching the
    /// clock). Negative height = character height (em box), matching how
    /// system metrics express font size. Antialiased (not ClearType) quality:
    /// the glyphs are composited per-pixel-alpha over the taskbar, where
    /// ClearType's subpixel color fringing would show as artifacts.
    /// Segoe UI at the system caption size with the given weight and font
    /// quality. Negative height = character height (em box), matching how
    /// system metrics express font size.
    ///
    /// `quality` is the GDI font-quality flag and differs by surface:
    /// - The taskbar overlay is composited per-pixel-alpha, so it must use
    ///   grayscale antialiasing (`ANTIALIASED_QUALITY`); ClearType's subpixel
    ///   color fringing would show as artifacts through the alpha channel.
    /// - The settings dialog is an opaque window drawn straight to the screen,
    ///   so it uses `CLEARTYPE_QUALITY` for the crispest LCD rendering.
    unsafe fn make_font_with(
        weight: i32,
        quality: windows::Win32::Graphics::Gdi::FONT_QUALITY,
    ) -> HFONT {
        let dpi = GetDpiForSystem();
        let face = utf16("Segoe UI");
        CreateFontW(
            -(12 * dpi as i32 / 96),
            0,
            0,
            0,
            weight,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            quality,
            (DEFAULT_PITCH.0 | FF_SWISS.0) as u32,
            windows::core::PCWSTR(face.as_ptr()),
        )
    }

    /// Overlay font: grayscale AA (safe under per-pixel-alpha compositing).
    unsafe fn make_font() -> HFONT {
        make_font_with(TEXT_WEIGHT.load(Ordering::Relaxed), ANTIALIASED_QUALITY)
    }

    /// Overlay font at an explicit pixel height. Used to render at a
    /// supersampled scale (2x) before downsampling, which yields smoother
    /// glyph edges than native grayscale AA at the small caption size.
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

    /// Settings-dialog font: ClearType for crisp LCD text on the opaque
    /// dialog surface.
    unsafe fn make_dialog_font() -> HFONT {
        make_font_with(TEXT_WEIGHT.load(Ordering::Relaxed), CLEARTYPE_QUALITY)
    }

    /// True when the foreground window covers the whole primary screen, i.e.
    /// a fullscreen app (game, video, presentation) that the overlay should
    /// make room for.
    unsafe fn foreground_is_fullscreen(me: HWND) -> bool {
        let fg = GetForegroundWindow();
        if fg.is_invalid() || fg == me {
            return false;
        }
        let mut r = RECT::default();
        if GetWindowRect(fg, &mut r).is_err() {
            return false;
        }
        let w = windows::Win32::UI::WindowsAndMessaging::GetSystemMetrics(
            windows::Win32::UI::WindowsAndMessaging::SM_CXSCREEN,
        );
        let h = windows::Win32::UI::WindowsAndMessaging::GetSystemMetrics(
            windows::Win32::UI::WindowsAndMessaging::SM_CYSCREEN,
        );
        r.left <= 0 && r.top <= 0 && r.right >= w && r.bottom >= h
    }

    /// Taskbar rect in screen coordinates, or `None` when the taskbar is not
    /// currently on screen (auto-hidden, or shell not ready).
    unsafe fn taskbar_rect() -> Option<RECT> {
        let mut abd = APPBARDATA {
            cbSize: std::mem::size_of::<APPBARDATA>() as u32,
            ..Default::default()
        };
        if SHAppBarMessage(ABM_GETTASKBARPOS, &mut abd) == 0 {
            return None;
        }
        let r = abd.rc;
        let screen_h = windows::Win32::UI::WindowsAndMessaging::GetSystemMetrics(
            windows::Win32::UI::WindowsAndMessaging::SM_CYSCREEN,
        );
        let screen_w = windows::Win32::UI::WindowsAndMessaging::GetSystemMetrics(
            windows::Win32::UI::WindowsAndMessaging::SM_CXSCREEN,
        );
        // An auto-hidden taskbar reports a rect parked off-screen.
        let on_screen = r.bottom > 0 && r.top < screen_h && r.right > 0 && r.left < screen_w;
        if !on_screen {
            return None;
        }
        Some(r)
    }

    /// Move-only placement. Right anchor: right edge `TRAY_GAP_PX` left of
    /// `TrayNotifyWnd`. Left anchor (Win11 centered only): taskbar left edge
    /// plus the gap. Vertically centered in the taskbar. Size never changes
    /// here. Hides the window when there is nothing to sit on (taskbar gone)
    /// or a fullscreen app owns the screen, and restores it otherwise.
    unsafe fn reposition(hwnd: HWND) {
        let Some(bar) = taskbar_rect() else {
            let _ = ShowWindow(hwnd, SW_HIDE);
            return;
        };
        if foreground_is_fullscreen(hwnd) {
            let _ = ShowWindow(hwnd, SW_HIDE);
            return;
        }

        let cfg = config();
        let left_anchor =
            cfg.windows.anchor == sysvitals_core::Anchor::Left && win11_centered_taskbar();
        let w = WIN_W.load(Ordering::Relaxed);
        let h = WIN_H.load(Ordering::Relaxed);
        let x = if left_anchor {
            // Flush against the taskbar's left edge; only the internal text
            // padding provides breathing room, so the indicator reads tight to
            // the edge like the native clock.
            bar.left
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
        if x < 0 || y < 0 || x + w > bar.right {
            return; // would leave the taskbar; keep last position
        }
        let _ = MoveWindow(hwnd, x, y, w, h, true);
        // Only re-show when actually hidden (e.g. returning from auto-hide or
        // fullscreen). Calling ShowWindow unconditionally every tick makes an
        // owned settings dialog flicker.
        if !IsWindowVisible(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        }
    }

    /// Geometry of one frame measured with the font currently selected into
    /// `hdc`. `right_x` is `Some` only for a true two-column (dual-row) frame
    /// and gives the pixel offset (from the text origin) where the right column
    /// must start so U/D line up across both rows. Drawing the two columns as
    /// separate strings at this measured offset is what guarantees pixel-level
    /// alignment in a proportional font — space padding alone cannot.
    struct FrameMetrics {
        line_h: i32,
        want_w: i32,
        right_x: Option<i32>,
        /// True when the frame has more than one line (dual-row layout).
        /// Multi-line frames are always left-aligned; single-line is centered.
        multi_line: bool,
    }

    unsafe fn measure_frame(
        hdc: windows::Win32::Graphics::Gdi::HDC,
        row: &FormattedRow,
        spacing: u32,
    ) -> FrameMetrics {
        let gap = " ".repeat(spacing.max(1) as usize);
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
        if !multi_line || !has_right_col {
            // Single line, or multi-line with only one column: measure each
            // line independently and use the widest.
            let mut want_w = 0i32;
            for line in &row.lines {
                want_w = want_w.max(text_w(&row_text(line)));
            }
            return FrameMetrics {
                line_h,
                want_w,
                right_x: None,
                multi_line,
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
        let gap_w = text_w(&gap);
        let right_x = left_col_w + gap_w;
        FrameMetrics {
            line_h,
            want_w: right_x + right_col_w,
            right_x: Some(right_x),
            multi_line,
        }
    }

    /// Measure the formatted rows and grow the window to fit. Width only ever
    /// grows (content must not make the widget jiggle); height follows the
    /// row count so single/dual row switches stay exact.
    unsafe fn resize_to_content(hwnd: HWND, row: &FormattedRow) {
        let hdc = GetDC(Some(hwnd));
        let font = make_font();
        let old = SelectObject(hdc, font.into());
        let spacing = config().display.item_spacing;
        let m = measure_frame(hdc, row, spacing);
        SelectObject(hdc, old);
        let _ = DeleteObject(font.into());
        let _ = ReleaseDC(Some(hwnd), hdc);
        if m.line_h == 0 {
            return;
        }
        let line_h = m.line_h + 4; // breathing room between the two rows
        let want_w = m.want_w + PAD_PX * 2;
        let want_h = line_h * row.lines.len() as i32 + 4;
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
        // Persist CPU/MEM to the history store (R12). Network rates are
        // intentionally not stored. A store failure must never break the live
        // indicator, so errors are swallowed here.
        record_history(&cfg, now, &snapshot);
        let row = format_frame(&snapshot, &cfg);
        resize_to_content(hwnd, &row);
        LAST_ROW.with(|l| *l.borrow_mut() = Some(row));
        render_layered(hwnd);
    }

    /// Insert the current CPU/MEM sample into the history database and prune
    /// rows older than the retention window. Pruning runs once per ~300 ticks
    /// rather than every tick to keep the steady-state cost near zero.
    fn record_history(cfg: &Config, now: i64, snapshot: &sysvitals_core::Snapshot) {
        if !cfg.history.enabled {
            return;
        }
        STORE.with(|s| {
            let mut guard = s.borrow_mut();
            if guard.is_none() {
                if let Ok(path) = sysvitals_core::config::history_path() {
                    if let Some(parent) = path.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    *guard = sysvitals_core::Store::open(&path).ok();
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
    /// onto the layered window with `UpdateLayeredWindow`.
    ///
    /// Unlike color-key transparency (which makes the background click-through
    /// along with its pixels), per-pixel alpha keeps every pixel of the
    /// indicator rectangle hit-testable: the background is drawn at a tiny but
    /// non-zero alpha so the whole widget area accepts right-click for the
    /// context menu and double-click for Task Manager, while remaining
    /// effectively invisible over the taskbar material.
    ///
    /// Glyphs are rendered at 2x supersampled resolution into a scratch
    /// surface and box-filtered down. The overlay cannot use ClearType (its
    /// subpixel color fringing corrupts per-pixel-alpha compositing), so
    /// supersampling is how we get smooth, crisp edges at the small caption
    /// size.
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
        // Glyphs are drawn in white so the red channel doubles as a coverage
        // mask; the final color is applied per-pixel below from the theme.
        SetTextColor(mem_dc, COLORREF(0x00FF_FFFF));

        // Clear the scratch surface to black (coverage 0).
        let ss_px = ss_bits as *mut u32;
        let ss_n = (sw * sh) as usize;
        for i in 0..ss_n {
            *ss_px.add(i) = 0;
        }

        let ss_line_h = sh / row.lines.len().max(1) as i32;
        // Dual-row uses left-aligned columns (CPU over MEM, U over D) so the
        // two columns read as aligned pairs; single-row stays centered.
        //
        // Column alignment: in a proportional font, padding with spaces does
        // not guarantee the right column starts at the same pixel on every
        // row. So for dual-row we measure the left column's max pixel width
        // and draw the right column as a separate string at that measured
        // offset (scaled by the supersample factor). This is what makes U/D
        // line up vertically.
        let spacing = config().display.item_spacing;
        let fm = measure_frame(mem_dc, &row, spacing);
        for (i, line) in row.lines.iter().enumerate() {
            let top = i as i32 * ss_line_h;
            let bottom = (i as i32 + 1) * ss_line_h;
            if let Some(right_x) = fm.right_x {
                // Left column, left-aligned at the padding.
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
                    DT_SINGLELINE | DT_LEFT | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX,
                );
                // Right column at the measured pixel offset.
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
                        DT_SINGLELINE | DT_LEFT | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX,
                    );
                }
            } else if fm.multi_line {
                // Multi-line but single column (e.g. dual-row with network off):
                // left-aligned, not centered.
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
                    DT_SINGLELINE | DT_LEFT | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX,
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
                    DT_SINGLELINE | DT_CENTER | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX,
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
            // Background: minimal (but non-zero) alpha black so the whole
            // rectangle stays hit-testable yet invisible over the taskbar.
            *px.add(i) = 0x0100_0000;
        }
        for y in 0..h {
            for x in 0..w {
                // Box-filter the 2x2 supersampled block's red channel into a
                // single coverage value.
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

    // ---------------------------------------------------------------- menu

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
            id if id == IDM_SETTINGS as u32 => open_settings(hwnd),
            id if id == IDM_QUIT as u32 => {
                let _ = KillTimer(Some(hwnd), TIMER_SAMPLE);
                let _ = KillTimer(Some(hwnd), TIMER_MOVE);
                let _ = DestroyWindow(hwnd);
            }
            _ => {}
        }
    }

    // ------------------------------------------------------------- settings

    const CB_ADDSTRING: u32 = 0x0143;
    const CB_SETCURSEL: u32 = 0x014E;
    const CB_GETCURSEL: u32 = 0x0147;
    const CB_RESETCONTENT: u32 = 0x014A;
    const BM_GETCHECK: u32 = 0x00F0;
    const BM_SETCHECK: u32 = 0x00F1;
    const WM_SETFONT: u32 = 0x0030;
    const BST_CHECKED: usize = 1;

    /// Apply the dialog font to every child control so the settings window
    /// uses the same Segoe UI as the indicator instead of the legacy system
    /// font.
    unsafe extern "system" fn set_child_font(hwnd: HWND, lparam: LPARAM) -> windows::core::BOOL {
        let _ = SendMessageW(
            hwnd,
            WM_SETFONT,
            Some(WPARAM(lparam.0 as usize)),
            Some(LPARAM(1)),
        );
        windows::core::BOOL(1)
    }

    unsafe fn combo_add(cb: HWND, text: &str) {
        let buf = utf16(text);
        let _ = SendMessageW(
            cb,
            CB_ADDSTRING,
            Some(WPARAM(0)),
            Some(LPARAM(buf.as_ptr() as isize)),
        );
    }

    unsafe fn combo_sel(cb: HWND) -> i32 {
        SendMessageW(cb, CB_GETCURSEL, Some(WPARAM(0)), Some(LPARAM(0))).0 as i32
    }

    unsafe fn edit_text(edit: HWND) -> String {
        let mut buf = [0u16; 64];
        let n = GetWindowTextW(edit, &mut buf);
        String::from_utf16_lossy(&buf[..n.max(0) as usize])
    }

    unsafe fn static_at(parent: HWND, x: i32, y: i32, w: i32, h: i32, text: &str) -> HWND {
        let cls = utf16("STATIC");
        let cap = utf16(text);
        CreateWindowExW(
            Default::default(),
            windows::core::PCWSTR(cls.as_ptr()),
            windows::core::PCWSTR(cap.as_ptr()),
            WS_CHILD | WS_VISIBLE,
            x,
            y,
            w,
            h,
            Some(parent),
            None,
            None,
            None,
        )
        .unwrap_or_default()
    }

    unsafe fn control(
        parent: HWND,
        class: &str,
        id: i32,
        rect: (i32, i32, i32, i32),
        style: i32,
    ) -> HWND {
        let cls = utf16(class);
        CreateWindowExW(
            Default::default(),
            windows::core::PCWSTR(cls.as_ptr()),
            None,
            WS_CHILD | WS_VISIBLE | WINDOW_STYLE(style as u32),
            rect.0,
            rect.1,
            rect.2,
            rect.3,
            Some(parent),
            Some(windows::Win32::UI::WindowsAndMessaging::HMENU(
                id as isize as *mut std::ffi::c_void,
            )),
            None,
            None,
        )
        .unwrap_or_default()
    }

    /// Re-translate every static label, checkbox caption, combo item and
    /// button in the settings dialog to `lang`. Called after Apply so the
    /// dialog reflects the newly chosen language immediately (issue #1).
    unsafe fn refresh_settings_texts(lang: Language) {
        let h = SETTINGS.with(|s| *s.borrow());
        let set = |hwnd: HWND, text: &str| {
            let buf = utf16(text);
            let _ = SetWindowTextW(hwnd, windows::core::PCWSTR(buf.as_ptr()));
        };
        set(h.dlg, t(lang, "settings.title"));
        set(h.lbl_lang, t(lang, "settings.language"));
        set(h.lbl_interval, t(lang, "settings.sample_interval"));
        set(h.dual, t(lang, "settings.dual_row"));
        set(h.lbl_spacing, t(lang, "settings.item_spacing"));
        set(h.lbl_anchor, t(lang, "settings.anchor"));
        set(h.lbl_theme, t(lang, "settings.theme"));
        set(
            h.show_res,
            &format!("{} / {}", t(lang, "metric.cpu"), t(lang, "metric.mem")),
        );
        set(
            h.show_net,
            &format!("{} / {}", t(lang, "metric.up"), t(lang, "metric.down")),
        );
        set(h.lbl_prefixes, t(lang, "settings.prefixes"));
        let metric_keys = ["metric.cpu", "metric.mem", "metric.up", "metric.down"];
        for i in 0..4 {
            set(h.lbl_prefix[i], t(lang, metric_keys[i]));
        }
        set(h.lbl_hint, t(lang, "settings.prefix_hint"));
        set(h.btn_apply, t(lang, "settings.apply"));
        set(h.btn_ok, t(lang, "settings.ok"));
        set(h.btn_cancel, t(lang, "settings.cancel"));
        // Rebuild the anchor combo items in the new language, keeping the
        // current selection index.
        let sel = combo_sel(h.anchor);
        let _ = SendMessageW(h.anchor, CB_RESETCONTENT, Some(WPARAM(0)), Some(LPARAM(0)));
        combo_add(h.anchor, t(lang, "anchor.right"));
        if win11_centered_taskbar() {
            combo_add(h.anchor, t(lang, "anchor.left"));
        }
        let _ = SendMessageW(
            h.anchor,
            CB_SETCURSEL,
            Some(WPARAM(sel.max(0) as usize)),
            Some(LPARAM(0)),
        );
    }

    unsafe fn open_settings(owner: HWND) {
        if SETTINGS_OPEN.swap(true, Ordering::Relaxed) {
            return;
        }
        SETTINGS_DONE.store(false, Ordering::Relaxed);
        let cfg = config();
        let lang = cfg.display.language;
        let dpi = GetDpiForSystem() as i32;
        let s = |v: i32| v * dpi / 96;

        let hmodule = GetModuleHandleW(None).expect("module handle");
        let class_name = utf16("SysVitalsSettings");
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(settings_wndproc),
            hInstance: hmodule.into(),
            hbrBackground: CreateSolidBrush(COLORREF(GetSysColor(COLOR_BTNFACE))),
            lpszClassName: windows::core::PCWSTR(class_name.as_ptr()),
            ..Default::default()
        };
        let _ = RegisterClassExW(&wc);

        let title = utf16(t(lang, "settings.title"));
        let dlg = CreateWindowExW(
            Default::default(),
            windows::core::PCWSTR(class_name.as_ptr()),
            windows::core::PCWSTR(title.as_ptr()),
            WS_CAPTION | WS_SYSMENU,
            s(200),
            s(120),
            s(380),
            s(500),
            Some(owner),
            None,
            Some(hmodule.into()),
            None,
        )
        .expect("settings window");

        let label_w = s(140);
        let field_x = s(150);
        let field_w = s(200);
        let mut y = s(14);
        let row_h = s(30);

        let lbl_lang = static_at(
            dlg,
            s(12),
            y + s(4),
            label_w,
            s(20),
            t(lang, "settings.language"),
        );
        let lang_cb = control(
            dlg,
            "COMBOBOX",
            IDC_LANG,
            (field_x, y, field_w, s(120)),
            CBS_DROPDOWNLIST,
        );
        for l in Language::ALL {
            combo_add(lang_cb, l.label());
        }
        let _ = SendMessageW(
            lang_cb,
            CB_SETCURSEL,
            Some(WPARAM(
                Language::ALL
                    .iter()
                    .position(|x| *x == cfg.display.language)
                    .unwrap_or(0),
            )),
            Some(LPARAM(0)),
        );
        y += row_h;

        let lbl_interval = static_at(
            dlg,
            s(12),
            y + s(4),
            label_w,
            s(20),
            t(lang, "settings.sample_interval"),
        );
        let int_cb = control(
            dlg,
            "COMBOBOX",
            IDC_INTERVAL,
            (field_x, y, field_w, s(120)),
            CBS_DROPDOWNLIST,
        );
        for ms in sysvitals_core::config::ALLOWED_INTERVAL_MS {
            combo_add(int_cb, &format!("{} s", ms / 1000));
        }
        let idx = sysvitals_core::config::ALLOWED_INTERVAL_MS
            .iter()
            .position(|v| *v == cfg.sample.interval_ms)
            .unwrap_or(0);
        let _ = SendMessageW(int_cb, CB_SETCURSEL, Some(WPARAM(idx)), Some(LPARAM(0)));
        y += row_h + s(8);

        let dual = control(
            dlg,
            "BUTTON",
            IDC_DUAL,
            (s(12), y, s(340), s(20)),
            BS_AUTOCHECKBOX,
        );
        let dual_text = utf16(t(lang, "settings.dual_row"));
        let _ = SetWindowTextW(dual, windows::core::PCWSTR(dual_text.as_ptr()));
        if cfg.windows.dual_row {
            let _ = SendMessageW(
                dual,
                BM_SETCHECK,
                Some(WPARAM(BST_CHECKED)),
                Some(LPARAM(0)),
            );
        }
        y += row_h;

        let lbl_spacing = static_at(
            dlg,
            s(12),
            y + s(4),
            label_w,
            s(20),
            t(lang, "settings.item_spacing"),
        );
        let spacing_cb = control(
            dlg,
            "COMBOBOX",
            IDC_SPACING,
            (field_x, y, field_w, s(120)),
            CBS_DROPDOWNLIST,
        );
        for sp in 1..=8u32 {
            combo_add(spacing_cb, &format!("{sp}"));
        }
        let spacing_idx = (cfg.display.item_spacing.clamp(1, 8) - 1) as usize;
        let _ = SendMessageW(
            spacing_cb,
            CB_SETCURSEL,
            Some(WPARAM(spacing_idx)),
            Some(LPARAM(0)),
        );
        y += row_h;

        let lbl_anchor = static_at(
            dlg,
            s(12),
            y + s(4),
            label_w,
            s(20),
            t(lang, "settings.anchor"),
        );
        let anchor_cb = control(
            dlg,
            "COMBOBOX",
            IDC_ANCHOR,
            (field_x, y, field_w, s(120)),
            CBS_DROPDOWNLIST,
        );
        combo_add(anchor_cb, t(lang, "anchor.right"));
        let centered = win11_centered_taskbar();
        if centered {
            combo_add(anchor_cb, t(lang, "anchor.left"));
        }
        let anchor_left = cfg.windows.anchor == sysvitals_core::Anchor::Left && centered;
        let _ = SendMessageW(
            anchor_cb,
            CB_SETCURSEL,
            Some(WPARAM(anchor_left as usize)),
            Some(LPARAM(0)),
        );
        y += row_h;

        let lbl_theme = static_at(
            dlg,
            s(12),
            y + s(4),
            label_w,
            s(20),
            t(lang, "settings.theme"),
        );
        let theme_cb = control(
            dlg,
            "COMBOBOX",
            IDC_THEME,
            (field_x, y, field_w, s(120)),
            CBS_DROPDOWNLIST,
        );
        combo_add(theme_cb, t(lang, "theme.auto"));
        combo_add(theme_cb, t(lang, "theme.light"));
        combo_add(theme_cb, t(lang, "theme.dark"));
        let theme_idx = match cfg.display.theme {
            Theme::Auto => 0,
            Theme::Light => 1,
            Theme::Dark => 2,
        };
        let _ = SendMessageW(
            theme_cb,
            CB_SETCURSEL,
            Some(WPARAM(theme_idx)),
            Some(LPARAM(0)),
        );
        y += row_h;

        let res = control(
            dlg,
            "BUTTON",
            IDC_SHOW_RES,
            (s(12), y, s(340), s(20)),
            BS_AUTOCHECKBOX,
        );
        let res_text = utf16(&format!(
            "{} / {}",
            t(lang, "metric.cpu"),
            t(lang, "metric.mem")
        ));
        let _ = SetWindowTextW(res, windows::core::PCWSTR(res_text.as_ptr()));
        if cfg.display.show_resources {
            let _ = SendMessageW(res, BM_SETCHECK, Some(WPARAM(BST_CHECKED)), Some(LPARAM(0)));
        }
        y += row_h;

        let net = control(
            dlg,
            "BUTTON",
            IDC_SHOW_NET,
            (s(12), y, s(340), s(20)),
            BS_AUTOCHECKBOX,
        );
        let net_text = utf16(&format!(
            "{} / {}",
            t(lang, "metric.up"),
            t(lang, "metric.down")
        ));
        let _ = SetWindowTextW(net, windows::core::PCWSTR(net_text.as_ptr()));
        if cfg.display.show_network {
            let _ = SendMessageW(net, BM_SETCHECK, Some(WPARAM(BST_CHECKED)), Some(LPARAM(0)));
        }
        y += row_h + s(8);

        let lbl_prefixes = static_at(dlg, s(12), y, s(340), s(20), t(lang, "settings.prefixes"));
        y += s(24);
        let labels = [
            t(lang, "metric.cpu"),
            t(lang, "metric.mem"),
            t(lang, "metric.up"),
            t(lang, "metric.down"),
        ];
        let values = [
            cfg.display.prefix.cpu.clone(),
            cfg.display.prefix.mem.clone(),
            cfg.display.prefix.up.clone(),
            cfg.display.prefix.down.clone(),
        ];
        let ids = [
            IDC_PREFIX_CPU,
            IDC_PREFIX_MEM,
            IDC_PREFIX_UP,
            IDC_PREFIX_DOWN,
        ];
        let mut prefix_hwnds = [HWND::default(); 4];
        let mut prefix_label_hwnds = [HWND::default(); 4];
        for i in 0..4 {
            prefix_label_hwnds[i] = static_at(dlg, s(12), y + s(4), label_w, s(20), labels[i]);
            let edit = control(
                dlg,
                "EDIT",
                ids[i],
                (field_x, y, field_w, s(22)),
                ES_AUTOHSCROLL | WS_BORDER.0 as i32,
            );
            let v = utf16(&values[i]);
            let _ = SetWindowTextW(edit, windows::core::PCWSTR(v.as_ptr()));
            prefix_hwnds[i] = edit;
            y += row_h;
        }
        let lbl_hint = static_at(
            dlg,
            s(12),
            y,
            s(340),
            s(18),
            t(lang, "settings.prefix_hint"),
        );
        y += s(28);

        let apply = control(dlg, "BUTTON", CMD_APPLY, (s(10), y, s(90), s(26)), 0);
        let apply_text = utf16(t(lang, "settings.apply"));
        let _ = SetWindowTextW(apply, windows::core::PCWSTR(apply_text.as_ptr()));
        let ok = control(
            dlg,
            "BUTTON",
            CMD_OK,
            (s(190), y, s(90), s(26)),
            BS_DEFPUSHBUTTON,
        );
        let ok_text = utf16(t(lang, "settings.ok"));
        let _ = SetWindowTextW(ok, windows::core::PCWSTR(ok_text.as_ptr()));
        let cancel = control(dlg, "BUTTON", CMD_CANCEL, (s(280), y, s(90), s(26)), 0);
        let cancel_text = utf16(t(lang, "settings.cancel"));
        let _ = SetWindowTextW(cancel, windows::core::PCWSTR(cancel_text.as_ptr()));

        // Controls inherit the indicator's Segoe UI instead of the legacy
        // default dialog font, keeping the settings window visually native
        // to the rest of the product. ClearType quality: the dialog is an
        // opaque window, so subpixel rendering is crisp here (unlike the
        // alpha-composited overlay, which must stay grayscale AA).
        let dlg_font = make_dialog_font();
        let _ = EnumChildWindows(Some(dlg), Some(set_child_font), LPARAM(dlg_font.0 as isize));

        SETTINGS.with(|s| {
            *s.borrow_mut() = SettingsHandles {
                dlg,
                lang: lang_cb,
                interval: int_cb,
                dual,
                spacing: spacing_cb,
                anchor: anchor_cb,
                theme: theme_cb,
                show_res: res,
                show_net: net,
                prefix: prefix_hwnds,
                lbl_lang,
                lbl_interval,
                lbl_spacing,
                lbl_anchor,
                lbl_theme,
                lbl_prefixes,
                lbl_prefix: prefix_label_hwnds,
                lbl_hint,
                btn_apply: apply,
                btn_ok: ok,
                btn_cancel: cancel,
            };
        });

        // Do NOT disable the owner overlay: the indicator should stay
        // interactive (right-click menu, double-click) while settings is open.
        // The settings dialog is an owned window, so it stays on top of the
        // overlay regardless.
        let _ = ShowWindow(dlg, SW_SHOW);

        // Nested modal loop: the overlay keeps ticking behind the dialog
        // because every message still reaches DispatchMessageW.
        let mut msg = MSG::default();
        while !SETTINGS_DONE.load(Ordering::Relaxed) && GetMessageW(&mut msg, None, 0, 0).as_bool()
        {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        let _ = DeleteObject(dlg_font.into());
        SETTINGS_OPEN.store(false, Ordering::Relaxed);
    }

    /// Read the dialog controls back into the config, persist it, and make
    /// the running indicator reflect it immediately. The dialog stays open so
    /// the caller decides whether to close it (OK) or keep tweaking (Apply).
    unsafe fn collect_and_apply_settings() {
        let handles = SETTINGS.with(|s| *s.borrow());
        let mut cfg = config();
        let lang_idx = combo_sel(handles.lang);
        if (0..Language::ALL.len() as i32).contains(&lang_idx) {
            cfg.display.language = Language::ALL[lang_idx as usize];
        }
        let int_idx = combo_sel(handles.interval);
        if (0..sysvitals_core::config::ALLOWED_INTERVAL_MS.len() as i32).contains(&int_idx) {
            cfg.sample.interval_ms = sysvitals_core::config::ALLOWED_INTERVAL_MS[int_idx as usize];
        }
        cfg.windows.dual_row =
            SendMessageW(handles.dual, BM_GETCHECK, Some(WPARAM(0)), Some(LPARAM(0))).0 as usize
                == BST_CHECKED;
        let spacing_idx = combo_sel(handles.spacing);
        if (0..8).contains(&spacing_idx) {
            cfg.display.item_spacing = (spacing_idx + 1) as u32;
        }
        let anchor_idx = combo_sel(handles.anchor);
        cfg.windows.anchor = if anchor_idx == 1 {
            sysvitals_core::Anchor::Left
        } else {
            sysvitals_core::Anchor::Right
        };
        cfg.display.theme = match combo_sel(handles.theme) {
            1 => Theme::Light,
            2 => Theme::Dark,
            _ => Theme::Auto,
        };
        cfg.display.show_resources = SendMessageW(
            handles.show_res,
            BM_GETCHECK,
            Some(WPARAM(0)),
            Some(LPARAM(0)),
        )
        .0 as usize
            == BST_CHECKED;
        cfg.display.show_network = SendMessageW(
            handles.show_net,
            BM_GETCHECK,
            Some(WPARAM(0)),
            Some(LPARAM(0)),
        )
        .0 as usize
            == BST_CHECKED;
        cfg.display.prefix.cpu = edit_text(handles.prefix[0]);
        cfg.display.prefix.mem = edit_text(handles.prefix[1]);
        cfg.display.prefix.up = edit_text(handles.prefix[2]);
        cfg.display.prefix.down = edit_text(handles.prefix[3]);
        cfg.sanitize();
        if let Ok(path) = sysvitals_core::config::config_path() {
            let _ = cfg.save(&path);
        }
        store_config(cfg.clone());
        // Re-seed sampling so the new interval takes effect immediately.
        SAMPLER.with(|s| *s.borrow_mut() = None);
        let overlay = HWND(OVERLAY.load(Ordering::Relaxed) as *mut _);
        let _ = KillTimer(Some(overlay), TIMER_SAMPLE);
        SetTimer(
            Some(overlay),
            TIMER_SAMPLE,
            cfg.sample.interval_ms.clamp(500, 5000) as u32,
            None,
        );
        reposition(overlay);
        sample_now(overlay);
        // Re-translate the dialog itself so a language change via Apply is
        // visible immediately without reopening the window.
        refresh_settings_texts(cfg.display.language);
    }

    unsafe extern "system" fn settings_wndproc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match msg {
            WM_COMMAND => {
                let id = (wparam.0 & 0xFFFF) as i32;
                if id == CMD_OK {
                    collect_and_apply_settings();
                    let _ = DestroyWindow(hwnd);
                    SETTINGS_DONE.store(true, Ordering::Relaxed);
                    return LRESULT(0);
                }
                if id == CMD_APPLY {
                    // Apply in place; keep the dialog open for further tweaks.
                    collect_and_apply_settings();
                    return LRESULT(0);
                }
                if id == CMD_CANCEL {
                    let _ = DestroyWindow(hwnd);
                    SETTINGS_DONE.store(true, Ordering::Relaxed);
                    return LRESULT(0);
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_CLOSE => {
                let _ = DestroyWindow(hwnd);
                SETTINGS_DONE.store(true, Ordering::Relaxed);
                LRESULT(0)
            }
            WM_DESTROY => {
                SETTINGS_DONE.store(true, Ordering::Relaxed);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }

    // -------------------------------------------------------------- overlay

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
                    _ => {}
                }
                LRESULT(0)
            }
            WM_SETTINGCHANGE => {
                // Theme flips arrive here; re-read the flag and repaint.
                refresh_theme();
                render_layered(hwnd);
                LRESULT(0)
            }
            WM_DISPLAYCHANGE => {
                reposition(hwnd);
                LRESULT(0)
            }
            WM_LBUTTONDBLCLK => {
                // Double-click opens Task Manager, matching the quick-access
                // convention for status indicators.
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
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }

    pub fn run() {
        unsafe {
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            refresh_theme();
            let cfg = config();
            let interval_ms = cfg.sample.interval_ms.clamp(500, 5000) as u32;

            let hmodule = GetModuleHandleW(None).expect("module handle");
            let class_name = utf16("SysVitalsOverlay");
            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                // CS_DBLCLKS so WM_LBUTTONDBLCLK reaches the window proc.
                style: CS_HREDRAW | CS_VREDRAW | CS_DBLCLKS,
                lpfnWndProc: Some(wndproc),
                hInstance: hmodule.into(),
                // No background brush: the layered surface is composed entirely
                // by UpdateLayeredWindow.
                hbrBackground: windows::Win32::Graphics::Gdi::HBRUSH::default(),
                lpszClassName: windows::core::PCWSTR(class_name.as_ptr()),
                ..Default::default()
            };
            if RegisterClassExW(&wc) == 0 {
                return;
            }

            let title = utf16("SysVitals");
            // A tool window: no taskbar button of its own, never in Alt-Tab.
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
            OVERLAY.store(hwnd.0 as usize, Ordering::Relaxed);

            // Topmost so the overlay sits above the taskbar's own content.
            let _ = windows::Win32::UI::WindowsAndMessaging::SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
            // Seed one frame before showing so the first paint has content.
            sample_now(hwnd);
            reposition(hwnd);
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            SetTimer(Some(hwnd), TIMER_SAMPLE, interval_ms, None);
            SetTimer(Some(hwnd), TIMER_MOVE, MOVE_MS, None);

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
