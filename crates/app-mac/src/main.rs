//! macOS display layer: NSStatusItem menu-bar indicator.
//!
//! Shows CPU / memory / network stats in the macOS menu bar using
//! NSStatusItem with the system menu-bar font. Samples periodically via
//! sysvitals-core, formats with the shared format engine, and updates
//! the status item title.
//!
//! Interaction model (matches Windows):
//! - Single-click: open history trend chart popup
//! - Double-click: open Activity Monitor (macOS task manager)
//! - Right-click: context menu (History / Settings / Restart / Quit)
//!
//! Architecture: pure objc2 + AppKit for the indicator itself. The GPUI
//! settings window (app-settings) and history chart (app-history) are
//! launched as separate processes, same as on Windows.

#[cfg(target_os = "macos")]
mod macos {
    use std::cell::RefCell;
    use std::time::{SystemTime, UNIX_EPOCH};

    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2::{define_class, msg_send, sel, MainThreadMarker, MainThreadOnly};
    use objc2_app_kit::{
        NSApplication, NSApplicationActivationPolicy, NSEventType, NSMenu, NSMenuItem, NSStatusBar,
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
        static MTM: RefCell<Option<MainThreadMarker>> = const { RefCell::new(None) };
    }

    /// Get the MainThreadMarker (must be called from main thread).
    fn mtm() -> MainThreadMarker {
        MTM.with(|m| m.borrow().expect("MainThreadMarker not set"))
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
        let mtm = mtm();
        STATUS_ITEM.with(|si| {
            if let Some(item) = si.borrow().as_ref() {
                if let Some(button) = item.button(mtm) {
                    button.setTitle(&title);
                }
            }
        });
    }

    /// Directory containing the running executable.
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

    /// Open Activity Monitor (macOS equivalent of Task Manager).
    fn open_activity_monitor() {
        let _ = std::process::Command::new("open")
            .args(["-a", "Activity Monitor"])
            .spawn();
    }

    /// Restart: schedule a delayed relaunch, then exit.
    fn restart_self() {
        let Some(exe) = std::env::current_exe().ok() else {
            return;
        };
        let exe_str = exe.to_string_lossy().to_string();
        // Use a background shell that waits 2 seconds then relaunches.
        let _ = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("sleep 2 && open '{}'", exe_str))
            .spawn();
        // Exit current instance.
        let mtm = mtm();
        let app = NSApplication::sharedApplication(mtm);
        app.terminate(None);
    }

    /// Show the context menu at the status item position.
    fn show_context_menu() {
        let mtm = mtm();
        STATUS_ITEM.with(|si| {
            let si_borrow = si.borrow();
            let Some(item) = si_borrow.as_ref() else {
                return;
            };
            let Some(button) = item.button(mtm) else {
                return;
            };

            let menu = NSMenu::new(mtm);

            let history_title = NSString::from_str("Show History");
            let history_item = NSMenuItem::new(mtm);
            history_item.setTitle(&history_title);
            // SAFETY: setting a valid selector on a menu item.
            unsafe { history_item.setAction(Some(sel!(showHistory:))) };
            menu.addItem(&history_item);

            let settings_title = NSString::from_str("Settings…");
            let settings_item = NSMenuItem::new(mtm);
            settings_item.setTitle(&settings_title);
            unsafe { settings_item.setAction(Some(sel!(showSettings:))) };
            menu.addItem(&settings_item);

            let restart_title = NSString::from_str("Restart");
            let restart_item = NSMenuItem::new(mtm);
            restart_item.setTitle(&restart_title);
            unsafe { restart_item.setAction(Some(sel!(restartApp:))) };
            menu.addItem(&restart_item);

            menu.addItem(&NSMenuItem::separatorItem(mtm));

            let quit_title = NSString::from_str("Quit SysVitals");
            let quit_item = NSMenuItem::new(mtm);
            quit_item.setTitle(&quit_title);
            unsafe { quit_item.setAction(Some(sel!(terminate:))) };
            menu.addItem(&quit_item);

            // Pop up the menu at the button's position.
            item.setMenu(Some(&menu));
            // SAFETY: performing a click on a valid button to show the menu.
            unsafe { button.performClick(None) };
            item.setMenu(None);
        });
    }

    // Action handler for button clicks and menu items.
    define_class!(
        // SAFETY: NSObject has no subclassing requirements; no Drop impl.
        #[unsafe(super(NSObject))]
        #[thread_kind = MainThreadOnly]
        #[name = "SysVitalsActionHandler"]
        struct ActionHandler;

        impl ActionHandler {
            /// Called when the status item button is clicked.
            /// Distinguishes single-click, double-click, and right-click
            /// using NSApplication.currentEvent.
            #[unsafe(method(buttonClicked:))]
            fn button_clicked(&self, _sender: Option<&AnyObject>) {
                let mtm = mtm();
                let app = NSApplication::sharedApplication(mtm);
                let event = app.currentEvent();
                let Some(event) = event else {
                    // Fallback: treat as single click.
                    launch_history();
                    return;
                };

                match event.r#type() {
                    NSEventType::RightMouseUp | NSEventType::RightMouseDown => {
                        show_context_menu();
                    }
                    _ => {
                        if event.clickCount() >= 2 {
                            open_activity_monitor();
                        } else {
                            launch_history();
                        }
                    }
                }
            }

            #[unsafe(method(showSettings:))]
            fn show_settings(&self, _sender: Option<&AnyObject>) {
                launch_settings();
            }

            #[unsafe(method(showHistory:))]
            fn show_history(&self, _sender: Option<&AnyObject>) {
                launch_history();
            }

            #[unsafe(method(restartApp:))]
            fn restart_app(&self, _sender: Option<&AnyObject>) {
                restart_self();
            }
        }

        unsafe impl NSObjectProtocol for ActionHandler {}
    );

    impl ActionHandler {
        fn new(mtm: MainThreadMarker) -> Retained<Self> {
            let this = Self::alloc(mtm);
            unsafe { msg_send![super(this), init] }
        }
    }

    pub fn run() {
        // Get MainThreadMarker — must be on main thread.
        let mtm = MainThreadMarker::new().expect("must run on main thread");
        MTM.with(|m| *m.borrow_mut() = Some(mtm));

        // NSApplication setup — must happen on the main thread.
        let app = NSApplication::sharedApplication(mtm);
        app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

        // Create the status item with variable width.
        let status_bar = NSStatusBar::systemStatusBar();
        let status_item = status_bar.statusItemWithLength(NSVariableStatusItemLength);

        // Set initial title so something shows immediately.
        if let Some(button) = status_item.button(mtm) {
            let title = NSString::from_str("SysVitals");
            button.setTitle(&title);
        }

        // Action handler (kept alive for the process lifetime).
        let handler = ActionHandler::new(mtm);

        // Wire button action (handles single/double/right click).
        if let Some(button) = status_item.button(mtm) {
            // SAFETY: setting a valid target and selector on the button.
            unsafe {
                button.setTarget(Some(&handler));
                button.setAction(Some(sel!(buttonClicked:)));
            }
        }

        // Keep the handler alive.
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

        let block = block2::RcBlock::new(|_timer: std::ptr::NonNull<NSTimer>| {
            tick();
        });
        // SAFETY: the block is sendable (captures nothing non-Send).
        let _timer = unsafe {
            NSTimer::scheduledTimerWithTimeInterval_repeats_block(interval_secs, true, &block)
        };

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
