//! Byte-metering hook for pass-through billing.
//!
//! The VFS layer reports chunk traffic here (stored bytes on upload,
//! retrieved bytes on storage fetch). Billing code attaches a sink that
//! forwards events to the UsageLedger; without one, metering is a no-op
//! with negligible overhead so tests and CLI paths stay fast.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// One transfer observation. Identity (which user) is bound by the sink,
/// not the transport layer - a single VFS may serve multiple mounts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsageEvent {
    pub stored_bytes: u64,
    pub retrieved_bytes: u64,
}

/// Receives each event as it happens. Sync signature on purpose: sinks do
/// tiny SQLite upserts (see lumen-api-server usage bridging) and forcing
/// async would push blocking IO into hot read/write loops.
pub type UsageSink = Arc<dyn Fn(UsageEvent) + Send + Sync>;

#[derive(Default)]
pub struct UsageMeter {
    sink: Option<UsageSink>,
    total_stored: AtomicU64,
    total_retrieved: AtomicU64,
}

impl Clone for UsageMeter {
    fn clone(&self) -> Self {
        // Sink is shared, not duplicated; counters are cloned by value so
        // every clone reports into the same sink while totals stay local.
        Self {
            sink: self.sink.clone(),
            total_stored: AtomicU64::new(self.total_stored.load(Ordering::Relaxed)),
            total_retrieved: AtomicU64::new(self.total_retrieved.load(Ordering::Relaxed)),
        }
    }
}

impl UsageMeter {
    /// Attach a billing sink. Once attached, every upload/download through
    /// this meter is reported exactly once.
    pub fn new(sink: UsageSink) -> Self {
        Self {
            sink: Some(sink),
            total_stored: AtomicU64::new(0),
            total_retrieved: AtomicU64::new(0),
        }
    }

    /// No-op meter: counts locally, reports nowhere.
    pub fn disabled() -> Self {
        Self::default()
    }

    pub fn with_sink(mut self, sink: UsageSink) -> Self {
        self.sink = Some(sink);
        self
    }

    pub fn report(&self, event: UsageEvent) {
        self.total_stored.fetch_add(event.stored_bytes, Ordering::Relaxed);
        self.total_retrieved.fetch_add(event.retrieved_bytes, Ordering::Relaxed);
        if let Some(sink) = &self.sink {
            sink(event);
        }
    }

    /// Bytes reported as stored since meter construction (local copy).
    pub fn stored(&self) -> u64 {
        self.total_stored.load(Ordering::Relaxed)
    }

    /// Bytes reported as retrieved since meter construction (local copy).
    pub fn retrieved(&self) -> u64 {
        self.total_retrieved.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn reports_events_to_sink_and_keeps_totals() {
        let seen: Mutex<Vec<UsageEvent>> = Mutex::new(Vec::new());
        let seen_ref = Arc::new(seen);
        let sink_seen = Arc::clone(&seen_ref);
        let sink: UsageSink = Arc::new(move |e| sink_seen.lock().unwrap().push(e));
        let meter = UsageMeter::new(sink);
        meter.report(UsageEvent { stored_bytes: 10, retrieved_bytes: 0 });
        meter.report(UsageEvent { stored_bytes: 0, retrieved_bytes: 25 });
        assert_eq!(meter.stored(), 10);
        assert_eq!(meter.retrieved(), 25);
        assert_eq!(*seen_ref.lock().unwrap(), vec![
            UsageEvent { stored_bytes: 10, retrieved_bytes: 0 },
            UsageEvent { stored_bytes: 0, retrieved_bytes: 25 },
        ]);
    }

    #[test]
    fn disabled_meter_silently_counts() {
        let m = UsageMeter::disabled();
        m.report(UsageEvent { stored_bytes: 1, retrieved_bytes: 2 });
        assert_eq!(m.stored(), 1);
        assert_eq!(m.retrieved(), 2);
    }

    #[test]
    fn clones_share_sink_but_not_totals() {
        let seen = Mutex::new(Vec::<UsageEvent>::new());
        let seen_ref = Arc::new(seen);
        let sink_seen = Arc::clone(&seen_ref);
        let sink: UsageSink = Arc::new(move |e| sink_seen.lock().unwrap().push(e));
        let m = UsageMeter::new(sink);
        let n = m.clone();
        n.report(UsageEvent { stored_bytes: 5, retrieved_bytes: 0 });
        assert_eq!(m.stored(), 0); // clone kept old totals
        assert_eq!(n.stored(), 5);
        assert_eq!(seen_ref.lock().unwrap().len(), 1); // shared sink
    }
}
