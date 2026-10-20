//! SysVitals History — a GPUI trend chart popup.
//!
//! Launched by the platform overlay when the user clicks the indicator.
//! Shows CPU and memory usage over time as line charts, with a time
//! range selector and a Copy CSV button. Closes on Esc or window blur.

// A GUI child process must not allocate a console window on Windows: without
// this attribute the binary is built as a console subsystem executable and a
// terminal flashes up beside the history window every time it launches.
#![cfg_attr(windows, windows_subsystem = "windows")]

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Local};
use gpui_kit::component::button::*;
use gpui_kit::component::chart::AreaChart;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use sysvitals_core::config::{config_path, history_path, Config, CsvScope};
use sysvitals_core::i18n::{t, Language};
use sysvitals_core::store::{Bucket, Store};

/// Time range options for the chart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimeRange {
    Min5,
    Hour1,
    Day1,
    Month1,
}

impl TimeRange {
    const ALL: [TimeRange; 4] = [
        TimeRange::Min5,
        TimeRange::Hour1,
        TimeRange::Day1,
        TimeRange::Month1,
    ];

    fn seconds(self) -> i64 {
        match self {
            TimeRange::Min5 => 300,
            TimeRange::Hour1 => 3600,
            TimeRange::Day1 => 86400,
            TimeRange::Month1 => 2_592_000,
        }
    }

    /// Bucket width in seconds for aggregation.
    fn bucket_secs(self) -> i64 {
        match self {
            TimeRange::Min5 => 2,
            TimeRange::Hour1 => 10,
            TimeRange::Day1 => 120,
            TimeRange::Month1 => 3600,
        }
    }

    fn label(self, lang: Language) -> &'static str {
        match self {
            TimeRange::Min5 => t(lang, "chart.range.5m"),
            TimeRange::Hour1 => t(lang, "chart.range.1h"),
            TimeRange::Day1 => t(lang, "chart.range.1d"),
            TimeRange::Month1 => t(lang, "chart.range.1mo"),
        }
    }
}

/// A single timestamped sample pair for the merged chart: one bucket renders
/// as one x position carrying both series values, which is why CPU and memory
/// live in one struct instead of two point lists.
#[derive(Debug, Clone)]
struct ChartPoint {
    label: String,
    cpu: f64,
    mem: f64,
}

/// How often a visible history window reloads the stored samples. The
/// indicator writes samples every sample interval, so redrawing every few
/// seconds makes the chart follow the indicator in near real time.
const REFRESH_INTERVAL: Duration = Duration::from_secs(3);

/// Chart stroke colors shared by the series, their fills, and the legend.
const CPU_HUE: f32 = 210.0;
const MEM_HUE: f32 = 150.0;

fn cpu_color() -> Hsla {
    hsla(CPU_HUE / 360.0, 0.8, 0.55, 1.0)
}

fn mem_color() -> Hsla {
    hsla(MEM_HUE / 360.0, 0.7, 0.45, 1.0)
}

/// A fill that is invisible: the merged chart draws two zero-alpha areas,
/// which is exactly two lines with no shading.
fn transparent_color() -> Hsla {
    hsla(0.0, 0.0, 0.0, 0.0)
}

struct HistoryView {
    lang: Language,
    range: TimeRange,
    buckets: Vec<Bucket>,
    csv_scope: CsvScope,
    copied: bool,
}

impl HistoryView {
    fn new(lang: Language, csv_scope: CsvScope, cx: &mut Context<Self>) -> Self {
        let mut view = Self {
            lang,
            range: TimeRange::Hour1,
            buckets: Vec::new(),
            csv_scope,
            copied: false,
        };
        view.load_data();
        view.start_refresh_loop(cx);
        view
    }

