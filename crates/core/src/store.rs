//! History storage layer (R12).
//!
//! SQLite is the **single source of truth** for historical data: the trend
//! chart queries straight from the database, so a month of history survives
//! process restarts. CSV is *not* a storage format — it is generated on
//! demand from the database when the user exports.
//!
//! Only CPU and memory percentages are stored (both 0–100). Network rates are
//! deliberately excluded: they are shown live on the indicator but never
//! persisted or charted.
//!
//! The store keeps a rolling window (`retention_days`, default 30) and prunes
//! anything older. For charting, raw samples are aggregated into fixed-width
//! time buckets on the fly (`GROUP BY ts / bucket * bucket`), so a month-long
//! view renders a bounded number of points instead of ~1.3M raw rows.

use rusqlite::{params, Connection, OptionalExtension};

/// One stored sample: a unix timestamp (seconds) plus CPU and memory usage
/// percentages.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sample {
    pub ts: i64,
    pub cpu: f64,
    pub mem: f64,
}

/// One aggregated bucket for charting: the bucket's start timestamp plus the
/// average CPU and memory over the bucket.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bucket {
    pub ts: i64,
    pub cpu: f64,
    pub mem: f64,
}

/// Thin wrapper over a SQLite connection holding the history schema.
pub struct Store {
    conn: Connection,
}

/// Floor `ts` down to the nearest boundary of a wall-clock grid whose spacing
/// is `step` seconds and whose zero sits at UTC offset `offset` (0 for the
/// epoch/UTC-midnight grid, the machine's local offset for a local-midnight
/// grid). This is the same alignment [`Store::buckets_offset`] applies to
/// bucket starts, exposed standalone so chart code can align a query window
/// or floor a label timestamp onto the identical grid.
///
/// `div_euclid` keeps the floor semantics for negative timestamps (samples
/// before 1970 or negative offsets), where plain integer division truncating
/// toward zero would land on the wrong side of the boundary.
pub fn floor_to_grid(ts: i64, step: i64, offset: i64) -> i64 {
    let step = step.max(1);
    (ts + offset).div_euclid(step) * step - offset
}

/// Which of `len` evenly-spaced data points a chart should label, when it
/// wants exactly `count` labels spread from the first point to the last:
/// indices `round(k * (len - 1) / (count - 1))` for `k` in `0..count`. Fewer
/// points than labels returns every index; `count == 1` returns just the
/// first. Mirrors the shape the trend chart's axis uses so a caller can
/// decide, per point, whether its own label text will actually be painted.
pub fn spread_label_indices(len: usize, count: usize) -> Vec<usize> {
    match count {
        0 => Vec::new(),
        1 => {
            if len == 0 {
                Vec::new()
            } else {
                vec![0]
            }
        }
        count if count >= len => (0..len).collect(),
        count => (0..count)
            .map(|k| (k as f32 * (len - 1) as f32 / (count - 1) as f32).round() as usize)
            .collect(),
    }
}

