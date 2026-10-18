//! CPU sampling helper.
//!
//! `sysinfo`'s `System::cpu_usage()` already returns a time-normalized whole
//! machine percentage (it diffs busy/idle internally across refreshes), so this
//! layer does not recompute a rate — it only dampens frame-to-frame jitter with
//! exponential smoothing and clamps to a sane range. The first real value is
//! passed through un-smoothed to avoid the classic EMA pull toward zero.

use std::time::Duration;

/// Exponential moving average over CPU percent, with warm-up handling.
#[derive(Debug, Clone)]
pub struct CpuSampler {
    /// Smoothing factor in `[0, 1]`. `0` disables smoothing (pass-through);
    /// larger values follow the raw signal more closely.
    alpha: f64,
    last: Option<f64>,
}

impl Default for CpuSampler {
    fn default() -> Self {
        CpuSampler {
            alpha: 0.3,
            last: None,
        }
    }
}

impl CpuSampler {
    pub fn new(alpha: f64) -> Self {
        CpuSampler {
            alpha: alpha.clamp(0.0, 1.0),
            last: None,
        }
    }

    pub fn set_smoothing(&mut self, alpha: f64) {
        self.alpha = alpha.clamp(0.0, 1.0);
    }

    pub fn reset(&mut self) {
        self.last = None;
    }

    /// Fold a raw sysinfo percentage into the smoothed value.
    ///
    /// `elapsed` is `Some` for a normal tick and `None` for the very first
    /// tick of the process. On the first tick we return the raw value and
    /// prime the state; smoothing only starts from the second observation.
    ///
    /// When `elapsed` is `Some(d)` but `d` is far larger than the nominal tick
    /// (e.g. the machine slept and the counter window is meaningless), callers
    /// should [`reset`] first — but we still avoid NaN output by clamping.
    pub fn update(&mut self, raw: f64, _elapsed: Option<Duration>) -> f64 {
        let raw = raw.clamp(0.0, 100.0);
        match self.last {
            // No previous sample: pass through and prime.
            None => {
                self.last = Some(raw);
                raw
            }
            Some(prev) => {
                // elapsed==None can't happen after priming, but treat a fresh
                // prime the same as pass-through defensively.
                let smoothed = if self.alpha >= 1.0 {
                    raw
                } else {
                    self.alpha * raw + (1.0 - self.alpha) * prev
                };
                self.last = Some(smoothed);
                smoothed
            }
        }
    }

    /// Current smoothed value, or NaN if nothing sampled yet.
    pub fn current(&self) -> f64 {
        self.last.unwrap_or(f64::NAN)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_value_passes_through_unsmoothed() {
        let mut s = CpuSampler::new(0.3);
        assert_eq!(s.update(50.0, None), 50.0);
        assert_eq!(s.current(), 50.0);
    }

    #[test]
    fn smoothing_moves_toward_new_value_by_alpha() {
        let mut s = CpuSampler::new(0.3);
        s.update(50.0, None); // prime at 50
                              // second: 0.3*100 + 0.7*50 = 30 + 35 = 65
        let v = s.update(100.0, Some(Duration::from_millis(2000)));
        assert!((v - 65.0).abs() < 1e-9, "got {v}");
    }

    #[test]
    fn alpha_zero_is_pass_through_of_prev() {
        let mut s = CpuSampler::new(0.0);
        s.update(50.0, None);
        // alpha 0 => smoothed == prev always
        let v = s.update(100.0, Some(Duration::from_millis(2000)));
        assert_eq!(v, 50.0);
    }

    #[test]
    fn alpha_one_tracks_raw() {
        let mut s = CpuSampler::new(1.0);
        s.update(50.0, None);
        let v = s.update(100.0, Some(Duration::from_millis(2000)));
        assert_eq!(v, 100.0);
    }

    #[test]
    fn raw_is_clamped_to_percent_range() {
        let mut s = CpuSampler::new(1.0);
        assert_eq!(s.update(140.0, None), 100.0);
        assert_eq!(s.update(-10.0, Some(Duration::from_millis(1000))), 0.0);
    }

    #[test]
    fn current_is_nan_before_first_sample() {
        let s = CpuSampler::new(0.3);
        assert!(s.current().is_nan());
    }

    #[test]
    fn reset_reprimes() {
        let mut s = CpuSampler::new(0.3);
        s.update(50.0, None);
        s.update(100.0, Some(Duration::from_millis(1000)));
        s.reset();
        assert!(s.current().is_nan());
        // after reset, next value passes through raw again
        assert_eq!(s.update(77.0, Some(Duration::from_millis(1000))), 77.0);
    }
}
