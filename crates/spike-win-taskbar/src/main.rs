//! Windows taskbar-embedding spike (throwaway risk gate).
//!
//! Proves four things on a real desktop before the formal display layer is
//! built: (1) a child window can be `SetParent`-ed into `Shell_TrayWnd` and
//! stay put without flicker; (2) it anchors to the tray's left edge and
//! repositions when tray icons appear/disappear; (3) it self-heals after an
//! explorer restart via the `TaskbarCreated` broadcast; (4) text stays
//! readable in both light and dark themes.
//!
//! Deliberate choices, matching known constraints:
//! - Fixed-width window; move-only updates (`MoveWindow` keeps size constant)
//!   so re-embedding never lets the child's width collapse (known pitfall).
//! - `WS_EX_LAYERED` + color-key transparency lets the taskbar material show
//!   through. `WS_EX_NOACTIVATE` is intentionally NOT set — it swallows mouse
//!   input on layered windows.
//! - Per-Monitor-V2 DPI awareness at runtime, before any window is created,
//!   so coordinates are physical pixels end to end.
//! - Right-click quits, detaching from the taskbar first so no orphaned child
//!   survives on explorer's side.
//!
//! Text is static on purpose: this gate validates embedding behavior, not
//! sampling. Real rendering (Direct2D/DirectWrite, live values) is Phase 2.

fn main() {
    #[cfg(windows)]
    win::run();
    #[cfg(not(windows))]
    println!(
        "spike-win-taskbar: Windows-only. Build/run with `cargo run -p spike-win-taskbar` on Windows."
    );
}

