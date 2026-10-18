//! Memory sampling helper.
//!
//! Memory is shown as used percentage (`used / total`). `sysinfo` reports
//! `used_memory()` and `total_memory()` in bytes; the OS-specific definition
//! of "used" (Windows excludes some cached pages, macOS folds compressed/wired
//! differently) is sysinfo's concern — we just render the ratio. A zero total
//! (uninitialized handle) yields NaN so the display shows `--`, never a divide
//! by zero or a misleading 0%.

/// Used-memory percentage in `[0, 100]`, or NaN when total is not yet known.
pub fn percent_used(used: u64, total: u64) -> f64 {
    if total == 0 {
        return f64::NAN;
    }
    let pct = (used as f64 / total as f64) * 100.0;
    pct.clamp(0.0, 100.0)
}

/// Stateless wrapper kept for symmetry with CPU/NET sub-samplers and so the
/// display layer can hold one sampler object per metric if it wants.
#[derive(Debug, Clone, Default)]
pub struct MemSampler;

impl MemSampler {
    pub fn new() -> Self {
        MemSampler
    }

    pub fn percent(&self, used: u64, total: u64) -> f64 {
        percent_used(used, total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_ratio() {
        assert_eq!(percent_used(50, 100), 50.0);
        assert_eq!(percent_used(0, 100), 0.0);
        assert_eq!(percent_used(100, 100), 100.0);
    }

    #[test]
    fn zero_total_is_nan_not_divide_by_zero() {
        assert!(percent_used(0, 0).is_nan());
        assert!(percent_used(10, 0).is_nan());
    }

    #[test]
    fn overcommitted_clamps() {
        // Guard against a driver reporting used > total.
        assert_eq!(percent_used(120, 100), 100.0);
    }

    #[test]
    fn realistic_gigabyte_values() {
        let gb: u64 = 1024 * 1024 * 1024;
        assert!((percent_used(6 * gb, 8 * gb) - 75.0).abs() < 1e-9);
    }
}
