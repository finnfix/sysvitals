//! SysVitals Settings — a modern GPUI-based settings window.
//!
//! Launched by the platform overlay (app-win / app-mac) when the user opens
//! settings. Reads the shared config, presents a modern grouped UI, and writes
//! every change back atomically as it happens: there are no dialog buttons,
//! the overlay watches the config file and hot-reloads it. The window itself
//! is closed with the system title-bar close button.
//!
//! Visual theme: light/dark follows the user's config preference (auto
//! mirrors the OS appearance and re-syncs on a live system flip), and on
//! Windows the primary button color is derived from the OS accent color so
//! the segmented controls match what the rest of the shell uses.

// A GUI child process must not allocate a console window on Windows: without
// this attribute the binary is built as a console subsystem executable and a
// terminal flashes up beside the settings window every time it launches.
#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_READ, REG_VALUE_TYPE, RegCloseKey, RegOpenKeyExW, RegQueryValueExW,
};
#[cfg(windows)]
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};

use gpui_kit::component::button::*;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::searchable_list::SearchableVec;
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::theme::Theme as UiTheme;
use gpui_kit::component::*;
// `.when(cond, |b| ...)` on builders comes from this trait; the glob above
// does not re-export it (app-history imports it the same way).
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use sysvitals_core::backdrop::Material;
use sysvitals_core::config::{
    ALLOWED_INTERVAL_MS, ALLOWED_ITEM_SPACING_PX, ALLOWED_LEFT_MARGIN_PX,
};
use sysvitals_core::{Anchor, Config, Language, Theme, t};

// ---------------------------------------------------------------------------
// Platform gate for the layout section
// ---------------------------------------------------------------------------

/// HKCU explorer keys, mirroring what the overlay reads: the Win11 centered-
/// taskbar flag decides whether the far-left anchor is offered, so the
/// settings window must consult the very same value.
#[cfg(windows)]
const TASKBAR_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\Advanced";
#[cfg(windows)]
const TASKBAR_VALUE: &str = "TaskbarAl";

#[cfg(windows)]
fn utf16(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(windows)]
fn hkcu_dword(key: &str, value: &str) -> Option<u32> {
    let key = utf16(key);
    let value = utf16(value);
    let mut hkey = HKEY::default();
    if unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            windows::core::PCWSTR(key.as_ptr()),
            None,
            KEY_READ,
            &mut hkey,
        )
    }
    .is_err()
    {
        return None;
    }
    let mut data = [0u8; 4];
    let mut size = 4u32;
    let mut kind = REG_VALUE_TYPE::default();
    let queried = unsafe {
        RegQueryValueExW(
            hkey,
            windows::core::PCWSTR(value.as_ptr()),
            None,
            Some(&mut kind),
            Some(data.as_mut_ptr()),
            Some(&mut size),
        )
    };
    let _ = unsafe { RegCloseKey(hkey) };
    if queried.is_ok() && size >= 4 {
        Some(u32::from_le_bytes(data))
    } else {
        None
    }
}

/// Whether the indicator's layout section is offered on this machine: the
/// far-left anchor exists only on Windows 11 with a centered taskbar.
#[cfg(windows)]
fn layout_available() -> bool {
    hkcu_dword(TASKBAR_KEY, TASKBAR_VALUE) == Some(1)
}

#[cfg(not(windows))]
fn layout_available() -> bool {
    false
}

/// Whether the taskbar is currently docked left or right. On such a bar the
/// margin value offsets the indicator from the bar's top edge, and that knob
/// must be editable even where the anchor section is not offered, so the
/// settings window probes the same app-bar position the overlay reads.
/// `false` anywhere the probe fails (macOS, hidden taskbar, pre-registered
/// shell) keeps the previous UI surface untouched.
#[cfg(windows)]
fn taskbar_is_vertical() -> bool {
    use windows::Win32::UI::Shell::{
        ABE_LEFT, ABE_RIGHT, ABM_GETTASKBARPOS, APPBARDATA, SHAppBarMessage,
    };
    use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CYSCREEN};
    let mut abd = APPBARDATA {
        cbSize: std::mem::size_of::<APPBARDATA>() as u32,
        ..Default::default()
    };
    if unsafe { SHAppBarMessage(ABM_GETTASKBARPOS, &mut abd) } == 0 {
        return false;
    }
    // Auto-hidden bars report a rect parked off-screen; then the orientation
    // does not matter for what is visible right now.
    let screen_h = unsafe { GetSystemMetrics(SM_CYSCREEN) };
    if abd.rc.bottom <= 0 || abd.rc.top >= screen_h {
        return false;
    }
    abd.uEdge == ABE_LEFT || abd.uEdge == ABE_RIGHT
}

#[cfg(not(windows))]
fn taskbar_is_vertical() -> bool {
    false
}

// ---------------------------------------------------------------------------
// Window backdrop material (Win11 Mica / macOS blur, with fallbacks)
// ---------------------------------------------------------------------------

/// Read the registry DWORD at `HKCU\Software\Microsoft\Windows\CurrentVersion\
/// Themes\Personalize\<value>`. `None` when the key is missing or unreadable.
#[cfg(windows)]
fn personalize_dword(value: &str) -> Option<u32> {
    const PERSONALIZE_KEY: &str =
        "Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize";
    hkcu_dword(PERSONALIZE_KEY, value)
}

/// The Windows OS build number, or `None` if the version probe fails.
///
/// `RtlGetVersion` is the documented way to read the real build (the classic
/// `GetVersionEx` is version-helpered and lies on 8.1+). The buffer is
/// zeroed first, so a failed call leaves `dwBuildNumber` at 0 and the caller
/// falls back to `Material::Opaque`.
#[cfg(windows)]
fn os_build_number() -> Option<u32> {
    use windows::Wdk::System::SystemServices::RtlGetVersion;
    use windows::Win32::System::SystemInformation::OSVERSIONINFOW;
    let mut info = OSVERSIONINFOW::default();
    info.dwOSVersionInfoSize = std::mem::size_of::<OSVERSIONINFOW>() as u32;
    if unsafe { RtlGetVersion(&mut info) }.is_err() {
        return None;
    }
    Some(info.dwBuildNumber)
}

