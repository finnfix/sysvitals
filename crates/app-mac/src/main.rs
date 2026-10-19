//! macOS display layer: NSStatusItem menu-bar indicator.
//!
//! Shows CPU / memory / network stats in the macOS menu bar using
//! NSStatusItem with the system menu-bar font. Samples periodically via
//! sysvitals-core, formats with the shared format engine, and updates
//! the status item title.
//!
//! Architecture: pure objc2 + AppKit for the indicator itself. The GPUI
//! settings window (app-settings) is launched as a separate process, same
//! as on Windows. A right-click menu offers Settings and Quit.

#[cfg(target_os = "macos")]
mod macos {
    use std::cell::RefCell;
    use std::time::{SystemTime, UNIX_EPOCH};

    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2::{define_class, msg_send, sel, ClassType, MainThreadOnly};
    use objc2_app_kit::{
        NSApplication, NSApplicationActivationPolicy, NSMenu, NSMenuItem, NSStatusBar,
        NSStatusItem, NSVariableStatusItemLength,
    };
    use objc2_foundation::{NSObject, NSObjectProtocol, NSString, NSTimer};

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

    /// Directory containing the running executable, used to locate the
    /// sibling GPUI settings binary.
    fn exe_dir() -> Option<std::path::PathBuf> {
        std::env::current_exe()
            .ok()?
            .parent()
            .map(|p| p.to_path_buf())
    }

    /// Launch the external GPUI settings process (`SysVitalsSettings`).
    fn launch_settings() {
        let Some(dir) = exe_dir() else {
            return;
        };
        let settings = dir.join("SysVitalsSettings");
        if !settings.exists() {
            return;
        }
        let _ = std::process::Command::new(settings).spawn();
    }

    /// Launch the external history chart process (`SysVitalsHistory`).
    fn launch_history() {
        let Some(dir) = exe_dir() else {
            return;
        };
        let history = dir.join("SysVitalsHistory");
        if !history.exists() {
            return;
        }
        let _ = std::process::Command::new(history).spawn();
    }

    // Menu action handler. A tiny NSObject subclass so NSMenuItem can target
    // it with a selector; the responder chain alone only covers `terminate:`.
    define_class!(
        // SAFETY: NSObject has no subclassing requirements; no Drop impl.
        #[unsafe(super(NSObject))]
        #[thread_kind = MainThreadOnly]
        #[name = "SysVitalsMenuHandler"]
        struct MenuHandler;

        impl MenuHandler {
            #[unsafe(method(showSettings:))]
            fn show_settings(&self, _sender: Option<&AnyObject>) {
                launch_settings();
            }

            #[unsafe(method(showHistory:))]
            fn show_history(&self, _sender: Option<&AnyObject>) {
                launch_history();
            }
        }

        unsafe impl NSObjectProtocol for MenuHandler {}
    );

    impl MenuHandler {
        fn new() -> Retained<Self> {
            let this = Self::alloc();
            unsafe { msg_send![super(this), init] }
        }
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

        // Menu action handler (kept alive for the process lifetime).
        let handler = MenuHandler::new();

        // Build right-click menu: History, Settings, separator, Quit.
        let menu = NSMenu::new();

        let history_title = NSString::from_str("Show History");
        let history_item = NSMenuItem::new();
        history_item.setTitle(&history_title);
        history_item.setTarget(Some(&handler));
        history_item.setAction(Some(sel!(showHistory:)));
        menu.addItem(&history_item);

        let settings_title = NSString::from_str("Settings…");
        let settings_item = NSMenuItem::new();
        settings_item.setTitle(&settings_title);
        settings_item.setTarget(Some(&handler));
        settings_item.setAction(Some(sel!(showSettings:)));
        menu.addItem(&settings_item);

        menu.addItem(&NSMenuItem::separatorItem());

        let quit_title = NSString::from_str("Quit SysVitals");
        let quit_item = NSMenuItem::new();
        quit_item.setTitle(&quit_title);
        quit_item.setAction(Some(sel!(terminate:)));
        menu.addItem(&quit_item);

        status_item.setMenu(Some(&menu));

        // Keep the handler alive as long as the menu references it.
        std::mem::forget(handler);

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
