//! SysVitals History — a GPUI trend chart popup.
//!
//! Launched by the platform overlay when the user clicks the indicator.
//! Shows CPU and memory usage over time as one merged chart, with a time
//! range selector and a Copy CSV button. When the launcher passes the
//! indicator rectangle via `--anchor`, the window opens as a topmost
//! borderless popup beside the indicator; otherwise it opens centered.
//! Closes on Esc or window blur.

// A GUI child process must not allocate a console window on Windows: without
// this attribute the binary is built as a console subsystem executable and a
// terminal flashes up beside the history window every time it launches.
#![cfg_attr(windows, windows_subsystem = "windows")]

use std::env;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Local};
use gpui_kit::component::button::*;
use gpui_kit::component::chart::AreaChart;
use gpui_kit::component::theme::Theme as UiTheme;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use sysvitals_core::config::{config_path, history_path, Config, CsvScope};
use sysvitals_core::i18n::{t, Language};
use sysvitals_core::store::{Bucket, Store};

/// Screen rectangle of the indicator, passed by the overlay in GPUI's
/// global logical pixel space (top-left origin, y growing downward).
/// Both platform overlays convert their native coordinates to that space
/// before launching.
struct AnchorRect {
    left: f32,
    top: f32,
    right: f32,
    bottom: f32,
}

/// Parse `--anchor L T R B` from the process arguments. Absent, malformed,
/// or degenerate rectangles yield `None`, which keeps the old centered
/// behavior — a bad argument must never prevent the window from opening.
fn parse_anchor() -> Option<AnchorRect> {
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg != "--anchor" {
            continue;
        }
        let mut nums = [0f32; 4];
        for slot in nums.iter_mut() {
            *slot = args.next()?.trim().parse().ok()?;
        }
        let [left, top, right, bottom] = nums;
        return (right > left && bottom > top).then_some(AnchorRect {
            left,
            top,
            right,
            bottom,
        });
    }
    None
}

/// The usable area of the display an anchor sits on (taskbar/dock excluded),
/// used to flip and clamp the popup inside a screen the indicator is on.
fn anchor_work_area(anchor: &AnchorRect, cx: &App) -> Bounds<Pixels> {
    let probe = point(
        px((anchor.left + anchor.right) * 0.5),
        px((anchor.top + anchor.bottom) * 0.5),
    );
    cx.displays()
        .into_iter()
        .find(|display| display.visible_bounds().contains(&probe))
        .or_else(|| cx.primary_display())
        .map(|display| display.visible_bounds())
        .unwrap_or_else(|| Bounds::new(point(px(0.), px(0.)), size(px(1280.), px(720.))))
}

