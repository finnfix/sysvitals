//! Pure formatting logic: percentage and rate rendering, prefix handling and
//! the status-bar layout (single-row, the 2x2 dual-row quadrant with CPU/MEM
//! over U/D, and a vertical one-metric-per-line column for vertical taskbars).
//! All functions are string-level and host-independent so they can be unit
//! tested anywhere.
//!
//! Width-stability: within one frame, dual-row columns are padded
//! to equal widths so the two lines line up. Across frames the rendered text
//! can still differ by a character (e.g. `9%` -> `100%`); the display layer is
//! responsible for sizing the embedded window to the maximum width it has
//! seen and never shrinking it. The renderer
//! should apply the OpenType `tnum` feature so digits align visually.

use crate::config::Config;
use crate::model::{Metric, Snapshot};

/// Bytes-per-second unit thresholds on the 1024 base. Four adaptive tiers:
/// `B/s` below 1 KB/s, `K/s` below 1 MB/s, `M/s` below 1 GB/s, `G/s` above.
pub const KB: f64 = 1024.0;
pub const MB: f64 = 1024.0 * KB;
pub const GB: f64 = 1024.0 * MB;

/// Render a CPU/MEM percentage as an integer percent string.
pub fn format_percent(value: f64) -> String {
    let v = value.clamp(0.0, 100.0).round();
    format!("{v:.0}%")
}

/// Render a rate in bytes/second on the 1024 base, adapting the unit to
/// magnitude across four tiers: `B/s` below 1 KB/s, `K/s` below 1 MB/s,
/// `M/s` below 1 GB/s, `G/s` at or above. Decimal places depend on the
/// integer-part width so the field stays roughly the same size:
///
/// ```text
///   314 B/s    (>=100 integer part -> 0 decimals)
///   12.5 K/s   (>=10  -> 1 decimal)
///   1.20 M/s   (<10   -> 2 decimals)
/// ```
pub fn format_rate(bps: f64) -> String {
    let bps = if bps < 0.0 || bps.is_nan() { 0.0 } else { bps };
    let (scaled, unit) = if bps < KB {
        (bps, "B/s")
    } else if bps < MB {
        (bps / KB, "K/s")
    } else if bps < GB {
        (bps / MB, "M/s")
    } else {
        (bps / GB, "G/s")
    };
    let decimals = if scaled >= 100.0 {
        0
    } else if scaled >= 10.0 {
        1
    } else {
        2
    };
    format!("{scaled:.decimals$} {unit}")
}

/// Combine a user prefix with an already-formatted value, inserting exactly
/// one space. An empty prefix shows the value alone (no leading space).
pub fn join_prefix(prefix: &str, value: &str) -> String {
    if prefix.is_empty() {
        value.to_string()
    } else {
        format!("{prefix} {value}")
    }
}

/// Approximate column width for proportional text with CJK: characters in the
/// East Asian Wide/Fullwidth ranges count as 2, combining marks as 0, the rest
/// as 1. Good enough for padding to visual width.
pub fn display_width(s: &str) -> usize {
    s.chars()
        .map(|c| match unicode_width(c) {
            0 => 0,
            2 => 2,
            _ => 1,
        })
        .sum()
}

fn unicode_width(c: char) -> usize {
    let cp = c as u32;
    // Combining marks (zero width).
    if (0x0300..=0x036F).contains(&cp) || (0x200B..=0x200F).contains(&cp) {
        return 0;
    }
    // CJK and other East Asian Wide/Fullwidth blocks commonly used in prefixes.
    let wide = matches!(cp,
        0x1100..=0x115F     // Hangul Jamo init
        | 0x2E80..=0x303E   // CJK radicals, Kangxi, punctuation
        | 0x3041..=0x33FF   // Hiragana..CJK compat
        | 0x3400..=0x4DBF   // CJK ext A
        | 0x4E00..=0x9FFF   // CJK unified
        | 0xA000..=0xA4CF   // Yi
        | 0xAC00..=0xD7A3   // Hangul syllables
        | 0xF900..=0xFAFF   // CJK compat ideographs
        | 0xFE30..=0xFE6F   // CJK compat forms
        | 0xFF00..=0xFF60   // Fullwidth ASCII
        | 0xFFE0..=0xFFE6   // Fullwidth signs
        | 0x1F300..=0x1F64F // emoji
        | 0x1F900..=0x1F9FF);
    if wide {
        2
    } else {
        1
    }
}