#[cfg(windows)]
mod win {
    use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};

    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
    use windows::Win32::Graphics::Gdi::{
        BeginPaint, CreateFontW, CreateSolidBrush, DeleteObject, DrawTextW, EndPaint, GetDC,
        GetSysColor, ReleaseDC, ScreenToClient, SelectObject, SetBkMode, SetTextColor,
        CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, COLOR_WINDOWTEXT, DEFAULT_CHARSET, DEFAULT_PITCH,
        DT_CALCRECT, DT_CENTER, DT_END_ELLIPSIS, DT_SINGLELINE, DT_VCENTER, FF_SWISS, FW_NORMAL,
        HDC, HFONT, OUT_DEFAULT_PRECIS, PAINTSTRUCT, TRANSPARENT,
    };
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::HiDpi::{
        GetDpiForSystem, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, FindWindowExW,
        FindWindowW, GetClientRect, GetMessageW, GetWindowRect, KillTimer, MoveWindow,
        PostQuitMessage, RegisterClassExW, RegisterWindowMessageW, SetLayeredWindowAttributes,
        SetParent, SetTimer, TranslateMessage, CS_HREDRAW, CS_VREDRAW, LWA_COLORKEY, MSG,
        WM_DESTROY, WM_PAINT, WM_RBUTTONUP, WM_TIMER, WNDCLASSEXW, WS_EX_LAYERED, WS_POPUP,
    };
    /// Static sample line — layout-accurate but not live (see module docs).
    const DEMO_TEXT: &str = "CPU 42%  MEM 68%  U 1.20 M/s  D 34.0 M/s";
    /// Color keyed out so the taskbar material shows through the background.
    /// Never used as the text color, so no collision.
    const KEY_COLOR: COLORREF = COLORREF(0x0002_0202); // ~ (2,2,0) in BGR
    /// Gap between the widget's right edge and the tray's left edge.
    const TRAY_GAP_PX: i32 = 8;
    const TIMER_ID: usize = 0x515;
    const TIMER_MS: u32 = 1_000;
    /// Horizontal padding around the measured text.
    const PAD_PX: i32 = 12;

    static WM_TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);
    static WIN_W: AtomicI32 = AtomicI32::new(0);
    static WIN_H: AtomicI32 = AtomicI32::new(0);

    fn utf16(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Segoe UI sized to the system DPI. Negative height = character height
    /// (em box), matching how system metrics express font size.
    unsafe fn make_font() -> HFONT {
        let face = utf16("Segoe UI");
        let dpi = GetDpiForSystem() as i32;
        CreateFontW(
            -12 * dpi / 96,
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
            DEFAULT_PITCH.0 as u32 | FF_SWISS.0 as u32,
            PCWSTR(face.as_ptr()),
        )
    }

    /// Measure the static text once so the window size is fixed before
    /// embedding and never changes afterwards.
    unsafe fn measure() -> (i32, i32) {
        let hdc = GetDC(None);
        let font = make_font();
        let old = SelectObject(hdc, font.into());
        let mut text = utf16(DEMO_TEXT);
        let mut rc = RECT::default();
        DrawTextW(hdc, &mut text, &mut rc, DT_CALCRECT | DT_SINGLELINE);
        SelectObject(hdc, old);
        let _ = DeleteObject(font.into());
        ReleaseDC(None, hdc);
        (rc.right + PAD_PX, rc.bottom)
    }

    pub fn run() {
        unsafe {
            // Physical pixels everywhere; must precede any window creation.
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);

            let (w, h) = measure();
            WIN_W.store(w, Ordering::Relaxed);
            WIN_H.store(h, Ordering::Relaxed);

            let hmodule = GetModuleHandleW(None).expect("module handle");
            let class_name = utf16("SysVitalsSpike");
            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(wndproc),
                hInstance: hmodule.into(),
                // Erased backgrounds paint the key color, which the color-key
                // then makes fully transparent.
                hbrBackground: CreateSolidBrush(KEY_COLOR),
                lpszClassName: PCWSTR(class_name.as_ptr()),
                ..Default::default()
            };
            if RegisterClassExW(&wc) == 0 {
                panic!("RegisterClassExW failed");
            }

            let rm = utf16("TaskbarCreated");
            let id = RegisterWindowMessageW(PCWSTR(rm.as_ptr()));
            WM_TASKBAR_CREATED.store(id, Ordering::Relaxed);

            let title = utf16("sysvitals-spike");
            // Start parked off-screen; embed() reparents and positions it.
            let hwnd = CreateWindowExW(
                WS_EX_LAYERED,
                PCWSTR(class_name.as_ptr()),
                PCWSTR(title.as_ptr()),
                WS_POPUP,
                -32_000,
                -32_000,
                w,
                h,
                None,
                None,
                Some(hmodule.into()),
                None,
            )
            .expect("CreateWindowExW");

            let _ = SetLayeredWindowAttributes(hwnd, KEY_COLOR, 0, LWA_COLORKEY);
            embed(hwnd);
            SetTimer(Some(hwnd), TIMER_ID, TIMER_MS, None);

            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }

    /// Attach to `Shell_TrayWnd` (primary screen only) and position.
    unsafe fn embed(hwnd: HWND) {
        let cls = utf16("Shell_TrayWnd");
        if let Ok(tray) = FindWindowW(PCWSTR(cls.as_ptr()), None) {
            let _ = SetParent(hwnd, Some(tray));
            reposition(hwnd);
        }
    }

    /// Move-only placement: right edge sits `TRAY_GAP_PX` left of
    /// `TrayNotifyWnd`, vertically centered in the taskbar. Size never changes.
    unsafe fn reposition(hwnd: HWND) {
        let cls = utf16("Shell_TrayWnd");
        let Ok(tray) = FindWindowW(PCWSTR(cls.as_ptr()), None) else {
            return;
        };
        let notify_cls = utf16("TrayNotifyWnd");
        let notify = FindWindowExW(Some(tray), None, PCWSTR(notify_cls.as_ptr()), None)
            .unwrap_or_else(|_| HWND::default());
        if notify.is_invalid() {
            return;
        }

        let mut notify_screen = RECT::default();
        if GetWindowRect(notify, &mut notify_screen).is_err() {
            return;
        }

        let w = WIN_W.load(Ordering::Relaxed);
        let h = WIN_H.load(Ordering::Relaxed);

        // Anchor in SCREEN coords: the tray region's left edge at its vertical
        // center, then converted once into the tray's client space (which is
        // where our child window coordinates live after SetParent).
        let mut pt = POINT {
            x: notify_screen.left,
            y: (notify_screen.top + notify_screen.bottom) / 2,
        };
        if !ScreenToClient(tray, &mut pt).as_bool() {
            return;
        }

        let x = pt.x - w - TRAY_GAP_PX;
        let y = pt.y - h / 2;
        if x < 0 || y < 0 {
            return; // would overlap the taskbar's start edge; keep last pos
        }
        let _ = MoveWindow(hwnd, x, y, w, h, true);
    }

    unsafe extern "system" fn wndproc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if msg != 0 && msg == WM_TASKBAR_CREATED.load(Ordering::Relaxed) {
            // explorer restarted: this window was orphaned; re-attach.
            embed(hwnd);
            return LRESULT(0);
        }
        match msg {
            WM_TIMER => {
                reposition(hwnd);
                LRESULT(0)
            }
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                let hdc = BeginPaint(hwnd, &mut ps);
                paint_text(hwnd, hdc);
                let _ = EndPaint(hwnd, &ps);
                LRESULT(0)
            }
            WM_RBUTTONUP => {
                // Detach first so no orphaned child survives on explorer's side.
                let _ = SetParent(hwnd, None);
                let _ = KillTimer(Some(hwnd), TIMER_ID);
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

    unsafe fn paint_text(hwnd: HWND, hdc: HDC) {
        let mut rc = RECT::default();
        if GetClientRect(hwnd, &mut rc).is_err() {
            return;
        }
        let font = make_font();
        let old = SelectObject(hdc, font.into());
        SetTextColor(hdc, COLORREF(GetSysColor(COLOR_WINDOWTEXT)));
        SetBkMode(hdc, TRANSPARENT);
        let mut text = utf16(DEMO_TEXT);
        DrawTextW(
            hdc,
            &mut text,
            &mut rc,
            DT_SINGLELINE | DT_CENTER | DT_VCENTER | DT_END_ELLIPSIS,
        );
        SelectObject(hdc, old);
        let _ = DeleteObject(font.into());
    }
}