/// The backdrop material this Windows host supports, honoring the user's
/// transparency-effects preference. Kept pure in `core`; this only feeds it
/// the two platform probes.
#[cfg(windows)]
fn detect_material() -> Material {
    // `EnableTransparency` = 0 means the user turned transparency effects off;
    // a missing value (None) is treated as enabled, matching the OS default.
    let transparency = personalize_dword("EnableTransparency")
        .map(|v| v != 0)
        .unwrap_or(true);
    sysvitals_core::backdrop::choose_windows_material(os_build_number(), transparency)
}

/// macOS always requests the blur material: `NSVisualEffectView` exists on
/// every supported host, and AppKit automatically flattens it when the user
/// enables "reduce transparency" — so the accessibility fallback is the
/// system's own, not something we re-implement.
#[cfg(target_os = "macos")]
fn detect_material() -> Material {
    sysvitals_core::backdrop::choose_macos_material(true)
}

/// Windows 11 ships Segoe UI Variable, whose default optical size is tuned
/// for UI text and renders small sizes more crisply than the legacy Segoe UI.
/// Older builds keep the system-resolved font; an unavailable family falls
/// back through GPUI's default stack.
#[cfg(windows)]
fn panel_font_family() -> Option<&'static str> {
    os_build_number()
        .filter(|build| *build >= 22000)
        .map(|_| "Segoe UI Variable")
}

#[cfg(not(any(windows, target_os = "macos")))]
fn detect_material() -> Material {
    Material::Opaque
}

/// Map the chosen material onto GPUI's window-background request. The pinned
/// platform layer recognizes all three variants on their respective hosts and
/// degrades gracefully to opaque where a variant is unsupported.
fn backdrop_for(material: Material) -> WindowBackgroundAppearance {
    match material {
        Material::Mica => WindowBackgroundAppearance::MicaBackdrop,
        Material::Blur => WindowBackgroundAppearance::Blurred,
        Material::Opaque => WindowBackgroundAppearance::Opaque,
    }
}

/// Unpack an AppKit `NSColor` into sRGB components packed `0xRRGGBB`.
///
/// Only for colors whose resolved value does not depend on the current
/// appearance (control accent, which reflects the user's color pick).
/// Dynamic-per-appearance catalog colors (window background, label colors)
/// would resolve to their light variants outside a draw cycle, so callers
/// wanting those must install the right NSAppearance first — none of our
/// calls do, so the tint path just uses the palette's own background (which
/// GPUI has already flipped on the same system signal).
///
/// Returns `None` if the sRGB conversion or the component read fails; callers
/// then keep their palette-driven fallback instead of painting black. All
/// call sites run on the main thread (GPUI drives the app from the UI thread).
#[cfg(target_os = "macos")]
fn system_color_hex(color: &objc2_app_kit::NSColor) -> Option<u32> {
    use objc2::rc::Retained;
    use objc2_app_kit::NSColorSpace;
    let srgb = NSColorSpace::sRGBColorSpace();
    let converted: Retained<objc2_app_kit::NSColor> = color.colorUsingColorSpace(&srgb)?;
    // `CGFloat` is a plain `f64` alias on the supported 64-bit targets, so
    // local `f64`s are exactly what the out-params expect.
    let (mut r, mut g, mut b, mut a) = (0f64, 0f64, 0f64, 0f64);
    unsafe { converted.getRed_green_blue_alpha(&mut r, &mut g, &mut b, &mut a) };
    let to8 = |x: f64| ((x.clamp(0.0, 1.0) * 255.0).round() as u32) & 0xFF;
    Some((to8(r) << 16) | (to8(g) << 8) | to8(b))
}

/// Apply the chosen backdrop to the palette: tint the root surface so the
/// material shows through. On the opaque fallback this is a no-op, keeping the
/// pre-material rendering byte-for-byte. Re-runs on every theme change (the
/// palette reload overwrites it) — the same replay contract as the accent.
fn apply_material(cx: &mut App) {
    use sysvitals_core::backdrop::{material_tint_alpha, should_tint};
    let material = detect_material();
    if !should_tint(material) {
        return;
    }
    UiTheme::update(cx, |theme| {
        let alpha = material_tint_alpha(theme.mode.is_dark());
        // Force an opaque base before scaling alpha, so the "set" and
        // "multiply" readings of `opacity` agree and the tint does not hinge
        // on the helper's exact semantics.
        let opaque = |c: Hsla| Hsla { a: 1.0, ..c };
        theme.colors.background = opaque(theme.colors.background).opacity(alpha);
    });
}

/// The system accent color packed as `0x00RRGGBB`, or `None` when it should
/// not be used at all.
///
/// Windows stores the accent the Settings app itself paints with in
/// `Explorer\Accent\AccentColorMenu` (ABGR). That value changes as soon as
/// the picker is confirmed. `DwmGetColorizationColor` is only a fallback: it
/// reports the DWM colorization blend, which can lag behind the picker and
/// is darkened by the transparency/color-prevalence mix, so it does not
/// always match the accent Windows paints on its own controls.
///
/// macOS follows only an *explicit* choice: `AppleAccentColor` in the
/// standard defaults is present exactly when the user picked a specific
/// accent (Graphite included) and absent under the default "auto" mode,
/// where AppKit derives the accent from the wallpaper. Staying absent under
/// auto keeps the library look (the agreed behavior — an auto wallpaper color
/// must not silently re-tint every primary control), and the check is
/// presence-only on purpose: if a future macOS reshuffles the key, the worst
/// case degrades to "keep the library default", never to a wrong color. When
/// present, `NSColor.controlAccentColor` is the user's pick and does not vary
/// with light/dark, so no appearance resolution is needed.
#[cfg(windows)]
fn system_accent_hex() -> Option<u32> {
    const ACCENT_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\Accent";
    if let Some(abgr) = hkcu_dword(ACCENT_KEY, "AccentColorMenu") {
        return Some(sysvitals_core::color::abgr_to_rgb(abgr));
    }
    // Older or unusual configurations may not have the Explorer value; the
    // DWM colorization color is still a usable approximation.
    use windows::Win32::Graphics::Dwm::DwmGetColorizationColor;
    let mut argb = 0u32;
    // Written via `Default::default()` so the code does not name the `BOOL`
    // type: in windows-core 0.62 that is a plain `pub type BOOL = i32` alias,
    // and naming it as `BOOL(0)` would compile as a tuple-struct constructor
    // (E0423). `Default` covers both aliases and newtypes.
    let opaque_result = unsafe { DwmGetColorizationColor(&mut argb, &mut Default::default()) };
    opaque_result.ok()?;
    Some(argb & 0x00FF_FFFF)
}

