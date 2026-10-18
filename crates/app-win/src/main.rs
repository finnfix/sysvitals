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
    use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
    use windows::Win32::Graphics::Gdi::{
        BeginPaint, CreateFontW, CreateSolidBrush, DeleteObject, DrawTextW, EndPaint, GetDC,
        GetSysColor, InvalidateRect, ReleaseDC, SelectObject, SetBkMode, SetTextColor,
        CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, COLOR_BTNFACE, DEFAULT_CHARSET, DEFAULT_PITCH,
        DT_CALCRECT, DT_CENTER, DT_END_ELLIPSIS, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, FF_SWISS,
        FW_SEMIBOLD, HDC, HFONT, OUT_DEFAULT_PRECIS, PAINTSTRUCT, TRANSPARENT,
    };
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CURRENT_USER, KEY_READ,
        REG_VALUE_TYPE,
    };
    use windows::Win32::UI::HiDpi::{
        GetDpiForSystem, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    };
    use windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
    use windows::Win32::UI::Shell::{SHAppBarMessage, ABM_GETTASKBARPOS, APPBARDATA};
    use windows::Win32::UI::WindowsAndMessaging::{
        AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
        DispatchMessageW, EnumChildWindows, FindWindowExW, FindWindowW, GetClientRect,
        GetCursorPos, GetForegroundWindow, GetMessageW, GetWindowRect, GetWindowTextW, KillTimer,
        MoveWindow, PostQuitMessage, RegisterClassExW, SendMessageW, SetLayeredWindowAttributes,
        SetTimer, SetWindowTextW, ShowWindow, TrackPopupMenu, TranslateMessage, BS_AUTOCHECKBOX,
        BS_DEFPUSHBUTTON, CBS_DROPDOWNLIST, CS_HREDRAW, CS_VREDRAW, ES_AUTOHSCROLL, HWND_TOPMOST,
        LWA_COLORKEY, MF_STRING, MSG, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SW_HIDE, SW_SHOW,
        SW_SHOWNOACTIVATE, TPM_LEFTALIGN, TPM_RETURNCMD, TPM_RIGHTBUTTON, WINDOW_STYLE, WM_CLOSE,
        WM_COMMAND, WM_DESTROY, WM_DISPLAYCHANGE, WM_PAINT, WM_RBUTTONUP, WM_SETTINGCHANGE,
        WM_TIMER, WNDCLASSEXW, WS_BORDER, WS_CAPTION, WS_CHILD, WS_EX_LAYERED, WS_EX_TOOLWINDOW,
        WS_POPUP, WS_SYSMENU, WS_VISIBLE,
    };

    /// Color keyed out so the taskbar material shows through the background.
    /// Never used as the text color, so no collision.
    const KEY_COLOR: COLORREF = COLORREF(0x0002_0202); // ~ (2,2,0) in BGR
    /// Gap between the widget's edge and its anchor (tray left / taskbar left).
    const TRAY_GAP_PX: i32 = 8;
    const TIMER_SAMPLE: usize = 0x515;
    const TIMER_MOVE: usize = 0x516;
    const MOVE_MS: u32 = 1_000;
    /// Horizontal padding around the measured text.
    const PAD_PX: i32 = 12;
    const THEME_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize";
    const THEME_VALUE: &str = "SystemUsesLightTheme";
    const TASKBAR_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\Advanced";
    const TASKBAR_VALUE: &str = "TaskbarAl";
    /// Glyph colors matching the taskbar clock: pure white on the dark
    /// taskbar, pure black on the light one.
    const TEXT_DARK_THEME: u32 = 0x00FF_FFFF;
    const TEXT_LIGHT_THEME: u32 = 0x0000_0000;

    const IDM_SETTINGS: usize = 901;
    const IDM_QUIT: usize = 902;
    // Dialog command ids (IDOK/IDCANCEL are messagebox results in this crate
    // generation, so the dialog uses its own stable ids).
    const CMD_OK: i32 = 1;
    const CMD_CANCEL: i32 = 2;
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

    static WIN_W: AtomicI32 = AtomicI32::new(0);
    static WIN_H: AtomicI32 = AtomicI32::new(0);
    /// Cached `SystemUsesLightTheme` (1 = light taskbar). Refreshed on
    /// `WM_SETTINGCHANGE` so a live theme switch repaints correctly.
    static LIGHT_THEME: AtomicU32 = AtomicU32::new(0);
    static CONFIG: OnceLock<RwLock<Config>> = OnceLock::new();
    static OVERLAY: AtomicUsize = AtomicUsize::new(0);
    static SETTINGS_OPEN: AtomicBool = AtomicBool::new(false);
    static SETTINGS_DONE: AtomicBool = AtomicBool::new(false);

    thread_local! {
        static SAMPLER: RefCell<Option<Sampler>> = const { RefCell::new(None) };
        static LAST_ROW: RefCell<Option<FormattedRow>> = const { RefCell::new(None) };
        static SETTINGS: RefCell<SettingsHandles> = RefCell::new(SettingsHandles::default());
    }

    #[derive(Default, Clone, Copy)]
    struct SettingsHandles {
        lang: HWND,
        interval: HWND,
        dual: HWND,
        anchor: HWND,
        theme: HWND,
        show_res: HWND,
        show_net: HWND,
        prefix: [HWND; 4],
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
    }

    /// Win11 with centered taskbar icons: the only case where the far-left
    /// anchor is offered (per product rules; left-aligned or Win10 taskbars
    /// keep the right anchor).
    unsafe fn win11_centered_taskbar() -> bool {
        hkcu_dword(TASKBAR_KEY, TASKBAR_VALUE) == Some(1)
    }

    /// Foreground color matching the effective theme (config override wins
    /// over the system flag).
    fn text_color() -> COLORREF {
        let light = match config().display.theme {
            Theme::Auto => LIGHT_THEME.load(Ordering::Relaxed) == 1,
            Theme::Light => true,
            Theme::Dark => false,
        };
        if light {
            COLORREF(TEXT_LIGHT_THEME)
        } else {
            COLORREF(TEXT_DARK_THEME)
        }
    }

    /// Segoe UI at the system caption size, semibold like the taskbar clock.
    /// Negative height = character height (em box), matching how system
    /// metrics express font size.
    unsafe fn make_font() -> HFONT {
        let dpi = GetDpiForSystem();
        let face = utf16("Segoe UI");
        CreateFontW(
            -(12 * dpi as i32 / 96),
            0,
            0,
            0,
            FW_SEMIBOLD.0 as i32,
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
            bar.left + TRAY_GAP_PX
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
            let text = row_text(line);
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
        let row = format_frame(&snapshot, &cfg);
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
            let text = row_text(line);
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
            s(470),
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

        static_at(
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

        static_at(
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

        static_at(
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

        static_at(
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

        static_at(dlg, s(12), y, s(340), s(20), t(lang, "settings.prefixes"));
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
        for i in 0..4 {
            static_at(dlg, s(12), y + s(4), label_w, s(20), labels[i]);
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
        static_at(
            dlg,
            s(12),
            y,
            s(340),
            s(18),
            t(lang, "settings.prefix_hint"),
        );
        y += s(28);

        let ok = control(
            dlg,
            "BUTTON",
            CMD_OK,
            (s(180), y, s(90), s(26)),
            BS_DEFPUSHBUTTON,
        );
        let ok_text = utf16("OK");
        let _ = SetWindowTextW(ok, windows::core::PCWSTR(ok_text.as_ptr()));
        let cancel = control(dlg, "BUTTON", CMD_CANCEL, (s(280), y, s(80), s(26)), 0);
        let cancel_text = utf16("Cancel");
        let _ = SetWindowTextW(cancel, windows::core::PCWSTR(cancel_text.as_ptr()));

        // Controls inherit the indicator's Segoe UI instead of the legacy
        // default dialog font, keeping the settings window visually native
        // to the rest of the product.
        let dlg_font = make_font();
        let _ = EnumChildWindows(Some(dlg), Some(set_child_font), LPARAM(dlg_font.0 as isize));

        SETTINGS.with(|s| {
            *s.borrow_mut() = SettingsHandles {
                lang: lang_cb,
                interval: int_cb,
                dual,
                anchor: anchor_cb,
                theme: theme_cb,
                show_res: res,
                show_net: net,
                prefix: prefix_hwnds,
            };
        });

        let _ = EnableWindow(owner, false);
        let _ = ShowWindow(dlg, SW_SHOW);

        // Nested modal loop: the overlay keeps ticking behind the dialog
        // because every message still reaches DispatchMessageW.
        let mut msg = MSG::default();
        while !SETTINGS_DONE.load(Ordering::Relaxed) && GetMessageW(&mut msg, None, 0, 0).as_bool()
        {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        let _ = EnableWindow(owner, true);
        let _ = DeleteObject(dlg_font.into());
        SETTINGS_OPEN.store(false, Ordering::Relaxed);
    }

    unsafe fn apply_settings(hwnd_dialog: HWND) {
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
        let _ = DestroyWindow(hwnd_dialog);
        SETTINGS_DONE.store(true, Ordering::Relaxed);
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
                    apply_settings(hwnd);
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
            OVERLAY.store(hwnd.0 as usize, Ordering::Relaxed);

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
