//! Terminal harness: run the pure core sampling+format chain in a
//! terminal loop to prove it works end-to-end with real data, on any host.
//!
//! `cargo run -p sysvitals-core --example monitor`
//!
//! It samples every 2 s (default), renders the status-bar frame exactly as
//! a display layer would, and prints it. After a couple of ticks the network
//! rates become real (the first tick only seeds baselines). This is the headless
//! stand-in for "run the app" until the Windows/macOS display layers exist.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sysvitals_core::{format_frame, Config, NetSelection, Sampler};

fn main() {
    let mut cfg = Config::default();
    cfg.sanitize(); // exercise validation; defaults are already valid

    let mut sampler = Sampler::new(NetSelection::Auto, cfg.sample.cpu_smoothing);
    let tick = Duration::from_millis(cfg.sample.interval_ms);

    println!("SysVitals core monitor (Ctrl-C to stop) — single row, then dual row\n");

    loop {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let snap = sampler.sample(now);

        // Single-row rendering.
        let single = format_frame(&snap, &cfg);
        println!("[single] {}", single.lines[0].render());

        // Dual-row rendering (2x2 quadrant).
        cfg.display.dual_row = true;
        let dual = format_frame(&snap, &cfg);
        for line in &dual.lines {
            println!("[dual  ] {}", line.render());
        }
        cfg.display.dual_row = false;

        println!(
            "raw: cpu={:.1} mem={:.1} up={:.0}B/s down={:.0}B/s\n",
            snap.cpu_pct, snap.mem_pct, snap.up_bps, snap.down_bps
        );

        std::thread::sleep(tick);
    }
}
