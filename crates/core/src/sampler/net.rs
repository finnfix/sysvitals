//! Network sampling helper.
//!
//! The tricky, bug-prone part of a taskbar monitor lives here, so it is split
//! into two pieces:
//!
//! 1. [`compute_rates`] is pure: it takes the previous baseline plus a vector
//!    of `(name, rx_total, tx_total)` cumulative counters and an elapsed time,
//!    and returns bytes/second. It encodes wrap-around handling, adapter
//!    add/remove reconciliation and the divide-by-zero/first-frame rules. It is
//!    unit-tested against synthetic counter feeds — no real NIC required.
//! 2. [`NetSampler::poll`] adapts `sysinfo::Networks` (real cumulative
//!    counters + operational state) into that `(name, rx, tx)` vector after
//!    applying interface selection, then calls `compute_rates`.
//!
//! We use `total_received`/`total_transmitted` (monotonic counters) rather than
//! sysinfo's own per-refresh diff so the differential policy is entirely ours
//! and is testable in isolation: sysinfo exposes cumulative counters, so
//! the differential policy lives entirely here.

use std::collections::HashMap;
use std::time::Duration;

use sysinfo::{InterfaceOperationalState, Networks};

/// NIC selection policy.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum NetSelection {
    /// Auto-aggregate physical adapters, excluding loopback/VPN/virtual by
    /// name heuristic and requiring the interface be operationally Up.
    #[default]
    Auto,
    /// Aggregate exactly these interface names (user override in settings).
    Interfaces(Vec<String>),
}

impl NetSelection {
    /// Whether an interface name is a candidate under this policy, ignoring
    /// link state. Auto mode rejects loopback/VPN/virtual by name; explicit
    /// mode requires an exact name match. (`collect_frame` additionally demands
    /// the interface be operationally Up in Auto mode.)
    pub fn matches(&self, name: &str) -> bool {
        match self {
            NetSelection::Auto => is_physical_name(name),
            NetSelection::Interfaces(list) => list.iter().any(|n| n == name),
        }
    }
}

/// Name prefixes that are never physical throughput adapters (loopback,
/// containers, tunnels, virtual bridges). Matched case-insensitively.
const EXCLUDED_PREFIXES: &[&str] = &[
    "lo",
    "loopback",
    "veth",
    "docker",
    "br-",
    "bridge",
    "virbr",
    "tap",
    "tun",
    "utun",
    "zt",   // zerotier
    "wg",   // wireguard
    "awdl", // macOS
    "llnw", // macOS
    "anpi", // macOS
    "stun", // macOS
    "gif",  // BSD tunnel
    "stf",
    "isatap",
    "teredo",
    "vethernet", // Windows Hyper-V virtual
    "vEthernet",
    "hapas", // Wi-Fi Direct
    "wsl",
];

/// Decide whether an interface name represents a physical-ish throughput
/// adapter in auto mode. Empty names and obviously virtual ones are rejected.
pub fn is_physical_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    if lower.is_empty() {
        return false;
    }
    // Any excluded prefix at the start wins.
    !EXCLUDED_PREFIXES.iter().any(|p| lower.starts_with(p))
}

/// One interface's cumulative counters in a frame: `(name, rx_total, tx_total)`.
pub type Counters = (String, u64, u64);

/// An owned counter frame (what `collect_frame` produces from real sysinfo).
/// Kept as plain data so tests can build it directly.
pub type CounterFrame = Vec<Counters>;

/// Diff a new counter frame against `baseline`, updating `baseline` in place.
///
/// Rules:
/// - `elapsed_secs <= 0` → `(NaN, NaN)` (no time to measure), but the baseline
///   is still refreshed so the next tick is correct.
/// - A gap larger than [`STALE_GAP_SECS`] (sleep / stalled process) → same
///   `(NaN, NaN)` drop: averaging bytes accumulated across a multi-minute
///   freeze into one instantaneous rate would be a lie; the baseline re-seeds
///   so the next normal tick is accurate.
/// - First frame (empty baseline) → `(NaN, NaN)`: seed the baseline only.
/// - Per interface: a counter that went *down* (wrap or driver reset) makes
///   that interface contribute nothing this frame and re-baselines it, instead
///   of producing a negative or absurd spike.
/// - Interfaces newly present contribute nothing (no prior to diff) but enter
///   the baseline for next time.
/// - Interfaces that disappeared (unplug / VPN off) are dropped from baseline.
/// - Final rate = summed byte deltas / `elapsed_secs`, never negative.
pub fn compute_rates(
    baseline: &mut HashMap<String, (u64, u64)>,
    frame: &[Counters],
    elapsed_secs: f64,
) -> (f64, f64) {
    // Rebuild the baseline map from the current frame each call; first we
    // compute deltas against the *previous* baseline.
    let mut rx_delta: u128 = 0;
    let mut tx_delta: u128 = 0;
    let mut valid_any = false;

    for (name, rx, tx) in frame {
        match baseline.get(name) {
            // Wrap/reset protection: only accumulate if monotonic.
            Some((prev_rx, prev_tx)) if *rx >= *prev_rx && *tx >= *prev_tx => {
                rx_delta += (*rx - *prev_rx) as u128;
                tx_delta += (*tx - *prev_tx) as u128;
                valid_any = true;
            }
            // Newly seen interface (no prior to diff) OR one whose counter
            // wrapped/reset this frame: contributes nothing, but is re-baselined
            // below.
            Some(_) | None => {}
        }
    }

    // Refresh baseline to the current frame.
    baseline.clear();
    for (name, rx, tx) in frame {
        baseline.insert(name.clone(), (*rx, *tx));
    }

    if elapsed_secs <= 0.0 || elapsed_secs > super::cpu::STALE_GAP_SECS || !valid_any {
        // No usable interval, a stale sleep-spanning gap, or no interface had
        // a valid diff (all new, or all wrapped, or the very first frame) →
        // report "no data".
        return (f64::NAN, f64::NAN);
    }

    let up = tx_delta as f64 / elapsed_secs;
    let down = rx_delta as f64 / elapsed_secs;
    (up.max(0.0), down.max(0.0))
}