/// Origin for the popup, anchored to the indicator rect: horizontally
/// centered on it, vertically preferred below and flipped above when it
/// would not fit, finally clamped into the work area.
fn anchor_origin(
    anchor: &AnchorRect,
    size_w: f32,
    size_h: f32,
    work: &Bounds<Pixels>,
) -> Point<Pixels> {
    let gap = 6.0f32;
    let work_left = f32::from(work.origin.x);
    let work_right = work_left + f32::from(work.size.width);
    let work_top = f32::from(work.origin.y);
    let work_bottom = work_top + f32::from(work.size.height);

    let below = anchor.bottom + gap;
    let above = anchor.top - gap - size_h;
    let y = if below + size_h <= work_bottom {
        below
    } else if above >= work_top {
        above
    } else {
        (work_bottom - size_h).max(work_top)
    };

    let mut x = (anchor.left + anchor.right) * 0.5 - size_w * 0.5;
    if x + size_w > work_right {
        x = work_right - size_w;
    }
    if x < work_left {
        x = work_left;
    }
    point(px(x), px(y))
}

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

    /// Bucket width in seconds for CSV export: fine enough to preserve
    /// resolution at each range while staying bounded (a month is ~720
    /// rows, not 1.3M). The chart uses a separate, coarser bucket grid.
    fn bucket_secs(self) -> i64 {
        match self {
            TimeRange::Min5 => 2,
            TimeRange::Hour1 => 10,
            TimeRange::Day1 => 120,
            TimeRange::Month1 => 3600,
        }
    }

    /// Chart bucket width, chosen so every bucket start lands on a clean
    /// wall-clock unit and the six labeled ticks all read as round times
    /// (the range picks the step, hence "auto per range"):
    ///   5 min    = 60 s       (every local minute)
    ///   1 hour   = 600 s      (every :10 / :20 / ... local ten-minute mark)
    ///   1 day    = 7 200 s    (every 2 h at 00/02/.../22 in a whole-hour zone)
    ///   1 month  = 432 000 s  (every 5 days at local midnight, offset-aligned)
    /// `x_tick_count(6)` then picks 6 of those evenly spread; because every
    /// data point sits on a clean boundary, the visible labels always read
    /// as round times. Boundaries are offset-aligned for the month range
    /// (see [`Store::buckets_offset`]) so "5 days" starts at local midnight
    /// rather than the epoch's UTC-midnight grid.
    fn chart_bucket_secs(self) -> i64 {
        match self {
            TimeRange::Min5 => 60,
            TimeRange::Hour1 => 600,
            TimeRange::Day1 => 7_200,
            TimeRange::Month1 => 432_000,
        }
    }

    /// Whether this range's chart buckets must be aligned to local midnight
    /// rather than UTC midnight. Only the month view needs it (5-day
    /// buckets); the shorter ranges' bucket widths (60/600/7200 s) all
    /// divide evenly into every real UTC offset, so their epoch-aligned
    /// buckets already land on clean local times.
    fn chart_needs_offset(self) -> bool {
        matches!(self, TimeRange::Month1)
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
/// live in one struct instead of two point lists. `label` is the sparse x-axis
/// tick text (range-aware: clock time, date, or both); `ts` is kept so the
/// hover tooltip can show the full date and time regardless of how terse the
/// axis label is.
#[derive(Debug, Clone)]
struct ChartPoint {
    ts: i64,
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
    /// Guards blur-close: only close after the window has been active at
    /// least once, so a transient "inactive" signal at open time (a macOS
    /// nonactivating panel that has not become key yet) cannot close the
    /// popup the instant it appears.
    ever_active: bool,
    /// Kept alive so the activation observer is not cancelled; dropped when
    /// the view is released.
    _activation: Option<Subscription>,
    /// Kept alive so the appearance observer is not cancelled. The panel
    /// always mirrors the OS light/dark theme (independent of the user's
    /// `display.theme` setting, which now only governs the indicator), so
    /// a live OS flip repaints this window too — see `observe_appearance`.
    _appearance: Option<Subscription>,
    /// Kept alive so the Esc observer is not cancelled. Esc closes the popup.
    /// This is an app-level keystroke observer, not a window paint-phase
    /// listener: registering from `render` with `Window::on_key_event` would
    /// touch the dispatch tree's active node, which does not exist yet for
    /// the root view on the first paint frame — GPUI unwraps it and panics,
    /// killing the process the moment the popup opens. This process owns a
    /// single window with no text fields, so a global keystroke observer is
    /// both safe and simpler.
    _escape: Option<Subscription>,
}

impl HistoryView {
    fn new(lang: Language, csv_scope: CsvScope, cx: &mut Context<Self>) -> Self {
        let mut view = Self {
            lang,
            range: TimeRange::Hour1,
            buckets: Vec::new(),
            csv_scope,
            copied: false,
            ever_active: false,
            _activation: None,
            _appearance: None,
            _escape: None,
        };
        view.load_data();
        view.start_refresh_loop(cx);
        view.start_escape_observer(cx);
        view
    }

    /// Dismiss the popup when Esc is pressed.
    fn start_escape_observer(&mut self, cx: &mut Context<Self>) {
        if self._escape.is_some() {
            return;
        }
        let subscription = cx.observe_keystrokes(|_view, event, window, _cx| {
            if event.keystroke.key == "escape" {
                window.remove_window();
            }
        });
        self._escape = Some(subscription);
    }

    /// Watch the window's OS-appearance changes and re-apply the user's
    /// preference each time. Idempotent, and registered from the first
    /// render pass like `observe_activation` — never from a paint-phase
    /// callback, which would hit the empty active-node stack on the root
    /// view's first frame (see the crash note on `_escape`).
    fn observe_appearance(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self._appearance.is_some() {
            return;
        }
        let subscription = cx.observe_window_appearance(window, |_view: &mut Self, _w, cx| {
            // The popup always mirrors the OS light/dark appearance; a
            // live system flip repaints it with the same palette that the
            // rest of the desktop is now using. Accent override must be
            // re-applied because a palette reload replaces every field.
            apply_theme_and_accent(cx);
            cx.notify();
        });
        self._appearance = Some(subscription);
    }

    /// Watch the window's activation and dismiss the popup when it loses
    /// focus after having been focused once. Idempotent.
    fn observe_activation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self._activation.is_some() {
            return;
        }
        // The observer only sees activation *changes*: a window created with
        // `focus: true` is already active when the first render registers it,
        // and that edge is not replayed. Seed the guard from the current
        // state, otherwise the first click-away would not close the popup.
        if window.is_window_active() {
            self.ever_active = true;
        }
        let subscription = cx.observe_window_activation(window, |view, window, _| {
            if window.is_window_active() {
                view.ever_active = true;
            } else if view.ever_active {
                // Clicked away: dismiss the popup like a native menu.
                window.remove_window();
            }
        });
        self._activation = Some(subscription);
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
                // The chart buckets on the clean wall-clock grid (see
                // `chart_bucket_secs`); the month range shifts the grid to
                // local midnight with the machine's current UTC offset so
                // five-day buckets start at "00:00", not "08:00" (+8 zones).
                let offset = if self.range.chart_needs_offset() {
                    Local::now().offset().local_minus_utc() as i64
                } else {
                    0
                };
                self.buckets = store
                    .buckets_offset(from, now, self.range.chart_bucket_secs(), offset)
                    .unwrap_or_default();
            }
        }
    }

    fn points(&self) -> Vec<ChartPoint> {
        // One point per bucket, each carrying a tick label. The chart shows
        // exactly 6 of them (`x_tick_count(6)`) spread first→last, and
        // because every bucket starts on a clean wall-clock boundary, all
        // six labels read as round times.
        //
        // Day-crossing: a bucket whose local calendar day differs from the
        // previous bucket's carries "MM-DD HH:MM" so the midnight crossing
        // is visible on the axis; the month view labels every point with
        // just "MM-DD" (a whole month of HH:MM ticks would be meaningless).
        let month_view = self.range == TimeRange::Month1;
        let mut prev_day: Option<String> = None;
        self.buckets
            .iter()
            .map(|b| {
                let label = if month_view {
                    fmt_date(b.ts)
                } else {
                    let day = day_of(b.ts);
                    // Date the tick that anchors the window start (the
                    // first bucket) and every bucket that opens a new
                    // local calendar day. So a 5-minute view reads all
                    // HH:MM except its opening tick (which shows "10-22
                    // 14:00" to date the start), and a 1-day view marks
                    // the midnight crossing exactly once.
                    let crosses = prev_day.as_ref().map_or(true, |p| *p != day);
                    prev_day = Some(day);
                    if crosses {
                        fmt_date_time(b.ts)
                    } else {
                        fmt_time(b.ts)
                    }
                };
                ChartPoint {
                    ts: b.ts,
                    label,
                    cpu: b.cpu,
                    mem: b.mem,
                }
            })
            .collect()
    }

    fn copy_csv(&mut self) {
        let lang = self.lang;
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
        // Header labels come from the core i18n table (single source of truth
        // for the CSV columns), not a local copy, so a header edit lives in
        // exactly one place.
        let header = format!(
            "{}\t{}\t{}",
            t(lang, "csv.header.timestamp"),
            t(lang, "csv.header.cpu"),
            t(lang, "csv.header.mem")
        );
        csv.push_str(&header);
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
fn fmt_time(ts: i64) -> String {
    DateTime::from_timestamp(ts, 0)
        .map(|dt| dt.with_timezone(&Local).format("%H:%M").to_string())
        .unwrap_or_default()
}

/// Format a unix timestamp as MM-DD, the terse date label for the month
/// view's x axis (year is redundant inside a 30-day window).
fn fmt_date(ts: i64) -> String {
    DateTime::from_timestamp(ts, 0)
        .map(|dt| dt.with_timezone(&Local).format("%m-%d").to_string())
        .unwrap_or_default()
}

/// Format a unix timestamp as `MM-DD HH:MM`, used when a shorter view's x
/// axis crosses midnight and the tick that opens a new day must show both.
fn fmt_date_time(ts: i64) -> String {
    DateTime::from_timestamp(ts, 0)
        .map(|dt| dt.with_timezone(&Local).format("%m-%d %H:%M").to_string())
        .unwrap_or_default()
}

/// Full local date and time (with seconds) for the hover tooltip: the axis
/// labels are deliberately terse (HH:MM or MM-DD HH:MM) and the popup's
/// point of existence is to give exact per-point reading, so the tooltip
/// carries the wall clock down to the second — matching the CSV export's
/// timestamp format.
fn fmt_full(ts: i64) -> String {
    DateTime::from_timestamp(ts, 0)
        .map(|dt| {
            dt.with_timezone(&Local)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_default()
}

/// Local calendar date as a stable comparison key (`YYYY-MM-DD`). Two
/// timestamps match only if their wall-clock date in the user's zone
/// matches, which is the exact "day crossed" test the x-axis labels need.
/// A string key is used rather than an integer day count because it relies
/// on only the `%Y-%m-%d` format directive, avoiding a version-dependent
/// helper on `NaiveDate`.
fn day_of(ts: i64) -> String {
    DateTime::from_timestamp(ts, 0)
        .map(|dt| dt.with_timezone(&Local).format("%Y-%m-%d").to_string())
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let lang = self.lang;
        let points = self.points();
        let copied = self.copied;
        let current_range = self.range;

        // Register the blur-close observer and the live appearance observer
        // exactly once. Both are registered here (not from `new`) because
        // neither is available before the window exists; and neither uses a
        // paint-phase API, so the empty-dispatch-tree root-view trap that
        // killed the original Esc handler does not apply.
        self.observe_activation(window, cx);
        self.observe_appearance(window, cx);

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
        // sane.
        //
        // `appear(false)`: the library's default 1s left-to-right draw-in
        // is what the user experiences as "slow to show" — the popup's
        // job is to be readable the instant it opens, so the sweep is off.
        //
        // `x_tick_count(6)` + `y_tick_count(6)`: six ticks each. The y
        // domain is 0..=100 and the library's `format_tick` prints integral
        // values with no decimals, so the y axis renders "0 / 20 / 40 / 60
        // / 80 / 100" as the user asked. The x ticks fall on the six evenly
        // spread data buckets (see `points`), all of which start on a clean
        // wall-clock unit, so every x label reads as a round time.
        //
        // `tooltip_title` overrides the axis label with the full
        // `YYYY-MM-DD HH:MM:SS` (with seconds), because the axis labels are
        // deliberately terse and a hover should give exact timing.
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
                .x_tick_count(6)
                .y_axis(true)
                .y_tick_count(6)
                .grid(true)
                .appear(false)
                .tooltip_title(|p: &ChartPoint| fmt_full(p.ts).into())
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

/// Push the OS accent color into the GPUI theme so the primary button style
/// (the active range tile, focus rings) matches the Windows accent picker.
///
/// The chart line colors are deliberately not overridden: they encode which
/// series is which and stay stable so the two lines remain distinguishable
/// after a user changes their accent. `Theme::change` (and the auto-sync
/// path) reloads the whole palette, so the override must be re-applied
/// after every theme change — see `apply_theme_and_accent`.
#[cfg(windows)]
fn apply_accent(cx: &mut App) {
    use windows::Win32::Graphics::Dwm::DwmGetColorizationColor;
    let mut argb = 0u32;
    // `Default::default()` avoids naming the `BOOL` type: in windows-core
    // 0.62 that is a plain `pub type BOOL = i32` alias, so writing
    // `BOOL(0)` would compile as a tuple-struct constructor (E0423). The
    // opaque-blend flag is not read — the accent color is meaningful either
    // way. See the same fix in app-settings.
    if unsafe { DwmGetColorizationColor(&mut argb, &mut Default::default()) }.is_err() {
        return;
    }
    let base = rgb_hex_to_hsla(argb & 0x00FF_FFFF);
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
        theme.colors.ring = base;
    });
}

#[cfg(not(windows))]
fn apply_accent(_cx: &mut App) {}

/// Convert a packed 24-bit RGB hex to GPUI's `Hsla` (h/s/l/a in 0..1).
/// Hand-rolled so the accent path does not depend on which color helpers the
/// current gpui-kit prelude happens to re-export; only `hsla` and `Hsla` are
/// required, both already used by the chart series colors.
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

/// Sync the popup's palette to the OS light/dark appearance. The user's
/// `display.theme` setting is deliberately not consulted here: the trend
/// chart panel always follows the system, and only the resident indicator
/// honors the "light/dark override" side of that config. `Theme::change`
/// (which `sync_system_appearance` calls under the hood) reloads the whole
/// palette, so this must be followed by [`apply_accent`] whenever the
/// Windows primary-color override is wanted — see [`apply_theme_and_accent`].
fn apply_system_theme(cx: &mut App) {
    UiTheme::sync_system_appearance(None, cx);
}

/// Theme sync and accent override applied together: a palette reload wipes
/// the accent override, so the accent must follow every theme change, never
/// precede it.
fn apply_theme_and_accent(cx: &mut App) {
    apply_system_theme(cx);
    apply_accent(cx);
}

fn main() {
    let cfg = config_path()
        .and_then(|p| Config::load(&p).map(|(c, _)| c))
        .unwrap_or_default();

    let lang = cfg.display.language;
    let csv_scope = cfg.history.csv_scope;

    // Register the bundled icon asset source before the run loop starts.
    // Without this the GPUI default asset source (`()`) returns `Ok(None)`
    // for every path, so every `IconName` renders as blank — this is exactly
    // what made the Copy / CircleCheck glyphs disappear from the copy button.
    //
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
        .with_assets(gpui_kit::assets::Assets)
        .with_windowing(WindowingRequest::Headless)
        .with_quit_mode(QuitMode::LastWindowClosed);
    #[cfg(not(target_os = "macos"))]
    let app = gpui_kit::application().with_assets(gpui_kit::assets::Assets);

    app.run(move |cx| {
        gpui_kit::init(cx);
        // gpui-component's `theme::init` forces the light palette; sync
        // to the OS appearance before the window opens so the first paint
        // already has the right colors (the popup always follows the
        // system, independent of `display.theme`, which is indicator-only
        // now). The Windows accent override comes after, because a palette
        // reload wipes it.
        apply_theme_and_accent(cx);

        // Popup geometry in GPUI logical pixels. Height cut 25% (520 -> 390):
        // a 3x shorter vertical span is still readable for a two-line trend,
        // and the popup sits closer to the indicator without dominating the
        // desktop. Anchor origin logic clamps this inside the work area.
        let popup_size = size(px(680.), px(390.));
        let anchor = parse_anchor();
        let window_bounds = match anchor.as_ref() {
            Some(a) => {
                let work = anchor_work_area(a, cx);
                Bounds::new(
                    anchor_origin(
                        a,
                        f32::from(popup_size.width),
                        f32::from(popup_size.height),
                        &work,
                    ),
                    popup_size,
                )
            }
            None => Bounds::centered(None, popup_size, cx),
        };
        let anchored = anchor.is_some();

        let window_options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(window_bounds)),
            // Anchored popups behave like a menu: no title bar, no shadow of
            // a normal window, cannot be dragged or resized. When no anchor
            // was passed, fall back to a plain titled window so the process
            // is still usable if the indicator failed to compute a rect.
            titlebar: Some(TitlebarOptions {
                title: Some("SysVitals History".into()),
                appears_transparent: anchored,
                ..Default::default()
            }),
            kind: if anchored {
                WindowKind::PopUp
            } else {
                WindowKind::Normal
            },
            is_movable: !anchored,
            is_resizable: !anchored,
            focus: true,
            ..Default::default()
        };

        // Blur-close and appearance observation are registered from the
        // first render pass (both idempotent); Esc is handled by an
        // app-level keystroke observer set up in `HistoryView::new`. The
        // build closure only needs to create the view.
        gpui_kit::open_window(window_options, cx, move |_window, cx| {
            cx.new(|cx| HistoryView::new(lang, csv_scope, cx))
        })
        .expect("Failed to open history window");
    });
}
