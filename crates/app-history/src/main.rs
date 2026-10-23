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

use std::collections::HashSet;
use std::env;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Local};
use gpui_kit::component::button::*;
use gpui_kit::component::chart::AreaChart;
use gpui_kit::component::theme::Theme as UiTheme;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use sysvitals_core::backdrop::Material;
use sysvitals_core::config::{config_path, history_path, Config, CsvScope};
use sysvitals_core::i18n::{t, Language};
use sysvitals_core::store::{floor_to_grid, spread_label_indices, Bucket, Store};

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
        if !nums.iter().all(|n| n.is_finite()) {
            return None;
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

    /// Fine bucket width shared by the chart curve and the CSV export:
    /// detailed enough to preserve resolution at each range while staying
    /// bounded (a month is ~720 rows, not 1.3M). The curve draws one point
    /// per fine bucket; only the sparse axis *labels* use the coarser clean
    /// step from [`Self::label_step_secs`].
    fn bucket_secs(self) -> i64 {
        match self {
            TimeRange::Min5 => 2,
            TimeRange::Hour1 => 10,
            TimeRange::Day1 => 120,
            TimeRange::Month1 => 3600,
        }
    }

    /// The clean wall-clock step the x-axis labels are floored to — the
    /// range picks the step so every label reads as a round local time:
    ///   5 min    = 60 s       (every local minute)
    ///   1 hour   = 600 s      (every :10 / :20 / ... local ten-minute mark)
    ///   1 day    = 7 200 s    (every 2 h at 00/02/.../22 local time)
    ///   1 month  = 432 000 s  (every 5 days at local midnight)
    /// Every grid is offset-aligned to the machine's local time zone; steps
    /// like 7200 s do not divide every real UTC offset (e.g. +5:30), so a
    /// zero-offset grid would read :30 instead of the intended marks.
    /// The curve is NOT drawn at this step: it uses [`Self::bucket_secs`] so
    /// the line keeps its detail. Each label point floors its own timestamp
    /// to this grid, so the six spread ticks read as clean times while the
    /// line stays fine-grained. The query window start is aligned down to
    /// this grid (see [`floor_to_grid`]), so the first label dates the window
    /// start.
    fn label_step_secs(self) -> i64 {
        match self {
            TimeRange::Min5 => 60,
            TimeRange::Hour1 => 600,
            TimeRange::Day1 => 7_200,
            TimeRange::Month1 => 432_000,
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

/// A single timestamped sample pair for the merged chart: one fine bucket
/// renders as one x position carrying both series values, which is why CPU
/// and memory live in one struct instead of two point lists. `label` is the
/// sparse x-axis tick text — only clean wall-clock points (per
/// [`TimeRange::label_step_secs`]) carry a non-empty label, the rest stay
/// empty so the curve stays detailed while the axis stays readable. `ts` is
/// kept so the hover tooltip can show the full date and time for any point.
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

/// How many x-axis tick labels the chart shows, spread evenly across the fine
/// data points (the value given to the chart's `x_tick_count`, and mirrored
/// by [`spread_label_indices`] when deciding which points carry label text).
const X_TICK_COUNT: usize = 6;

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
    copy_failed: bool,
    /// Last config mtime applied, in nanoseconds; the popup keeps language and
    /// CSV-scope settings live instead of freezing them at launch.
    config_mtime: u64,
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
    /// Last accent value seen by the live-refresh poll; the override only
    /// re-applies when this moves (see `start_accent_watch`). Seeded with
    /// `None` so the first tick always re-applies once (idempotent when the
    /// startup override already succeeded).
    last_accent: Option<u32>,
}

impl HistoryView {
    fn new(lang: Language, csv_scope: CsvScope, cx: &mut Context<Self>) -> Self {
        let mut view = Self {
            lang,
            range: TimeRange::Hour1,
            buckets: Vec::new(),
            csv_scope,
            copied: false,
            copy_failed: false,
            config_mtime: 0,
            ever_active: false,
            _activation: None,
            _appearance: None,
            _escape: None,
            last_accent: None,
        };
        view.load_data();
        view.start_refresh_loop(cx);
        view.start_escape_observer(cx);
        view.start_accent_watch(cx);
        view
    }

    /// Poll the OS accent every 500 ms and re-apply the override when it
    /// moved. The appearance observer only fires on a light/dark MODE flip
    /// (the Windows platform layer filters `WM_SETTINGCHANGE` down to
    /// `ImmersiveColorSet` with an appearance change), so an accent-only
    /// change never re-runs `apply_accent` on its own. The probe runs inside
    /// `update`: on macOS `system_accent_hex` reads AppKit's
    /// `controlAccentColor`, which belongs to the main thread. The loop stops
    /// with the view entity, i.e. when the window closes. Same shape as the
    /// settings window's accent poll.
    fn start_accent_watch(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(500))
                .await;
            let alive = this
                .update(cx, |view: &mut HistoryView, cx| {
                    let current = system_accent_hex();
                    if current == view.last_accent {
                        return true;
                    }
                    view.last_accent = current;
                    if current.is_some() {
                        apply_accent(cx);
                    } else {
                        // The OS moved back to "multicolor": reload the palette
                        // so the old accent override is removed.
                        apply_theme_and_accent(cx);
                    }
                    cx.notify();
                    true
                })
                .unwrap_or(false);
            if !alive {
                break;
            }
        })
        .detach();
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

    /// Watch the window's OS-appearance changes and re-apply the system
    /// palette (the popup always follows the system; `display.theme` is
    /// indicator-only). Idempotent, and registered from the first render
    /// pass like `observe_activation` — never from a paint-phase callback,
    /// which would hit the empty active-node stack on the root view's first
    /// frame (see the crash note on `_escape`).
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
        // On Windows the popup can open *without* ever being activated: the
        // indicator overlay was the foreground process when it launched this
        // child, and the foreground-lock rules can reject the startup
        // activation attempt — then `ever_active` never flips and a click away
        // never closes the window (the reported bug: a fast double click opens
        // the popup but clicking elsewhere leaves it stuck; a single click
        // usually activates fine). When the first-frame seed found the window
        // inactive, claim activation through GPUI's own `activate_window` path
        // (reassert placement + inject an Alt press so `SetForegroundWindow`
        // is permitted). The resulting WM_ACTIVATE edge flows through the
        // observer below and arms the blur-close. macOS is deliberately
        // untouched: its panel is an accessory app's window with its own
        // key-window lifecycle, unverified at runtime, and the bug was only
        // ever reproduced on Windows.
        #[cfg(windows)]
        if !self.ever_active {
            window.activate_window();
            // If activation completed synchronously, the observer below will
            // not see an edge; seed the guard from the post-activation state.
            if window.is_window_active() {
                self.ever_active = true;
            }
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
    /// Reload language and CSV scope when the config file changes while the
    /// popup is open. The theme is system-owned; only these two settings are
    /// consumed by this panel.
    fn reload_config(&mut self) {
        let Ok(path) = config_path() else {
            return;
        };
        let mtime = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        if mtime == self.config_mtime {
            return;
        }
        match Config::load(&path) {
            Ok((cfg, _)) => {
                self.config_mtime = mtime;
                self.lang = cfg.display.language;
                self.csv_scope = cfg.history.csv_scope;
            }
            Err(e) => {
                // Retry only when the file changes again, and keep the last
                // good values in the meantime.
                self.config_mtime = mtime;
                eprintln!("app-history: config reload failed: {e}");
            }
        }
    }

    fn start_refresh_loop(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(REFRESH_INTERVAL).await;
            if this
                .update(cx, |view: &mut HistoryView, cx| {
                    view.reload_config();
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

    /// The exact query window the chart is showing: a start floored to the
    /// range's label grid, the current time, the fine curve bucket width, and
    /// the local-time offset used for both bucketing and label flooring.
    /// CSV export and the chart both use this so "copy what you see" is true
    /// at the window boundary too.
    fn visible_query(&self, now: i64) -> (i64, i64, i64, i64) {
        let step = self.range.label_step_secs();
        let offset = Local::now().offset().local_minus_utc() as i64;
        let from = floor_to_grid(now - self.range.seconds(), step, offset);
        (from, now, self.range.bucket_secs(), offset)
    }

    fn load_data(&mut self) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let (from, to, bucket, offset) = self.visible_query(now);
        // Clear on failure: otherwise a range switch would keep showing the
        // previous range's points under the new axis.
        let mut buckets = Vec::new();
        if let Ok(path) = history_path() {
            if let Ok(store) = Store::open(&path) {
                // The curve draws at the fine CSV bucket width; axis labels
                // floor each point's timestamp to the clean grid (see
                // `points`). One query serves both — no coarse pass anymore.
                buckets = store
                    .buckets_offset(from, to, bucket, offset)
                    .unwrap_or_default();
            }
        }
        self.buckets = buckets;
    }

    fn points(&self) -> Vec<ChartPoint> {
        // One point per fine bucket: the curve keeps its full resolution.
        // Only the indices the axis paints labels at (`spread_label_indices`,
        // mirroring the library's selection rule) carry label text, floored to
        // the range's clean grid (60/600/7200/432000 s) so every visible tick
        // reads as a round time. A labeled point's pixel position may sit up
        // to one fine bucket off that clean boundary — under 1% of the window
        // — which is the cost of keeping the line detailed; the label text,
        // not the geometry, carries the roundness.
        //
        // Day-crossing: the first labeled point, and any whose floored local
        // day differs from the previous labeled point's, carries "MM-DD HH:MM"
        // so a midnight crossing is visible; within one day the label is just
        // HH:MM. The month view labels with MM-DD only. A freshly started
        // indicator can floor two labeled points to the same wall-clock string
        // — painting it twice reads as broken, so only the first keeps it.
        let month_view = self.range == TimeRange::Month1;
        let step = self.range.label_step_secs();
        let offset = Local::now().offset().local_minus_utc() as i64;
        let labeled: HashSet<usize> = spread_label_indices(self.buckets.len(), X_TICK_COUNT)
            .into_iter()
            .collect();
        let mut points = Vec::with_capacity(self.buckets.len());
        let mut prev_day: Option<String> = None;
        let mut last_shown: Option<String> = None;
        for (i, b) in self.buckets.iter().enumerate() {
            let mut label = String::new();
            if labeled.contains(&i) {
                let clean = floor_to_grid(b.ts, step, offset);
                let day = day_of(clean);
                let crosses = prev_day.as_ref().map_or(true, |p| *p != day);
                prev_day = Some(day);
                label = if month_view {
                    fmt_date(clean)
                } else if crosses {
                    fmt_date_time(clean)
                } else {
                    fmt_time(clean)
                };
                if last_shown.as_deref() == Some(label.as_str()) {
                    label = String::new();
                } else {
                    last_shown = Some(label.clone());
                }
            }
            points.push(ChartPoint {
                ts: b.ts,
                label,
                cpu: b.cpu,
                mem: b.mem,
            });
        }
        points
    }

    fn copy_csv(&mut self) {
        let lang = self.lang;
        self.copy_failed = false;

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

        let buckets = if self.csv_scope == CsvScope::Visible {
            // Exactly the points currently drawn, including the grid-aligned
            // window start and the local-time bucket phase.
            Some(self.buckets.clone())
        } else {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let (seconds, bucket_secs) = match self.csv_scope {
                CsvScope::Visible => unreachable!(),
                CsvScope::Last24h => (86_400, 120),
                CsvScope::Last7d => (604_800, 600),
                CsvScope::Last30d => (2_592_000, 3600),
            };
            let offset = Local::now().offset().local_minus_utc() as i64;
            let from = floor_to_grid(now - seconds, bucket_secs, offset);
            let mut loaded = None;
            if let Ok(path) = history_path() {
                if let Ok(store) = Store::open(&path) {
                    loaded = store.buckets_offset(from, now, bucket_secs, offset).ok();
                }
            }
            loaded
        };

        let Some(buckets) = buckets else {
            self.copied = false;
            self.copy_failed = true;
            eprintln!("app-history: CSV query failed");
            return;
        };
        for b in &buckets {
            let ts_str = format_ts_full(b.ts);
            csv.push_str(&format!("{ts_str}\t{:.1}\t{:.1}\n", b.cpu, b.mem));
        }

        match arboard::Clipboard::new() {
            Ok(mut clipboard) => match clipboard.set_text(csv) {
                Ok(()) => {
                    self.copied = true;
                    self.copy_failed = false;
                }
                Err(e) => {
                    self.copied = false;
                    self.copy_failed = true;
                    eprintln!("app-history: clipboard write failed: {e}");
                }
            },
            Err(e) => {
                self.copied = false;
                self.copy_failed = true;
                eprintln!("app-history: clipboard unavailable: {e}");
            }
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
                view.copy_failed = false;
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
/// time zone. Used by both the CSV export and the hover tooltip so exported
/// rows and the point readout agree.
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
        let copy_failed = self.copy_failed;
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
        } else if copy_failed {
            t(lang, "chart.copy_failed").to_string()
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
            .when(copy_failed, |b| b.text_color(hsla(0.0, 0.7, 0.5, 1.0)))
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
        // Y axis: `y_domain(0, 100)` + `y_tick_count(6)` + `y_padding(0, 0)`.
        // The library spaces the ticks evenly over the *full* plot height but
        // maps the pinned domain only into the height shrunk by the top
        // padding (10 px default), so the labels would read the padded
        // positions (e.g. 20.8 / 41.6 / ... instead of 20 / 40 / ...). With
        // zero padding the tick grid and the domain grid coincide and the
        // six labels land exactly on 0 / 20 / 40 / 60 / 80 / 100; the series
        // clips at the domain edges as before (pinned-plot mask).
        //
        // X axis: `x_tick_count(X_TICK_COUNT)` picks that many data indices
        // evenly spread first→last; only those indices carry label text, each
        // its own bucket time floored to the range's clean grid (see
        // `points` and `spread_label_indices`), so every visible label reads as a
        // round time while the curve keeps the fine per-bucket shape.
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
                .y_padding(0.0, 0.0)
                .x_axis(true)
                .x_tick_count(X_TICK_COUNT)
                .y_axis(true)
                .y_tick_count(6)
                .grid(true)
                .appear(false)
                .tooltip_title(|p: &ChartPoint| format_ts_full(p.ts).into())
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

// ---------------------------------------------------------------------------
// System accent + backdrop material (mirrors app-settings; see there for the
// full rationale on each helper)
// ---------------------------------------------------------------------------

/// The system accent color packed as `0x00RRGGBB`, or `None` when it should
/// not be used at all. Windows reads `DwmGetColorizationColor` (the ARGB
/// DWORD; opaque flag ignored — the RGB channels are the same accent the
/// Settings app paints with). macOS follows only an *explicit* choice:
/// `AppleAccentColor` in the standard defaults is present exactly when the
/// user picked a specific accent (Graphite included) and absent under the
/// default "multicolor / auto" mode, where AppKit derives the accent from
/// the wallpaper. Staying absent under auto keeps the library look; the
/// check is presence-only on purpose so a future key-reshuffle degrades to
/// the safe path, not to a wrong color. When present,
/// `NSColor.controlAccentColor` reflects the user's pick without any
/// light/dark resolution. See app-settings for the same shape.
#[cfg(windows)]
fn system_accent_hex() -> Option<u32> {
    use windows::Win32::Graphics::Dwm::DwmGetColorizationColor;
    let mut argb = 0u32;
    // `Default::default()` avoids naming the `BOOL` type: in windows-core
    // 0.62 that is a plain `pub type BOOL = i32` alias, so writing
    // `BOOL(0)` would compile as a tuple-struct constructor (E0423). The
    // opaque-blend flag is not read — the accent color is meaningful either
    // way. See the same fix in app-settings.
    if unsafe { DwmGetColorizationColor(&mut argb, &mut Default::default()) }.is_err() {
        return None;
    }
    Some(argb & 0x00FF_FFFF)
}

#[cfg(target_os = "macos")]
fn system_accent_hex() -> Option<u32> {
    use objc2::rc::Retained;
    use objc2_app_kit::{NSColor, NSColorSpace};
    use objc2_foundation::{NSString, NSUserDefaults};
    NSUserDefaults::standardUserDefaults().objectForKey(&NSString::from_str("AppleAccentColor"))?;
    let color = NSColor::controlAccentColor();
    let srgb = NSColorSpace::sRGBColorSpace();
    let converted: Retained<NSColor> = color.colorUsingColorSpace(&srgb)?;
    let (mut r, mut g, mut b, mut a) = (0f64, 0f64, 0f64, 0f64);
    unsafe { converted.getRed_green_blue_alpha(&mut r, &mut g, &mut b, &mut a) };
    let to8 = |x: f64| ((x.clamp(0.0, 1.0) * 255.0).round() as u32) & 0xFF;
    Some((to8(r) << 16) | (to8(g) << 8) | to8(b))
}

#[cfg(not(any(windows, target_os = "macos")))]
fn system_accent_hex() -> Option<u32> {
    None
}

/// Push the OS accent into the theme so the primary button style (the active
/// range tile, focus rings) matches the OS picker, writing both the legacy
/// `primary` and `button_primary` fields plus the lightness-shifted
/// hover/active variants and `ring` (see app-settings).
///
/// The chart line colors are deliberately *not* overridden here: they encode
/// which series is which and must stay stable so the two lines remain
/// distinguishable after a user changes their accent.
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
    });
}

// ---------------------------------------------------------------------------
// Window backdrop material (Win11 Mica / macOS blur, with fallbacks)
// ---------------------------------------------------------------------------

/// Read a HKCU registry DWORD (same pattern as app-settings, which reads the
/// layout flag; the windows feature set in the workspace manifest covers the
/// registry API).
#[cfg(windows)]
fn hkcu_dword(key: &str, value: &str) -> Option<u32> {
    use windows::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CURRENT_USER, KEY_READ,
        REG_VALUE_TYPE,
    };
    let utf16 = |s: &str| -> Vec<u16> { s.encode_utf16().chain(std::iter::once(0)).collect() };
    let key_w = utf16(key);
    let value_w = utf16(value);
    let mut hkey = HKEY::default();
    if unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            windows::core::PCWSTR(key_w.as_ptr()),
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
            windows::core::PCWSTR(value_w.as_ptr()),
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

/// The Windows OS build number via `RtlGetVersion` (see app-settings for why
/// the classic API lies); `None` when the probe fails.
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

/// The backdrop this Windows host supports, honoring the transparency-effects
/// preference. Same rule as app-settings, fed by the same probes; the pure
/// threshold decision lives in core.
#[cfg(windows)]
fn detect_material() -> Material {
    const PERSONALIZE_KEY: &str =
        "Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize";
    let transparency = hkcu_dword(PERSONALIZE_KEY, "EnableTransparency")
        .map(|v| v != 0)
        .unwrap_or(true);
    sysvitals_core::backdrop::choose_windows_material(os_build_number(), transparency)
}

/// macOS always requests the blur material: the window blur exists on every
/// supported host, and AppKit flattens it automatically when the user turns
/// on "reduce transparency", so the accessibility fallback is the system's
/// own — see app-settings for the same reasoning.
#[cfg(target_os = "macos")]
fn detect_material() -> Material {
    sysvitals_core::backdrop::choose_macos_material(true)
}

#[cfg(not(any(windows, target_os = "macos")))]
fn detect_material() -> Material {
    Material::Opaque
}

/// Map the material onto GPUI's window-background request (see app-settings).
fn backdrop_for(material: Material) -> WindowBackgroundAppearance {
    match material {
        Material::Mica => WindowBackgroundAppearance::MicaBackdrop,
        Material::Blur => WindowBackgroundAppearance::Blurred,
        Material::Opaque => WindowBackgroundAppearance::Opaque,
    }
}

/// Tint the popup's root surface so the backdrop reads through, with the
/// opaque fallback as a no-op. Same replay-after-theme-change contract as the
/// accent; see the matching function in app-settings.
fn apply_material(cx: &mut App) {
    use sysvitals_core::backdrop::{should_tint, MATERIAL_TINT_ALPHA};
    let material = detect_material();
    if !should_tint(material) {
        return;
    }
    UiTheme::update(cx, |theme| {
        let opaque = |c: Hsla| Hsla { a: 1.0, ..c };
        theme.colors.background = opaque(theme.colors.background).opacity(MATERIAL_TINT_ALPHA);
    });
}

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

/// Theme sync, accent override, and material tint applied together, in that
/// order. The palette reload wipes every runtime patch, so both follow every
/// theme change (see the matching rule in app-settings).
fn apply_theme_and_accent(cx: &mut App) {
    apply_system_theme(cx);
    apply_accent(cx);
    apply_material(cx);
}

fn main() {
    let cfg = match config_path().and_then(|p| Config::load(&p).map(|(c, _)| c)) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("app-history: config load failed: {e}; using defaults");
            Config::default()
        }
    };

    let lang = cfg.display.language;
    let csv_scope = cfg.history.csv_scope;
    let window_title = format!("SysVitals {}", t(lang, "chart.title"));

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

        // Anchored popups behave like a menu: no title bar, no shadow of
        // a normal window, cannot be dragged or resized. When no anchor
        // was passed, fall back to a plain titled window so the process
        // is still usable if the indicator failed to compute a rect.
        //
        // macOS needs the anchored case to pass `titlebar: None`. The
        // transparent-titlebar path keeps `NSClosableWindowMask` while adding
        // `NSFullSizeContentViewWindowMask`, so AppKit still renders the
        // traffic-light buttons on top of content that now spans the whole
        // window — they collided with the chart's top-left. Dropping the
        // titlebar options removes the closable bit from the style mask, so no
        // window buttons are created. The library's only other lever,
        // `traffic_light_position`, merely relocates the buttons rather than
        // removing them. Windows ignores this field for its borderless popup,
        // so that platform keeps the previous options unchanged.
        #[cfg(target_os = "macos")]
        let titlebar = if anchored {
            None
        } else {
            Some(TitlebarOptions {
                title: Some(window_title.clone()),
                ..Default::default()
            })
        };
        #[cfg(not(target_os = "macos"))]
        let titlebar = Some(TitlebarOptions {
            title: Some(window_title.clone()),
            appears_transparent: anchored,
            ..Default::default()
        });

        let window_options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(window_bounds)),
            titlebar,
            kind: if anchored {
                WindowKind::PopUp
            } else {
                WindowKind::Normal
            },
            is_movable: !anchored,
            is_resizable: !anchored,
            // System backdrop material (Win11 Mica / Win10-era acrylic blur /
            // macOS window blur, opaque fallback on anything older or when the
            // user disabled transparency effects). The palette tint applied in
            // `apply_theme_and_accent` is what lets it show through. This is
            // purely a compositor attribute — it does not change the window's
            // activation edges, so the blur-close contract is untouched.
            window_background: backdrop_for(detect_material()),
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