    /// Reload the stored samples every [`REFRESH_INTERVAL`] while the window
    /// lives. The loop stops as soon as the view entity is released, i.e.
    /// when the window closes.
    fn start_refresh_loop(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(REFRESH_INTERVAL).await;
            if this
                .update(cx, |view: &mut HistoryView, cx| {
                    view.load_data();
                    cx.notify();
                })
                .is_err()
            {
                break;
            }
        })
        .detach();
    }

    fn load_data(&mut self) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let from = now - self.range.seconds();

        if let Ok(path) = history_path() {
            if let Ok(store) = Store::open(&path) {
                self.buckets = store
                    .buckets(from, now, self.range.bucket_secs())
                    .unwrap_or_default();
            }
        }
    }

    fn points(&self) -> Vec<ChartPoint> {
        self.buckets
            .iter()
            .map(|b| ChartPoint {
                label: format_ts(b.ts),
                cpu: b.cpu,
                mem: b.mem,
            })
            .collect()
    }

    fn copy_csv(&mut self) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        let (from, bucket_secs) = match self.csv_scope {
            CsvScope::Visible => (now - self.range.seconds(), self.range.bucket_secs()),
            CsvScope::Last24h => (now - 86400, 120),
            CsvScope::Last7d => (now - 604800, 600),
            CsvScope::Last30d => (now - 2_592_000, 3600),
        };

        // Tab-separated rows paste straight into spreadsheet cells (WPS /
        // Excel split on tabs on paste), and unlike a space separator they
        // do not collide with the internal space of the "YYYY-MM-DD HH:MM:SS"
        // timestamp column. The UTF-8 BOM keeps CJK headers readable in Excel.
        let mut csv = String::from("\u{FEFF}");
        let header = match self.lang {
            Language::En => "timestamp\tcpu_pct\tmem_pct",
            Language::Zh => {
                "\u{65F6}\u{95F4}\tCPU\u{5360}\u{7528}\t\u{5185}\u{5B58}\u{5360}\u{7528}"
            }
        };
        csv.push_str(header);
        csv.push('\n');

        if let Ok(path) = history_path() {
            if let Ok(store) = Store::open(&path) {
                if let Ok(buckets) = store.buckets(from, now, bucket_secs) {
                    for b in &buckets {
                        let ts_str = format_ts_full(b.ts);
                        csv.push_str(&format!("{ts_str}\t{:.1}\t{:.1}\n", b.cpu, b.mem));
                    }
                }
            }
        }

        if let Ok(mut clipboard) = arboard::Clipboard::new() {
            let _ = clipboard.set_text(csv);
            self.copied = true;
        }
    }

    /// Clear the "copied" confirmation once the history window has enough
    /// time to show it. Spawning instead of storing the task lets a new copy
    /// arm a fresh reset while an older timer simply clears an already-cleared
    /// flag.
    fn schedule_copied_reset(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(1200))
                .await;
            let _ = this.update(cx, |view: &mut HistoryView, cx| {
                view.copied = false;
                cx.notify();
            });
        })
        .detach();
    }
}

/// Format a unix timestamp as HH:MM in the machine's local time zone.
fn format_ts(ts: i64) -> String {
    DateTime::from_timestamp(ts, 0)
        .map(|dt| dt.with_timezone(&Local).format("%H:%M").to_string())
        .unwrap_or_default()
}

