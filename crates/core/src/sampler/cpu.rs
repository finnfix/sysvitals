//! CPU sampling helper.
//!
//! `sysinfo`'s `System::cpu_usage()` already returns a time-normalized whole
//! machine percentage (it diffs busy/idle internally across refreshes), so this
//! layer does not recompute a rate — it only dampens frame-to-frame jitter with
//! exponential smoothing and clamps to a sane range. The first real value is
//! passed through un-smoothed to avoid the classic EMA pull toward zero.
//!
//! Smoothing semantics: `alpha` is the weight of history, matching the
//! `sample.cpu_smoothing` config field ("exponential smoothing factor; 0
//! disables smoothing"). `alpha = 0` therefore tracks the raw signal exactly;
//! larger values follow the raw signal *less* closely.

use std::time::Duration;

/// A tick gap larger than this many seconds means the machine slept or the
/// process was stalled (the fastest sample interval is 3 s, so a real tick
/// never approaches this). The frame spanning such a gap is stale: CPU
/// re-primes from the raw signal instead of blending across the gap, and
/// network reports NaN rather than a sleep-spanning rate.
pub const STALE_GAP_SECS: f64 = 60.0;

/// Exponential moving average over CPU percent, with warm-up handling.
#[derive(Debug, Clone)]
pub struct CpuSampler {
    /// History weight in `[0, 1)`. `0` disables smoothing (pass-through).
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

    pub fn reset(&mut self) {
        self.last = None;
    }

    /// Fold a raw sysinfo percentage into the smoothed value.
    ///
    /// `elapsed` is the gap since the previous tick, or `None` on the very
    /// first tick. A gap far larger than any configured interval means the
    /// machine slept (or the process was stalled): the previous smoothed value
    /// is stale, so we re-prime on the raw signal instead of blending across
    /// the sleep. Otherwise `None`/first tick passes the raw value through to
    /// avoid the classic EMA pull toward zero.
    pub fn update(&mut self, raw: f64, elapsed: Option<Duration>) -> f64 {
        let raw = raw.clamp(0.0, 100.0);
        // Wake/stall guard: a gap beyond the stale threshold invalidates the
        // whole smoothing history, not just one frame.
        if elapsed.map(|d| d.as_secs_f64() > STALE_GAP_SECS) == Some(true) {
            self.last = Some(raw);
            return raw;
        }
        match self.last {
            // No previous sample: pass through and prime.
            None => {
                self.last = Some(raw);
                raw
            }
            Some(prev) => {
                // Standard EMA with `alpha` as the history weight. The explicit
                // zero branch keeps a pass-through exact (no float drift) and
                // documents the "smoothing disabled" contract.
                let smoothed = if self.alpha <= 0.0 {
                    raw
                } else {
                    (1.0 - self.alpha) * raw + self.alpha * prev
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
    fn smoothing_weights_history_by_alpha() {
        let mut s = CpuSampler::new(0.3);
        s.update(50.0, None); // prime at 50
                              // second: 0.7*raw + 0.3*prev = 70 + 15 = 85
        let v = s.update(100.0, Some(Duration::from_millis(2000)));
        assert!((v - 85.0).abs() < 1e-9, "got {v}");
    }

    #[test]
    fn alpha_zero_disables_smoothing_and_tracks_raw() {
        // The documented meaning of `cpu_smoothing = 0`: no smoothing, so the
        // displayed value is the raw reading (not frozen at the previous one).
        let mut s = CpuSampler::new(0.0);
        s.update(50.0, None);
        let v = s.update(100.0, Some(Duration::from_millis(2000)));
        assert_eq!(v, 100.0);
    }

    #[test]
    fn alpha_one_freezes_at_previous() {
        // alpha = 1 is full history weight (maximum smoothing): the value never
        // moves off the primed sample.
        let mut s = CpuSampler::new(1.0);
        s.update(50.0, None);
        let v = s.update(100.0, Some(Duration::from_millis(2000)));
        assert_eq!(v, 50.0);
    }

    #[test]
    fn raw_is_clamped_to_percent_range() {
        // alpha 0 = pass-through, so the clamp on the raw value is directly
        // observable.
        let mut s = CpuSampler::new(0.0);
        assert_eq!(s.update(140.0, None), 100.0);
        assert_eq!(s.update(-10.0, Some(Duration::from_millis(1000))), 0.0);
    }

    #[test]
    fn stale_gap_reprimes_instead_of_blending() {
        // A gap past STALE_GAP_SECS (sleep/stall) must drop the stale history:
        // the raw value passes through untouched rather than blending toward
        // the pre-sleep sample.
        let mut s = CpuSampler::new(0.3);
        s.update(50.0, None);
        let big = Duration::from_secs_f64(STALE_GAP_SECS + 10.0);
        let v = s.update(100.0, Some(big));
        assert_eq!(v, 100.0, "stale gap must re-prime to raw, not blend");
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
