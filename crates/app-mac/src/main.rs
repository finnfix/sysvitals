//! macOS display layer: NSStatusItem menu-bar indicator.
//!
//! Shows CPU / memory / network stats in the macOS menu bar using
//! NSStatusItem with the system menu-bar font. Samples periodically via
//! sysvitals-core, formats with the shared format engine, and updates
//! the status item title.
//!
//! Interaction model (matches Windows):
//! - Single-click: open history trend chart popup
//! - Right-click: context menu (Activity Monitor / History / Settings /
//!   Restart / Quit)
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
        NSApplication, NSApplicationActivationPolicy, NSEvent, NSEventMask, NSEventType, NSFont,
        NSMenu, NSMenuItem, NSScreen, NSStatusBar, NSStatusItem, NSVariableStatusItemLength,
        NSView,
    };
    use objc2_foundation::{NSObject, NSObjectProtocol, NSPoint, NSString, NSTimer};

    use sysvitals_core::config::{config_path, history_path};
    use sysvitals_core::format::format_frame;
    use sysvitals_core::{t, Config, NetSelection, Sampler};

    /// Font-size factor for the dual-row layout: the menu bar has a fixed
    /// height, so two lines only fit with the system menu-bar font scaled
    /// down (≈0.75 — the same factor the Windows overlay uses, see the
    /// `dual_row` note in core config).
    const DUAL_ROW_FONT_FACTOR: f64 = 0.75;

    thread_local! {
        static SAMPLER: RefCell<Option<Sampler>> = const { RefCell::new(None) };
        static CONFIG: RefCell<Config> = RefCell::new(Config::default());
        static STATUS_ITEM: RefCell<Option<Retained<NSStatusItem>>> = const { RefCell::new(None) };
        static CONFIG_MTIME: RefCell<i64> = const { RefCell::new(0) };
        static MTM: RefCell<Option<MainThreadMarker>> = const { RefCell::new(None) };
        // History store, lazily opened on the first tick so a missing data
        // dir never blocks the indicator (mirrors the Windows overlay).
        static STORE: RefCell<Option<sysvitals_core::Store>> = const { RefCell::new(None) };
        static STORE_ATTEMPTED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        static TICK_COUNT: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
        // The repeating sample timer, kept so the interval can be re-applied
        // when the config file changes it (parity with the Windows overlay,
        // which re-seeds its sample timer on config hot-reload).
        static SAMPLE_TIMER: RefCell<Option<Retained<NSTimer>>> = const { RefCell::new(None) };
        /// Interval currently in effect on the timer; the first tick primes
        /// it (0) and schedules the real timer from there.
        static APPLIED_INTERVAL_MS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
        // Child process handles, kept so a still-running instance is not
        // duplicated (a second spawn is a no-op until the first exits).
        static SETTINGS_CHILD: RefCell<Option<Child>> = const { RefCell::new(None) };
        static HISTORY_CHILD: RefCell<Option<Child>> = const { RefCell::new(None) };
        // The button/menu action target. Kept addressable so context-menu items
        // can name it explicitly: the handler lives outside the responder chain
        // (nothing inserts it there), so a nil target would leave AppKit's
        // automatic enabling unable to find a receiver and gray the items out.
        static ACTION_HANDLER: RefCell<Option<Retained<ActionHandler>>> = const { RefCell::new(None) };
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

    /// Own and (re-)arm the repeating sample timer. The first tick primes it;
    /// later ticks re-schedule only when a config hot-reload changed
    /// `sample.interval_ms`, so a new interval takes effect within one old tick
    /// — the macOS counterpart of the Windows overlay re-seeding its sample
    /// timer on config change (previously the interval was read once at startup
    /// and never updated live).
    fn reschedule_if_interval_changed() {
        let interval_ms = CONFIG.with(|c| c.borrow().sample.interval_ms);
        if APPLIED_INTERVAL_MS.get() == interval_ms {
            return;
        }
        let block = block2::RcBlock::new(|_timer: std::ptr::NonNull<NSTimer>| {
            tick();
        });
        // SAFETY: the block captures nothing non-Send and fires on the main run
        // loop, same contract as the other timers here.
        let timer = unsafe {
            NSTimer::scheduledTimerWithTimeInterval_repeats_block(
                interval_ms as f64 / 1000.0,
                true,
                &block,
            )
        };
        // Install the new timer and invalidate the one it replaces so exactly
        // one keeps firing.
        let prev = SAMPLE_TIMER.with(|slot| slot.replace(Some(timer)));
        if let Some(old) = prev {
            old.invalidate();
        }
        APPLIED_INTERVAL_MS.set(interval_ms);
    }

    /// Insert the current CPU/MEM sample into the history database and prune
    /// old rows periodically. Mirrors the Windows overlay: a failed open is
    /// logged-attempt once and never retried so a broken data dir does not
    /// spam on every tick. (Without this, macOS never writes history and the
    /// trend popup would always be empty.)
    fn record_history(cfg: &Config, now: i64, snapshot: &sysvitals_core::Snapshot) {
        if !cfg.history.enabled {
            return;
        }
        STORE.with(|s| {
            let mut guard = s.borrow_mut();
            if guard.is_none() && !STORE_ATTEMPTED.replace(true) {
                if let Ok(path) = history_path() {
                    if let Some(parent) = path.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    match sysvitals_core::Store::open(&path) {
                        Ok(store) => *guard = Some(store),
                        Err(_) => *guard = None,
                    }
                }
            }
            if let Some(store) = guard.as_ref() {
                // Non-finite samples (first/wake ticks) are skipped inside
                // `insert`; the returned bool is not acted on here.
                let _ = store.insert(now, snapshot.cpu_pct, snapshot.mem_pct);
                let tick = TICK_COUNT.with(|c| {
                    let v = c.get() + 1;
                    c.set(v);
                    v
                });
                if tick % 300 == 0 {
                    let _ = store.prune(now, cfg.history.retention_days);
                }
            }
        });
    }

    /// Sample metrics, record the history row, and update the status item
    /// title.
    fn tick() {
        reload_config_if_changed();
        reschedule_if_interval_changed();

        let cfg = CONFIG.with(|c| c.borrow().clone());
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        let snapshot = SAMPLER.with(|s| {
            let mut guard = s.borrow_mut();
            let sampler = guard
                .get_or_insert_with(|| Sampler::new(NetSelection::Auto, cfg.sample.cpu_smoothing));
            sampler.sample(now)
        });

        record_history(&cfg, now, &snapshot);

        let row = format_frame(&snapshot, &cfg);

        // Dual-row: stack the two formatted lines into one two-line title.
        // The menu bar height is fixed, so both lines only fit with the font
        // scaled down (0.75 of the system menu-bar size — the same factor the
        // Windows overlay uses for the 2x2 quadrant).
        let dual = cfg.display.dual_row && row.lines.len() > 1;

        // Build the display text from the formatted row.
        let text = if dual {
            row.lines
                .iter()
                .map(|l| l.render().trim_end().to_string())
                .collect::<Vec<_>>()
                .join("\n")
        } else if row.items.is_empty() {
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
        // Single-row strictly follows the system menu-bar font (size 0 =
        // standard); dual-row uses the 0.75-scaled variant. `menuBarFontOfSize`
        // is the canonical menu-bar face at the requested size, so this only
        // changes the size, never the family.
        let font = if dual {
            let base = NSFont::menuBarFontOfSize(0.0);
            let scaled = (base.pointSize() * DUAL_ROW_FONT_FACTOR).max(1.0);
            NSFont::menuBarFontOfSize(scaled)
        } else {
            NSFont::menuBarFontOfSize(0.0)
        };
        STATUS_ITEM.with(|si| {
            if let Some(item) = si.borrow().as_ref() {
                if let Some(button) = item.button(mtm) {
                    button.setTitle(&title);
                    button.setFont(Some(&font));
                    // The two-line title only renders as two lines when the
                    // cell wraps; single-line titles are unaffected.
                    if let Some(cell) = button.cell() {
                        cell.setWraps(dual);
                    }
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
        // `thread_local!` statics expose a `LocalKey`; reach the cell through
        // `with`, and re-entrancy (`child_alive` borrows it) must not nest, so
        // query liveness first, then spawn, then store.
        if SETTINGS_CHILD.with(|slot| child_alive(slot)) {
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
            SETTINGS_CHILD.with(|slot| *slot.borrow_mut() = Some(child));
        }
    }

    /// Launch the external history chart process (`SysVitalsHistory`), passing
    /// the indicator's screen rect so the popup can anchor to it. One live
    /// instance at a time.
    fn launch_history() {
        if HISTORY_CHILD.with(|slot| child_alive(slot)) {
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
            cmd.args([
                "--anchor",
                &l.to_string(),
                &t.to_string(),
                &r.to_string(),
                &b.to_string(),
            ]);
        }
        if let Ok(child) = cmd.spawn() {
            HISTORY_CHILD.with(|slot| *slot.borrow_mut() = Some(child));
        }
    }

    /// Open Activity Monitor (macOS equivalent of Task Manager).
    fn open_activity_monitor() {
        let _ = std::process::Command::new("open")
            .args(["-a", "Activity Monitor"])
            .spawn();
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
        let target = ACTION_HANDLER.with(|h| h.borrow().clone());
        let Some(target) = target.as_ref() else {
            return;
        };
        STATUS_ITEM.with(|si| {
            let si_borrow = si.borrow();
            let Some(item) = si_borrow.as_ref() else {
                return;
            };
            let Some(button) = item.button(mtm) else {
                return;
            };

            let menu = NSMenu::new(mtm);
            // Menu labels follow the configured language, like the Windows
            // overlay's tray menu (previously hardcoded English here).
            let lang = CONFIG.with(|c| c.borrow().display.language);

            // Activity Monitor first: it used to be the double-click action,
            // and moving it to the top of the context menu is what freed the
            // left click to open the trend chart instantly (mirrors the
            // Windows overlay's Task Manager entry).
            let activity_title = NSString::from_str(t(lang, "menu.activity_monitor"));
            let activity_item = NSMenuItem::new(mtm);
            activity_item.setTitle(&activity_title);
            // SAFETY: setting a valid selector on a menu item.
            unsafe { activity_item.setAction(Some(sel!(showActivityMonitor:))) };
            // SAFETY: `target` is the live ActionHandler kept in ACTION_HANDLER.
            unsafe { activity_item.setTarget(Some(target)) };
            menu.addItem(&activity_item);

            let history_title = NSString::from_str(t(lang, "menu.show_history"));
            let history_item = NSMenuItem::new(mtm);
            history_item.setTitle(&history_title);
            // SAFETY: setting a valid selector on a menu item.
            unsafe { history_item.setAction(Some(sel!(showHistory:))) };
            // SAFETY: `target` is the live ActionHandler kept in ACTION_HANDLER.
            unsafe { history_item.setTarget(Some(target)) };
            menu.addItem(&history_item);

            let settings_title = NSString::from_str(t(lang, "menu.settings"));
            let settings_item = NSMenuItem::new(mtm);
            settings_item.setTitle(&settings_title);
            unsafe { settings_item.setAction(Some(sel!(showSettings:))) };
            // SAFETY: `target` is the live ActionHandler kept in ACTION_HANDLER.
            unsafe { settings_item.setTarget(Some(target)) };
            menu.addItem(&settings_item);

            let restart_title = NSString::from_str(t(lang, "menu.restart"));
            let restart_item = NSMenuItem::new(mtm);
            restart_item.setTitle(&restart_title);
            unsafe { restart_item.setAction(Some(sel!(restartApp:))) };
            // SAFETY: `target` is the live ActionHandler kept in ACTION_HANDLER.
            unsafe { restart_item.setTarget(Some(target)) };
            menu.addItem(&restart_item);

            menu.addItem(&NSMenuItem::separatorItem(mtm));

            let quit_title = NSString::from_str(t(lang, "menu.quit"));
            let quit_item = NSMenuItem::new(mtm);
            quit_item.setTitle(&quit_title);
            unsafe { quit_item.setAction(Some(sel!(terminate:))) };
            // Quit goes through NSApplication itself: `terminate:` is resolved
            // down the responder chain to the shared app, so this item keeps a
            // nil target on purpose (the handler does not implement it).
            menu.addItem(&quit_item);

            // Pop the menu directly at the button's lower-left corner. The
            // previous approach (attach with `setMenu`, then `performClick`,
            // then detach) is unreliable once the item lives in the notch
            // overflow panel: `performClick` there highlights the button but
            // does not pop the attached menu, so a right-click looked like it
            // did nothing. `popUpMenuPositioningItem` shows the menu against
            // the button view regardless of how the item is positioned, and it
            // runs a nested tracking loop that returns once the menu closes.
            let view: &NSView = &button;
            let location = NSPoint::new(0.0, button.bounds().size.height);
            let _ = menu.popUpMenuPositioningItem_atLocation_inView(None, location, Some(view));
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
            /// Called when the status item button is clicked. A right click
            /// opens the context menu; any other click opens the trend chart
            /// immediately. There is no double-click handling — Activity
            /// Monitor lives in the context menu, so a left click never waits
            /// for a possible second press.
            #[unsafe(method(buttonClicked:))]
            fn button_clicked(&self, _sender: Option<&AnyObject>) {
                let mtm = mtm();
                let app = NSApplication::sharedApplication(mtm);
                let event = app.currentEvent();
                match event.as_ref().map(|e| e.r#type()) {
                    Some(NSEventType::RightMouseUp) | Some(NSEventType::RightMouseDown) => {
                        show_context_menu();
                    }
                    // Any non-right click opens the trend chart immediately.
                    // There is no double-click branch and no deferral: the
                    // Activity Monitor launcher moved to the context menu's
                    // first item, so a left click never has to wait for a
                    // possible second press (matching the Windows overlay).
                    _ => launch_history(),
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

            /// Menu-item action: opens Activity Monitor. Used to be the
            /// double-click target on the status item; moved here so the
            /// left click can open the trend chart instantly without a
            /// deferral window.
            #[unsafe(method(showActivityMonitor:))]
            fn show_activity_monitor(&self, _sender: Option<&AnyObject>) {
                open_activity_monitor();
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

        // Wire button action (single click opens the chart, right click opens
        // the context menu).
        if let Some(button) = status_item.button(mtm) {
            // SAFETY: setting a valid target and selector on the button.
            unsafe {
                button.setTarget(Some(&handler));
                button.setAction(Some(sel!(buttonClicked:)));
            }
            // Status-item buttons default to firing on mouse-down only, so a
            // right-click never reaches the action and the context menu could
            // not be raised from `buttonClicked:`. Subscribe explicitly to both
            // mouse-up events (Apple's documented pattern for status items).
            let mask = NSEventMask::LeftMouseUp | NSEventMask::RightMouseUp;
            let _ = button.sendActionOn(mask);
        }

        // Keep the handler alive for the process lifetime, addressable so the
        // context menu can name it as the action target (see ACTION_HANDLER).
        ACTION_HANDLER.with(|h| *h.borrow_mut() = Some(handler.clone()));
        std::mem::forget(handler);

        // Store the status item for the tick callback.
        STATUS_ITEM.with(|si| *si.borrow_mut() = Some(status_item));

        // Load initial config.
        if let Ok(path) = config_path() {
            if let Ok((cfg, _)) = Config::load(&path) {
                CONFIG.with(|c| *c.borrow_mut() = cfg);
            }
        }

        // Initial tick — also primes the repeating sample timer (see
        // `reschedule_if_interval_changed`).
        tick();

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
