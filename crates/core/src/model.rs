//! Core data model: the value types exchanged between sampler, format and the
//! display/UI layers.

use serde::{Deserialize, Serialize};

/// The four indicators the status bar can show.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Metric {
    Cpu,
    Mem,
    Up,
    Down,
}

impl Metric {
    pub const ALL: [Metric; 4] = [Metric::Cpu, Metric::Mem, Metric::Up, Metric::Down];

    /// i18n key for this metric's default label (not the user prefix).
    pub fn i18n_key(self) -> &'static str {
        match self {
            Metric::Cpu => "metric.cpu",
            Metric::Mem => "metric.mem",
            Metric::Up => "metric.up",
            Metric::Down => "metric.down",
        }
    }
}

/// Taskbar anchor position (Windows only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Anchor {
    /// Just left of the system tray (default, available on Win10 and Win11).
    #[default]
    Right,
    /// Far left of the taskbar. Only honored on Win11 with centered icons;
    /// silently falls back to [`Anchor::Right`] otherwise.
    Left,
}

/// Theme mode. `Auto` follows the OS light/dark setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    #[default]
    Auto,
    Light,
    Dark,
}

/// A single sampled reading. The unit of both status-bar display and history
/// storage. Network fields are instantaneous bytes-per-second rates.
///
/// `cpu_pct`/`mem_pct` are `NaN` when that metric was unavailable/suppressed
/// for this tick; `up_bps`/`down_bps` are `NaN` on the first tick or after a
/// baseline reset (adapter change / sleep-wake), signaling "no rate yet".
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Snapshot {
    /// Unix timestamp (seconds) of the sample, local wall clock.
    pub ts: i64,
    pub cpu_pct: f64,
    pub mem_pct: f64,
    pub up_bps: f64,
    pub down_bps: f64,
}

impl Snapshot {
    pub const NO_DATA: f64 = f64::NAN;

    pub fn value(self, metric: Metric) -> f64 {
        match metric {
            Metric::Cpu => self.cpu_pct,
            Metric::Mem => self.mem_pct,
            Metric::Up => self.up_bps,
            Metric::Down => self.down_bps,
        }
    }

    pub fn is_valid(value: f64) -> bool {
        !value.is_nan()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_value_routes_by_metric() {
        let s = Snapshot {
            ts: 0,
            cpu_pct: 12.0,
            mem_pct: 34.0,
            up_bps: 5.0,
            down_bps: 6.0,
        };
        assert_eq!(s.value(Metric::Cpu), 12.0);
        assert_eq!(s.value(Metric::Mem), 34.0);
        assert_eq!(s.value(Metric::Up), 5.0);
        assert_eq!(s.value(Metric::Down), 6.0);
    }

    #[test]
    fn no_data_is_nan_and_invalid() {
        assert!(!Snapshot::is_valid(Snapshot::NO_DATA));
        assert!(Snapshot::is_valid(0.0));
    }

    #[test]
    fn anchor_and_theme_defaults() {
        assert_eq!(Anchor::default(), Anchor::Right);
        assert_eq!(Theme::default(), Theme::Auto);
    }

    #[test]
    fn serde_roundtrip_lowercase() {
        let a: Anchor = serde_json_free(r#" "left" "#);
        assert_eq!(a, Anchor::Left);
        let t: Theme = serde_json_free(r#" "dark" "#);
        assert_eq!(t, Theme::Dark);
        let m: Metric = serde_json_free(r#" "up" "#);
        assert_eq!(m, Metric::Up);
    }

    // Tiny helper: parse a single serde value from TOML-compatible JSON-ish
    // text without pulling in a JSON crate.
    fn serde_json_free<T: serde::de::DeserializeOwned>(s: &str) -> T {
        let toml_str = format!("v = {}", s.trim());
        let de: Wrap<T> = toml::from_str(&toml_str).unwrap();
        de.v
    }

    #[derive(Deserialize)]
    struct Wrap<T> {
        v: T,
    }
}
