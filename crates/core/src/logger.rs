//! Minimal file logger: one file per local calendar day, pruned to a
//! configurable retention window (default 7 days).
//!
//! Design: the core crate stays free of platform time APIs and timezone
//! assumptions. The display layer (which can query the real local clock, e.g.
//! `GetLocalTime` on Windows) passes in the already-formatted local date
//! (`YYYY-MM-DD`) and time (`HH:MM:SS`) strings. This crate only handles file
//! naming, rolling, formatting and retention pruning.
//!
//! Logging must never break the indicator, so every public function swallows
//! I/O errors silently.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Default number of daily log files to keep.
pub const DEFAULT_RETENTION_DAYS: u32 = 7;

static LOG_FILE: Mutex<Option<File>> = Mutex::new(None);
static LOG_DATE: Mutex<String> = Mutex::new(String::new());

/// Where log files live: `<data_dir>/logs/`.
pub fn log_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("logs")
}

/// Open (or roll to) the log file for the given local date. Returns the file
/// handle, or `None` if the directory cannot be created.
fn open_for_date(dir: &Path, date: &str) -> Option<File> {
    let _ = fs::create_dir_all(dir);
    let path = dir.join(format!("sysvitals-{date}.log"));
    OpenOptions::new().create(true).append(true).open(path).ok()
}

/// Append one line. `date` is the local date `YYYY-MM-DD` and `time` the local
/// time `HH:MM:SS`, both supplied by the caller from the platform clock.
/// `level` is a short tag like `INFO` / `WARN` / `ERROR`. Rolls to a new file
/// when the date changes. Never panics.
pub fn log_line(data_dir: &Path, date: &str, time: &str, level: &str, message: &str) {
    let mut file_guard = LOG_FILE.lock().unwrap_or_else(|e| e.into_inner());
    let mut date_guard = LOG_DATE.lock().unwrap_or_else(|e| e.into_inner());
    if *date_guard != date {
        *file_guard = open_for_date(&log_dir(data_dir), date);
        *date_guard = date.to_string();
    }
    if let Some(file) = file_guard.as_mut() {
        let _ = writeln!(file, "{date} {time} [{level}] {message}");
    }
}

/// Delete log files older than `retention_days`. Only touches files matching
/// the `sysvitals-YYYY-MM-DD.log` pattern so unrelated files are never
/// removed. Best-effort; errors are ignored. `today` is the current local date.
pub fn prune_logs(data_dir: &Path, today: &str, retention_days: u32) {
    let dir = log_dir(data_dir);
    let Ok(entries) = fs::read_dir(&dir) else {
        return;
    };
    let Some(today_days) = date_to_days(today) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(file_date) = parse_log_name(&name) else {
            continue;
        };
        let Some(file_days) = date_to_days(&file_date) else {
            continue;
        };
        if today_days.saturating_sub(file_days) > retention_days as i64 {
            let _ = fs::remove_file(entry.path());
        }
    }
}

fn parse_log_name(name: &str) -> Option<String> {
    let rest = name.strip_prefix("sysvitals-")?.strip_suffix(".log")?;
    if rest.len() != 10 || !rest.as_bytes()[4].eq(&b'-') || !rest.as_bytes()[7].eq(&b'-') {
        return None;
    }
    if !rest.bytes().enumerate().all(|(i, b)| {
        if i == 4 || i == 7 {
            b == b'-'
        } else {
            b.is_ascii_digit()
        }
    }) {
        return None;
    }
    Some(rest.to_string())
}

/// Days since 1970-01-01 for a `YYYY-MM-DD` string, used to compare file ages.
fn date_to_days(date: &str) -> Option<i64> {
    let y: i64 = date.get(0..4)?.parse().ok()?;
    let m: u32 = date.get(5..7)?.parse().ok()?;
    let d: u32 = date.get(8..10)?.parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    Some(days_from_civil(y, m, d))
}

/// Days since 1970-01-01 for a civil (year, month, day) date. Inverse of the
/// standard civil-from-days algorithm.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn days_from_civil_known_dates() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(1970, 1, 2), 1);
        assert_eq!(days_from_civil(2000, 3, 1), 11017);
        // Monotonic across a leap-year boundary.
        assert!(days_from_civil(2024, 3, 1) > days_from_civil(2024, 2, 28));
    }

    #[test]
    fn parse_log_name_accepts_only_pattern() {
        assert_eq!(
            parse_log_name("sysvitals-2026-10-19.log"),
            Some("2026-10-19".into())
        );
        assert_eq!(parse_log_name("sysvitals-2026-10-1.log"), None);
        assert_eq!(parse_log_name("other-2026-10-19.log"), None);
        assert_eq!(parse_log_name("sysvitals-2026-10-19.txt"), None);
    }

    #[test]
    fn prune_removes_only_old_matching_files() {
        let dir = std::env::temp_dir().join(format!("sysvitals-logtest-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let logs = log_dir(&dir);
        fs::create_dir_all(&logs).unwrap();
        for name in [
            "sysvitals-2026-10-09.log",
            "sysvitals-2026-10-17.log",
            "sysvitals-2026-10-19.log",
            "notes.txt",
        ] {
            fs::write(logs.join(name), b"x").unwrap();
        }
        prune_logs(&dir, "2026-10-19", DEFAULT_RETENTION_DAYS);
        assert!(
            !logs.join("sysvitals-2026-10-09.log").exists(),
            "old removed"
        );
        assert!(
            logs.join("sysvitals-2026-10-17.log").exists(),
            "recent kept"
        );
        assert!(logs.join("sysvitals-2026-10-19.log").exists(), "today kept");
        assert!(logs.join("notes.txt").exists(), "decoy untouched");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn log_line_writes_and_rolls() {
        let dir = std::env::temp_dir().join(format!("sysvitals-logwrite-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        log_line(&dir, "2026-10-19", "13:00:00", "INFO", "hello");
        log_line(&dir, "2026-10-19", "13:01:00", "WARN", "world");
        let f1 = log_dir(&dir).join("sysvitals-2026-10-19.log");
        let content = fs::read_to_string(&f1).unwrap();
        assert!(content.contains("2026-10-19 13:00:00 [INFO] hello"));
        assert!(content.contains("2026-10-19 13:01:00 [WARN] world"));
        // Rolling to the next day creates a second file.
        log_line(&dir, "2026-10-20", "09:00:00", "INFO", "next day");
        assert!(log_dir(&dir).join("sysvitals-2026-10-20.log").exists());
        // Reset globals so other tests are unaffected.
        *LOG_FILE.lock().unwrap() = None;
        *LOG_DATE.lock().unwrap() = String::new();
        let _ = fs::remove_dir_all(&dir);
    }
}
