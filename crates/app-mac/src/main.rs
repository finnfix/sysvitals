//! macOS display layer: NSStatusItem menu-bar indicator.
//!
//! Shows CPU / memory / network stats in the macOS menu bar using
//! NSStatusItem with the system menu-bar font. Samples periodically via
//! sysvitals-core, formats with the shared format engine, and updates
//! the status item title.
//!
//! Interaction model (matches Windows):
//! - Single-click: toggle the history trend chart popup
//! - Right-click: context menu (Activity Monitor / Settings / Restart / Quit)
//!
//! Architecture: pure objc2 + AppKit for the indicator itself. The GPUI
//! settings window (app-settings) and history chart (app-history) are
//! launched as separate processes, same as on Windows.

#[cfg(target_os = "macos")]
mod macos {
    use std::cell::RefCell;
    use std::os::fd::AsRawFd;
    use std::process::Child;
    use std::time::{SystemTime, UNIX_EPOCH};

    use chrono::Local;
    use objc2::rc::Retained;
    use objc2::runtime::{AnyObject, Bool};
    use objc2::{define_class, msg_send, sel, ClassType, MainThreadMarker, MainThreadOnly};
    use objc2_app_kit::{
        NSApplication, NSApplicationActivationPolicy, NSAttributedStringNSStringDrawing,
        NSCellImagePosition, NSEventMask, NSEventType, NSFont, NSFontAttributeName, NSImage,
        NSImageScaling, NSMenu, NSMenuItem, NSScreen, NSStatusBar, NSStatusItem,
        NSVariableStatusItemLength, NSView,
    };
    use objc2_foundation::{
        NSAttributedString, NSDictionary, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize,
        NSString, NSTimer,
    };

    use sysvitals_core::config::{config_path, history_path};
    use sysvitals_core::format::format_frame;
    use sysvitals_core::{t, Config, FormattedRow, NetSelection, Sampler};

    /// Font-size factor for the dual-row layout: the menu bar has a fixed
    /// height, so two lines only fit with the system menu-bar font scaled
    /// down (≈0.75 — the same factor the Windows overlay uses, see the
    /// `dual_row` note in core config).
    const DUAL_ROW_FONT_FACTOR: f64 = 0.75;

    // ------------------------------------------------------------- diagnostics

    /// Local date (`YYYY-MM-DD`) and time (`HH:MM:SS`) for the log stamp, from
    /// chrono's local clock (the macOS counterpart of the Windows overlay's
    /// `GetLocalTime`-based `local_now`).
    fn local_now() -> (String, String) {
        let now = Local::now();
        (
            now.format("%Y-%m-%d").to_string(),
            now.format("%H:%M:%S").to_string(),
        )
    }

    /// Append one line to today's log under the data dir (mirrors the Windows
    /// overlay: `core::logger` is platform-neutral, only the clock differs).
    /// Logging must never break the indicator, so failures are ignored.
    fn log_event(level: &str, message: &str) {
        let Ok(dir) = sysvitals_core::config::data_dir() else {
            return;
        };
        let (date, time) = local_now();
        sysvitals_core::logger::log_line(&dir, &date, &time, level, message);
    }

    /// Remove log files older than the retention window. Run once at startup.
    fn prune_old_logs() {
        let Ok(dir) = sysvitals_core::config::data_dir() else {
            return;
        };
        let (date, _) = local_now();
        sysvitals_core::logger::prune_logs(
            &dir,
            &date,
            sysvitals_core::logger::DEFAULT_RETENTION_DAYS,
        );
    }

