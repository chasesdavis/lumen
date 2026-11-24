//! Per-user usage metering for pass-through billing.
//!
//! The billing model is "$3/mo base + storage at provider cost + margin",
//! so the meter that matters is bytes stored (per-provider, per-month) and
//! bytes retrieved. Writes flow in from the sync layer: each chunk upload
//! reports stored bytes; each range read reports retrieved bytes.

use lumen_core::error::{LumenError, Result};
use rusqlite::{params, Connection};
use std::sync::Mutex;

pub struct UsageLedger {
    conn: Mutex<Connection>,
}

/// Bind a core-layer UsageMeter to this ledger for one user. Every event
/// the VFS reports is upserted into the user's current billing period.
pub fn ledger_sink(
    ledger: std::sync::Arc<UsageLedger>,
    user_id: String,
) -> lumen_core::meter::UsageSink {
    std::sync::Arc::new(move |e| {
        let _ = ledger.record(
            &user_id,
            e.stored_bytes as i64,
            e.retrieved_bytes as i64,
            chrono::Utc::now(),
        );
    })
}

impl UsageLedger {
    pub fn open_memory() -> Result<Self> {
        Self::from_conn(Connection::open_in_memory())
    }

    pub fn open<P: AsRef<std::path::Path>>(path: P) -> Result<Self> {
        Self::from_conn(Connection::open(path))
    }

    fn from_conn(conn: std::result::Result<Connection, rusqlite::Error>) -> Result<Self> {
        let conn = conn.map_err(|e| LumenError::Storage(e.to_string()))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS usage_events (
                -- composite key enables the upsert used by record()
                user_id TEXT NOT NULL,
                period TEXT NOT NULL,
                stored_bytes INTEGER NOT NULL DEFAULT 0,
                retrieved_bytes INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (user_id, period)
            );
            CREATE INDEX IF NOT EXISTS idx_usage_user_period ON usage_events(user_id, period);",
        )
        .map_err(|e| LumenError::Storage(e.to_string()))?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// YYYY-MM billing period covering a timestamp.
    pub fn period_of(ts: chrono::DateTime<chrono::Utc>) -> String {
        ts.format("%Y-%m").to_string()
    }

    /// Record chunk activity for one user in the current period.
    /// Upsert keeps a single row per (user, period) so the table stays tiny.
    pub fn record(
        &self,
        user_id: &str,
        stored_bytes: i64,
        retrieved_bytes: i64,
        when: chrono::DateTime<chrono::Utc>,
    ) -> Result<()> {
        let period = Self::period_of(when);
        let conn = self.conn.lock().expect("usage ledger poisoned");
        conn.execute(
            "INSERT INTO usage_events (user_id, period, stored_bytes, retrieved_bytes)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(user_id, period) DO UPDATE SET
                stored_bytes = stored_bytes + ?3,
                retrieved_bytes = retrieved_bytes + ?4",
            params![user_id, period, stored_bytes, retrieved_bytes],
        )
        .map_err(|e| LumenError::Storage(e.to_string()))?;
        Ok(())
    }

    /// Aggregated usage snapshot for the user's current period.
    pub fn current_period_usage(&self, user_id: &str) -> Result<(i64, i64)> {
        let period = Self::period_of(chrono::Utc::now());
        self.period_usage(user_id, &period)
    }

    /// Aggregated usage for an explicit period - lets billing code and tests
    /// pin exact months instead of depending on wall-clock time.
    pub fn period_usage(&self, user_id: &str, period: &str) -> Result<(i64, i64)> {
        let conn = self.conn.lock().expect("usage ledger poisoned");
        conn.query_row(
            "SELECT stored_bytes, retrieved_bytes FROM usage_events WHERE user_id = ?1 AND period = ?2",
            params![user_id, period],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
        )
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok((0, 0)),
            other => Err(LumenError::Storage(other.to_string())),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_core::meter::{UsageEvent, UsageMeter};
    use std::sync::Arc;

    #[test]
    fn record_accumulates_within_period() {
        let ledger = UsageLedger::open_memory().unwrap();
        let t = chrono::Utc::now();
        ledger.record("user-1", 100, 10, t).unwrap();
        ledger.record("user-1", 50, 5, t).unwrap();
        let (stored, retrieved) = ledger.current_period_usage("user-1").unwrap();
        assert_eq!((stored, retrieved), (150, 15));
    }

    #[test]
    fn users_and_periods_are_isolated() {
        let ledger = UsageLedger::open_memory().unwrap();
        ledger.record("a", 100, 10, chrono::Utc::now()).unwrap();
        ledger.record("b", 200, 20, chrono::Utc::now()).unwrap();
        // Pin an explicit prior period to prove it never leaks into "now".
        ledger
            .record(
                "a",
                999,
                999,
                chrono::DateTime::from_timestamp(1767225600, 0).unwrap(),
            )
            .unwrap();
        assert_eq!(ledger.current_period_usage("a").unwrap(), (100, 10));
        assert_eq!(ledger.current_period_usage("b").unwrap(), (200, 20));
        assert_eq!(ledger.period_usage("a", "2026-01").unwrap(), (999, 999));
    }

    #[test]
    fn unknown_user_reports_zero() {
        let ledger = UsageLedger::open_memory().unwrap();
        assert_eq!(ledger.current_period_usage("nobody").unwrap(), (0, 0));
    }

    #[test]
    fn meter_events_flow_into_the_ledger_via_sink() {
        let ledger = Arc::new(UsageLedger::open_memory().unwrap());
        // Simulate exactly what VfsOps does: attach the ledger-backed sink
        // to a meter and report upload/download events.
        let meter = UsageMeter::new(ledger_sink(ledger.clone(), "user-9".into()));
        meter.report(UsageEvent {
            stored_bytes: 1024,
            retrieved_bytes: 0,
        });
        meter.report(UsageEvent {
            stored_bytes: 512,
            retrieved_bytes: 4096,
        });

        assert_eq!(ledger.current_period_usage("user-9").unwrap(), (1536, 4096));
        // Other users are never affected by another mount's traffic.
        assert_eq!(ledger.current_period_usage("user-10").unwrap(), (0, 0));
        // Meter totals mirror what reached the ledger.
        assert_eq!((meter.stored(), meter.retrieved()), (1536, 4096));
    }
}
