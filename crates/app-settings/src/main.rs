//! SysVitals Settings — a modern GPUI-based settings window.
//!
//! Launched by the platform overlay (app-win / app-mac) when the user opens
//! settings. Reads the shared config, presents a modern grouped UI, and writes
//! every change back atomically as it happens: there are no dialog buttons,
//! the overlay watches the config file and hot-reloads it. The window itself
//! is closed with the system title-bar close button.

// A GUI child process must not allocate a console window on Windows: without
// this attribute the binary is built as a console subsystem executable and a
// terminal flashes up beside the settings window every time it launches.
#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_READ, REG_VALUE_TYPE, RegCloseKey, RegOpenKeyExW, RegQueryValueExW,
};

use gpui_kit::component::button::*;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::searchable_list::SearchableVec;
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::*;
// `.when(cond, |b| ...)` on builders comes from this trait; the glob above
// does not re-export it (app-history imports it the same way).
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

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

/// Whether this machine offers the taskbar layout section at all: Windows 11
/// with centered taskbar icons is the only environment where the far-left
/// anchor is honored (the indicator itself falls back to the tray-side anchor
/// everywhere else, so the section is hidden instead).
#[cfg(windows)]
fn layout_available() -> bool {
    hkcu_dword(TASKBAR_KEY, TASKBAR_VALUE) == Some(1)
}

#[cfg(not(windows))]
fn layout_available() -> bool {
    false
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
    /// Keep event subscriptions alive for the lifetime of the view.
    _subscriptions: Vec<Subscription>,
    /// Flush guard: saves the current state whenever the window loses focus,
    /// so a prefix typed right before closing with the title-bar button is
    /// not lost (its field may never have received an explicit blur).
    _deactivation: Option<Subscription>,
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

        Self {
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
            _subscriptions,
            _deactivation: None,
        }
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
        if let Ok(path) = sysvitals_core::config::config_path() {
            let _ = cfg.save(&path);
        }
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

/// A section heading: small, semibold, dimmed.
fn section_label(text: String, dim: Hsla) -> impl IntoElement {
    div().text_sm().font_semibold().text_color(dim).child(text)
}

/// A hairline separator between sections.
fn divider(border: Hsla) -> impl IntoElement {
    div().h(px(1.)).bg(border).my_1()
}

/// A labelled control row: title on the left, a wrapped tile group on the
/// right.
fn labeled_row(title: String, control: impl IntoElement) -> impl IntoElement {
    h_flex()
        .justify_between()
        .items_start()
        .gap_4()
        .py_1()
        .child(title)
        .child(control)
}

impl Render for SettingsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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

        // Every label comes from the core i18n table, keyed on the
        // *currently selected* language, so switching language repaints the
        // whole window immediately instead of only after saving.
        let lang = self.current_lang();

        // Theme colors are read once up front so the conditional `.when`
        // closure below never touches `cx` (keeps the borrow checker out of
        // the builder-chain argument positions).
        let dim = cx.theme().foreground.opacity(0.5);
        let faint = cx.theme().foreground.opacity(0.4);
        let border = cx.theme().border;

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

        const THEME_KEYS: [&str; 3] = ["theme.auto", "theme.light", "theme.dark"];
        let mut theme_row = h_flex().gap_1().flex_wrap();
        for i in 0..THEME_KEYS.len() {
            let active = i == self.theme_idx;
            let text = t(lang, THEME_KEYS[i]).to_string();
            theme_row = theme_row.child(
                Button::new(format!("theme-{i}"))
                    .small()
                    .when(active, |b| b.primary())
                    .label(text)
                    .on_click(cx.listener(
                        move |this: &mut Self,
                              _: &ClickEvent,
                              _: &mut Window,
                              cx: &mut Context<Self>| {
                            this.theme_idx = i;
                            this.save(cx);
                            cx.notify();
                        },
                    )),
            );
        }

        v_flex()
            .size_full()
            .p_4()
            .gap_2()
            .overflow_y_scrollbar()
            // --- General ---
            .child(section_label(t(lang, "settings.general").to_string(), dim))
            .child(labeled_row(
                t(lang, "settings.language").to_string(),
                lang_row,
            ))
            .child(labeled_row(
                t(lang, "settings.sample_interval").to_string(),
                interval_row,
            ))
            .child(labeled_row(
                t(lang, "settings.theme").to_string(),
                theme_row,
            ))
            // --- Display ---
            .child(divider(border))
            .child(section_label(t(lang, "settings.display").to_string(), dim))
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .py_1()
                    .child(t(lang, "settings.dual_row").to_string())
                    .child(
                        Switch::new("dual-row")
                            .checked(self.dual_row)
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                this.dual_row = *checked;
                                this.save(cx);
                                cx.notify();
                            })),
                    ),
            )
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .py_1()
                    .child(t(lang, "settings.group_resources").to_string())
                    .child(
                        Switch::new("show-res")
                            .checked(self.show_resources)
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                this.set_resources(*checked);
                                this.save(cx);
                                cx.notify();
                            })),
                    ),
            )
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .py_1()
                    .child(t(lang, "settings.group_network").to_string())
                    .child(Switch::new("show-net").checked(self.show_network).on_click(
                        cx.listener(|this, checked: &bool, _, cx| {
                            this.set_network(*checked);
                            this.save(cx);
                            cx.notify();
                        }),
                    )),
            )
            .child(labeled_row(
                t(lang, "settings.item_spacing").to_string(),
                div().w(px(160.)).child(Select::new(&self.spacing_select)),
            ))
            // --- Layout: only offered where the choice actually exists
            // (Windows 11 with a centered taskbar, read once at view start);
            // the margin row additionally appears only for the far-left
            // anchor.
            .when(self.layout_available, |this| {
                this.child(divider(border))
                    .child(section_label(t(lang, "settings.layout").to_string(), dim))
                    .child(labeled_row(
                        t(lang, "settings.anchor").to_string(),
                        div().w(px(240.)).child(Select::new(&self.anchor_select)),
                    ))
            })
            .when(self.layout_available && self.anchor_idx == 1, |this| {
                this.child(labeled_row(
                    t(lang, "settings.left_margin").to_string(),
                    div().w(px(160.)).child(Select::new(&self.margin_select)),
                ))
            })
            // --- Prefixes ---
            .child(divider(border))
            .child(section_label(t(lang, "settings.prefixes").to_string(), dim))
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .py_1()
                    .child(t(lang, "settings.cpu").to_string())
                    .child(Input::new(&self.prefix_cpu).w(px(180.))),
            )
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .py_1()
                    .child(t(lang, "settings.mem").to_string())
                    .child(Input::new(&self.prefix_mem).w(px(180.))),
            )
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .py_1()
                    .child(t(lang, "settings.upload").to_string())
                    .child(Input::new(&self.prefix_up).w(px(180.))),
            )
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .py_1()
                    .child(t(lang, "settings.download").to_string())
                    .child(Input::new(&self.prefix_down).w(px(180.))),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(faint)
                    .child(t(lang, "settings.prefix_hint").to_string()),
            )
    }
}

