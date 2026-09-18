//! What the indexer is doing, exposed over the health endpoint.

use std::collections::BTreeMap;
use std::sync::Arc;

use regional_core::model::now_ts;
use serde::Serialize;
use tokio::sync::RwLock;

use crate::pipeline::IngestStats;

#[derive(Debug, Default, Clone, Serialize)]
pub struct SourceMetrics {
    pub runs: u64,
    pub last_run_at: Option<i64>,
    pub last_run_iso: Option<String>,
    pub last_error: Option<String>,
    /// Drives the backoff: a source that keeps failing is tried less often.
    pub consecutive_errors: u32,
    pub swept: u64,
    pub totals: IngestStats,
}

#[derive(Debug, Default)]
pub struct Metrics {
    started_at: i64,
    sources: BTreeMap<String, SourceMetrics>,
}

#[derive(Clone, Default)]
pub struct SharedMetrics(Arc<RwLock<Metrics>>);

impl SharedMetrics {
    pub fn new() -> Self {
        Self(Arc::new(RwLock::new(Metrics {
            started_at: now_ts(),
            sources: BTreeMap::new(),
        })))
    }

    pub async fn record_success(&self, source: &str, stats: IngestStats, swept: bool) {
        let mut m = self.0.write().await;
        let e = m.sources.entry(source.to_string()).or_default();
        e.runs += 1;
        e.last_run_at = Some(now_ts());
        e.last_run_iso = chrono::Utc::now().to_rfc3339().into();
        e.last_error = None;
        e.consecutive_errors = 0;
        e.swept += u64::from(swept);
        e.totals.merge(stats);
    }

    pub async fn record_error(&self, source: &str, error: &str) -> u32 {
        let mut m = self.0.write().await;
        let e = m.sources.entry(source.to_string()).or_default();
        e.runs += 1;
        e.last_run_at = Some(now_ts());
        e.last_run_iso = chrono::Utc::now().to_rfc3339().into();
        e.last_error = Some(error.chars().take(500).collect());
        e.consecutive_errors = e.consecutive_errors.saturating_add(1);
        e.consecutive_errors
    }

    pub async fn snapshot(&self) -> (i64, BTreeMap<String, SourceMetrics>) {
        let m = self.0.read().await;
        (m.started_at, m.sources.clone())
    }
}
