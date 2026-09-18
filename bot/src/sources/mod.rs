//! Data sources.
//!
//! A source knows how to walk one upstream corpus incrementally. It returns
//! a batch plus an opaque cursor that the scheduler persists, so progress
//! survives restarts and no source has to hold a whole state in memory.

pub mod crawl;
pub mod overpass;
pub mod submissions;
pub mod wikipedia;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use regional_core::model::Doc;
use regional_core::region::RegionConfig;
use serde_json::Value;

use crate::config::Config;
use crate::geocode::Geocoder;
use crate::http::Fetcher;
use crate::state::BotState;

/// Everything a source is allowed to reach.
pub struct Ctx {
    pub http: Arc<Fetcher>,
    pub region: Arc<RegionConfig>,
    pub state: Arc<BotState>,
    pub geocoder: Arc<Geocoder>,
    pub config: Arc<Config>,
}

pub struct Batch {
    pub docs: Vec<Doc>,
    /// Where to resume. Persisted verbatim by the scheduler.
    pub next_cursor: Option<Value>,
    /// True when this batch completed a full pass over the source's space.
    /// Only used for logging — the source itself decides how to start over.
    pub swept: bool,
}

impl Batch {
    pub fn new(docs: Vec<Doc>, next_cursor: Option<Value>) -> Self {
        Self {
            docs,
            next_cursor,
            swept: false,
        }
    }

    pub fn swept(mut self) -> Self {
        self.swept = true;
        self
    }

    pub fn empty() -> Self {
        Self {
            docs: Vec::new(),
            next_cursor: None,
            swept: false,
        }
    }
}

#[async_trait]
pub trait Source: Send + Sync {
    /// Stable name. Used as the cursor key and in `SOURCE_INTERVALS`.
    fn name(&self) -> &'static str;

    /// How often to run when nothing overrides it.
    fn default_interval(&self) -> Duration;

    /// Fetch the next slice of this source's corpus.
    async fn fetch(&self, ctx: &Ctx, cursor: Option<Value>) -> Result<Batch>;
}

/// Every source the indexer knows how to run.
pub fn all() -> Vec<Box<dyn Source>> {
    vec![
        // Submissions first: a reviewer's approval should show up quickly,
        // and the batch is tiny compared with a source sweep.
        Box::new(submissions::Submissions),
        Box::new(overpass::Overpass),
        Box::new(wikipedia::Wikipedia),
        Box::new(crawl::Crawl),
    ]
}

/// Read a `usize` out of an opaque cursor, defaulting on anything odd, so a
/// cursor written by an older build can never wedge a source.
pub fn cursor_usize(cursor: &Option<Value>, key: &str) -> usize {
    cursor
        .as_ref()
        .and_then(|c| c.get(key))
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_source_has_a_distinct_name_and_a_nonzero_interval() {
        let sources = all();
        let mut names: Vec<&str> = sources.iter().map(|s| s.name()).collect();
        names.sort_unstable();
        let unique = names.len();
        names.dedup();
        assert_eq!(names.len(), unique, "source names must be unique");
        for s in &sources {
            assert!(!s.default_interval().is_zero(), "{}", s.name());
        }
    }

    #[test]
    fn a_missing_or_malformed_cursor_starts_from_zero() {
        assert_eq!(cursor_usize(&None, "tile"), 0);
        assert_eq!(cursor_usize(&Some(serde_json::json!({})), "tile"), 0);
        assert_eq!(
            cursor_usize(&Some(serde_json::json!({"tile": "x"})), "tile"),
            0
        );
        assert_eq!(
            cursor_usize(&Some(serde_json::json!({"tile": 7})), "tile"),
            7
        );
    }
}
