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
//! Rendering is GDI text for now (measured, DPI-scaled Segoe UI, theme-aware
//! foreground color); the layout strings come from the pure core crate so the
//! single-row / dual-row quadrant rules stay unit-tested there.

#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
mod win {
    use std::cell::RefCell;
    use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};
    use std::sync::OnceLock;
    use std::time::{SystemTime, UNIX_EPOCH};

    use sysvitals_core::format::FormattedRow;
    use sysvitals_core::{format_frame, Config, NetSelection, Sampler};
    use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
    use windows::Win32::Graphics::Gdi::{
        BeginPaint, CreateFontW, CreateSolidBrush, DeleteObject, DrawTextW, EndPaint, GetDC,
        InvalidateRect, ReleaseDC, SelectObject, SetBkMode, SetTextColor, CLEARTYPE_QUALITY,
        CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DEFAULT_PITCH, DT_CALCRECT, DT_CENTER,
        DT_END_ELLIPSIS, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, FF_SWISS, FW_NORMAL, HDC, HFONT,
        OUT_DEFAULT_PRECIS, PAINTSTRUCT, TRANSPARENT,
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
        SHAppBarMessage, ABM_GETSTATE, ABM_GETTASKBARPOS, ABS_AUTOHIDE, APPBARDATA,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, FindWindowExW,
        FindWindowW, GetClientRect, GetForegroundWindow, GetMessageW, GetWindowRect, KillTimer,
        MoveWindow, PostQuitMessage, RegisterClassExW, SetLayeredWindowAttributes, SetTimer,
        ShowWindow, TranslateMessage, CS_HREDRAW, CS_VREDRAW, HWND_TOPMOST, LWA_COLORKEY, MSG,
        SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SW_SHOWNOACTIVATE, WM_DESTROY, WM_DISPLAYCHANGE,
        WM_PAINT, WM_RBUTTONUP, WM_SETTINGCHANGE, WM_TIMER, WNDCLASSEXW, WS_EX_LAYERED,
        WS_EX_TOOLWINDOW, WS_POPUP,
    };

    /// Color keyed out so the taskbar material shows through the background.
    /// Never used as the text color, so no collision.
    const KEY_COLOR: COLORREF = COLORREF(0x0002_0202); // ~ (2,2,0) in BGR
    /// Gap between the widget's right edge and the tray's left edge.
    const TRAY_GAP_PX: i32 = 8;
    const TIMER_SAMPLE: usize = 0x515;
    const TIMER_MOVE: usize = 0x516;
    const MOVE_MS: u32 = 1_000;
    /// Horizontal padding around the measured text.
    const PAD_PX: i32 = 12;
    /// Registry value naming the personalization key that holds the theme flag.
    const THEME_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize";
    const THEME_VALUE: &str = "SystemUsesLightTheme";
    /// Text color on a light taskbar (dark glyphs).
    const TEXT_DARK: u32 = 0x001A_1A1A;
    /// Text color on a dark taskbar (light glyphs).
    const TEXT_LIGHT: u32 = 0x00F2_F2F2;

    static WIN_W: AtomicI32 = AtomicI32::new(0);
    static WIN_H: AtomicI32 = AtomicI32::new(0);
    /// Cached `SystemUsesLightTheme` (1 = light taskbar). Refreshed on
    /// `WM_SETTINGCHANGE` so a live theme switch repaints correctly.
    static LIGHT_THEME: AtomicU32 = AtomicU32::new(0);
    static CONFIG: OnceLock<Config> = OnceLock::new();

    thread_local! {
        static SAMPLER: RefCell<Option<Sampler>> = const { RefCell::new(None) };
        static LAST_ROW: RefCell<Option<FormattedRow>> = const { RefCell::new(None) };
    }

    fn config() -> &'static Config {
        CONFIG.get_or_init(|| {
            // A missing or broken config file must not stop the indicator:
            // fall back to defaults and keep running.
            sysvitals_core::config::config_path()
                .and_then(|p| sysvitals_core::config::Config::load(&p).map(|(c, _)| c))
                .unwrap_or_default()
        })
    }

    fn utf16(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Read the taskbar theme from the documented personalization key.
    /// Classic `GetSysColor(COLOR_WINDOWTEXT)` is deliberately NOT used: it
    /// never follows the dark theme, which makes black text land on the dark
    /// taskbar (invisible). Missing value or key = assume dark (safe default
    /// on modern Windows).
    unsafe fn refresh_theme() {
        let key = utf16(THEME_KEY);
        let value = utf16(THEME_VALUE);
        let mut hkey = HKEY::default();
        let opened = RegOpenKeyExW(
            HKEY_CURRENT_USER,
            windows::core::PCWSTR(key.as_ptr()),
            None,
            KEY_READ,
            &mut hkey,
        );
        if opened.is_err() {
            LIGHT_THEME.store(0, Ordering::Relaxed);
            return;
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
        let light = if queried.is_ok() && size >= 4 {
            u32::from_le_bytes(data)
        } else {
            0
        };
        LIGHT_THEME.store(light.min(1), Ordering::Relaxed);
    }

    /// Foreground color matching the current taskbar theme.
    fn text_color() -> COLORREF {
        if LIGHT_THEME.load(Ordering::Relaxed) == 1 {
            COLORREF(TEXT_DARK)
        } else {
            COLORREF(TEXT_LIGHT)
        }
    }

    /// Segoe UI sized to the system DPI. Negative height = character height
    /// (em box), matching how system metrics express font size.
    unsafe fn make_font() -> HFONT {
        let dpi = GetDpiForSystem();
        let face = utf16("Segoe UI");
        CreateFontW(
            -(12 * dpi as i32 / 96),
            0,
            0,
            0,
            FW_NORMAL.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            CLEARTYPE_QUALITY,
            (DEFAULT_PITCH.0 | FF_SWISS.0) as u32,
            windows::core::PCWSTR(face.as_ptr()),
        )
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
        let _ = SHAppBarMessage(ABM_GETSTATE, &mut abd);
        let _ = ABS_AUTOHIDE; // state only matters via the off-screen check above
        Some(r)
    }

    /// Move-only placement: right edge sits `TRAY_GAP_PX` left of
    /// `TrayNotifyWnd`, vertically centered in the taskbar. Size never changes
    /// here. Hides the window when there is nothing to sit on (taskbar gone)
    /// or a fullscreen app owns the screen, and restores it otherwise.
    unsafe fn reposition(hwnd: HWND) {
        let Some(bar) = taskbar_rect() else {
            let _ = ShowWindow(hwnd, windows::Win32::UI::WindowsAndMessaging::SW_HIDE);
            return;
        };
        if foreground_is_fullscreen(hwnd) {
            let _ = ShowWindow(hwnd, windows::Win32::UI::WindowsAndMessaging::SW_HIDE);
            return;
        }

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

        let w = WIN_W.load(Ordering::Relaxed);
        let h = WIN_H.load(Ordering::Relaxed);
        let x = anchor_x - w - TRAY_GAP_PX;
        let y = bar.top + (bar.bottom - bar.top - h) / 2;
        if x < 0 || y < 0 {
            return; // would leave the screen; keep last position
        }
        let _ = MoveWindow(hwnd, x, y, w, h, true);
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    }

    /// Measure the formatted rows and grow the window to fit. Width only ever
    /// grows (content must not make the widget jiggle); height follows the
    /// row count so single/dual row switches stay exact.
    unsafe fn resize_to_content(hwnd: HWND, row: &FormattedRow) {
        let hdc = GetDC(Some(hwnd));
        let font = make_font();
        let old = SelectObject(hdc, font.into());
        let mut line_h = 0i32;
        let mut want_w = 0i32;
        for line in &row.lines {
            let text = if line.right.is_empty() {
                line.left.clone()
            } else {
                format!("{}  {}", line.left, line.right)
            };
            let mut rc = RECT::default();
            let mut buf = utf16(&text);
            DrawTextW(
                hdc,
                &mut buf,
                &mut rc,
                DT_CALCRECT | DT_SINGLELINE | DT_NOPREFIX,
            );
            line_h = line_h.max(rc.bottom - rc.top);
            want_w = want_w.max(rc.right - rc.left);
        }
        SelectObject(hdc, old);
        let _ = DeleteObject(font.into());
        let _ = ReleaseDC(Some(hwnd), hdc);
        if line_h == 0 {
            return;
        }
        let line_h = line_h + 4; // breathing room between the two rows
        let want_w = want_w + PAD_PX * 2;
        let want_h = line_h * row.lines.len() as i32 + 4;
        WIN_W.fetch_max(want_w, Ordering::Relaxed);
        WIN_H.store(want_h, Ordering::Relaxed);
        let w = WIN_W.load(Ordering::Relaxed);
        let mut rc = RECT::default();
        if GetWindowRect(hwnd, &mut rc).is_ok() {
            let _ = MoveWindow(hwnd, rc.left, rc.top, w, want_h, true);
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
        let row = format_frame(&snapshot, cfg);
        resize_to_content(hwnd, &row);
        LAST_ROW.with(|l| *l.borrow_mut() = Some(row));
        let _ = InvalidateRect(Some(hwnd), None, true);
    }

    unsafe fn paint_text(hwnd: HWND, hdc: HDC) {
        let mut rc = RECT::default();
        if GetClientRect(hwnd, &mut rc).is_err() {
            return;
        }
        let Some(row) = LAST_ROW.with(|l| l.borrow().clone()) else {
            return;
        };
        let font = make_font();
        let old = SelectObject(hdc, font.into());
        SetTextColor(hdc, text_color());
        SetBkMode(hdc, TRANSPARENT);
        let line_h = (rc.bottom - rc.top) / row.lines.len().max(1) as i32;
        for (i, line) in row.lines.iter().enumerate() {
            let text = if line.right.is_empty() {
                line.left.clone()
            } else {
                format!("{}  {}", line.left, line.right)
            };
            let mut line_rc = RECT {
                left: rc.left,
                top: rc.top + i as i32 * line_h,
                right: rc.right,
                bottom: rc.top + (i as i32 + 1) * line_h,
            };
            let mut buf = utf16(&text);
            DrawTextW(
                hdc,
                &mut buf,
                &mut line_rc,
                DT_SINGLELINE | DT_CENTER | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX,
            );
        }
        SelectObject(hdc, old);
        let _ = DeleteObject(font.into());
    }

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
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                let hdc = BeginPaint(hwnd, &mut ps);
                paint_text(hwnd, hdc);
                let _ = EndPaint(hwnd, &ps);
                LRESULT(0)
            }
            WM_SETTINGCHANGE => {
                // Theme flips arrive here; re-read the flag and repaint.
                refresh_theme();
                let _ = InvalidateRect(Some(hwnd), None, true);
                LRESULT(0)
            }
            WM_DISPLAYCHANGE => {
                reposition(hwnd);
                LRESULT(0)
            }
            WM_RBUTTONUP => {
                let _ = KillTimer(Some(hwnd), TIMER_SAMPLE);
                let _ = KillTimer(Some(hwnd), TIMER_MOVE);
                let _ = DestroyWindow(hwnd);
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
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(wndproc),
                hInstance: hmodule.into(),
                // Erased backgrounds paint the key color, which the color-key
                // then makes fully transparent.
                hbrBackground: CreateSolidBrush(KEY_COLOR),
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

            let _ = SetLayeredWindowAttributes(hwnd, KEY_COLOR, 0, LWA_COLORKEY);
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
