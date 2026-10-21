//! Configuration model, defaults, sanitization, persistence and path
//! resolution. Persistence uses temp-file + atomic rename so a crash mid-write
//! never leaves a half-written config.
//!
//! Prefixes are stored exactly as the user set them and are independent of
//! `language`: switching language never rewrites them. Sanitization
//! only rejects control characters and caps length.

use std::fs;
use std::io::Write;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::i18n::Language;
use crate::model::{Anchor, Theme};

/// Maximum prefix length in characters.
pub const MAX_PREFIX_CHARS: usize = 8;

/// Allowed sample intervals in milliseconds. The settings UI offers exactly
/// these three choices (1s / 2s / 3s); any other value is snapped to the
/// nearest one.
pub const ALLOWED_INTERVAL_MS: [u64; 3] = [1000, 2000, 3000];

/// Default sample interval: 2 seconds.
pub const DEFAULT_INTERVAL_MS: u64 = 2000;

/// Allowed item-spacing values in pixels for the single-row layout. The
/// settings UI offers exactly these choices; any other value snaps to the
/// nearest one.
pub const ALLOWED_ITEM_SPACING_PX: [u32; 4] = [8, 12, 16, 24];

/// Default item spacing: 16 px.
pub const DEFAULT_ITEM_SPACING_PX: u32 = 16;

/// Allowed left-margin values in pixels for the far-left anchor. The settings
/// UI offers exactly these choices; any other value snaps to the nearest one.
pub const ALLOWED_LEFT_MARGIN_PX: [u32; 5] = [8, 16, 48, 128, 256];

