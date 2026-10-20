//! SysVitals History — a GPUI trend chart popup.
//!
//! Launched by the platform overlay when the user clicks the indicator.
//! Shows CPU and memory usage over time as line charts, with a time
//! range selector and a Copy CSV button. Closes on Esc or window blur.

use std::time::{SystemTime, UNIX_EPOCH};

use gpui_kit::component::button::*;
use gpui_kit::component::chart::LineChart;
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

/// A single data point for the chart.
#[derive(Debug, Clone)]
struct ChartPoint {
    label: String,
    value: f64,
}

struct HistoryView {
    lang: Language,
    range: TimeRange,
    buckets: Vec<Bucket>,
    csv_scope: CsvScope,
    copied: bool,
}

impl HistoryView {
    fn new(lang: Language, csv_scope: CsvScope) -> Self {
        let mut view = Self {
            lang,
            range: TimeRange::Hour1,
            buckets: Vec::new(),
            csv_scope,
            copied: false,
        };
        view.load_data();
        view
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

    fn cpu_points(&self) -> Vec<ChartPoint> {
        self.buckets
            .iter()
            .map(|b| ChartPoint {
                label: format_ts(b.ts),
                value: b.cpu,
            })
            .collect()
    }

    fn mem_points(&self) -> Vec<ChartPoint> {
        self.buckets
            .iter()
            .map(|b| ChartPoint {
                label: format_ts(b.ts),
                value: b.mem,
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

        let mut csv = String::from("\u{FEFF}"); // UTF-8 BOM
        let header = match self.lang {
            Language::En => "timestamp,cpu_pct,mem_pct",
            Language::Zh => "\u{65F6}\u{95F4},CPU\u{5360}\u{7528},\u{5185}\u{5B58}\u{5360}\u{7528}",
        };
        csv.push_str(header);
        csv.push('\n');

        if let Ok(path) = history_path() {
            if let Ok(store) = Store::open(&path) {
                if let Ok(buckets) = store.buckets(from, now, bucket_secs) {
                    for b in &buckets {
                        let ts_str = format_ts_full(b.ts);
                        csv.push_str(&format!("{},{:.1},{:.1}\n", ts_str, b.cpu, b.mem));
                    }
                }
            }
        }

        if let Ok(mut clipboard) = arboard::Clipboard::new() {
            let _ = clipboard.set_text(csv);
            self.copied = true;
        }
    }
}

/// Format a unix timestamp as HH:MM for chart labels.
fn format_ts(ts: i64) -> String {
    let secs = ts % 86400;
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    format!("{:02}:{:02}", h, m)
}

/// Format a unix timestamp as YYYY-MM-DD HH:MM:SS for CSV.
fn format_ts_full(ts: i64) -> String {
    // Simple UTC-based formatting (local time offset not applied for simplicity).
    let days = ts / 86400;
    let secs = ts % 86400;
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;

    // Days since epoch to Y-M-D (civil calendar algorithm).
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };

    format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", y, mo, d, h, m, s)
}

impl Render for HistoryView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let lang = self.lang;
        let cpu_points = self.cpu_points();
        let mem_points = self.mem_points();
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

        let copy_label = if copied {
            t(lang, "chart.copied").to_string()
        } else {
            t(lang, "chart.copy_csv").to_string()
        };

        let copy_btn = Button::new("copy-csv")
            .small()
            .label(copy_label)
            .on_click(cx.listener(|this: &mut Self, _: &ClickEvent, _, cx| {
                this.copy_csv();
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

        // CPU chart (wrapped in a fixed-height container since LineChart fills parent)
        let cpu_chart = div().h(px(200.)).child(
            LineChart::new(cpu_points)
                .x(|p: &ChartPoint| p.label.clone())
                .y(|p: &ChartPoint| p.value)
                .y_domain(0.0, 100.0)
                .x_axis(true)
                .y_axis(true)
                .grid(true)
                .stroke(hsla(210.0 / 360.0, 0.8, 0.55, 1.0))
                .name(t(lang, "chart.cpu").to_string()),
        );

        // MEM chart
        let mem_chart = div().h(px(200.)).child(
            LineChart::new(mem_points)
                .x(|p: &ChartPoint| p.label.clone())
                .y(|p: &ChartPoint| p.value)
                .y_domain(0.0, 100.0)
                .x_axis(true)
                .y_axis(true)
                .grid(true)
                .stroke(hsla(150.0 / 360.0, 0.7, 0.45, 1.0))
                .name(t(lang, "chart.mem").to_string()),
        );

        // Legend row: colored dot + label for each series.
        let cpu_legend = h_flex()
            .gap_1()
            .items_center()
            .child(div().w(px(8.)).h(px(8.)).rounded(px(4.)).bg(hsla(
                210.0 / 360.0,
                0.8,
                0.55,
                1.0,
            )))
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().foreground.opacity(0.7))
                    .child(t(lang, "chart.cpu").to_string()),
            );

        let mem_legend = h_flex()
            .gap_1()
            .items_center()
            .child(div().w(px(8.)).h(px(8.)).rounded(px(4.)).bg(hsla(
                150.0 / 360.0,
                0.7,
                0.45,
                1.0,
            )))
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().foreground.opacity(0.7))
                    .child(t(lang, "chart.mem").to_string()),
            );

        v_flex()
            .size_full()
            .p_4()
            .gap_3()
            .child(header)
            .child(cpu_legend)
            .child(cpu_chart)
            .child(mem_legend)
            .child(mem_chart)
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
            cx.new(|_cx| HistoryView::new(lang, csv_scope))
        })
        .expect("Failed to open history window");
    });
}
