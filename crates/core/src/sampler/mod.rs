//! Sampling layer.
//!
//! `sysinfo` is the ONLY place in the whole crate tree that touches a system
//! API. Everything else consumes the pure [`Snapshot`] it produces, so each
//! metric can later be swapped for a native call (`GetSystemTimes`,
//! `GetIfTable2`, `host_statistics`, ...) without changing any caller.
//!
//! Correctness rules baked in here, each unit-testable via a
//! fake counter feed (see `net` tests — they do not need a real NIC):
//! - Rates are differentials of monotonically increasing cumulative byte
//!   counters. A counter below its baseline (wrap-around or driver reset)
//!   yields "no data" (NaN) for that frame rather than a negative rate.
//! - Interface set changes (add/remove/unplug) rebuild the baseline instead
//!   of panicking.
//! - A tick gap far larger than any configured interval means the machine
//!   slept or the process was stalled: the frame is dropped as stale (CPU
//!   re-primes from raw, network reports NaN) so no sleep-spanning rate
//!   reaches the display or the history ([`STALE_GAP_SECS`]).
//! - Only selected physical NICs are aggregated; loopback / VPN / virtual
//!   adapters are excluded by default, with a user override to pin a list.
//! - CPU percent is exponentially smoothed with the `sample.cpu_smoothing`
//!   history weight (0 disables) and clamped.

mod cpu;
mod mem;
mod net;

pub use cpu::CpuSampler;
pub use net::{NetSampler, NetSelection};

pub use cpu::STALE_GAP_SECS;

use std::time::Instant;

use sysinfo::{CpuRefreshKind, MemoryRefreshKind, Networks, RefreshKind, System};

use crate::model::Snapshot;

/// Owns the `sysinfo` handles and the inter-frame state needed to turn raw
/// cumulative counters into instantaneous rates. Construct once, call
/// [`Sampler::sample`] on every tick.
pub struct Sampler {
    system: System,
    networks: Networks,
    net: NetSampler,
    cpu: CpuSampler,
    /// Monotonic clock for real elapsed-time deltas, immune to wall-clock
    /// jumps (DST/NTP/sleep). The divisor for every differential is this, not
    /// the nominal interval, so a late tick still yields an accurate rate.
    last_tick: Option<Instant>,
}

impl Sampler {
    /// Build a sampler and seed the first cumulative-counter / memory
    /// baselines. `cpu_smoothing` is the `sample.cpu_smoothing` history
    /// weight (0 disables smoothing); it is fixed for the process lifetime —
    /// the config poller treats it as a startup-only debug setting.
    pub fn new(interfaces: NetSelection, cpu_smoothing: f64) -> Self {
        let system = System::new_with_specifics(
            RefreshKind::nothing()
                .with_cpu(CpuRefreshKind::everything())
                .with_memory(MemoryRefreshKind::nothing().with_ram()),
        );
        let networks = Networks::new_with_refreshed_list();
        Sampler {
            system,
            networks,
            net: NetSampler::new(interfaces),
            cpu: CpuSampler::new(cpu_smoothing),
            last_tick: None,
        }
    }

    /// Take one reading. The first call returns CPU/MEM with NaN network rates
    /// (it only seeds baselines); from the second call on, network rates are
    /// real. `now_unix` is the local wall-clock timestamp to stamp the sample.
    pub fn sample(&mut self, now_unix: i64) -> Snapshot {
        self.system.refresh_cpu_usage();
        self.system.refresh_memory();
        // `false` keeps interfaces that momentarily disappear from the list so
        // a transient unplug doesn't destroy the baseline; reconciliation lives
        // in NetSampler.
        self.networks.refresh(false);

        let now = Instant::now();
        // On the very first tick there is no prior tick; elapsed is 0, and the
        // sub-samplers treat a 0 gap as "no rate yet".
        let elapsed = self.last_tick.map(|prev| now.duration_since(prev));
        self.last_tick = Some(now);

        // Whole-machine CPU percent (already time-normalized by sysinfo across
        // refreshes). In sysinfo 0.39 this is `global_cpu_usage()`.
        let cpu_raw = self.system.global_cpu_usage() as f64;
        let cpu_pct = self.cpu.update(cpu_raw, elapsed);
        let mem_pct = mem::percent_used(self.system.used_memory(), self.system.total_memory());
        let (up, down) = self.net.poll(&self.networks, elapsed);

        Snapshot {
            ts: now_unix,
            cpu_pct,
            mem_pct,
            up_bps: up,
            down_bps: down,
        }
    }
}