/// Pad a cell's value so the numeric field occupies `width` columns (right
/// aligned within the cell), keeping the prefix fixed at the left.
fn pad_to_width(s: &str, width: usize) -> String {
    let cur = display_width(s);
    if cur >= width {
        s.to_string()
    } else {
        let mut out = String::with_capacity(s.len() + (width - cur));
        out.push_str(s);
        for _ in 0..(width - cur) {
            out.push(' ');
        }
        out
    }
}

/// One horizontal band of the status-bar widget. A row contains up to two
/// columns (the 2x2 dual-row quadrant fills `left` and `right`; single-row
/// puts everything in `left` and leaves `right` empty).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub left: String,
    pub right: String,
}

impl Line {
    pub fn single(text: impl Into<String>) -> Self {
        Line {
            left: text.into(),
            right: String::new(),
        }
    }

    /// The full rendered text of the row, including the inter-column gap.
    pub fn render(&self) -> String {
        if self.right.is_empty() {
            self.left.clone()
        } else {
            format!("{}  {}", self.left, self.right)
        }
    }
}

/// The complete frame the display layer draws for one sample.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormattedRow {
    pub lines: Vec<Line>,
    /// Individual metric strings for the single-row layout. When non-empty the
    /// display layer lays these out with a pixel gap (`item_spacing`) instead
    /// of using the pre-joined text in `lines[0]`. Empty for dual-row and
    /// vertical layouts where the column/line structure handles spacing.
    pub items: Vec<String>,
}

impl FormattedRow {
    /// Convenience: total pixel-independent character width of the widest line,
    /// used by the display layer to size the embedded window. An embedded
    /// window must not resize itself from changing content, so this feeds a
    /// grow-only sizing policy.
    pub fn max_width(&self) -> usize {
        self.lines
            .iter()
            .map(|l| {
                if l.right.is_empty() {
                    display_width(&l.left)
                } else {
                    display_width(&l.left) + 2 + display_width(&l.right)
                }
            })
            .max()
            .unwrap_or(0)
    }
}

fn prefix_for(metric: Metric, cfg: &Config) -> &str {
    match metric {
        Metric::Cpu => &cfg.display.prefix.cpu,
        Metric::Mem => &cfg.display.prefix.mem,
        Metric::Up => &cfg.display.prefix.up,
        Metric::Down => &cfg.display.prefix.down,
    }
}

fn shown(metric: Metric, cfg: &Config) -> bool {
    cfg.display.metric_visible(metric)
}

/// Format a single metric value to its display string (no prefix).
pub fn format_value(metric: Metric, snapshot: &Snapshot) -> String {
    match metric {
        Metric::Cpu | Metric::Mem => {
            let v = snapshot.value(metric);
            if v.is_nan() {
                "--%".to_string()
            } else {
                format_percent(v)
            }
        }
        Metric::Up | Metric::Down => {
            let v = snapshot.value(metric);
            if v.is_nan() {
                "--".to_string()
            } else {
                format_rate(v)
            }
        }
    }
}