#[cfg(target_os = "macos")]
fn system_accent_hex() -> Option<u32> {
    use objc2_foundation::{NSString, NSUserDefaults};
    let key = NSString::from_str("AppleAccentColor");
    NSUserDefaults::standardUserDefaults().objectForKey(&key)?;
    system_color_hex(&objc2_app_kit::NSColor::controlAccentColor())
}

#[cfg(not(any(windows, target_os = "macos")))]
fn system_accent_hex() -> Option<u32> {
    None
}

/// Convert a packed 24-bit RGB hex to GPUI's `Hsla` (h in 0..1, s/l/a in
/// 0..1). Hand-rolled so this code path does not depend on whether the
/// current gpui-kit re-export exposes `rgb` and `Rgba`; only `hsla`, `Hsla`,
/// and `f32` math are required, all of which are already used in this file.
fn rgb_hex_to_hsla(hex: u32) -> Hsla {
    let r = ((hex >> 16) & 0xFF) as f32 / 255.0;
    let g = ((hex >> 8) & 0xFF) as f32 / 255.0;
    let b = (hex & 0xFF) as f32 / 255.0;
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) * 0.5;
    if (max - min).abs() < 1e-6 {
        return hsla(0.0, 0.0, l, 1.0);
    }
    let d = max - min;
    let s = if l > 0.5 {
        d / (2.0 - max - min)
    } else {
        d / (max + min)
    };
    let h = if (max - r).abs() < 1e-6 {
        ((g - b) / d + if g < b { 6.0 } else { 0.0 }) / 6.0
    } else if (max - g).abs() < 1e-6 {
        ((b - r) / d + 2.0) / 6.0
    } else {
        ((r - g) / d + 4.0) / 6.0
    };
    hsla(h, s, l, 1.0)
}

/// Push the OS accent color into the GPUI theme so every primary-tinted
/// control (segmented tiles, switches, focus rings, the Select popup's
/// active row) matches the Windows "Personalization / Colors" choice.
///
/// The library's default `ThemeColor.button_primary` is a fixed brand blue
/// that does NOT derive from `ThemeColor.primary`, so both fields are
/// written; the hover/active variants are derived by shifting lightness
/// around the accent, and `ring` follows so the focus outline agrees with
/// the button. The foreground is recomputed from the accent's lightness,
/// mirroring the OS: near-white text on a dark accent, near-black on a
/// light one. The palette default cannot be reused here — the shadcn
/// neutral set flips per mode (dark mode ships `neutral-900` black on
/// `neutral-50` white), which renders unreadable black text once the
/// background is an accent of mid luminance. A no-op on hosts with no
/// accent source or when the DWM call fails: the library's default palette
/// is preferable to a half-set theme.
fn apply_accent(cx: &mut App) {
    let Some(hex) = system_accent_hex() else {
        return;
    };
    let base = rgb_hex_to_hsla(hex);
    // Use WCAG relative luminance, not HSL lightness: a saturated yellow has
    // an HSL lightness near 0.5 but is far brighter than a mid-blue, so the
    // old HSL threshold painted white text on it.
    let foreground = if sysvitals_core::color::prefers_dark_foreground(hex) {
        hsla(0.0, 0.0, 0.08, 1.0)
    } else {
        hsla(0.0, 0.0, 0.97, 1.0)
    };
    UiTheme::update(cx, |theme| {
        let shift = |dl: f32| Hsla {
            l: (base.l + dl).clamp(0.05, 0.95),
            ..base
        };
        theme.colors.primary = base;
        theme.colors.button_primary = base;
        theme.colors.primary_hover = shift(0.06);
        theme.colors.button_primary_hover = shift(0.06);
        theme.colors.primary_active = shift(-0.06);
        theme.colors.button_primary_active = shift(-0.06);
        theme.colors.primary_foreground = foreground;
        theme.colors.button_primary_foreground = foreground;
        theme.colors.ring = base;
        // Keep the library's dropdown selection in the same accent family as
        // the rest of the system controls. The stock palette paints the
        // selected row blue no matter which accent the user picked.
        theme.colors.list_active = Hsla {
            a: if theme.mode.is_dark() { 0.26 } else { 0.16 },
            ..base
        };
        theme.colors.list_active_border = base;
        theme.colors.selection = Hsla { a: 0.30, ..base };
        theme.colors.caret = base;
    });
}

/// Sync the window's palette to the OS light/dark appearance. The panel
/// always follows the system, independent of the user's `display.theme`
/// setting (which now governs only the resident indicator). `Theme::change`
/// (which `sync_system_appearance` calls under the hood) reloads the whole
/// palette, so this must be followed by [`apply_accent`] — see
/// [`apply_theme_and_accent`].
fn apply_system_theme(cx: &mut App) {
    UiTheme::sync_system_appearance(None, cx);
}

/// Re-apply the theme palette, the accent override, and the backdrop tint
/// together, in that order. `Theme::change` (which `sync_system_appearance`
/// calls) loads the registered palette for the mode, overwriting every color
/// field — the accent and the material tint are runtime patches applied on
/// top, so both must follow every theme change, never precede it.
fn apply_theme_and_accent(cx: &mut App) {
    apply_system_theme(cx);
    apply_accent(cx);
    apply_material(cx);
    // Typography: keep the library's 16px rem base so its 0.875rem controls
    // (buttons, inputs, selects, dropdown rows) land on 14px, matching the
    // option labels on the left — the render root pins body text to that
    // 14px size. The focus ring is disabled because its 2px outer ring reads
    // as a heavy border on these compact controls, and clicking outside a
    // control is handled separately by blurring the window.
    UiTheme::update(cx, |theme| {
        theme.font_size = px(16.);
        theme.focus_ring = false;
        #[cfg(windows)]
        {
            if let Some(family) = panel_font_family() {
                theme.font_family = family.into();
            }
            let dark = theme.mode.is_dark();
            // WinUI control strokes and flyout surfaces. The library's
            // neutral-200/neutral-950 pair is much flatter than the Windows
            // Settings controls, which draw a visible stroke around a
            // subtly-filled input and a lighter flyout than the page behind
            // it.
            theme.colors.input = if dark {
                hsla(0., 0., 1., 0.30)
            } else {
                hsla(0., 0., 0., 0.26)
            };
            theme.colors.popover = if dark {
                hsla(0., 0., 0.17, 1.)
            } else {
                hsla(0., 0., 0.976, 1.)
            };
            theme.colors.popover_foreground = theme.colors.foreground;
        }
    });
}

