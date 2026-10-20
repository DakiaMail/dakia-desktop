//! Process-local measurements for repeatable mail acceptance runs.
//! Counters contain durations only, never account identifiers or mail content.

use serde::Serialize;
use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

#[derive(Default)]
struct Timings {
    count: AtomicU64,
    total_nanos: AtomicU64,
    max_nanos: AtomicU64,
}

impl Timings {
    fn record(&self, duration: Duration) {
        let nanos = u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX);
        self.total_nanos.fetch_add(nanos, Ordering::Relaxed);
        self.max_nanos.fetch_max(nanos, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Release);
    }

    fn snapshot(&self) -> TimingSnapshot {
        let count = self.count.load(Ordering::Acquire);
        let total_ms = self.total_nanos.load(Ordering::Relaxed) as f64 / 1_000_000.0;
        TimingSnapshot {
            count,
            total_ms,
            mean_ms: (count > 0).then(|| total_ms / count as f64),
            max_ms: (count > 0)
                .then(|| self.max_nanos.load(Ordering::Relaxed) as f64 / 1_000_000.0),
        }
    }
}

static PUBLICATION_TRANSACTIONS: Timings = Timings {
    count: AtomicU64::new(0),
    total_nanos: AtomicU64::new(0),
    max_nanos: AtomicU64::new(0),
};
static SUBMISSION_QUEUE: Timings = Timings {
    count: AtomicU64::new(0),
    total_nanos: AtomicU64::new(0),
    max_nanos: AtomicU64::new(0),
};

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TimingSnapshot {
    pub count: u64,
    pub total_ms: f64,
    pub mean_ms: Option<f64>,
    pub max_ms: Option<f64>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MailMetricsSnapshot {
    pub publication_transactions: TimingSnapshot,
    pub submission_queue: TimingSnapshot,
}

pub fn snapshot() -> MailMetricsSnapshot {
    MailMetricsSnapshot {
        publication_transactions: PUBLICATION_TRANSACTIONS.snapshot(),
        submission_queue: SUBMISSION_QUEUE.snapshot(),
    }
}

/// Start after acquiring the SQLite write transaction. Record only successful
/// publication commits; rolled-back or rejected receipts are not counted.
pub struct PublicationTransactionTimer(Instant);

impl PublicationTransactionTimer {
    pub fn start() -> Self {
        Self(Instant::now())
    }
    pub fn committed(self) {
        PUBLICATION_TRANSACTIONS.record(self.0.elapsed());
    }
}

/// Time from a submission's durable enqueue to ownership of its first SMTP
/// attempt. Sent-copy claims are separate work and must not enter this metric.
pub fn record_submission_queue_wait(duration: Duration) {
    SUBMISSION_QUEUE.record(duration);
    if cfg!(debug_assertions) && std::env::var("DAKIA_ACCEPTANCE_METRICS").as_deref() == Ok("1") {
        println!(
            "DAKIA_METRIC {}",
            serde_json::json!({
                "name": "submission-queue-wait",
                "durationMs": duration.as_secs_f64() * 1000.0,
                "mailMetrics": snapshot(),
            })
        );
    }
}
