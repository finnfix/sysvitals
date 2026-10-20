//! SysVitals Settings — a modern GPUI-based settings window.
//!
//! Launched by the platform overlay (app-win / app-mac) when the user opens
//! settings. Reads the shared config, presents a modern grouped UI, and writes
//! changes back atomically. The overlay watches the config file and reloads.

use gpui_kit::component::button::*;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::scroll::ScrollableElement as _;
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

    fn lang_label(&self) -> &'static str {
        Language::ALL[self.lang_idx].label()
    }

    fn interval_label(&self) -> String {
        format!("{} s", ALLOWED_INTERVAL_MS[self.interval_idx] / 1000)
    }

    fn spacing_label(&self) -> String {
        format!("{} px", ALLOWED_ITEM_SPACING_PX[self.spacing_idx])
    }

    fn margin_label(&self) -> String {
        format!("{} px", ALLOWED_LEFT_MARGIN_PX[self.margin_idx])
    }

    fn theme_label(&self) -> &'static str {
        match self.theme_idx {
            1 => "Light",
            2 => "Dark",
            _ => "Follow system",
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

/// A row with a label on the left and a cycle-button control on the right.
/// Generic over the click handler types so each call site can pass its own
/// `cx.listener()` closure without type unification conflicts.
fn cycle_row<F1, F2>(label: &str, value: String, on_prev: F1, on_next: F2) -> impl IntoElement
where
    F1: Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    F2: Fn(&ClickEvent, &mut Window, &mut App) + 'static,
{
    h_flex()
        .justify_between()
        .items_center()
        .py_1()
        .child(label.to_string())
        .child(
            h_flex()
                .gap_1()
                .items_center()
                .child(
                    Button::new(format!("prev-{label}"))
                        .small()
                        .child("<")
                        .on_click(on_prev),
                )
                .child(div().min_w(px(100.)).text_center().child(value))
                .child(
                    Button::new(format!("next-{label}"))
                        .small()
                        .child(">")
                        .on_click(on_next),
                ),
        )
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

        v_flex()
            .size_full()
            .p_4()
            .gap_2()
            .overflow_y_scrollbar()
            // --- General ---
            .child(section_label("General"))
            .child(cycle_row(
                "Language",
                self.lang_label().to_string(),
                cx.listener(
                    |this: &mut Self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>| {
                        this.lang_idx =
                            (this.lang_idx + Language::ALL.len() - 1) % Language::ALL.len();
                        cx.notify();
                    },
                ),
                cx.listener(
                    |this: &mut Self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>| {
                        this.lang_idx = (this.lang_idx + 1) % Language::ALL.len();
                        cx.notify();
                    },
                ),
            ))
            .child(cycle_row(
                "Sample interval",
                self.interval_label(),
                cx.listener(
                    |this: &mut Self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>| {
                        this.interval_idx = (this.interval_idx + ALLOWED_INTERVAL_MS.len() - 1)
                            % ALLOWED_INTERVAL_MS.len();
                        cx.notify();
                    },
                ),
                cx.listener(
                    |this: &mut Self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>| {
                        this.interval_idx = (this.interval_idx + 1) % ALLOWED_INTERVAL_MS.len();
                        cx.notify();
                    },
                ),
            ))
            .child(cycle_row(
                "Theme",
                self.theme_label().to_string(),
                cx.listener(
                    |this: &mut Self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>| {
                        this.theme_idx = (this.theme_idx + 2) % 3;
                        cx.notify();
                    },
                ),
                cx.listener(
                    |this: &mut Self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>| {
                        this.theme_idx = (this.theme_idx + 1) % 3;
                        cx.notify();
                    },
                ),
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
            .child(cycle_row(
                "Item spacing",
                self.spacing_label(),
                cx.listener(
                    |this: &mut Self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>| {
                        this.spacing_idx = (this.spacing_idx + ALLOWED_ITEM_SPACING_PX.len() - 1)
                            % ALLOWED_ITEM_SPACING_PX.len();
                        cx.notify();
                    },
                ),
                cx.listener(
                    |this: &mut Self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>| {
                        this.spacing_idx = (this.spacing_idx + 1) % ALLOWED_ITEM_SPACING_PX.len();
                        cx.notify();
                    },
                ),
            ))
            .child(div().h(px(1.)).bg(cx.theme().border).my_1())
            // --- Layout ---
            .child(section_label("Layout (Windows)"))
            .child(cycle_row(
                "Left margin",
                self.margin_label(),
                cx.listener(
                    |this: &mut Self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>| {
                        this.margin_idx = (this.margin_idx + ALLOWED_LEFT_MARGIN_PX.len() - 1)
                            % ALLOWED_LEFT_MARGIN_PX.len();
                        cx.notify();
                    },
                ),
                cx.listener(
                    |this: &mut Self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>| {
                        this.margin_idx = (this.margin_idx + 1) % ALLOWED_LEFT_MARGIN_PX.len();
                        cx.notify();
                    },
                ),
            ))
            .child(div().h(px(1.)).bg(cx.theme().border).my_1())
            // --- Prefixes ---
            .child(section_label("Prefixes"))
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .py_1()
                    .child("CPU")
                    .child(Input::new(&self.prefix_cpu).w(px(180.))),
            )
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .py_1()
                    .child("MEM")
                    .child(Input::new(&self.prefix_mem).w(px(180.))),
            )
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .py_1()
                    .child("Upload")
                    .child(Input::new(&self.prefix_up).w(px(180.))),
            )
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .py_1()
                    .child("Download")
                    .child(Input::new(&self.prefix_down).w(px(180.))),
            )
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