// ---------------------------------------------------------------------------
// Dropdown item builders
// ---------------------------------------------------------------------------

/// Pixel-choice items are bare numbers; the row label carries the meaning, so
/// no unit suffix is repeated in every option.
fn px_items(values: &[u32]) -> Vec<String> {
    values.iter().map(|v| v.to_string()).collect()
}

/// Anchor items are translated text (the choice labels), rebuilt whenever the
/// selected language changes. Index 0 = right of the taskbar anchor space
/// (left of the tray), index 1 = far left.
fn anchor_items(lang: Language) -> Vec<String> {
    vec![
        t(lang, "anchor.right").to_string(),
        t(lang, "anchor.left").to_string(),
    ]
}

/// Every dropdown here is a single-choice list of strings (pixel values
/// rendered as decimal text, or translated anchor labels).
type StringSelectState = SelectState<SearchableVec<String>>;

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

struct SettingsView {
    cfg: Config,
    // Input states for prefixes
    prefix_cpu: Entity<InputState>,
    prefix_mem: Entity<InputState>,
    prefix_up: Entity<InputState>,
    prefix_down: Entity<InputState>,
    // Toggle states
    dual_row: bool,
    show_resources: bool,
    show_network: bool,
    // Index-based selections
    lang_idx: usize,
    interval_idx: usize,
    spacing_idx: usize,
    margin_idx: usize,
    theme_idx: usize,
    anchor_idx: usize,
    // Dropdown states (item text may carry the selected language)
    spacing_select: Entity<StringSelectState>,
    margin_select: Entity<StringSelectState>,
    anchor_select: Entity<StringSelectState>,
    /// Whether the layout section is offered on this machine (see
    /// `layout_available`). False hides it entirely.
    layout_available: bool,
    /// Whether the taskbar is docked left/right (see `taskbar_is_vertical`),
    /// probed once at view start. On such a bar the margin row is shown
    /// unconditionally (the value offsets the indicator from the bar's top
    /// edge), while the anchor dropdown — which selects between the tray end
    /// and the far-left end of a HORIZONTAL bar — would be a dead control and
    /// stays hidden.
    vertical_taskbar: bool,
    /// Last accent value seen by the live-refresh poll; the override only
    /// re-applies when this moves (see `new`).
    last_accent: Option<u32>,
    /// Whether the most recent config write failed. Shown in the footer so a
    /// read-only/full disk does not silently swallow every setting change.
    save_failed: std::cell::Cell<bool>,
    /// Keep event subscriptions alive for the lifetime of the view.
    _subscriptions: Vec<Subscription>,
    /// Flush guard: saves the current state whenever the window loses focus,
    /// so a prefix typed right before closing with the title-bar button is
    /// not lost (its field may never have received an explicit blur).
    _deactivation: Option<Subscription>,
    /// Kept alive so the appearance observer is not cancelled: a live OS
    /// light/dark flip reloads the system palette (the panel always follows
    /// the system, independent of the indicator-only `display.theme`).
    _appearance: Option<Subscription>,
}

