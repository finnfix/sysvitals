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

    /// Insert one sample. A duplicate timestamp overwrites the previous value
    /// (`INSERT OR REPLACE`), which keeps re-runs and clock adjustments from
    /// producing duplicate rows.
    pub fn insert(&self, ts: i64, cpu: f64, mem: f64) -> anyhow::Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO samples (ts, cpu, mem) VALUES (?1, ?2, ?3)",
            params![ts, cpu, mem],
        )?;
        Ok(())
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
    /// many raw samples exist.
    pub fn buckets(&self, from: i64, to: i64, bucket_secs: i64) -> anyhow::Result<Vec<Bucket>> {
        let bucket = bucket_secs.max(1);
        let mut stmt = self.conn.prepare(
            "SELECT (ts / ?3) * ?3 AS b, AVG(cpu), AVG(mem)
             FROM samples
             WHERE ts >= ?1 AND ts <= ?2
             GROUP BY b
             ORDER BY b",
        )?;
        let rows = stmt.query_map(params![from, to, bucket], |row| {
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
    fn empty_store_returns_none_and_empty() {
        let s = Store::in_memory().unwrap();
        assert!(s.latest().unwrap().is_none());
        assert!(s.range(0, 100).unwrap().is_empty());
        assert!(s.buckets(0, 100, 10).unwrap().is_empty());
        assert_eq!(s.count().unwrap(), 0);
    }
}