fn main() {
    let cfg = sysvitals_core::config::config_path()
        .and_then(|p| Config::load(&p).map(|(c, _)| c))
        .unwrap_or_default();

    // macOS: this process is a transient panel of a menu-bar utility, so it
    // must not own foreground application UI (a Dock icon and a menu bar named
    // after the process). Requesting the headless windowing mode maps to the
    // AppKit accessory activation policy before the run loop starts, which
    // still opens windows normally. The default quit mode on macOS is explicit
    // (the process survives its last window), which would leave an invisible
    // accessory process running after the user closes this window, so the
    // process is asked to quit with its last window instead.
    //
    // Windows has no equivalent: a headless windowing request there refuses to
    // open windows, so the default windowed startup is left untouched there.
    #[cfg(target_os = "macos")]
    let app = gpui_kit::application()
        .with_windowing(WindowingRequest::Headless)
        .with_quit_mode(QuitMode::LastWindowClosed);
    #[cfg(not(target_os = "macos"))]
    let app = gpui_kit::application();

    app.run(move |cx| {
        gpui_kit::init(cx);

        let window_options = WindowOptions {
            window_bounds: Some(WindowBounds::centered(size(px(420.), px(640.)), cx)),
            titlebar: Some(TitlebarOptions {
                title: Some("SysVitals Settings".into()),
                ..Default::default()
            }),
            focus: true,
            ..Default::default()
        };

        gpui_kit::open_window(window_options, cx, move |window, cx| {
            cx.new(|cx| SettingsView::new(cfg, window, cx))
        })
        .expect("Failed to open settings window");
    });
}