impl SettingsView {
    fn new(cfg: Config, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let prefix_cpu =
            cx.new(|cx| InputState::new(window, cx).default_value(&cfg.display.prefix.cpu));
        let prefix_mem =
            cx.new(|cx| InputState::new(window, cx).default_value(&cfg.display.prefix.mem));
        let prefix_up =
            cx.new(|cx| InputState::new(window, cx).default_value(&cfg.display.prefix.up));
        let prefix_down =
            cx.new(|cx| InputState::new(window, cx).default_value(&cfg.display.prefix.down));

        let lang_idx = Language::ALL
            .iter()
            .position(|l| *l == cfg.display.language)
            .unwrap_or(0);
        let interval_idx = ALLOWED_INTERVAL_MS
            .iter()
            .position(|v| *v == cfg.sample.interval_ms)
            .unwrap_or(0);
        let spacing_idx = ALLOWED_ITEM_SPACING_PX
            .iter()
            .position(|v| *v == cfg.display.item_spacing)
            .unwrap_or(0);
        let margin_idx = ALLOWED_LEFT_MARGIN_PX
            .iter()
            .position(|v| *v == cfg.windows.left_margin_px)
            .unwrap_or(0);
        let anchor_idx = if cfg.windows.anchor == Anchor::Left {
            1
        } else {
            0
        };
        let theme_idx = match cfg.display.theme {
            Theme::Auto => 0,
            Theme::Light => 1,
            Theme::Dark => 2,
        };

        let lang = Language::ALL[lang_idx];
        let spacing_select = cx.new(|cx| {
            SelectState::new(
                SearchableVec::new(px_items(&ALLOWED_ITEM_SPACING_PX)),
                Some(IndexPath::new(spacing_idx)),
                window,
                cx,
            )
        });
        let margin_select = cx.new(|cx| {
            SelectState::new(
                SearchableVec::new(px_items(&ALLOWED_LEFT_MARGIN_PX)),
                Some(IndexPath::new(margin_idx)),
                window,
                cx,
            )
        });
        let anchor_select = cx.new(|cx| {
            SelectState::new(
                SearchableVec::new(anchor_items(lang)),
                Some(IndexPath::new(anchor_idx)),
                window,
                cx,
            )
        });

        // Every change is applied immediately: dropdown selections commit on
        // confirm, text fields commit on blur or Enter.
        let mut _subscriptions: Vec<Subscription> = Vec::new();
        _subscriptions.push(cx.subscribe(&spacing_select, |this: &mut Self, _, ev, cx| {
            if let SelectEvent::Confirm(Some(value)) = ev {
                if let Some(i) = ALLOWED_ITEM_SPACING_PX
                    .iter()
                    .position(|p| value.parse::<u32>().ok() == Some(*p))
                {
                    this.spacing_idx = i;
                    this.save(cx);
                    cx.notify();
                }
            }
        }));
        _subscriptions.push(cx.subscribe(&margin_select, |this: &mut Self, _, ev, cx| {
            if let SelectEvent::Confirm(Some(value)) = ev {
                if let Some(i) = ALLOWED_LEFT_MARGIN_PX
                    .iter()
                    .position(|p| value.parse::<u32>().ok() == Some(*p))
                {
                    this.margin_idx = i;
                    this.save(cx);
                    cx.notify();
                }
            }
        }));
        _subscriptions.push(cx.subscribe(&anchor_select, |this: &mut Self, _, ev, cx| {
            if let SelectEvent::Confirm(Some(value)) = ev {
                let items = anchor_items(this.current_lang());
                if let Some(i) = items.iter().position(|s| s == value) {
                    this.anchor_idx = i;
                    this.save(cx);
                    // The margin row appears only for the far-left anchor.
                    cx.notify();
                }
            }
        }));
        for state in [&prefix_cpu, &prefix_mem, &prefix_up, &prefix_down] {
            _subscriptions.push(
                cx.subscribe(state, |this: &mut Self, _, ev: &InputEvent, cx| {
                    match ev {
                        // Prefix text commits when the field loses focus or
                        // the user presses Enter, not on every keystroke.
                        // Safe to read the input states here: events are
                        // dispatched through the app's effect queue, so the
                        // emitting entity's borrow is already released.
                        InputEvent::Blur | InputEvent::PressEnter { .. } => this.save(cx),
                        _ => {}
                    }
                }),
            );
        }

        let view = Self {
            dual_row: cfg.display.dual_row,
            show_resources: cfg.display.show_resources,
            show_network: cfg.display.show_network,
            cfg,
            prefix_cpu,
            prefix_mem,
            prefix_up,
            prefix_down,
            lang_idx,
            interval_idx,
            spacing_idx,
            margin_idx,
            theme_idx,
            anchor_idx,
            spacing_select,
            margin_select,
            anchor_select,
            layout_available: layout_available(),
            vertical_taskbar: taskbar_is_vertical(),
            // Seed with `None` (a sentinel), not the live accent: if the
            // startup `apply_accent` were ever overwritten by a later
            // palette load, a real seed would make the poll see "no change"
            // and never re-apply. `None` forces one guaranteed re-apply on the
            // first 500 ms tick (idempotent when startup already succeeded),
            // after which it tracks the real value.
            last_accent: None,
            save_failed: std::cell::Cell::new(false),
            _subscriptions,
            _deactivation: None,
            _appearance: None,
        };
        // OS accent live-refresh. The appearance observer registered in
        // `render` only fires when the light/dark MODE flips (the platform
        // layer filters `WM_SETTINGCHANGE` to `ImmersiveColorSet` with a mode
        // change); the user changing the accent in the system Settings app
        // does not re-run it, so the panel would keep the old primary color
        // until it reopens. Poll the accent source every 500 ms and re-apply
        // the override only when the value actually moved; the loop stops
        // with the view entity, i.e. when the window closes. Same shape as
        // the history popup's data-refresh loop. The probe runs inside
        // `update` on purpose: on macOS `system_accent_hex` reads AppKit
        // (`controlAccentColor`), which belongs to the main thread — the same
        // thread `update` runs on.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(500))
                    .await;
                let alive = this
                    .update(cx, |view: &mut SettingsView, cx| {
                        let current = system_accent_hex();
                        if current == view.last_accent {
                            return true;
                        }
                        view.last_accent = current;
                        if current.is_some() {
                            apply_accent(cx);
                        } else {
                            // The OS moved back to "multicolor" (or the accent
                            // probe failed): reload the palette so the old accent
                            // override is removed.
                            apply_theme_and_accent(cx);
                        }
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);
                if !alive {
                    break;
                }
            }
        })
        .detach();
        view
    }

    fn current_lang(&self) -> Language {
        Language::ALL[self.lang_idx]
    }

    /// Enforce "at least one indicator group stays on": when the user turns
    /// off the last remaining group, re-enable the other one instead, so the
    /// indicator is never left with nothing to show.
    fn set_resources(&mut self, checked: bool) {
        self.show_resources = checked;
        if !self.show_resources && !self.show_network {
            self.show_network = true;
        }
    }

    fn set_network(&mut self, checked: bool) {
        self.show_network = checked;
        if !self.show_network && !self.show_resources {
            self.show_resources = true;
        }
    }

    fn collect_config(&self, cx: &App) -> Config {
        let mut cfg = self.cfg.clone();
        cfg.display.language = Language::ALL[self.lang_idx];
        cfg.sample.interval_ms = ALLOWED_INTERVAL_MS[self.interval_idx];
        cfg.display.item_spacing = ALLOWED_ITEM_SPACING_PX[self.spacing_idx];
        cfg.windows.left_margin_px = ALLOWED_LEFT_MARGIN_PX[self.margin_idx];
        cfg.windows.anchor = if self.anchor_idx == 1 {
            Anchor::Left
        } else {
            Anchor::Right
        };
        cfg.display.theme = match self.theme_idx {
            1 => Theme::Light,
            2 => Theme::Dark,
            _ => Theme::Auto,
        };
        cfg.display.dual_row = self.dual_row;
        cfg.display.show_resources = self.show_resources;
        cfg.display.show_network = self.show_network;
        cfg.display.prefix.cpu = self.prefix_cpu.read(cx).value().to_string();
        cfg.display.prefix.mem = self.prefix_mem.read(cx).value().to_string();
        cfg.display.prefix.up = self.prefix_up.read(cx).value().to_string();
        cfg.display.prefix.down = self.prefix_down.read(cx).value().to_string();
        cfg.sanitize();
        cfg
    }

    fn save(&self, cx: &App) {
        let cfg = self.collect_config(cx);
        let result = sysvitals_core::config::config_path().and_then(|path| cfg.save(&path));
        if let Err(e) = &result {
            eprintln!("app-settings: config save failed: {e}");
        }
        self.save_failed.set(result.is_err());
    }

    /// Rebuild the language-dependent anchor dropdown after a language flip,
    /// keeping the current selection. The dropdown entity and its
    /// subscription stay in place; only the items are swapped.
    fn refresh_anchor_items(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let items = anchor_items(self.current_lang());
        let idx = self.anchor_idx;
        self.anchor_select.update(cx, |state, cx| {
            state.set_items(SearchableVec::new(items), window, cx);
            state.set_selected_index(Some(IndexPath::new(idx)), window, cx);
        });
    }
}

