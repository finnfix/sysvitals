//! macOS display layer: NSStatusItem menu-bar indicator.
//!
//! Shows CPU / memory / network stats in the macOS menu bar using
//! NSStatusItem with the system menu-bar font. Samples periodically via
//! sysvitals-core, formats with the shared format engine, and updates
//! the status item title.
//!
//! Architecture: pure objc2 + AppKit (no GPUI for the indicator itself).
//! The GPUI settings window (app-settings) is launched as a separate
//! process, same as on Windows.

#[cfg(target_os = "macos")]
mod macos {
    use std::cell::RefCell;
    use std::time::{SystemTime, UNIX_EPOCH};

    use objc2::rc::Retained;
    use objc2::sel;
    use objc2_app_kit::{
        NSApplication, NSApplicationActivationPolicy, NSMenu, NSMenuItem, NSStatusBar,
        NSStatusItem, NSVariableStatusItemLength,
    };
    use objc2_foundation::{NSString, NSTimer};

    use sysvitals_core::config::config_path;
    use sysvitals_core::format::format_frame;
    use sysvitals_core::{Config, NetSelection, Sampler};

    thread_local! {
        static SAMPLER: RefCell<Option<Sampler>> = const { RefCell::new(None) };
        static CONFIG: RefCell<Config> = RefCell::new(Config::default());
        static STATUS_ITEM: RefCell<Option<Retained<NSStatusItem>>> = const { RefCell::new(None) };
        static CONFIG_MTIME: RefCell<i64> = const { RefCell::new(0) };
    }

    /// Load or reload the config file, updating the cached mtime.
    fn reload_config_if_changed() {
        if let Ok(path) = config_path() {
            let mtime = std::fs::metadata(&path)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);

            let prev = CONFIG_MTIME.with(|m| *m.borrow());
            if mtime != prev {
                CONFIG_MTIME.with(|m| *m.borrow_mut() = mtime);
                if let Ok((cfg, _)) = Config::load(&path) {
                    CONFIG.with(|c| *c.borrow_mut() = cfg);
                }
            }
        }
    }

    /// Sample metrics and update the status item title.
    fn tick() {
        reload_config_if_changed();

        let cfg = CONFIG.with(|c| c.borrow().clone());
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        let snapshot = SAMPLER.with(|s| {
            let mut guard = s.borrow_mut();
            let sampler = guard.get_or_insert_with(|| Sampler::new(NetSelection::Auto));
            sampler.sample(now)
        });

        let row = format_frame(&snapshot, &cfg);

        // Build the display text from the formatted row.
        let text = if row.items.is_empty() {
            // Dual-row or fallback: join lines.
            row.lines
                .iter()
                .map(|l| {
                    if l.right.is_empty() {
                        l.left.clone()
                    } else {
                        format!("{}  {}", l.left, l.right)
                    }
                })
                .collect::<Vec<_>>()
                .join("  ")
        } else {
            row.items.join("  ")
        };

        let title = NSString::from_str(&text);
        STATUS_ITEM.with(|si| {
            if let Some(item) = si.borrow().as_ref() {
                if let Some(button) = item.button() {
                    button.setTitle(&title);
                }
            }
        });
    }

    pub fn run() {
        // NSApplication setup — must happen on the main thread.
        let app = NSApplication::sharedApplication();
        app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

        // Create the status item with variable width.
        let status_bar = NSStatusBar::systemStatusBar();
        let status_item = unsafe { status_bar.statusItemWithLength(NSVariableStatusItemLength) };

        // Set initial title so something shows immediately.
        if let Some(button) = status_item.button() {
            let title = NSString::from_str("SysVitals");
            button.setTitle(&title);
        }

        // Build right-click menu.
        let menu = NSMenu::new();
        let quit_title = NSString::from_str("Quit SysVitals");
        let quit_item = NSMenuItem::new();
        quit_item.setTitle(&quit_title);
        quit_item.setAction(Some(sel!(terminate:)));
        menu.addItem(&quit_item);
        status_item.setMenu(Some(&menu));

        // Store the status item for the tick callback.
        STATUS_ITEM.with(|si| *si.borrow_mut() = Some(status_item));

        // Load initial config.
        if let Ok(path) = config_path() {
            if let Ok((cfg, _)) = Config::load(&path) {
                CONFIG.with(|c| *c.borrow_mut() = cfg);
            }
        }

        // Initial tick.
        tick();

        // Schedule a repeating timer for periodic sampling.
        let interval_ms = CONFIG.with(|c| c.borrow().sample.interval_ms);
        let interval_secs = interval_ms as f64 / 1000.0;

        let block = block2::RcBlock::new(|_timer: &NSTimer| {
            tick();
        });
        let _timer =
            NSTimer::scheduledTimerWithTimeInterval_repeats_block(interval_secs, true, &block);

        // Enter the application event loop (blocks forever).
        app.run();
    }
}

#[cfg(not(target_os = "macos"))]
mod macos {
    pub fn run() {
        eprintln!("app-mac: this binary only runs on macOS.");
    }
}

fn main() {
    macos::run();
}
