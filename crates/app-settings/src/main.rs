//! SysVitals Settings — a modern GPUI-based settings window.
//!
//! Launched by the platform overlay (app-win / app-mac) when the user opens
//! settings. Reads the shared config, presents a modern grouped UI, and writes
//! changes back atomically. The overlay watches the config file and reloads.

// A GUI child process must not allocate a console window on Windows: without
// this attribute the binary is built as a console subsystem executable and a
// terminal flashes up beside the settings window every time it launches.
#![cfg_attr(windows, windows_subsystem = "windows")]

use gpui_kit::component::button::*;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::*;
use gpui_kit::*;

use sysvitals_core::config::{
    ALLOWED_INTERVAL_MS, ALLOWED_ITEM_SPACING_PX, ALLOWED_LEFT_MARGIN_PX,
};
use sysvitals_core::{Config, Language, Theme, t};

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
    // Index-based selections (cycled with buttons)
    lang_idx: usize,
    interval_idx: usize,
    spacing_idx: usize,
    margin_idx: usize,
    theme_idx: usize,
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
        let theme_idx = match cfg.display.theme {
            Theme::Auto => 0,
            Theme::Light => 1,
            Theme::Dark => 2,
        };

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
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let section_label = |text: &str| {
            div()
                .text_sm()
                .font_semibold()
                .text_color(cx.theme().foreground.opacity(0.5))
                .child(text.to_string())
        };
        let divider = div().h(px(1.)).bg(cx.theme().border).my_1();

        // Every label comes from the core i18n table, keyed on the
        // *currently selected* language, so switching language repaints the
        // whole window immediately instead of only after saving.
        let lang = self.current_lang();

        // --- Flattened (segmented) option rows: every choice is a visible
        // tile, the active one carries the primary style. Clicking a tile
        // selects it directly; no previous/next cycling.
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
                              _: &mut Window,
                              cx: &mut Context<Self>| {
                            this.lang_idx = i;
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
                            cx.notify();
                        },
                    )),
            );
        }

        let mut spacing_row = h_flex().gap_1().flex_wrap();
        for i in 0..ALLOWED_ITEM_SPACING_PX.len() {
            let active = i == self.spacing_idx;
            let text = format!(
                "{} {}",
                ALLOWED_ITEM_SPACING_PX[i],
                t(lang, "settings.unit_pixels")
            );
            spacing_row = spacing_row.child(
                Button::new(format!("spacing-{i}"))
                    .small()
                    .when(active, |b| b.primary())
                    .label(text)
                    .on_click(cx.listener(
                        move |this: &mut Self,
                              _: &ClickEvent,
                              _: &mut Window,
                              cx: &mut Context<Self>| {
                            this.spacing_idx = i;
                            cx.notify();
                        },
                    )),
            );
        }

        let mut margin_row = h_flex().gap_1().flex_wrap();
        for i in 0..ALLOWED_LEFT_MARGIN_PX.len() {
            let active = i == self.margin_idx;
            let text = format!(
                "{} {}",
                ALLOWED_LEFT_MARGIN_PX[i],
                t(lang, "settings.unit_pixels")
            );
            margin_row = margin_row.child(
                Button::new(format!("margin-{i}"))
                    .small()
                    .when(active, |b| b.primary())
                    .label(text)
                    .on_click(cx.listener(
                        move |this: &mut Self,
                              _: &ClickEvent,
                              _: &mut Window,
                              cx: &mut Context<Self>| {
                            this.margin_idx = i;
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
            .child(section_label(t(lang, "settings.general")))
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
            .child(divider)
            .child(section_label(t(lang, "settings.display")))
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
                            cx.notify();
                        }),
                    )),
            )
            .child(labeled_row(
                t(lang, "settings.item_spacing").to_string(),
                spacing_row,
            ))
            // --- Layout (Windows) ---
            .child(div().h(px(1.)).bg(cx.theme().border).my_1())
            .child(section_label(t(lang, "settings.layout")))
            .child(labeled_row(
                t(lang, "settings.left_margin").to_string(),
                margin_row,
            ))
            // --- Prefixes ---
            .child(div().h(px(1.)).bg(cx.theme().border).my_1())
            .child(section_label(t(lang, "settings.prefixes")))
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
                    .text_color(cx.theme().foreground.opacity(0.4))
                    .child(t(lang, "settings.prefix_hint").to_string()),
            )
            // --- Buttons ---
            .child(div().flex_1())
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .pt_2()
                    .child(
                        Button::new("apply")
                            .label(t(lang, "settings.apply").to_string())
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.save(cx);
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("ok")
                            .primary()
                            .label(t(lang, "settings.ok").to_string())
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.save(cx);
                                window.remove_window();
                            })),
                    )
                    .child(
                        Button::new("cancel")
                            .label(t(lang, "settings.cancel").to_string())
                            .on_click(cx.listener(|_, _, window, _| {
                                window.remove_window();
                            })),
                    ),
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