/// A section heading: WinUI group headers are 12px semibold and muted.
fn section_label(text: String, dim: Hsla) -> impl IntoElement {
    div().text_xs().font_semibold().text_color(dim).child(text)
}

/// A Windows Settings-style card surface: rounded, subtly filled, and only
/// wide enough to group its rows. The fill lets the Mica backdrop show
/// through while giving the rows the same "card" separation the OS uses.
fn settings_card(bg: Hsla) -> Div {
    v_flex().w_full().rounded(px(8.)).bg(bg).p_3().gap_1()
}

/// A section heading paired with its card at the tight 4px label gap.
fn settings_section(title: String, card: impl IntoElement, dim: Hsla) -> impl IntoElement {
    v_flex()
        .gap_1()
        .child(section_label(title, dim))
        .child(card)
}

/// Blur the window when a mouse-down occurs outside `element`. Used around
/// inputs/selects so their focus border disappears when the user clicks any
/// other part of the settings page (GPUI keeps focus otherwise).
fn blur_on_outside(element: Div) -> Div {
    element.on_mouse_down_out(|_, window, cx| window.blur(cx))
}

/// A labelled control row: title on the left, a wrapped tile group on the
/// right. Centered vertically like a WinUI settings row.
fn labeled_row(title: String, control: impl IntoElement) -> impl IntoElement {
    h_flex()
        .justify_between()
        .items_center()
        .gap_4()
        .py_2()
        .child(title)
        .child(control)
}

/// A labelled on/off row with the WinUI row height.
fn switch_row(title: String, control: impl IntoElement) -> impl IntoElement {
    h_flex()
        .justify_between()
        .items_center()
        .py_2()
        .child(title)
        .child(control)
}