impl Store {
    /// Open (or create) the database at `path` and ensure the schema exists.
    /// Uses WAL with `synchronous=NORMAL` so steady 1–3s inserts stay cheap and
    /// do not fsync on every write.
    pub fn open(path: &std::path::Path) -> anyhow::Result<Self> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS samples (
                 ts  INTEGER PRIMARY KEY,
                 cpu REAL NOT NULL,
                 mem REAL NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_samples_ts ON samples (ts);",
        )?;
        Ok(Self { conn })
    }

    /// Open an in-memory database (used by tests).
    #[cfg(test)]
    fn in_memory() -> anyhow::Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS samples (
                 ts  INTEGER PRIMARY KEY,
                 cpu REAL NOT NULL,
                 mem REAL NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_samples_ts ON samples (ts);",
        )?;
        Ok(Self { conn })
    }

    /// Insert one sample. Returns `false` (a no-op) when either value is not a
    /// finite number: SQLite's `REAL NOT NULL` column rejects a NaN binding
    /// with a constraint error (verified empirically), and first/wake ticks
    /// legitimately carry NaN — skipping the row is correct, not an error. A
    /// duplicate timestamp overwrites the previous value (`INSERT OR REPLACE`),
    /// which keeps re-runs and clock adjustments from producing duplicate rows.
    /// Real I/O failures still propagate.
    pub fn insert(&self, ts: i64, cpu: f64, mem: f64) -> anyhow::Result<bool> {
        if !cpu.is_finite() || !mem.is_finite() {
            return Ok(false);
        }
        self.conn.execute(
            "INSERT OR REPLACE INTO samples (ts, cpu, mem) VALUES (?1, ?2, ?3)",
            params![ts, cpu, mem],
        )?;
        Ok(true)
    }

    /// Insert many samples in a single transaction. Batched writes keep the
    /// per-insert overhead low and avoid fsync storms on the steady tick.
    pub fn insert_batch(&self, rows: &[(i64, f64, f64)]) -> anyhow::Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        {
            let mut stmt =
                tx.prepare("INSERT OR REPLACE INTO samples (ts, cpu, mem) VALUES (?1, ?2, ?3)")?;
            for (ts, cpu, mem) in rows {
                stmt.execute(params![ts, cpu, mem])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Delete samples older than `retention_days` days before `now`. Returns
    /// the number of rows removed.
    pub fn prune(&self, now: i64, retention_days: u32) -> anyhow::Result<usize> {
        let cutoff = now - i64::from(retention_days) * 86_400;
        let n = self
            .conn
            .execute("DELETE FROM samples WHERE ts < ?1", params![cutoff])?;
        Ok(n)
    }

    /// Return raw samples in `[from, to]` ordered by time. Used for short
    /// ranges where full resolution is wanted.
    pub fn range(&self, from: i64, to: i64) -> anyhow::Result<Vec<Sample>> {
        let mut stmt = self
            .conn
            .prepare("SELECT ts, cpu, mem FROM samples WHERE ts >= ?1 AND ts <= ?2 ORDER BY ts")?;
        let rows = stmt.query_map(params![from, to], |row| {
            Ok(Sample {
                ts: row.get(0)?,
                cpu: row.get(1)?,
                mem: row.get(2)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Return the average CPU and memory per `bucket_secs`-wide bucket in
    /// `[from, to]`, ordered by bucket start. This is what the trend chart
    /// renders for long ranges: a bounded number of points regardless of how
    /// many raw samples exist. Bucket boundaries align to the UNIX epoch
    /// (UTC midnight). Callers that need buckets on a wall-clock grid —
    /// local midnight, hour boundaries, etc. — pass an offset through
    /// [`Self::buckets_offset`].
    pub fn buckets(&self, from: i64, to: i64, bucket_secs: i64) -> anyhow::Result<Vec<Bucket>> {
        self.buckets_offset(from, to, bucket_secs, 0)
    }

    /// The same aggregation with an explicit timezone offset: bucket
    /// boundaries align to the wall clock of a timezone whose UTC offset is
    /// `offset_secs` (i.e. boundaries are `(ts + off) div b * b - off`).
    /// A chart showing 5-day buckets in a +8h zone (offset 28800 s) gets
    /// boundaries at local midnight; the plain epoch-aligned call would
    /// instead put them at local 08:00, which reads as an off-by-hours grid
    /// on the axis.
    ///
    /// All real samples have positive unix seconds and offsets are bounded
    /// by ±14 hours, so `ts + off` stays positive and SQLite's integer
    /// division truncation ("toward zero") equals Euclidean floor here. No
    /// negative-side hazard.
    pub fn buckets_offset(
        &self,
        from: i64,
        to: i64,
        bucket_secs: i64,
        offset_secs: i64,
    ) -> anyhow::Result<Vec<Bucket>> {
        let bucket = bucket_secs.max(1);
        let mut stmt = self.conn.prepare(
            "SELECT ((ts + ?4) / ?3) * ?3 - ?4 AS b, AVG(cpu), AVG(mem)
             FROM samples
             WHERE ts >= ?1 AND ts <= ?2
             GROUP BY b
             ORDER BY b",
        )?;
        let rows = stmt.query_map(params![from, to, bucket, offset_secs], |row| {
            Ok(Bucket {
                ts: row.get(0)?,
                cpu: row.get(1)?,
                mem: row.get(2)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// The newest sample, if any. Handy for "last seen" displays and tests.
    pub fn latest(&self) -> anyhow::Result<Option<Sample>> {
        let mut stmt = self
            .conn
            .prepare("SELECT ts, cpu, mem FROM samples ORDER BY ts DESC LIMIT 1")?;
        let row = stmt
            .query_row([], |row| {
                Ok(Sample {
                    ts: row.get(0)?,
                    cpu: row.get(1)?,
                    mem: row.get(2)?,
                })
            })
            .optional()?;
        Ok(row)
    }

    /// Total number of stored samples.
    pub fn count(&self) -> anyhow::Result<i64> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM samples", [], |r| r.get(0))?;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_read_back() {
        let s = Store::in_memory().unwrap();
        s.insert(100, 25.0, 60.0).unwrap();
        let latest = s.latest().unwrap().unwrap();
        assert_eq!(latest.ts, 100);
        assert_eq!(latest.cpu, 25.0);
        assert_eq!(latest.mem, 60.0);
        assert_eq!(s.count().unwrap(), 1);
    }

    #[test]
    fn duplicate_timestamp_overwrites() {
        let s = Store::in_memory().unwrap();
        s.insert(100, 10.0, 10.0).unwrap();
        s.insert(100, 90.0, 90.0).unwrap();
        assert_eq!(s.count().unwrap(), 1);
        assert_eq!(s.latest().unwrap().unwrap().cpu, 90.0);
    }

    #[test]
    fn nan_sample_is_skipped_not_error() {
        // SQLite rejects a NaN binding into a `REAL NOT NULL` column, so the
        // store must skip non-finite samples (first/wake ticks) as a no-op
        // rather than error or store a poison row.
        let s = Store::in_memory().unwrap();
        assert!(!s.insert(100, f64::NAN, 50.0).unwrap());
        assert!(!s.insert(101, 50.0, f64::NAN).unwrap());
        assert!(!s.insert(102, f64::NAN, f64::NAN).unwrap());
        assert_eq!(s.count().unwrap(), 0, "NaN rows must not be stored");
        // A finite sample right after still inserts and becomes the latest.
        assert!(s.insert(103, 50.0, 50.0).unwrap());
        assert_eq!(s.count().unwrap(), 1);
        assert_eq!(s.latest().unwrap().unwrap().ts, 103);
    }

    #[test]
    fn batch_insert_and_range_query() {
        let s = Store::in_memory().unwrap();
        let rows: Vec<(i64, f64, f64)> = (0..10).map(|i| (i * 2, i as f64, i as f64)).collect();
        s.insert_batch(&rows).unwrap();
        assert_eq!(s.count().unwrap(), 10);

        let r = s.range(4, 10).unwrap();
        let ts: Vec<i64> = r.iter().map(|x| x.ts).collect();
        assert_eq!(ts, vec![4, 6, 8, 10]);
    }

    #[test]
    fn prune_removes_old_rows() {
        let s = Store::in_memory().unwrap();
        let now = 1_000_000i64;
        // One sample well inside 30 days, one far older.
        s.insert(now - 86_400, 1.0, 1.0).unwrap(); // 1 day old: keep
        s.insert(now - 86_400 * 60, 2.0, 2.0).unwrap(); // 60 days old: prune
        let removed = s.prune(now, 30).unwrap();
        assert_eq!(removed, 1);
        assert_eq!(s.count().unwrap(), 1);
        assert_eq!(s.latest().unwrap().unwrap().cpu, 1.0);
    }

    #[test]
    fn buckets_average_within_window() {
        let s = Store::in_memory().unwrap();
        // Bucket width 10s. Two samples in bucket [0,10), one in [10,20).
        s.insert(0, 10.0, 20.0).unwrap();
        s.insert(5, 30.0, 40.0).unwrap();
        s.insert(12, 50.0, 60.0).unwrap();

        let b = s.buckets(0, 20, 10).unwrap();
        assert_eq!(b.len(), 2);
        assert_eq!(b[0].ts, 0);
        assert_eq!(b[0].cpu, 20.0); // avg(10,30)
        assert_eq!(b[0].mem, 30.0); // avg(20,40)
        assert_eq!(b[1].ts, 10);
        assert_eq!(b[1].cpu, 50.0);
        assert_eq!(b[1].mem, 60.0);
    }

    #[test]
    fn buckets_respect_range_bounds() {
        let s = Store::in_memory().unwrap();
        for i in 0..30 {
            s.insert(i, i as f64, i as f64).unwrap();
        }
        let b = s.buckets(10, 19, 5).unwrap();
        // Only buckets whose rows fall inside [10,19].
        assert!(b.iter().all(|x| x.ts >= 10 && x.ts <= 19));
    }

    #[test]
    fn buckets_align_to_epoch_by_default() {
        let s = Store::in_memory().unwrap();
        // One sample near the start of each of three 10-wide buckets.
        s.insert(3, 10.0, 10.0).unwrap();
        s.insert(13, 20.0, 20.0).unwrap();
        s.insert(23, 30.0, 30.0).unwrap();
        let b = s.buckets(0, 30, 10).unwrap();
        let starts: Vec<i64> = b.iter().map(|x| x.ts).collect();
        // Epoch-aligned: boundaries are multiples of the bucket width.
        assert_eq!(starts, vec![0, 10, 20]);
    }

    #[test]
    fn buckets_offset_shifts_boundary_to_wall_clock() {
        let s = Store::in_memory().unwrap();
        // A +8h zone (28800 s) with a day-wide (86400) bucket: a sample at
        // 02:00 UTC belongs to the day that started at 16:00 UTC the prior
        // day, which is 00:00 local (+8) — the local-midnight grid.
        let utc_day_1_0200 = 86_400 + 2 * 3600; // 93600
        s.insert(utc_day_1_0200, 42.0, 42.0).unwrap();
        let offset = s.buckets_offset(0, 200_000, 86_400, 28_800).unwrap();
        // Boundary ts = ((93600 + 28800)/86400)*86400 - 28800 = 86400 - 28800
        // = 57600, i.e. the local midnight that opened this bucket.
        assert_eq!(offset[0].ts, 57_600);
        // Without the offset the same sample buckets to 86400 (UTC midnight).
        let plain = s.buckets(0, 200_000, 86_400).unwrap();
        assert_eq!(plain[0].ts, 86_400);
    }

    #[test]
    fn empty_store_returns_none_and_empty() {
        let s = Store::in_memory().unwrap();
        assert!(s.latest().unwrap().is_none());
        assert!(s.range(0, 100).unwrap().is_empty());
        assert!(s.buckets(0, 100, 10).unwrap().is_empty());
        assert!(s.buckets_offset(0, 100, 10, 3600).unwrap().is_empty());
        assert_eq!(s.count().unwrap(), 0);
    }

    #[test]
    fn floor_to_grid_snaps_down_to_the_wall_clock_grid() {
        // Epoch grid (+0 offset): 3 725 s lands on the minute 3 720 s below.
        assert_eq!(floor_to_grid(3_725, 60, 0), 3_720);
        // Already on a boundary: unchanged.
        assert_eq!(floor_to_grid(3_660, 60, 0), 3_660);
        // Local grid (+8h): boundaries are local midnight multiples, i.e.
        // 57 600 = 16:00 UTC of the prior day is 00:00 local. 93 600 (02:00
        // UTC) floors to it — the same sample the buckets_offset test pins.
        assert_eq!(floor_to_grid(93_600, 86_400, 28_800), 57_600);
        // Negative timestamps floor downward, not toward zero: -5 with a
        // step of 10 belongs to the boundary -10, while truncating division
        // would wrongly give 0.
        assert_eq!(floor_to_grid(-5, 10, 0), -10);
        // A zero/garbage step is clamped to 1, never a division by zero.
        assert_eq!(floor_to_grid(42, 0, 0), 42);
    }

    #[test]
    fn spread_label_indices_pins_ends_and_stays_unique() {
        // 6 of 15 points: first and last are always labeled, indices advance.
        let idx = spread_label_indices(15, 6);
        assert_eq!(idx.len(), 6);
        assert_eq!(idx.first(), Some(&0));
        assert_eq!(idx.last(), Some(&14));
        let mut sorted = idx.clone();
        sorted.dedup();
        assert_eq!(sorted.len(), idx.len(), "no index repeats");
        // Fewer points than labels: every point gets a label.
        assert_eq!(spread_label_indices(3, 6), vec![0, 1, 2]);
        // Degenerate inputs never panic and never invent an index.
        assert_eq!(spread_label_indices(0, 6), Vec::<usize>::new());
        assert_eq!(spread_label_indices(10, 0), Vec::<usize>::new());
        assert_eq!(spread_label_indices(10, 1), vec![0]);
        // Every returned index is inside the data range.
        for i in spread_label_indices(100, 6) {
            assert!(i < 100);
        }
    }
}