    /// Rust panic hook writing the panic to the log before the process dies.
    /// The macOS counterpart of the Windows crash diagnostics minus the Win32
    /// unhandled-exception filter (a SEH-only API; a mach-exception port
    /// handler was judged out of scope — panics and normal exits are covered).
    fn install_panic_hook() {
        std::panic::set_hook(Box::new(|info| {
            let msg = if let Some(s) = info.payload().downcast_ref::<&str>() {
                s.to_string()
            } else if let Some(s) = info.payload().downcast_ref::<String>() {
                s.clone()
            } else {
                "unknown panic".to_string()
            };
            let loc = info
                .location()
                .map(|l| format!("{}:{}", l.file(), l.line()))
                .unwrap_or_else(|| "unknown location".to_string());
            log_event("FATAL", &format!("panic at {loc}: {msg}"));
        }));
    }

    /// Single-instance guard: an exclusive, non-blocking `flock` on
    /// `<data_dir>/sysvitals.lock`. Returns `true` when this process now holds
    /// the lock; a second launch finds it taken and exits. The file is
    /// deliberately leaked (`mem::forget`) because `flock` locks live on the
    /// open file description — keeping the fd open for the process lifetime is
    /// what keeps the lock held, and process exit releases it (no stale lock).
    /// Every failure to even open the file returns `true`: a broken data dir
    /// must never block the indicator from starting.
    fn acquire_single_instance() -> bool {
        let Ok(dir) = sysvitals_core::config::data_dir() else {
            return true;
        };
        let _ = std::fs::create_dir_all(&dir);
        let Ok(file) = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .open(dir.join("sysvitals.lock"))
        else {
            return true;
        };
        // SAFETY: `flock` on our own freshly opened fd; LOCK_EX|LOCK_NB never
        // blocks and only fails when another holder exists.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            return false;
        }
        std::mem::forget(file);
        true
    }

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

        // --- Self-drawn indicator image -----------------------------------
        // The status item shows a template `NSImage` we render ourselves
        // instead of a plain `setTitle`. That is the only way to control the
        // two-column dual-row layout to the pixel: item spacing, left-column
        // alignment and vertical centering are all impossible through AppKit's
        // string-titled status button, whose text placement we cannot reach.
        // Making the image a *template* keeps menu-bar light/dark inversion for
        // free (AppKit tints it from the system appearance), consistent with the
        // "indicator always follows the system theme" conclusion that also let
        // us hide the theme picker on macOS.

        /// Render signature of the image currently mounted (layout parameters +
        /// all texts). `tick` compares it first and skips everything when the
        /// frame is identical — no rebuild, no redraw, no bitmap churn.
        static DRAW_SIGNATURE: RefCell<String> = const { RefCell::new(String::new()) };
        /// Grow-only ceiling for the item width: the indicator never shrinks
        /// below the widest it has been, so a number losing a digit (or a rate
        /// unit stepping down) cannot shift the block left/right. Mirrors the
        /// Windows overlay's grow-only window sizing.
        static DRAW_WIDTH_CEIL: RefCell<f64> = const { RefCell::new(0.0) };
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
                        Ok(store) => {
                            log_event("INFO", &format!("history store opened: {}", path.display()));
                            *guard = Some(store);
                        }
                        Err(e) => {
                            // One-shot: the trend popup would stay empty, so
                            // this must be diagnosable from the log.
                            log_event(
                                "WARN",
                                &format!("history store open failed: {} — {e}", path.display()),
                            );
                            *guard = None;
                        }
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

    /// One text run baked into the current indicator image: an attributed
    /// string (carrying the font) plus the point to draw it at, in the image's
    /// flipped (top-left origin, y downward) space. The list is captured by the
    /// image's drawing handler, so AppKit can rasterize it without touching
    /// thread-local state from a possibly non-main thread.
    struct DrawItem {
        attr: Retained<NSAttributedString>,
        x: f64,
        y: f64,
    }

    /// One laid-out dual-row line: left run, optional right run, with measured
    /// widths/heights kept for the column math.
    struct RowRun {
        la: Retained<NSAttributedString>,
        lw: f64,
        lh: f64,
        ra: Option<Retained<NSAttributedString>>,
        rw: f64,
        rh: f64,
    }

    /// Build one attributed run (font baked in) and measure it. `size()` reads
    /// the attributed string's own attributes, so measurement and drawing can
    /// never disagree on the font.
    fn make_run(text: &str, font: &Retained<NSFont>) -> (Retained<NSAttributedString>, f64, f64) {
        let s = NSString::from_str(text);
        // NSFont's super chain is NSFont -> NSObject -> AnyObject; the
        // attribute-dict value type is AnyObject, so two `as_super` steps land
        // on the right reference (`as_super` comes from `objc2::ClassType`).
        let obj: &AnyObject = font.as_super().as_super();
        // `NSFontAttributeName` is an extern static (the pointer itself lives
        // in AppKit), so reading it needs unsafe; dereferencing it to the
        // `&NSString` it always holds is the only operation, which the
        // framework guarantees to be valid.
        let key: &NSString = unsafe { NSFontAttributeName };
        let dict = NSDictionary::from_slices(&[key], &[obj]);
        // SAFETY: the attribute dictionary holds a valid `NSFont` under the
        // documented `NSFontAttributeName` key.
        let attr = unsafe { NSAttributedString::new_with_attributes(&s, &dict) };
        let sz = attr.size();
        (attr, sz.width, sz.height)
    }

    /// Render the formatted frame into the status item as a self-drawn
    /// template image — the macOS counterpart of the Windows overlay's GDI
    /// painting, giving pixel control over dual-row alignment, item spacing
    /// and vertical centering that AppKit's string title does not offer.
    ///
    /// Skips every action when the render signature (layout parameters plus
    /// all texts) is unchanged: the mounted image simply stays, so an
    /// idle-second tick costs nothing (no rebuild, no bitmap churn). On change
    /// a fresh block-image is built and mounted; the previous image is
    /// reference-counted away. Grow-only width keeps the block from shifting
    /// when values lose a digit.
    fn render_indicator(row: &FormattedRow, cfg: &Config) {
        let mtm = mtm();
        let dual = cfg.display.dual_row && row.lines.len() > 1;
        let spacing = f64::from(cfg.display.item_spacing.max(1));
        // The menu-bar height (logical points) is the canvas height; drawing
        // inside it keeps the glyphs at the system's own vertical band.
        let img_h = NSStatusBar::systemStatusBar().thickness();

        // Single-row strictly follows the system menu-bar font (size 0 =
        // standard); dual-row uses the 0.75-scaled variant so two lines fit
        // the fixed bar height (same factor as the Windows quadrant).
        let base = NSFont::menuBarFontOfSize(0.0);
        let font = if dual {
            let scaled = (base.pointSize() * DUAL_ROW_FONT_FACTOR).max(1.0);
            NSFont::menuBarFontOfSize(scaled)
        } else {
            base
        };

        // --- lay out the runs (flipped space: y grows downward) -----------
        let mut items: Vec<DrawItem> = Vec::new();
        let mut texts: Vec<String> = Vec::new();
        let mut width = 0f64;
        // `display.theme` is deliberately not consulted: the indicator is a
        // template image, which AppKit tints from the menu-bar appearance —
        // light/dark inversion is automatic and per-item color is not a thing
        // on the menu bar (see the theme-picker removal in app-settings).
        if dual {
            // 2x2 quadrant mirroring the Windows overlay's `measure_frame`:
            // both rows' left columns start at x = 0 (left alignment, ②),
            // the right column starts at `left_col_w + item_spacing` (③),
            // and the two-line block is vertically centered in the bar (⑤).
            let mut runs: Vec<RowRun> = Vec::with_capacity(row.lines.len());
            for line in &row.lines {
                let (la, lw, lh) = make_run(line.left.trim_end(), &font);
                let (ra, rw, rh) = if line.right.is_empty() {
                    (None, 0.0, 0.0)
                } else {
                    let (a, w, h) = make_run(line.right.trim_end(), &font);
                    (Some(a), w, h)
                };
                texts.push(line.left.clone());
                texts.push(line.right.clone());
                runs.push(RowRun {
                    la,
                    lw,
                    lh,
                    ra,
                    rw,
                    rh,
                });
            }
            let left_w = runs.iter().map(|r| r.lw).fold(0f64, f64::max);
            let right_w = runs.iter().map(|r| r.rw).fold(0f64, f64::max);
            let line_h = runs.iter().map(|r| r.lh.max(r.rh)).fold(0f64, f64::max);
            let n = runs.len() as f64;
            let block_h = line_h * n;
            let y0 = ((img_h - block_h) / 2.0).max(0.0);
            let right_x = left_w + spacing;
            for (i, r) in runs.iter().enumerate() {
                let y = y0 + line_h * i as f64;
                items.push(DrawItem {
                    attr: r.la.clone(),
                    x: 0.0,
                    y,
                });
                if let Some(ra) = &r.ra {
                    items.push(DrawItem {
                        attr: ra.clone(),
                        x: right_x,
                        y,
                    });
                }
            }
            width = if right_w > 0.0 {
                right_x + right_w
            } else {
                left_w
            };
        } else {
            // Single row: lay the individual metric items with a pixel gap
            // (③ exact `item_spacing`, mirroring the Windows single-row path);
            // vertically centered on its own measured height (⑤).
            let parts: Vec<String> = if !row.items.is_empty() {
                row.items.clone()
            } else {
                let mut v = Vec::new();
                for line in &row.lines {
                    v.push(line.left.clone());
                    if !line.right.is_empty() {
                        v.push(line.right.clone());
                    }
                }
                v
            };
            let mut x = 0f64;
            for part in parts {
                let (attr, w, h) = make_run(part.trim_end(), &font);
                texts.push(part);
                let y = ((img_h - h) / 2.0).max(0.0);
                items.push(DrawItem { attr, x, y });
                x += w + spacing;
                width = width.max(x - spacing);
            }
        }
        if items.is_empty() || width <= 0.0 {
            return;
        }

        // Grow-only: keep the widest width ever laid out so later frames with
        // narrower content do not shift the block (② anti-jitter). Content
        // stays left-aligned inside the padded canvas.
        let ceil = DRAW_WIDTH_CEIL.with(|c| {
            let prev = *c.borrow();
            let next = prev.max(width);
            if next != prev {
                *c.borrow_mut() = next;
            }
            next
        });
        let sig = format!(
            "{dual}|{spacing}|{img_h:.1}|{ceil:.1}|{}",
            texts.join("\u{1}")
        );
        if DRAW_SIGNATURE.with(|s| *s.borrow() == sig) {
            return;
        }

        // --- build the template image --------------------------------------
        // Flipped drawing context: origin top-left, y downward — the space the
        // layout above was computed in. The handler captures the run list, so
        // rasterization never touches thread-local state.
        let block = block2::RcBlock::new(move |_rect: NSRect| -> Bool {
            for it in &items {
                it.attr.drawAtPoint(NSPoint::new(it.x, it.y));
            }
            Bool::YES
        });
        let image =
            NSImage::imageWithSize_flipped_drawingHandler(NSSize::new(ceil, img_h), true, &block);
        image.setTemplate(true);

        STATUS_ITEM.with(|si| {
            let binding = si.borrow();
            let Some(item) = binding.as_ref() else {
                return;
            };
            let Some(button) = item.button(mtm) else {
                return;
            };
            // Title cleared so only the image renders; `ImageOnly` +
            // `ScaleNone` keeps AppKit from rescaling our point-sized layout.
            button.setTitle(&NSString::from_str(""));
            button.setImage(Some(&image));
            button.setImagePosition(NSCellImagePosition::ImageOnly);
            button.setImageScaling(NSImageScaling::ScaleNone);
            // Fixed width = canvas width (grow-only), replacing the variable
            // length used for the string title.
            item.setLength(ceil);
        });
        DRAW_SIGNATURE.with(|s| *s.borrow_mut() = sig);
    }

    /// Sample metrics, record the history row, and update the indicator.
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
        render_indicator(&row, &cfg);
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
            log_event("WARN", "launch_settings: cannot determine exe directory");
            return;
        };
        let settings = dir.join("SysVitalsSettings");
        if !settings.exists() {
            log_event(
                "WARN",
                &format!("launch_settings: not found: {}", settings.display()),
            );
            return;
        }
        match std::process::Command::new(settings).spawn() {
            Ok(child) => {
                SETTINGS_CHILD.with(|slot| *slot.borrow_mut() = Some(child));
            }
            Err(e) => log_event("WARN", &format!("launch_settings: spawn failed: {e}")),
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
            log_event("WARN", "launch_history: cannot determine exe directory");
            return;
        };
        let history = dir.join("SysVitalsHistory");
        if !history.exists() {
            log_event(
                "WARN",
                &format!("launch_history: not found: {}", history.display()),
            );
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
        match cmd.spawn() {
            Ok(child) => {
                HISTORY_CHILD.with(|slot| *slot.borrow_mut() = Some(child));
            }
            Err(e) => log_event("WARN", &format!("launch_history: spawn failed: {e}")),
        }
    }

    /// Close the trend popup if one is running: the click-to-toggle half of
    /// `button_clicked:`. Kills the child and reaps it (`wait`) so no zombie
    /// lingers under the long-running indicator. Returns `true` when a live
    /// popup was closed; `false` when nothing was running (or it had already
    /// exited, which is reaped here so the slot is free for the next open).
    fn close_history() -> bool {
        let closed = HISTORY_CHILD.with(|slot| {
            let mut borrow = slot.borrow_mut();
            match borrow.as_mut() {
                Some(child) => match child.try_wait() {
                    Ok(Some(_status)) => {
                        *borrow = None;
                        false
                    }
                    _ => {
                        let _ = child.kill();
                        // The kill guarantees a prompt exit, so this wait
                        // cannot block meaningfully; it only reaps.
                        let _ = child.wait();
                        *borrow = None;
                        true
                    }
                },
                None => false,
            }
        });
        if closed {
            log_event("INFO", "history popup closed by indicator click");
        }
        closed
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
                    // Any non-right click toggles the trend chart: a live
                    // popup is closed, otherwise a new one opens (matching the
                    // Windows overlay). There is no double-click branch and no
                    // deferral — Activity Monitor lives in the context menu's
                    // first item, so a left click never waits for a possible
                    // second press.
                    _ => {
                        if !close_history() {
                            launch_history();
                        }
                    }
                }
            }

            #[unsafe(method(showSettings:))]
            fn show_settings(&self, _sender: Option<&AnyObject>) {
                launch_settings();
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
        // Crash diagnostics and the single-instance guard run before anything
        // else, mirroring the Windows overlay's startup sequence. A second
        // launch exits quietly; the lock is held via an fd that is deliberately
        // leaked (see `acquire_single_instance`).
        install_panic_hook();
        if !acquire_single_instance() {
            eprintln!("app-mac: another instance is already running; exiting");
            return;
        }

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

        // Daily-file logging: prune stale logs once, then stamp the start and
        // the effective config (same shape as the Windows overlay's startup
        // log, so a user can just send this file when something looks wrong).
        prune_old_logs();
        let cfg = CONFIG.with(|c| c.borrow().clone());
        log_event("INFO", "SysVitals (mac) starting");
        log_event(
            "INFO",
            &format!(
                "config loaded: interval={}ms language={:?} theme={:?} dual_row={} \
                 show_resources={} show_network={} item_spacing={}px",
                cfg.sample.interval_ms,
                cfg.display.language,
                cfg.display.theme,
                cfg.display.dual_row,
                cfg.display.show_resources,
                cfg.display.show_network,
                cfg.display.item_spacing,
            ),
        );

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