impl Render for SettingsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Re-probe the taskbar shape on every render: the user can move or
        // auto-hide the bar while this window stays open, and the Layout row
        // visibility must follow within the next repaint.
        self.vertical_taskbar = taskbar_is_vertical();
        self.layout_available = layout_available();
        // Deactivation flush (registered once): persist the current state
        // whenever the window loses focus, covering the case where a prefix
        // field still holds the last edit when the window is closed or the
        // user clicks away.
        if self._deactivation.is_none() {
            let sub = cx.observe_window_activation(window, |this: &mut Self, window, cx| {
                if !window.is_window_active() {
                    this.save(cx);
                }
            });
            self._deactivation = Some(sub);
        }

        // Live OS appearance flip (registered once): the settings panel
        // always mirrors the system light/dark theme (the `display.theme`
        // setting is indicator-only), so a system flip repaints this window
        // immediately. Same first-render registration pattern as the
        // activation observer (never from a paint-phase API — the root
        // view's empty active-node stack would panic the unwrap inside
        // `window.on_key_event`).
        if self._appearance.is_none() {
            let sub = cx.observe_window_appearance(window, |_this: &mut Self, _w, cx| {
                apply_theme_and_accent(cx);
                cx.notify();
            });
            self._appearance = Some(sub);
        }

        // Every label comes from the core i18n table, keyed on the
        // *currently selected* language, so switching language repaints the
        // whole window immediately instead of only after saving.
        let lang = self.current_lang();

        // Theme colors are read once up front so the conditional `.when`
        // closure below never touches `cx` (keeps the borrow checker out of
        // the builder-chain argument positions).
        let dim = cx.theme().foreground.opacity(0.5);
        let faint = cx.theme().foreground.opacity(0.4);

        // --- Flattened (segmented) option rows for small choice sets: every
        // choice is a visible tile, the active one carries the primary style.
        // Spacing/margin/anchor use dropdowns instead (longer option lists).
        let mut lang_row = h_flex().gap_1().flex_wrap();
        for i in 0..Language::ALL.len() {
            let active = i == self.lang_idx;
            lang_row = lang_row.child(
                Button::new(format!("lang-{i}"))
                    .small()
                    .when(active, |b| b.primary())
                    .label(Language::ALL[i].label().to_string())
                    .on_click(cx.listener(
                        move |this: &mut Self,
                              _: &ClickEvent,
                              window: &mut Window,
                              cx: &mut Context<Self>| {
                            this.lang_idx = i;
                            // The anchor choices are translated text: rebuild
                            // them so the dropdown repaints in the new
                            // language, then persist immediately.
                            this.refresh_anchor_items(window, cx);
                            this.save(cx);
                            cx.notify();
                        },
                    )),
            );
        }

        let mut interval_row = h_flex().gap_1().flex_wrap();
        for i in 0..ALLOWED_INTERVAL_MS.len() {
            let active = i == self.interval_idx;
            let secs = ALLOWED_INTERVAL_MS[i] / 1000;
            let text = format!("{secs} {}", t(lang, "settings.unit_seconds"));
            interval_row = interval_row.child(
                Button::new(format!("interval-{i}"))
                    .small()
                    .when(active, |b| b.primary())
                    .label(text)
                    .on_click(cx.listener(
                        move |this: &mut Self,
                              _: &ClickEvent,
                              _: &mut Window,
                              cx: &mut Context<Self>| {
                            this.interval_idx = i;
                            this.save(cx);
                            cx.notify();
                        },
                    )),
            );
        }

        // Theme choices render as icons (globe = follow system, sun = light,
        // moon = dark) with the translated name kept as the tooltip. All
        // three glyphs live in the bundled default asset set, so they paint
        // through the registered `Assets` source — a catalog name outside
        // that set (e.g. Monitor) would silently render blank.
        const THEME_KEYS: [&str; 3] = ["theme.auto", "theme.light", "theme.dark"];
        const THEME_ICONS: [IconName; 3] = [IconName::Globe, IconName::Sun, IconName::Moon];
        let mut theme_row = h_flex().gap_1();
        for i in 0..THEME_KEYS.len() {
            let active = i == self.theme_idx;
            let tip = t(lang, THEME_KEYS[i]).to_string();
            theme_row = theme_row.child(
                Button::new(format!("theme-{i}"))
                    .small()
                    .when(active, |b| b.primary())
                    .icon(THEME_ICONS[i].clone())
                    .tooltip(tip)
                    .on_click(cx.listener(
                        move |this: &mut Self,
                              _: &ClickEvent,
                              _: &mut Window,
                              cx: &mut Context<Self>| {
                            this.theme_idx = i;
                            // The choice is persisted for the indicator to
                            // hot-reload. It deliberately does NOT re-theme
                            // this window or the history popup: those always
                            // follow the system appearance, so changing it
                            // must not restyle the panels the user is
                            // looking at.
                            this.save(cx);
                            cx.notify();
                        },
                    )),
            );
        }

        // Style the dropdown triggers/inputs like Windows Settings: a filled
        // control brush, a visible 1px stroke and the WinUI 4px corner. The
        // per-element overrides win over the component's internal appearance
        // (its style refinement is applied last). Other hosts keep the
        // library look.
        let control: Option<(Hsla, Hsla, f32)> = {
            #[cfg(windows)]
            {
                let dark = cx.theme().mode.is_dark();
                let (bg, border) = if dark {
                    (hsla(0., 0., 1., 0.06), hsla(0., 0., 1., 0.30))
                } else {
                    (hsla(0., 0., 1., 0.72), hsla(0., 0., 0., 0.26))
                };
                Some((bg, border, 4.0))
            }
            #[cfg(not(windows))]
            {
                None
            }
        };
        let mut spacing_select = Select::new(&self.spacing_select);
        let mut margin_select = Select::new(&self.margin_select);
        let mut anchor_select = Select::new(&self.anchor_select);
        if let Some((bg, border, radius)) = control {
            spacing_select = spacing_select
                .bg(bg)
                .border_color(border)
                .rounded(px(radius));
            margin_select = margin_select
                .bg(bg)
                .border_color(border)
                .rounded(px(radius));
            anchor_select = anchor_select
                .bg(bg)
                .border_color(border)
                .rounded(px(radius));
        }

        let dark = cx.theme().mode.is_dark();
        // Shared card surface for both hosts. The library's `group_box` token
        // is neutral-950 in dark mode — the same value as the page background
        // — so using it would make the macOS cards disappear. A translucent
        // neutral lift follows each platform's grouped-settings look while
        // leaving the actual WinUI control chrome Windows-gated.
        let card_bg = if dark {
            hsla(0., 0., 1., 0.06)
        } else {
            hsla(0., 0., 1., 0.55)
        };

        let general_card = settings_card(card_bg)
            .child(labeled_row(
                t(lang, "settings.language").to_string(),
                lang_row,
            ))
            .child(labeled_row(
                t(lang, "settings.sample_interval").to_string(),
                interval_row,
            ))
            .when(cfg!(not(target_os = "macos")), |this| {
                // Offered only where the choice is actually honored. On
                // macOS the indicator title is drawn by AppKit, which picks
                // its own color from the menu-bar appearance.
                this.child(labeled_row(
                    t(lang, "settings.theme").to_string(),
                    theme_row,
                ))
            });

        let display_card = settings_card(card_bg)
            .child(switch_row(
                t(lang, "settings.dual_row").to_string(),
                Switch::new("dual-row")
                    .checked(self.dual_row)
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        this.dual_row = *checked;
                        this.save(cx);
                        cx.notify();
                    })),
            ))
            .child(switch_row(
                t(lang, "settings.group_resources").to_string(),
                Switch::new("show-res")
                    .checked(self.show_resources)
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        this.set_resources(*checked);
                        this.save(cx);
                        cx.notify();
                    })),
            ))
            .child(switch_row(
                t(lang, "settings.group_network").to_string(),
                Switch::new("show-net")
                    .checked(self.show_network)
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        this.set_network(*checked);
                        this.save(cx);
                        cx.notify();
                    })),
            ))
            .child(labeled_row(
                t(lang, "settings.item_spacing").to_string(),
                blur_on_outside(div().w(px(160.))).child(spacing_select),
            ));

        // One layout card holds the anchor and margin rows together; either
        // row may be hidden by the platform/taskbar shape, but they read as a
        // single group when both apply.
        let mut layout_card = settings_card(card_bg);
        let mut show_layout = false;
        if self.layout_available && !self.vertical_taskbar {
            layout_card = layout_card.child(labeled_row(
                t(lang, "settings.anchor").to_string(),
                blur_on_outside(div().w(px(240.))).child(anchor_select),
            ));
            show_layout = true;
        }
        let show_margin = (self.layout_available && self.anchor_idx == 1 && !self.vertical_taskbar)
            || self.vertical_taskbar;
        if show_margin {
            layout_card = layout_card.child(labeled_row(
                if self.vertical_taskbar {
                    t(lang, "settings.top_margin").to_string()
                } else {
                    t(lang, "settings.left_margin").to_string()
                },
                blur_on_outside(div().w(px(160.))).child(margin_select),
            ));
            show_layout = true;
        }

        let prefix_card = settings_card(card_bg)
            .child(
                v_flex()
                    .gap_3()
                    .child(
                        h_flex()
                            .gap_4()
                            .child(prefix_cell(
                                t(lang, "settings.cpu").to_string(),
                                &self.prefix_cpu,
                                faint,
                                control,
                            ))
                            .child(prefix_cell(
                                t(lang, "settings.mem").to_string(),
                                &self.prefix_mem,
                                faint,
                                control,
                            )),
                    )
                    .child(
                        h_flex()
                            .gap_4()
                            .child(prefix_cell(
                                t(lang, "settings.upload").to_string(),
                                &self.prefix_up,
                                faint,
                                control,
                            ))
                            .child(prefix_cell(
                                t(lang, "settings.download").to_string(),
                                &self.prefix_down,
                                faint,
                                control,
                            )),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(faint)
                    .child(t(lang, "settings.prefix_hint").to_string()),
            );

        v_flex()
            .size_full()
            .p_4()
            .gap_3()
            .text_size(px(14.))
            .overflow_y_scrollbar()
            .child(settings_section(
                t(lang, "settings.general").to_string(),
                general_card,
                dim,
            ))
            .child(settings_section(
                t(lang, "settings.display").to_string(),
                display_card,
                dim,
            ))
            .when(show_layout, |this| {
                this.child(settings_section(
                    t(lang, "settings.layout").to_string(),
                    layout_card,
                    dim,
                ))
            })
            .child(settings_section(
                t(lang, "settings.prefixes").to_string(),
                prefix_card,
                dim,
            ))
            .when(self.save_failed.get(), |this| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(faint)
                        .text_center()
                        .child(t(lang, "settings.save_failed").to_string()),
                )
            })
            // Version footer: pinned to the bottom of the scrollable column
            // and built from `SV_BUILD_ID` (see the const below).
            .child(
                div()
                    .mt_auto()
                    .pt_2()
                    .text_xs()
                    .text_color(faint)
                    .text_center()
                    .child(SV_BUILD_ID),
            )
    }
}

