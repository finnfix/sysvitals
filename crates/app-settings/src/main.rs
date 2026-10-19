//! SysVitals Settings — a modern GPUI-based settings window.
//!
//! Launched by the platform overlay (app-win / app-mac) when the user opens
//! settings. Reads the shared config, presents a modern grouped UI, and writes
//! changes back atomically. The overlay watches the config file and reloads.

use gpui_kit::component::button::*;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::select::*;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::*;
use gpui_kit::*;

use sysvitals_core::config::{
    ALLOWED_INTERVAL_MS, ALLOWED_ITEM_SPACING_PX, ALLOWED_LEFT_MARGIN_PX,
};
use sysvitals_core::{Config, Language, Theme};

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

struct SettingsView {
    cfg: Config,
    // Select states
    lang_select: Entity<SelectState<SearchableVec<usize>>>,
    interval_select: Entity<SelectState<SearchableVec<usize>>>,
    spacing_select: Entity<SelectState<SearchableVec<usize>>>,
    margin_select: Entity<SelectState<SearchableVec<usize>>>,
    theme_select: Entity<SelectState<SearchableVec<usize>>>,
    // Input states for prefixes
    prefix_cpu: Entity<InputState>,
    prefix_mem: Entity<InputState>,
    prefix_up: Entity<InputState>,
    prefix_down: Entity<InputState>,
    // Toggle states
    dual_row: bool,
    show_resources: bool,
    show_network: bool,
}

fn make_select(
    items: Vec<(usize, String)>,
    selected: usize,
    window: &mut Window,
    cx: &mut App,
) -> Entity<SelectState<SearchableVec<usize>>> {
    cx.new(|cx| {
        let mut delegate = SearchableVec::new();
        delegate.set_items(
            items
                .into_iter()
                .map(|(v, label)| SelectItem::new(v, label))
                .collect::<Vec<_>>(),
        );
        let mut state = SelectState::new(delegate, window, cx);
        state.select(Some(selected), window, cx);
        state
    })
}

impl SettingsView {
    fn new(cfg: Config, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let lang_items: Vec<(usize, String)> = Language::ALL
            .iter()
            .enumerate()
            .map(|(i, l)| (i, l.label().to_string()))
            .collect();
        let lang_sel = Language::ALL
            .iter()
            .position(|l| *l == cfg.display.language)
            .unwrap_or(0);
        let lang_select = make_select(lang_items, lang_sel, window, cx);

        let interval_items: Vec<(usize, String)> = ALLOWED_INTERVAL_MS
            .iter()
            .enumerate()
            .map(|(i, ms)| (i, format!("{} s", ms / 1000)))
            .collect();
        let interval_sel = ALLOWED_INTERVAL_MS
            .iter()
            .position(|v| *v == cfg.sample.interval_ms)
            .unwrap_or(0);
        let interval_select = make_select(interval_items, interval_sel, window, cx);

        let spacing_items: Vec<(usize, String)> = ALLOWED_ITEM_SPACING_PX
            .iter()
            .enumerate()
            .map(|(i, px)| (i, format!("{} px", px)))
            .collect();
        let spacing_sel = ALLOWED_ITEM_SPACING_PX
            .iter()
            .position(|v| *v == cfg.display.item_spacing)
            .unwrap_or(0);
        let spacing_select = make_select(spacing_items, spacing_sel, window, cx);

        let margin_items: Vec<(usize, String)> = ALLOWED_LEFT_MARGIN_PX
            .iter()
            .enumerate()
            .map(|(i, px)| (i, format!("{} px", px)))
            .collect();
        let margin_sel = ALLOWED_LEFT_MARGIN_PX
            .iter()
            .position(|v| *v == cfg.windows.left_margin_px)
            .unwrap_or(0);
        let margin_select = make_select(margin_items, margin_sel, window, cx);

        let theme_items: Vec<(usize, String)> = vec![
            (0, "Follow system".into()),
            (1, "Light".into()),
            (2, "Dark".into()),
        ];
        let theme_sel = match cfg.display.theme {
            Theme::Auto => 0,
            Theme::Light => 1,
            Theme::Dark => 2,
        };
        let theme_select = make_select(theme_items, theme_sel, window, cx);

        let prefix_cpu =
            cx.new(|cx| InputState::new(window, cx).default_value(&cfg.display.prefix.cpu));
        let prefix_mem =
            cx.new(|cx| InputState::new(window, cx).default_value(&cfg.display.prefix.mem));
        let prefix_up =
            cx.new(|cx| InputState::new(window, cx).default_value(&cfg.display.prefix.up));
        let prefix_down =
            cx.new(|cx| InputState::new(window, cx).default_value(&cfg.display.prefix.down));

        Self {
            dual_row: cfg.display.dual_row,
            show_resources: cfg.display.show_resources,
            show_network: cfg.display.show_network,
            cfg,
            lang_select,
            interval_select,
            spacing_select,
            margin_select,
            theme_select,
            prefix_cpu,
            prefix_mem,
            prefix_up,
            prefix_down,
        }
    }