/// Build the full status-bar frame from a snapshot and config, honoring the
/// per-metric show toggles and the dual-row layout choice.
pub fn format_frame(snapshot: &Snapshot, cfg: &Config) -> FormattedRow {
    let enabled: Vec<Metric> = Metric::ALL.into_iter().filter(|m| shown(*m, cfg)).collect();

    if cfg.display.dual_row {
        // 2x2 quadrant: left column CPU over MEM, right column U over D
        // CPU/MEM over U/D. Only metrics that are both enabled and in
        // the column's role participate.
        let left_role = [Metric::Cpu, Metric::Mem];
        let right_role = [Metric::Up, Metric::Down];
        let left: Vec<(Metric, String)> = enabled
            .iter()
            .filter(|m| left_role.contains(m))
            .map(|m| {
                (
                    *m,
                    join_prefix(prefix_for(*m, cfg), &format_value(*m, snapshot)),
                )
            })
            .collect();
        let right: Vec<(Metric, String)> = enabled
            .iter()
            .filter(|m| right_role.contains(m))
            .map(|m| {
                (
                    *m,
                    join_prefix(prefix_for(*m, cfg), &format_value(*m, snapshot)),
                )
            })
            .collect();

        // Column widths are computed from the two cells in each column so the
        // numbers stay aligned across the two rows.
        let left_w = left
            .iter()
            .map(|(_, s)| display_width(s))
            .max()
            .unwrap_or(0);
        let right_w = right
            .iter()
            .map(|(_, s)| display_width(s))
            .max()
            .unwrap_or(0);

        // If one column has no enabled metric, keep the two-line layout
        // with only the populated column (the user chose dual-row; collapsing
        // to a single line would silently override that choice).
        if left.is_empty() || right.is_empty() {
            let cells = if left.is_empty() { &right } else { &left };
            let w = cells
                .iter()
                .map(|(_, s)| display_width(s))
                .max()
                .unwrap_or(0);
            return FormattedRow {
                lines: vec![
                    Line::single(cell_at(cells, 0, w)),
                    Line::single(cell_at(cells, 1, w)),
                ],
                items: Vec::new(),
            };
        }

        // Fill missing rows in a column with a blank of the same width so the
        // grid stays rectangular.
        let row0_left = cell_at(&left, 0, left_w);
        let row1_left = cell_at(&left, 1, left_w);
        let row0_right = cell_at(&right, 0, right_w);
        let row1_right = cell_at(&right, 1, right_w);
        FormattedRow {
            lines: vec![
                Line {
                    left: row0_left,
                    right: row0_right,
                },
                Line {
                    left: row1_left,
                    right: row1_right,
                },
            ],
            items: Vec::new(),
        }
    } else {
        // Single row: all enabled metrics. The display layer lays these out
        // with a pixel gap (`item_spacing`), so we expose the individual items
        // and also keep a space-joined fallback in `lines[0]` for consumers
        // that only want a single string (e.g. the terminal example).
        let items: Vec<String> = enabled
            .iter()
            .map(|m| join_prefix(prefix_for(*m, cfg), &format_value(*m, snapshot)))
            .collect();
        // Space-joined fallback for string-only consumers (terminal example).
        // The display layer ignores this and uses `items` with a pixel gap.
        let text = items.join("  ");
        FormattedRow {
            lines: vec![Line::single(text)],
            items,
        }
    }
}

fn cell_at(cells: &[(Metric, String)], idx: usize, width: usize) -> String {
    match cells.get(idx) {
        Some((_, s)) => pad_to_width(s, width),
        None => " ".repeat(width),
    }
}