/// Version banner shown in the settings footer — see the footer for how it
/// is produced by the build script.
const SV_BUILD_ID: &str = match option_env!("SV_BUILD_ID") {
    Some(id) => id,
    None => concat!("v", env!("CARGO_PKG_VERSION")),
};

/// One prefix cell of the settings 2x2 grid: a dim caption above a flexible
/// text input. Takes the entity by reference so the render borrow survives
/// the child call.
fn prefix_cell(
    label: String,
    input: &Entity<InputState>,
    faint: Hsla,
    control: Option<(Hsla, Hsla, f32)>,
) -> impl IntoElement {
    let mut field = Input::new(input).w_full();
    if let Some((bg, border, radius)) = control {
        field = field.bg(bg).border_color(border).rounded(px(radius));
    }
    v_flex()
        .flex_1()
        .gap_1()
        .child(div().text_xs().text_color(faint).child(label))
        .child(blur_on_outside(div().w_full()).child(field))
}

/// Load the config, backing up a malformed file before falling back to
/// defaults. Without the backup the first instant-save would overwrite the
/// user's (hand-edited) file with defaults.
fn load_config_with_backup() -> Config {
    let Ok(path) = sysvitals_core::config::config_path() else {
        return Config::default();
    };
    match Config::load(&path) {
        Ok((cfg, _)) => cfg,
        Err(e) => {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let backup = path.with_file_name(format!("config.toml.corrupt-{stamp}"));
            let _ = std::fs::rename(&path, &backup);
            eprintln!(
                "app-settings: config load failed: {e}; backed up to {}",
                backup.display()
            );
            Config::default()
        }
    }
}

fn main() {
    // GPUI does not declare DPI awareness itself. Without this the process
    // is treated as DPI-unaware and Windows bitmap-stretches the whole window
    // on a scaled display, which is what makes the text look soft.
    #[cfg(windows)]
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    let cfg = load_config_with_backup();

    // macOS: run as a regular foreground app so opening the settings window
    // puts an icon in the Dock (user requirement), and quit with the last
    // window so closing it leaves no invisible process behind. Two platform
    // mechanics make this work: the binary lives inside
    // `SysVitals.app/Contents/MacOS`, so `+[NSBundle mainBundle]` resolves the
    // enclosing bundle and AppKit reads its `CFBundleIconFile` (AppIcon.icns)
    // for the Dock tile; and the default `Regular` policy is re-applied via
    // `setActivationPolicy_` at `did_finish_launching`, which overrides the
    // bundle's `LSUIElement` agent default at runtime. Whether the tile shows
    // the app icon or a generic one is an on-device check.
    //
    // The history popup deliberately keeps the headless request (Accessory
    // policy = no Dock icon): a transient chart panel should not announce
    // itself in the Dock.
    //
    // Windows has no equivalent concerns: the default windowed startup
    // already gives the window a taskbar entry via the icon embedded by the
    // build script.
    //
    // `with_assets(Assets)` registers the bundled Lucide icon catalog as the
    // GPUI asset source. Without it, the default source (`()`) returns
    // `Ok(None)` for every path, so every `IconName` — including the Select
    // dropdown's ChevronDown arrow — renders as blank.
    #[cfg(target_os = "macos")]
    let app = gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .with_quit_mode(QuitMode::LastWindowClosed);
    #[cfg(not(target_os = "macos"))]
    let app = gpui_kit::application().with_assets(gpui_kit::assets::Assets);

    app.run(move |cx| {
        gpui_kit::init(cx);
        // `gpui_component::theme::init` hardcodes `Theme::change(Light)`, so
        // the panel must sync to the system appearance before the window is
        // opened — otherwise a dark-mode desktop shows a light settings
        // panel for the first frame. The panel always follows the system,
        // independent of `display.theme` (which now only governs the
        // indicator). The OS accent override and the backdrop tint come with
        // it (all three are theme-color writes and must be sequenced
        // together).
        apply_theme_and_accent(cx);

        let window_options = WindowOptions {
            window_bounds: Some(WindowBounds::centered(size(px(420.), px(640.)), cx)),
            titlebar: Some(TitlebarOptions {
                title: Some(t(cfg.display.language, "settings.title").to_string().into()),
                ..Default::default()
            }),
            // System backdrop material (Win11 Mica / Win10-era acrylic blur /
            // macOS window blur, with an opaque fallback on anything older or
            // when the user disabled transparency effects). The palette tint
            // applied above is what lets this show through.
            window_background: backdrop_for(detect_material()),
            focus: true,
            ..Default::default()
        };

        gpui_kit::open_window(window_options, cx, move |window, cx| {
            cx.new(|cx| SettingsView::new(cfg, window, cx))
        })
        .expect("Failed to open settings window");
    });
}