/// Stateful NIC sampler wrapping [`compute_rates`] plus selection.
pub struct NetSampler {
    selection: NetSelection,
    baseline: HashMap<String, (u64, u64)>,
}

impl NetSampler {
    pub fn new(selection: NetSelection) -> Self {
        NetSampler {
            selection,
            baseline: HashMap::new(),
        }
    }

    pub fn reset(&mut self) {
        self.baseline.clear();
    }

    /// Read `sysinfo` cumulative counters, apply selection, and diff against
    /// the internal baseline using the measured `elapsed`. Returns `(up_bps,
    /// down_bps)` (NaN on the first frame / after a reset).
    pub fn poll(&mut self, networks: &Networks, elapsed: Option<Duration>) -> (f64, f64) {
        let elapsed_secs = match elapsed {
            Some(d) => d.as_secs_f64(),
            None => 0.0,
        };

        let frame = collect_frame(networks, &self.selection);
        compute_rates(&mut self.baseline, &frame, elapsed_secs)
    }
}

/// Extract a filtered counter frame from a `sysinfo::Networks` handle.
/// In Auto mode an interface must be operationally Up and pass the name
/// heuristic. Split out so a future native backend can reuse the same shape.
fn collect_frame(networks: &Networks, selection: &NetSelection) -> CounterFrame {
    networks
        .list()
        .iter()
        .filter(|(name, data)| {
            // Auto mode also requires the link to be up; an explicit user
            // choice is honored even if momentarily reported otherwise.
            let link_ok = match selection {
                NetSelection::Auto => data.operational_state() == InterfaceOperationalState::Up,
                NetSelection::Interfaces(_) => true,
            };
            link_ok && selection.matches(name)
        })
        .map(|(name, data)| {
            (
                name.clone(),
                data.total_received(),
                data.total_transmitted(),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(ms: u64) -> f64 {
        ms as f64 / 1000.0
    }

    #[test]
    fn first_frame_seeds_baseline_and_reports_nan() {
        let mut base = HashMap::new();
        let frame = vec![("eth0".to_string(), 1000u64, 500u64)];
        let (up, down) = compute_rates(&mut base, &frame, secs(2000));
        assert!(up.is_nan() && down.is_nan());
        // baseline now seeded
        assert_eq!(base.get("eth0"), Some(&(1000, 500)));
    }

    #[test]
    fn steady_increment_produces_expected_bps() {
        let mut base = HashMap::new();
        // seed
        compute_rates(&mut base, &[("eth0".to_string(), 0, 0)], secs(2000));
        // +2048 down, +1024 up over 2s => 1024 B/s down, 512 B/s up
        let (up, down) = compute_rates(&mut base, &[("eth0".to_string(), 2048, 1024)], secs(2000));
        assert_eq!(up, 512.0);
        assert_eq!(down, 1024.0);
    }

    #[test]
    fn aggregates_across_multiple_interfaces() {
        let mut base = HashMap::new();
        compute_rates(
            &mut base,
            &[("eth0".to_string(), 0, 0), ("wlan0".to_string(), 0, 0)],
            secs(1000),
        );
        // eth0 +1000 down, wlan0 +500 down => 1500 total over 1s
        let (_, down) = compute_rates(
            &mut base,
            &[("eth0".to_string(), 1000, 0), ("wlan0".to_string(), 500, 0)],
            secs(1000),
        );
        assert_eq!(down, 1500.0);
    }

    #[test]
    fn counter_wrap_produces_no_negative_and_rebaselines() {
        let mut base = HashMap::new();
        compute_rates(&mut base, &[("eth0".to_string(), 5000, 5000)], secs(1000));
        // counters dropped to 100 (reset): contribution discarded, NaN because
        // no interface had a valid diff this frame.
        let (up, down) = compute_rates(&mut base, &[("eth0".to_string(), 100, 100)], secs(1000));
        assert!(up.is_nan() && down.is_nan());
        // baseline re-primed to the new lower value; next increment is valid.
        assert_eq!(base.get("eth0"), Some(&(100, 100)));
        let (up, down) = compute_rates(
            &mut base,
            &[("eth0".to_string(), 100 + 200, 100)],
            secs(1000),
        );
        assert_eq!(down, 200.0);
        assert_eq!(up, 0.0);
    }

    #[test]
    fn wrap_on_one_interface_does_not_poison_others() {
        let mut base = HashMap::new();
        compute_rates(
            &mut base,
            &[
                ("eth0".to_string(), 1000, 1000),
                ("wlan0".to_string(), 0, 0),
            ],
            secs(1000),
        );
        // eth0 wraps (1000 -> 10): skipped; wlan0 +700 valid. Only wlan0 counts.
        let (_, down) = compute_rates(
            &mut base,
            &[("eth0".to_string(), 10, 5), ("wlan0".to_string(), 700, 0)],
            secs(1000),
        );
        assert_eq!(down, 700.0);
    }

    #[test]
    fn removed_interface_drops_from_baseline() {
        let mut base = HashMap::new();
        compute_rates(
            &mut base,
            &[("eth0".to_string(), 0, 0), ("vpn0".to_string(), 0, 0)],
            secs(1000),
        );
        assert_eq!(base.len(), 2);
        // vpn0 unplugged: frame has only eth0.
        compute_rates(&mut base, &[("eth0".to_string(), 500, 500)], secs(1000));
        assert_eq!(base.len(), 1);
        assert!(base.contains_key("eth0"));
        assert!(!base.contains_key("vpn0"));
    }

    #[test]
    fn new_interface_contributes_zero_first_frame_it_appears() {
        let mut base = HashMap::new();
        compute_rates(&mut base, &[("eth0".to_string(), 100, 100)], secs(1000));
        // wlan0 just appeared with nonzero counters: no prior => not counted.
        let (up, down) = compute_rates(
            &mut base,
            &[
                ("eth0".to_string(), 200, 200),
                ("wlan0".to_string(), 99999, 99999),
            ],
            secs(1000),
        );
        // only eth0's +100 counts.
        assert_eq!(down, 100.0);
        assert_eq!(up, 100.0);
    }

    #[test]
    fn zero_elapsed_is_nan() {
        let mut base = HashMap::new();
        compute_rates(&mut base, &[("eth0".to_string(), 0, 0)], secs(1000));
        let (up, down) = compute_rates(&mut base, &[("eth0".to_string(), 1000, 1000)], 0.0);
        assert!(up.is_nan() && down.is_nan());
    }

    #[test]
    fn stale_gap_is_dropped_as_no_data() {
        // A gap beyond STALE_GAP_SECS (the machine slept) must not divide the
        // bytes accumulated across the freeze into a bogus instantaneous rate.
        let mut base = HashMap::new();
        compute_rates(&mut base, &[("eth0".to_string(), 0, 0)], secs(1000));
        let stale = super::super::cpu::STALE_GAP_SECS + 30.0;
        let (up, down) = compute_rates(&mut base, &[("eth0".to_string(), 100000, 100000)], stale);
        assert!(up.is_nan() && down.is_nan(), "stale gap → no data");
        // Baseline still re-seeded so the next normal tick is correct.
        assert_eq!(base.get("eth0"), Some(&(100000, 100000)));
        let (up, down) = compute_rates(
            &mut base,
            &[("eth0".to_string(), 102000, 100000)],
            secs(2000),
        );
        assert_eq!(down, 1000.0);
        assert!(up.is_nan() || up == 0.0);
    }

    #[test]
    fn physical_name_heuristic() {
        assert!(is_physical_name("eth0"));
        assert!(is_physical_name("wlan0"));
        assert!(is_physical_name("en0"));
        assert!(is_physical_name("Wi-Fi"));
        assert!(!is_physical_name("lo"));
        assert!(!is_physical_name("Loopback"));
        assert!(!is_physical_name("docker0"));
        assert!(!is_physical_name("veth1234"));
        assert!(!is_physical_name("br-abc"));
        assert!(!is_physical_name("utun4"));
        assert!(!is_physical_name("wg0"));
        assert!(!is_physical_name("vEthernet (WSL)"));
        assert!(!is_physical_name(""));
    }

    #[test]
    fn selection_only_mode_filters_names() {
        let sel = NetSelection::Interfaces(vec!["en0".to_string()]);
        assert!(sel.matches("en0"));
        assert!(!sel.matches("en1"));
        // An explicit choice matches even a virtual-looking name (user intent).
        let sel2 = NetSelection::Interfaces(vec!["lo".to_string()]);
        assert!(sel2.matches("lo"));
        // Auto mode rejects virtual names via the heuristic.
        assert!(NetSelection::Auto.matches("eth0"));
        assert!(!NetSelection::Auto.matches("lo"));
    }
}
