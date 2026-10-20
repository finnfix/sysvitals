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
    use std::process::Child;
    use std::time::{SystemTime, UNIX_EPOCH};

    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2::{define_class, msg_send, sel, MainThreadMarker, MainThreadOnly};
    use objc2_app_kit::{
        NSApplication, NSApplicationActivationPolicy, NSEvent, NSEventType, NSMenu, NSMenuItem,
        NSScreen, NSStatusBar, NSStatusItem, NSVariableStatusItemLength,
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
        // Child process handles, kept so a still-running instance is not
        // duplicated (a second spawn is a no-op until the first exits).
        static SETTINGS_CHILD: RefCell<Option<Child>> = const { RefCell::new(None) };
        static HISTORY_CHILD: RefCell<Option<Child>> = const { RefCell::new(None) };
        // Deferred single-click: armed on a single press, cancelled if a
        // double-click arrives within the system double-click interval.
        static PENDING_SINGLE_CLICK: RefCell<Option<Retained<NSTimer>>> =
            const { RefCell::new(None) };
    }

    /// Screen rectangle (left, top, right, bottom) in GPUI's global top-left,
    /// y-downward logical coordinate space, derived from the status item's
    /// button window. Returns `None` when the button or window is unavailable.
    fn anchor_rect() -> Option<(i64, i64, i64, i64)> {
        let mtm = mtm();
        STATUS_ITEM.with(|si| {
            let item = si.borrow().clone()?;
            let button = item.button(mtm)?;
            let window = button.window()?;
            let frame = window.frame();
            let screen = window.screen().or_else(|| NSScreen::mainScreen(mtm))?;
            let screen_frame = screen.frame();
            // Cocoa origin is bottom-left with y growing upward; GPUI wants
            // top-left with y downward, so flip around the screen top edge.
            let left = frame.origin.x;
            let top = screen_frame.size.height - (frame.origin.y + frame.size.height);
            let right = left + frame.size.width;
            let bottom = top + frame.size.height;
            Some((
                left.round() as i64,
                top.round() as i64,
                right.round() as i64,
                bottom.round() as i64,
            ))
        })
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

    /// Path to relaunch this program as: the enclosing `.app` bundle when
    /// running from one, otherwise the executable itself.
    ///
    /// `open` resolves a bundle path through LaunchServices, which applies the
    /// bundle's Info.plist (notably `LSUIElement`) and reuses the registered
    /// application identity. Handing it the raw binary inside
    /// `Contents/MacOS/` instead bypasses that registration, so the relaunched
    /// process may not be treated as the same app.
    fn relaunch_target() -> Option<std::path::PathBuf> {
        let exe = std::env::current_exe().ok()?;
        let bundle = exe
            .ancestors()
            .find(|p| p.extension() == Some(std::ffi::OsStr::new("app")))?;
        Some(bundle.to_path_buf())
    }

    /// Whether a previously spawned child (held in `slot`) is still running.
    /// Reaps it once it has exited.
    fn child_alive(slot: &RefCell<Option<Child>>) -> bool {
        let mut borrow = slot.borrow_mut();
        match borrow.as_mut() {
            Some(child) => match child.try_wait() {
                Ok(Some(_status)) => {
                    *borrow = None;
                    false
                }
                _ => true,
            },
            None => false,
        }
    }

    /// Launch the external GPUI settings process (`SysVitalsSettings`), one
    /// live instance at a time.
    fn launch_settings() {
        if child_alive(&SETTINGS_CHILD) {
            return;
        }
        let Some(dir) = exe_dir() else {
            return;
        };
        let settings = dir.join("SysVitalsSettings");
        if !settings.exists() {
            return;
        }
        if let Ok(child) = std::process::Command::new(settings).spawn() {
            *SETTINGS_CHILD.borrow_mut() = Some(child);
        }
    }

    /// Launch the external history chart process (`SysVitalsHistory`), passing
    /// the indicator's screen rect so the popup can anchor to it. One live
    /// instance at a time.
    fn launch_history() {
        if child_alive(&HISTORY_CHILD) {
            return;
        }
        let Some(dir) = exe_dir() else {
            return;
        };
        let history = dir.join("SysVitalsHistory");
        if !history.exists() {
            return;
        }
        let mut cmd = std::process::Command::new(history);
        if let Some((l, t, r, b)) = anchor_rect() {
            cmd.args(["--anchor", &l.to_string(), &t.to_string(), &r.to_string(), &b.to_string()]);
        }
        if let Ok(child) = cmd.spawn() {
            *HISTORY_CHILD.borrow_mut() = Some(child);
        }
    }

    /// Open Activity Monitor (macOS equivalent of Task Manager).
    fn open_activity_monitor() {
        let _ = std::process::Command::new("open")
            .args(["-a", "Activity Monitor"])
            .spawn();
    }

    /// Cancel any deferred single-click so a double-click does not also fire
    /// the single-click action.
    fn cancel_pending_single_click() {
        PENDING_SINGLE_CLICK.with(|p| {
            if let Some(timer) = p.borrow_mut().take() {
                timer.invalidate();
            }
        });
    }

    /// Defer the single-click action by the system double-click interval. If a
    /// double-click arrives first, [`cancel_pending_single_click`] drops it.
    fn schedule_single_click() {
        let delay = {
            let d = NSEvent::doubleClickInterval();
            if d <= 0.0 { 0.1 } else { d as f64 }
        };
        cancel_pending_single_click();
        let block = block2::RcBlock::new(|_timer: std::ptr::NonNull<NSTimer>| {
            PENDING_SINGLE_CLICK.with(|p| {
                *p.borrow_mut() = None;
            });
            launch_history();
        });
        // SAFETY: the block captures nothing non-Send and the timer fires on
        // the main run loop.
        let timer = unsafe {
            NSTimer::scheduledTimerWithTimeInterval_repeats_block(delay, false, &block)
        };
        PENDING_SINGLE_CLICK.with(|p| {
            *p.borrow_mut() = Some(timer);
        });
    }

    /// Restart: schedule a delayed relaunch, then exit.
    fn restart_self() {
        // Prefer the enclosing .app bundle; fall back to the bare executable
        // when running unpackaged (e.g. straight from target/release).
        let Some(target) = relaunch_target().or_else(|| std::env::current_exe().ok()) else {
            return;
        };
        let target_str = target.to_string_lossy().to_string();
        // Use a background shell that waits 2 seconds then relaunches.
        let _ = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("sleep 2 && open '{}'", target_str))
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
            /// using NSApplication.currentEvent. The single-click action is
            /// deferred so a double-click can cancel it (otherwise both the
            /// history popup and Activity Monitor would open).
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
                            cancel_pending_single_click();
                            open_activity_monitor();
                        } else {
                            schedule_single_click();
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
            // ActionHandler does not override init, so calling it directly
            // dispatches to NSObject's init. Avoids the super() wrapper which
            // triggers RetainSemantics issues with Allocated<T> in objc2 0.6.5.
            unsafe { msg_send![this, init] }
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