/// Format a unix timestamp as YYYY-MM-DD HH:MM:SS in the machine's local
/// time zone, so exported rows line up with the user's wall clock.
fn format_ts_full(ts: i64) -> String {
    DateTime::from_timestamp(ts, 0)
        .map(|dt| {
            dt.with_timezone(&Local)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_default()
}

impl Render for HistoryView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let lang = self.lang;
        let points = self.points();
        let copied = self.copied;
        let current_range = self.range;

        // Build range buttons individually to avoid borrow issues with cx.listener.
        let mut range_buttons = h_flex().gap_1();
        for r in TimeRange::ALL {
            let is_active = r == current_range;
            let label = r.label(lang).to_string();
            let btn = Button::new(format!("range-{:?}", r))
                .small()
                .when(is_active, |b| b.primary())
                .label(label)
                .on_click(cx.listener(move |this: &mut Self, _: &ClickEvent, _, cx| {
                    this.range = r;
                    this.load_data();
                    cx.notify();
                }));
            range_buttons = range_buttons.child(btn);
        }

        // Icon-only copy button: a green check flashes for ~1.2s after a
        // successful copy, then the button returns to the copy icon. The
        // tooltip keeps the textual hint in the current language.
        let copy_tip = if copied {
            t(lang, "chart.copied").to_string()
        } else {
            t(lang, "chart.copy_csv").to_string()
        };

        let copy_btn = Button::new("copy-csv")
            .small()
            .tooltip(copy_tip)
            .when(copied, |b| {
                b.icon(IconName::CircleCheck)
                    .text_color(hsla(150.0 / 360.0, 0.7, 0.45, 1.0))
            })
            .when(!copied, |b| b.icon(IconName::Copy))
            .on_click(cx.listener(|this: &mut Self, _: &ClickEvent, _, cx| {
                this.copy_csv();
                this.schedule_copied_reset(cx);
                cx.notify();
            }));

        let header = h_flex()
            .justify_between()
            .items_center()
            .mb_2()
            .child(
                div()
                    .text_base()
                    .font_semibold()
                    .child(t(lang, "chart.title").to_string()),
            )
            .child(h_flex().gap_2().child(range_buttons).child(copy_btn));

        // One merged chart for both series. AreaChart accumulates one entry
        // per `y`/`stroke`/`fill`/`name` call in order, so two of each give
        // two lines; the zero-alpha fills keep the look of plain lines.
        // Percentages share one 0..=100 axis, which is what makes merging
        // sane. The x axis stays on (sparse tick labels read as a time
        // reference) while the hover tooltip carries the exact value with
        // two decimals and a percent sign.
        let merged_chart = div().flex_1().child(
            AreaChart::new(points)
                .x(|p: &ChartPoint| p.label.clone())
                .y(|p: &ChartPoint| p.cpu)
                .stroke(cpu_color())
                .fill(transparent_color())
                .name(t(lang, "chart.cpu").to_string())
                .y(|p: &ChartPoint| p.mem)
                .stroke(mem_color())
                .fill(transparent_color())
                .name(t(lang, "chart.mem").to_string())
                .y_domain(0.0, 100.0)
                .x_axis(true)
                .x_tick_count(4)
                .y_axis(true)
                .grid(true)
                .tooltip_value(|_p: &ChartPoint, _series: usize, v: f64| format!("{v:.2}%").into()),
        );

        // Shared legend: dot + label per series, aligned with the chart's
        // own colors.
        let legend = h_flex()
            .gap_4()
            .items_center()
            .child(
                h_flex()
                    .gap_1()
                    .items_center()
                    .child(div().w(px(8.)).h(px(8.)).rounded(px(4.)).bg(cpu_color()))
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().foreground.opacity(0.7))
                            .child(t(lang, "chart.cpu").to_string()),
                    ),
            )
            .child(
                h_flex()
                    .gap_1()
                    .items_center()
                    .child(div().w(px(8.)).h(px(8.)).rounded(px(4.)).bg(mem_color()))
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().foreground.opacity(0.7))
                            .child(t(lang, "chart.mem").to_string()),
                    ),
            );

        v_flex()
            .size_full()
            .p_4()
            .gap_3()
            .child(header)
            .child(legend)
            .child(merged_chart)
    }
}

fn main() {
    let cfg = config_path()
        .and_then(|p| Config::load(&p).map(|(c, _)| c))
        .unwrap_or_default();

    let lang = cfg.display.language;
    let csv_scope = cfg.history.csv_scope;

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
            window_bounds: Some(WindowBounds::centered(size(px(680.), px(520.)), cx)),
            titlebar: Some(TitlebarOptions {
                title: Some("SysVitals History".into()),
                ..Default::default()
            }),
            focus: true,
            ..Default::default()
        };

        gpui_kit::open_window(window_options, cx, move |_window, cx| {
            cx.new(|cx| HistoryView::new(lang, csv_scope, cx))
        })
        .expect("Failed to open history window");
    });
}