/// Build a vertical frame for a vertical taskbar (left or right edge), where
/// the bar's thickness — not its length — limits the usable width.
///
/// Layout: one metric per block, and inside a block the label and the value
/// each occupy their own line so the widest line is only as wide as the label
/// or the value, never their concatenation. Blocks are separated by one blank
/// line. A metric whose prefix is empty gets just its value line (an empty
/// label would be indistinguishable from a block separator).
///
/// The prefix is emitted verbatim: it already carries whatever punctuation the
/// user configured (the built-in defaults end with `:`), and it stays
/// independent of the UI language.
pub fn format_frame_vertical(snapshot: &Snapshot, cfg: &Config) -> FormattedRow {
    let enabled: Vec<Metric> = Metric::ALL.into_iter().filter(|m| shown(*m, cfg)).collect();
    let mut lines: Vec<Line> = Vec::with_capacity(enabled.len() * 2);
    for (i, m) in enabled.iter().enumerate() {
        if i > 0 {
            lines.push(Line::single(""));
        }
        let prefix = prefix_for(*m, cfg);
        if !prefix.is_empty() {
            lines.push(Line::single(prefix.to_string()));
        }
        lines.push(Line::single(format_value(*m, snapshot)));
    }
    // Vertical layout stacks lines and blanks; pixel item spacing does not
    // apply, so `items` stays empty.
    FormattedRow {
        lines,
        items: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Snapshot;

    fn snap(cpu: f64, mem: f64, up: f64, down: f64) -> Snapshot {
        Snapshot {
            ts: 0,
            cpu_pct: cpu,
            mem_pct: mem,
            up_bps: up,
            down_bps: down,
        }
    }

    #[test]
    fn percent_clamps_and_rounds() {
        assert_eq!(format_percent(29.4), "29%");
        assert_eq!(format_percent(29.6), "30%");
        assert_eq!(format_percent(-3.0), "0%");
        assert_eq!(format_percent(140.0), "100%");
    }

    #[test]
    fn rate_uses_binary_units_and_decimal_rules() {
        // B/s tier: below 1 KB/s.
        assert_eq!(format_rate(314.0), "314 B/s");
        assert_eq!(format_rate(0.0), "0.00 B/s");
        assert_eq!(format_rate(1023.0), "1023 B/s");
        // K/s tier: 1 KB/s .. 1 MB/s.
        assert_eq!(format_rate(1024.0), "1.00 K/s");
        assert_eq!(format_rate(314.0 * KB), "314 K/s");
        assert_eq!(format_rate(12.0 * KB + 512.0), "12.5 K/s");
        // M/s tier: 1 MB/s .. 1 GB/s.
        assert_eq!(format_rate(1.2 * MB), "1.20 M/s");
        assert_eq!(format_rate(100.0 * MB), "100 M/s");
        // G/s tier: 1 GB/s and above.
        assert_eq!(format_rate(1.5 * GB), "1.50 G/s");
        assert_eq!(format_rate(1024.0 * MB), "1.00 G/s");
        assert_eq!(format_rate(120.0 * GB), "120 G/s");
    }

    #[test]
    fn rate_nan_and_negative_become_zero() {
        assert_eq!(format_rate(f64::NAN), "0.00 B/s");
        assert_eq!(format_rate(-5.0), "0.00 B/s");
    }

    #[test]
    fn prefix_join_inserts_one_space() {
        assert_eq!(join_prefix("CPU", "29%"), "CPU 29%");
        assert_eq!(join_prefix("", "29%"), "29%");
    }

    #[test]
    fn display_width_counts_cjk_as_two() {
        assert_eq!(display_width("CPU"), 3);
        assert_eq!(display_width("内存"), 4);
        assert_eq!(display_width("上传"), 4);
        assert_eq!(display_width("U 1.2M/s"), 8);
    }

    #[test]
    fn single_row_layout() {
        let cfg = Config::default();
        let f = format_frame(&snap(29.0, 68.0, 1.2 * MB, 314.0 * KB), &cfg);
        assert_eq!(f.lines.len(), 1);
        assert_eq!(
            f.lines[0].render(),
            "CPU: 29%  MEM: 68%  U: 1.20 M/s  D: 314 K/s"
        );
    }

    #[test]
    fn single_row_exposes_items_for_pixel_spacing() {
        // Item spacing is now a pixel value applied by the display layer, so
        // the core exposes the individual metric strings (`items`) for the
        // single-row layout instead of baking a space count into the text.
        let cfg = Config::default();
        let f = format_frame(&snap(29.0, 68.0, 1024.0, 2048.0), &cfg);
        assert_eq!(
            f.items,
            vec!["CPU: 29%", "MEM: 68%", "U: 1.00 K/s", "D: 2.00 K/s"]
        );
        // The fallback joined string is stable regardless of item_spacing.
        let mut cfg2 = Config::default();
        cfg2.display.item_spacing = 24;
        let f2 = format_frame(&snap(29.0, 68.0, 1024.0, 2048.0), &cfg2);
        assert_eq!(f2.items, f.items);
        assert_eq!(f2.lines[0].render(), f.lines[0].render());
    }

    #[test]
    fn dual_row_has_no_items() {
        // Dual-row layout relies on column structure, not pixel item spacing,
        // so `items` stays empty there.
        let mut cfg = Config::default();
        cfg.display.dual_row = true;
        let f = format_frame(&snap(29.0, 68.0, 1024.0, 2048.0), &cfg);
        assert!(f.items.is_empty());
        assert_eq!(f.lines.len(), 2);
    }

    #[test]
    fn paired_toggles_hide_groups() {
        // Resources and network are each one toggle: turning off a group hides
        // both of its metrics at once.
        let mut cfg = Config::default();
        cfg.display.show_resources = false;
        let f = format_frame(&snap(29.0, 68.0, 1024.0, 2048.0), &cfg);
        let text = f.lines[0].render();
        assert!(!text.contains("CPU") && !text.contains("MEM"));
        assert!(text.starts_with("U: "));

        let mut cfg2 = Config::default();
        cfg2.display.show_network = false;
        let f2 = format_frame(&snap(29.0, 68.0, 1024.0, 2048.0), &cfg2);
        assert_eq!(f2.lines[0].render(), "CPU: 29%  MEM: 68%");
    }

    #[test]
    fn dual_row_is_two_lines_aligned() {
        let mut cfg = Config::default();
        cfg.display.dual_row = true;
        let f = format_frame(&snap(29.0, 68.0, 1.2 * MB, 314.0 * KB), &cfg);
        assert_eq!(f.lines.len(), 2);
        // Left column width fixed by max(CPU.., MEM..) so right column starts
        // at the same offset in both rows.
        assert_eq!(f.lines[0].left.len(), f.lines[1].left.len());
        assert_eq!(f.lines[0].render(), "CPU: 29%  U: 1.20 M/s");
        // "D: 314 K/s" is one col narrower, padded to the column width -> the
        // trailing space is the alignment, matching the row above.
        assert_eq!(f.lines[1].render(), "MEM: 68%  D: 314 K/s ");
    }

    #[test]
    fn dual_row_columns_align_within_frame() {
        let mut cfg = Config::default();
        cfg.display.dual_row = true;
        // CPU and MEM have different natural widths; MEM's row is padded so
        // both right-column cells start at the same offset.
        let f = format_frame(&snap(9.0, 68.0, 1024.0, 1024.0 * 1024.0), &cfg);
        assert_eq!(
            display_width(&f.lines[0].left),
            display_width(&f.lines[1].left)
        );
        assert_eq!(
            display_width(&f.lines[0].right),
            display_width(&f.lines[1].right)
        );
    }

    #[test]
    fn frame_width_can_grow_across_samples_display_layer_pads() {
        // Documented behavior: the core does not clamp cross-frame width (an
        // unbounded rate cannot be padded to a fixed size). The display layer
        // grows the embedded window to the max width it has seen and never
        // shrinks it. Here we only assert the wider frame is at least as wide.
        let cfg = Config::default();
        let small = format_frame(&snap(9.0, 9.0, 1024.0, 1024.0), &cfg).max_width();
        let large =
            format_frame(&snap(99.0, 100.0, 1024.0 * 1024.0, 1024.0 * 1024.0), &cfg).max_width();
        assert!(large > small);
    }

    #[test]
    fn nan_values_show_placeholder() {
        let cfg = Config::default();
        let f = format_frame(&snap(f64::NAN, f64::NAN, f64::NAN, f64::NAN), &cfg);
        assert_eq!(f.lines[0].render(), "CPU: --%  MEM: --%  U: --  D: --");
        // And the whole group disappears when its paired toggle is off.
        let mut c2 = Config::default();
        c2.display.show_network = false;
        let f2 = format_frame(&snap(f64::NAN, f64::NAN, f64::NAN, f64::NAN), &c2);
        assert_eq!(f2.lines[0].render(), "CPU: --%  MEM: --%");
    }

    #[test]
    fn dual_row_keeps_two_lines_when_a_column_empty() {
        let mut cfg = Config::default();
        cfg.display.dual_row = true;
        cfg.display.show_network = false;
        let f = format_frame(&snap(29.0, 68.0, 0.0, 0.0), &cfg);
        // Right column has nothing -> still two lines, left column only.
        assert_eq!(f.lines.len(), 2);
        assert_eq!(f.lines[0].render(), "CPU: 29%");
        assert_eq!(f.lines[1].render(), "MEM: 68%");
    }

    #[test]
    fn custom_and_empty_prefix() {
        let mut cfg = Config::default();
        cfg.display.prefix.cpu = "".to_string();
        cfg.display.prefix.up = "↑".to_string();
        let f = format_frame(&snap(29.0, 68.0, 1024.0, 0.0), &cfg);
        assert_eq!(
            f.lines[0].render(),
            "29%  MEM: 68%  ↑ 1.00 K/s  D: 0.00 B/s"
        );
    }

    #[test]
    fn vertical_layout_stacks_label_and_value_per_metric() {
        let cfg = Config::default();
        let f = format_frame_vertical(&snap(29.0, 68.0, 1.2 * MB, 314.0 * KB), &cfg);
        let texts: Vec<&str> = f.lines.iter().map(|l| l.left.as_str()).collect();
        assert_eq!(
            texts,
            vec!["CPU:", "29%", "", "MEM:", "68%", "", "U:", "1.20 M/s", "", "D:", "314 K/s"]
        );
        assert!(f.items.is_empty());
    }

    #[test]
    fn vertical_layout_keeps_widest_line_below_joined_width() {
        // The whole point of the stacked layout: on a vertical taskbar the bar
        // thickness limits the width, so no line may be wider than the value
        // alone (the old joined "U: 1.20 M/s" line was the cause of clipping).
        let cfg = Config::default();
        let f = format_frame_vertical(&snap(90.0, 60.0, 1.56 * MB, 100.0 * KB), &cfg);
        for l in &f.lines {
            assert!(
                display_width(&l.left) <= display_width("1.56 M/s"),
                "line {:?} exceeds the value-only width",
                l.left
            );
        }
    }

    #[test]
    fn vertical_layout_respects_toggles() {
        let mut cfg = Config::default();
        cfg.display.show_network = false;
        let f = format_frame_vertical(&snap(29.0, 68.0, 1024.0, 2048.0), &cfg);
        let texts: Vec<&str> = f.lines.iter().map(|l| l.left.as_str()).collect();
        assert_eq!(texts, vec!["CPU:", "29%", "", "MEM:", "68%"]);
    }

    #[test]
    fn vertical_layout_empty_prefix_shows_value_only() {
        // An empty prefix must not produce an extra blank line that would be
        // indistinguishable from the inter-metric separator.
        let mut cfg = Config::default();
        cfg.display.prefix.cpu = "".to_string();
        let f = format_frame_vertical(&snap(29.0, 68.0, 1024.0, 2048.0), &cfg);
        let texts: Vec<&str> = f.lines.iter().map(|l| l.left.as_str()).collect();
        assert_eq!(
            texts,
            vec!["29%", "", "MEM:", "68%", "", "U:", "1.00 K/s", "", "D:", "2.00 K/s"]
        );
    }
}