/// Default left margin: 16 px.
pub const DEFAULT_LEFT_MARGIN_PX: u32 = 16;

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub sample: SampleConfig,
    pub display: DisplayConfig,
    pub history: HistoryConfig,
    pub windows: WindowsConfig,
    pub macos: MacOSConfig,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SampleConfig {
    pub interval_ms: u64,
    /// Exponential smoothing factor for CPU percent. 0 disables smoothing.
    pub cpu_smoothing: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DisplayConfig {
    pub language: Language,
    /// CPU and MEM share one toggle: they are shown/hidden together.
    pub show_resources: bool,
    /// U and D share one toggle: they are shown/hidden together.
    pub show_network: bool,
    pub theme: Theme,
    /// Fixed 1024 base. Kept as a field so a future decimal toggle is a
    /// config-only change. Anything other than `true` is ignored at format
    /// time; format always divides by 1024.
    pub binary_unit: bool,
    /// Horizontal gap between adjacent metrics on the single-row layout, in
    /// pixels. One of [`ALLOWED_ITEM_SPACING_PX`]; anything else snaps to the
    /// nearest allowed value via `sanitize`.
    pub item_spacing: u32,
    /// Dual-row (2x2 quadrant) layout. Cross-platform: Windows renders it in
    /// the taskbar overlay; macOS will render it in the menu bar (Phase 4).
    pub dual_row: bool,
    pub prefix: PrefixConfig,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PrefixConfig {
    pub cpu: String,
    pub mem: String,
    pub up: String,
    pub down: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HistoryConfig {
    pub enabled: bool,
    pub retention_days: u32,
    pub sample_interval_ms: u64,
    pub csv_scope: CsvScope,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CsvScope {
    /// Copy exactly what the chart currently shows.
    #[default]
    Visible,
    Last24h,
    Last7d,
    Last30d,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WindowsConfig {
    pub anchor: Anchor,
    pub row_spacing: u32,
    pub auto_start: bool,
    /// Manual font-size override (points). `None` = follow the system metric.
    /// Dual-row scales the system font (≈0.75) automatically instead.
    pub font_size_override: Option<f32>,
    /// Extra gap (pixels) between the taskbar's left edge and the indicator
    /// when the far-left anchor is active. One of [`ALLOWED_LEFT_MARGIN_PX`];
    /// anything else snaps to the nearest allowed value via `sanitize`.
    /// Default 16.
    pub left_margin_px: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MacOSConfig {
    pub font_size_mode: FontSizeMode,
    pub custom_font_size: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum FontSizeMode {
    #[default]
    System,
    Custom,
}

impl Default for SampleConfig {
    fn default() -> Self {
        SampleConfig {
            interval_ms: DEFAULT_INTERVAL_MS,
            cpu_smoothing: 0.3,
        }
    }
}

impl Default for DisplayConfig {
    fn default() -> Self {
        DisplayConfig {
            language: Language::En,
            show_resources: true,
            show_network: true,
            theme: Theme::Auto,
            binary_unit: true,
            item_spacing: DEFAULT_ITEM_SPACING_PX,
            dual_row: false,
            prefix: PrefixConfig::default(),
        }
    }
}

impl DisplayConfig {
    /// Which of the four metrics are visible, given the two paired toggles.
    /// Resources (CPU/MEM) and network (U/D) can each be on or off together.
    pub fn metric_visible(&self, metric: crate::model::Metric) -> bool {
        use crate::model::Metric;
        match metric {
            Metric::Cpu | Metric::Mem => self.show_resources,
            Metric::Up | Metric::Down => self.show_network,
        }
    }
}

impl Default for PrefixConfig {
    fn default() -> Self {
        PrefixConfig {
            cpu: "CPU:".to_string(),
            mem: "MEM:".to_string(),
            up: "U:".to_string(),
            down: "D:".to_string(),
        }
    }
}

impl Default for HistoryConfig {
    fn default() -> Self {
        HistoryConfig {
            enabled: true,
            retention_days: 30,
            sample_interval_ms: DEFAULT_INTERVAL_MS,
            csv_scope: CsvScope::default(),
        }
    }
}

impl Default for WindowsConfig {
    fn default() -> Self {
        WindowsConfig {
            anchor: Anchor::Right,
            row_spacing: 1,
            auto_start: true,
            font_size_override: None,
            left_margin_px: DEFAULT_LEFT_MARGIN_PX,
        }
    }
}

impl Default for MacOSConfig {
    fn default() -> Self {
        MacOSConfig {
            font_size_mode: FontSizeMode::System,
            custom_font_size: 13.0,
        }
    }
}

/// A recoverable problem found while sanitizing config. The value was fixed;
/// these are surfaced to the user (e.g. in the settings panel) but never
/// block startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SanitizeIssue {
    pub field: &'static str,
    pub message: String,
}

impl Config {
    /// Clamp/repair an in-memory config into a valid state and report what was
    /// changed. Called after load and after any edit. Idempotent: a sanitized
    /// config yields no issues.
    pub fn sanitize(&mut self) -> Vec<SanitizeIssue> {
        let mut issues = Vec::new();

        self.sample.interval_ms = snap_interval(self.sample.interval_ms, &mut issues);
        self.history.sample_interval_ms =
            snap_interval(self.history.sample_interval_ms, &mut issues);

        if !(0.0..=1.0).contains(&self.sample.cpu_smoothing) {
            let old = self.sample.cpu_smoothing;
            self.sample.cpu_smoothing = old.clamp(0.0, 1.0);
            issues.push(SanitizeIssue {
                field: "sample.cpu_smoothing",
                message: format!("clamped to [0,1] (was {old})"),
            });
        }
        if self.history.retention_days == 0 {
            self.history.retention_days = 30;
            issues.push(SanitizeIssue {
                field: "history.retention_days",
                message: "must be >= 1, reset to 30".to_string(),
            });
        }

        if !ALLOWED_ITEM_SPACING_PX.contains(&self.display.item_spacing) {
            let old = self.display.item_spacing;
            self.display.item_spacing = snap_u32(old, &ALLOWED_ITEM_SPACING_PX);
            issues.push(SanitizeIssue {
                field: "display.item_spacing",
                message: format!("snapped to nearest allowed value (was {old})"),
            });
        }

        for (key, field) in [
            ("cpu", &mut self.display.prefix.cpu),
            ("mem", &mut self.display.prefix.mem),
            ("up", &mut self.display.prefix.up),
            ("down", &mut self.display.prefix.down),
        ] {
            if let Some(msg) = sanitize_prefix(field) {
                issues.push(SanitizeIssue {
                    field: match key {
                        "cpu" => "display.prefix.cpu",
                        "mem" => "display.prefix.mem",
                        "up" => "display.prefix.up",
                        _ => "display.prefix.down",
                    },
                    message: msg,
                });
            }
        }

        if self.windows.row_spacing > 8 {
            self.windows.row_spacing = 8;
        }
        if !ALLOWED_LEFT_MARGIN_PX.contains(&self.windows.left_margin_px) {
            let old = self.windows.left_margin_px;
            self.windows.left_margin_px = snap_u32(old, &ALLOWED_LEFT_MARGIN_PX);
            issues.push(SanitizeIssue {
                field: "windows.left_margin_px",
                message: format!("snapped to nearest allowed value (was {old})"),
            });
        }
        // At least one metric group must stay visible: if both toggles are
        // off, re-enable resources (CPU/MEM) so the indicator never goes blank.
        if !self.display.show_resources && !self.display.show_network {
            self.display.show_resources = true;
            issues.push(SanitizeIssue {
                field: "display.show_resources",
                message: "at least one group must remain visible; resources re-enabled".to_string(),
            });
        }
        issues
    }

    /// Serialize to TOML text.
    pub fn to_toml_string(&self) -> anyhow::Result<String> {
        toml::to_string_pretty(self).map_err(Into::into)
    }

    /// Parse TOML text, apply defaults for anything missing, then sanitize.
    pub fn from_toml_str(text: &str) -> anyhow::Result<(Config, Vec<SanitizeIssue>)> {
        let mut cfg: Config = toml::from_str(text)?;
        let issues = cfg.sanitize();
        Ok((cfg, issues))
    }

    /// Load from disk. A missing file yields defaults (not an error); other
    /// read/parse errors are returned so the caller can decide to back up.
    pub fn load(path: &PathBuf) -> anyhow::Result<(Config, Vec<SanitizeIssue>)> {
        match fs::read_to_string(path) {
            Ok(text) => Self::from_toml_str(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let mut cfg = Config::default();
                let issues = cfg.sanitize();
                Ok((cfg, issues))
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Persist atomically: write `path.tmp` (fsync), then rename over `path`.
    pub fn save(&self, path: &PathBuf) -> anyhow::Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = {
            let mut name = path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "config.toml".to_string());
            name.push_str(".tmp");
            dir_join(path.parent(), &name)
        };
        let text = self.to_toml_string()?;

        let mut file = fs::File::create(&tmp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        drop(file);

        fs::rename(&tmp, path)?;
        Ok(())
    }
}

fn snap_interval(value: u64, issues: &mut Vec<SanitizeIssue>) -> u64 {
    if ALLOWED_INTERVAL_MS.contains(&value) {
        value
    } else {
        let snapped = *ALLOWED_INTERVAL_MS
            .iter()
            .min_by_key(|x| x.abs_diff(value))
            .unwrap_or(&DEFAULT_INTERVAL_MS);
        issues.push(SanitizeIssue {
            field: "sample.interval_ms",
            message: format!("{value} is not allowed, snapped to {snapped}"),
        });
        snapped
    }
}

/// Snap an out-of-set `u32` value to the nearest entry of `allowed`. The
/// caller guarantees `allowed` is non-empty.
fn snap_u32(value: u32, allowed: &[u32]) -> u32 {
    *allowed
        .iter()
        .min_by_key(|x| x.abs_diff(value))
        .expect("allowed must be non-empty")
}

/// Trim outer whitespace, drop control chars, cap to MAX_PREFIX_CHARS.
/// Returns Some(message) if anything was changed.
fn sanitize_prefix(value: &mut String) -> Option<String> {
    let original = value.clone();
    let mut cleaned: String = value
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_PREFIX_CHARS)
        .collect();
    // Normalize internal runs of whitespace to a single space so prefixes stay
    // compact; the display layer always inserts exactly one space before value.
    while cleaned.contains("  ") {
        cleaned = cleaned.replace("  ", " ");
    }
    if cleaned != original {
        let msg = if original.chars().any(char::is_control) {
            "control characters removed"
        } else if original.chars().count() > MAX_PREFIX_CHARS {
            "truncated to 8 characters"
        } else {
            "whitespace normalized"
        };
        *value = cleaned;
        Some(msg.to_string())
    } else {
        None
    }
}

fn dir_join(parent: Option<&std::path::Path>, name: &str) -> PathBuf {
    match parent {
        Some(p) => p.join(name),
        None => PathBuf::from(name),
    }
}

/// Where the config file lives. Windows: `%APPDATA%\SysVitals\config.toml`;
/// macOS: `~/Library/Application Support/SysVitals/config.toml`; Linux dev
/// falls back to the same helper's config dir.
pub fn config_path() -> anyhow::Result<PathBuf> {
    use directories::ProjectDirs;
    let dirs = ProjectDirs::from("", "", "SysVitals")
        .ok_or_else(|| anyhow::anyhow!("could not resolve project directories"))?;
    Ok(dirs.config_dir().join("config.toml"))
}

/// Where the history database lives. Windows:
/// `%APPDATA%\SysVitals\history.db`; macOS:
/// `~/Library/Application Support/SysVitals/history.db`. SQLite is the single
/// source of truth for historical data (R12); CSV is export-only.
pub fn history_path() -> anyhow::Result<PathBuf> {
    use directories::ProjectDirs;
    let dirs = ProjectDirs::from("", "", "SysVitals")
        .ok_or_else(|| anyhow::anyhow!("could not resolve project directories"))?;
    Ok(dirs.data_dir().join("history.db"))
}

/// The per-user data directory (`%APPDATA%\SysVitals` on Windows,
/// `~/Library/Application Support/SysVitals` on macOS). Log files live under
/// `<data_dir>/logs/` (see [`crate::logger`]).
pub fn data_dir() -> anyhow::Result<PathBuf> {
    use directories::ProjectDirs;
    let dirs = ProjectDirs::from("", "", "SysVitals")
        .ok_or_else(|| anyhow::anyhow!("could not resolve project directories"))?;
    Ok(dirs.data_dir().to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_spec() {
        let c = Config::default();
        assert_eq!(c.sample.interval_ms, 2000);
        assert_eq!(c.display.language, Language::En);
        assert_eq!(c.display.prefix.cpu, "CPU:");
        assert_eq!(c.display.prefix.up, "U:");
        assert_eq!(c.windows.anchor, Anchor::Right);
        assert!(!c.display.dual_row);
        assert_eq!(c.history.retention_days, 30);
        assert_eq!(c.display.item_spacing, DEFAULT_ITEM_SPACING_PX);
        assert_eq!(c.windows.left_margin_px, DEFAULT_LEFT_MARGIN_PX);
    }

    #[test]
    fn sanitize_default_reports_nothing() {
        let mut c = Config::default();
        assert!(c.sanitize().is_empty());
    }

    #[test]
    fn interval_snaps_to_nearest_allowed() {
        // 1/2/3 s are the only choices; 3000 is now legal and stays unchanged.
        let mut c = Config::default();
        c.sample.interval_ms = 3000;
        assert!(c.sanitize().iter().all(|i| i.field != "sample.interval_ms"));
        assert_eq!(c.sample.interval_ms, 3000);

        // 4000 is out of range: nearest of {1000,2000,3000} is 3000.
        let mut c2 = Config::default();
        c2.sample.interval_ms = 4000;
        let issues = c2.sanitize();
        assert_eq!(c2.sample.interval_ms, 3000);
        assert!(issues.iter().any(|i| i.field == "sample.interval_ms"));

        // 500 snaps up to the smallest allowed tier, 1000.
        let mut c3 = Config::default();
        c3.sample.interval_ms = 500;
        c3.sanitize();
        assert_eq!(c3.sample.interval_ms, 1000);
    }

    #[test]
    fn prefix_control_chars_and_length_capped() {
        let mut c = Config::default();
        c.display.prefix.cpu = "AB\u{7}CDEF".to_string(); // BEL in middle
        c.sanitize();
        assert_eq!(c.display.prefix.cpu, "ABCDEF");

        let mut c2 = Config::default();
        c2.display.prefix.mem = "TOOLONGPREFIX".to_string();
        let issues = c2.sanitize();
        assert_eq!(c2.display.prefix.mem.chars().count(), MAX_PREFIX_CHARS);
        assert!(issues.iter().any(|i| i.message.contains("truncated")));
    }

    #[test]
    fn empty_prefix_allowed() {
        let mut c = Config::default();
        c.display.prefix.down = "".to_string();
        assert!(c.sanitize().is_empty());
        assert_eq!(c.display.prefix.down, "");
    }

    #[test]
    fn toml_roundtrip_preserves_values() {
        let mut c = Config::default();
        c.display.language = Language::Zh;
        c.display.dual_row = true;
        c.windows.anchor = Anchor::Left;
        c.history.csv_scope = CsvScope::Last7d;
        let text = c.to_toml_string().unwrap();
        let (back, _) = Config::from_toml_str(&text).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn missing_keys_fall_back_to_defaults() {
        // A hand-written partial config must still load with defaults filled in.
        let (c, _) = Config::from_toml_str("[display]\nlanguage = \"zh\"\n").unwrap();
        assert_eq!(c.display.language, Language::Zh);
        assert_eq!(c.display.prefix.cpu, "CPU:");
        assert_eq!(c.sample.interval_ms, 2000);
    }

    #[test]
    fn cpu_smoothing_out_of_range_clamped() {
        let mut c = Config::default();
        c.sample.cpu_smoothing = 1.5;
        let issues = c.sanitize();
        assert_eq!(c.sample.cpu_smoothing, 1.0);
        assert!(issues.iter().any(|i| i.field == "sample.cpu_smoothing"));
    }

    #[test]
    fn item_spacing_snaps_to_allowed_set() {
        // Allowed values pass through untouched.
        for v in ALLOWED_ITEM_SPACING_PX {
            let mut c = Config::default();
            c.display.item_spacing = v;
            let issues = c.sanitize();
            assert_eq!(c.display.item_spacing, v);
            assert!(issues.iter().all(|i| i.field != "display.item_spacing"));
        }
        // Out-of-set values snap to the nearest allowed entry.
        let mut low = Config::default();
        low.display.item_spacing = 1;
        let issues = low.sanitize();
        assert_eq!(low.display.item_spacing, 8);
        assert!(issues.iter().any(|i| i.field == "display.item_spacing"));

        let mut high = Config::default();
        high.display.item_spacing = 20;
        high.sanitize();
        // 20 is equidistant from 16 and 24; the tie resolves to the first
        // (smaller) allowed value.
        assert_eq!(high.display.item_spacing, 16);

        // Values above the max snap down to the max allowed entry.
        let mut above = Config::default();
        above.display.item_spacing = 100;
        above.sanitize();
        assert_eq!(above.display.item_spacing, 24);

        // Legacy space-count values from old configs snap sensibly.
        let mut legacy = Config::default();
        legacy.display.item_spacing = 2;
        legacy.sanitize();
        assert_eq!(legacy.display.item_spacing, 8);
    }

    #[test]
    fn left_margin_snaps_to_allowed_set() {
        for v in ALLOWED_LEFT_MARGIN_PX {
            let mut c = Config::default();
            c.windows.left_margin_px = v;
            let issues = c.sanitize();
            assert_eq!(c.windows.left_margin_px, v);
            assert!(issues.iter().all(|i| i.field != "windows.left_margin_px"));
        }
        // 4 (the old default) snaps to the nearest allowed value, 8.
        let mut c = Config::default();
        c.windows.left_margin_px = 4;
        let issues = c.sanitize();
        assert_eq!(c.windows.left_margin_px, 8);
        assert!(issues.iter().any(|i| i.field == "windows.left_margin_px"));

        // Large values snap down to the max allowed entry.
        let mut big = Config::default();
        big.windows.left_margin_px = 500;
        big.sanitize();
        assert_eq!(big.windows.left_margin_px, 256);
    }

    #[test]
    fn paired_show_toggles_default_on_and_route_metrics() {
        use crate::model::Metric;
        let c = Config::default();
        assert!(c.display.show_resources && c.display.show_network);
        // Group membership: CPU/MEM follow show_resources, U/D follow
        // show_network — there is no per-metric switch anymore.
        assert!(c.display.metric_visible(Metric::Cpu) && c.display.metric_visible(Metric::Mem));
        assert!(c.display.metric_visible(Metric::Up) && c.display.metric_visible(Metric::Down));
        let mut off = Config::default();
        off.display.show_resources = false;
        assert!(!off.display.metric_visible(Metric::Cpu));
        assert!(!off.display.metric_visible(Metric::Mem));
        assert!(off.display.metric_visible(Metric::Up));
    }

    fn temp_config_path(tag: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        let uniq = format!("sysvitals-cfgtest-{}-{}", tag, std::process::id());
        dir.push(uniq);
        let _ = fs::create_dir_all(&dir);
        dir.join("config.toml")
    }

    #[test]
    fn atomic_save_then_load() {
        let path = temp_config_path("save");
        let mut c = Config::default();
        c.display.prefix.cpu = "负载".to_string();
        c.save(&path).unwrap();
        let (back, _) = Config::load(&path).unwrap();
        assert_eq!(back.display.prefix.cpu, "负载");
        // No leftover temp file next to the config.
        let mut tmp = path.clone();
        tmp.set_file_name("config.toml.tmp");
        assert!(!tmp.exists());
    }

    #[test]
    fn load_missing_file_yields_defaults() {
        let path = temp_config_path("missing").with_file_name("no-such-file.toml");
        let (c, issues) = Config::load(&path).unwrap();
        assert_eq!(c, Config::default());
        assert!(issues.is_empty());
    }
}