    fn collect_config(&self, cx: &App) -> Config {
        let mut cfg = self.cfg.clone();

        if let Some(idx) = self.lang_select.read(cx).selected_value() {
            if let Some(lang) = Language::ALL.get(idx) {
                cfg.display.language = *lang;
            }
        }
        if let Some(idx) = self.interval_select.read(cx).selected_value() {
            if let Some(ms) = ALLOWED_INTERVAL_MS.get(idx) {
                cfg.sample.interval_ms = *ms;
            }
        }
        if let Some(idx) = self.spacing_select.read(cx).selected_value() {
            if let Some(px) = ALLOWED_ITEM_SPACING_PX.get(idx) {
                cfg.display.item_spacing = *px;
            }
        }
        if let Some(idx) = self.margin_select.read(cx).selected_value() {
            if let Some(px) = ALLOWED_LEFT_MARGIN_PX.get(idx) {
                cfg.windows.left_margin_px = *px;
            }
        }
        if let Some(idx) = self.theme_select.read(cx).selected_value() {
            cfg.display.theme = match idx {
                1 => Theme::Light,
                2 => Theme::Dark,
                _ => Theme::Auto,
            };
        }
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
}

impl Render for SettingsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let section_label = |text: &str| {
            div()
                .text_sm()
                .font_semibold()
                .text_color(cx.theme().foreground.opacity(0.5))
                .child(text.to_string())
        };

        let row = |label: &str, control: AnyElement| {
            h_flex()
                .justify_between()
                .items_center()
                .py_1()
                .child(label.to_string())
                .child(control)
                .into_any_element()
        };

        v_flex()
            .size_full()
            .p_4()
            .gap_2()
            .overflow_y_scroll()
            // --- General ---
            .child(section_label("General"))
            .child(row(
                "Language",
                Select::new(&self.lang_select)
                    .w(px(180.))
                    .into_any_element(),
            ))
            .child(row(
                "Sample interval",
                Select::new(&self.interval_select)
                    .w(px(180.))
                    .into_any_element(),
            ))
            .child(row(
                "Theme",
                Select::new(&self.theme_select)
                    .w(px(180.))
                    .into_any_element(),
            ))
            .child(div().h(px(1.)).bg(cx.theme().border).my_1())
            // --- Display ---
            .child(section_label("Display"))
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .py_1()
                    .child("Dual-row layout (Windows)")
                    .child(
                        Switch::new("dual-row")
                            .checked(self.dual_row)
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                this.dual_row = *checked;
                                cx.notify();
                            })),
                    ),
            )
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .py_1()
                    .child("CPU / MEM")
                    .child(
                        Switch::new("show-res")
                            .checked(self.show_resources)
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                this.show_resources = *checked;
                                cx.notify();
                            })),
                    ),
            )
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .py_1()
                    .child("Upload / Download")
                    .child(Switch::new("show-net").checked(self.show_network).on_click(
                        cx.listener(|this, checked: &bool, _, cx| {
                            this.show_network = *checked;
                            cx.notify();
                        }),
                    )),
            )
            .child(row(
                "Item spacing",
                Select::new(&self.spacing_select)
                    .w(px(180.))
                    .into_any_element(),
            ))
            .child(div().h(px(1.)).bg(cx.theme().border).my_1())
            // --- Layout ---
            .child(section_label("Layout (Windows)"))
            .child(row(
                "Left margin",
                Select::new(&self.margin_select)
                    .w(px(180.))
                    .into_any_element(),
            ))
            .child(div().h(px(1.)).bg(cx.theme().border).my_1())
            // --- Prefixes ---
            .child(section_label("Prefixes"))
            .child(row(
                "CPU",
                Input::new(&self.prefix_cpu).w(px(180.)).into_any_element(),
            ))
            .child(row(
                "MEM",
                Input::new(&self.prefix_mem).w(px(180.)).into_any_element(),
            ))
            .child(row(
                "Upload",
                Input::new(&self.prefix_up).w(px(180.)).into_any_element(),
            ))
            .child(row(
                "Download",
                Input::new(&self.prefix_down).w(px(180.)).into_any_element(),
            ))
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().foreground.opacity(0.4))
                    .child("Independent of language; up to 8 characters."),
            )
            // --- Buttons ---
            .child(div().flex_1())
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .pt_2()
                    .child(Button::new("apply").label("Apply").on_click(cx.listener(
                        |this, _, _, cx| {
                            this.save(cx);
                            cx.notify();
                        },
                    )))
                    .child(
                        Button::new("ok")
                            .primary()
                            .label("OK")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.save(cx);
                                window.remove_window();
                            })),
                    )
                    .child(Button::new("cancel").label("Cancel").on_click(cx.listener(
                        |_, _, window, _| {
                            window.remove_window();
                        },
                    ))),
            )
    }
}

fn main() {
    let cfg = sysvitals_core::config::config_path()
        .and_then(|p| Config::load(&p).map(|(c, _)| c))
        .unwrap_or_default();

    gpui_kit::application().run(move |cx| {
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
