//! Shared, read-only server state.

use std::collections::BTreeMap;

use meilisearch_sdk::errors::{Error as MeiliError, ErrorCode};
use meilisearch_sdk::search::SearchQuery;
use regional_core::index::CONTENT_INDEXES;
use regional_core::meili::Client;
use regional_core::region::RegionConfig;

use crate::search::RawHit;

#[derive(Debug)]
pub struct AppState {
    pub client: Client,
    pub region: RegionConfig,
}

impl AppState {
    pub fn new(client: Client, region: RegionConfig) -> Self {
        Self { client, region }
    }

    /// Best-effort lookup of a place by name, used to resolve anchors the
    /// gazetteer does not know (neighbourhoods, parks, landmarks).
    pub async fn top_place(&self, name: &str, region_filter: &str) -> Option<RawHit> {
        let idx = self.client.index(regional_core::index::PLACES);
        let mut q = SearchQuery::new(&idx);
        q.with_query(name).with_limit(1).with_filter(region_filter);
        match q.execute::<RawHit>().await {
            Ok(res) => res.hits.into_iter().next().map(|h| h.result),
            Err(e) => {
                tracing::warn!(error = %e, name, "place lookup failed while resolving an anchor");
                None
            }
        }
    }

    /// Document count per content index. Reported by `describe_region` so a
    /// caller can tell an empty index from a query that found nothing.
    ///
    /// Counted with an empty search rather than the stats endpoint, because
    /// this service's key is search-only and stats need more than that. A
    /// count that cannot be read is `None`, never zero: zero makes
    /// `describe_region` tell the caller every search will come back empty.
    pub async fn doc_counts(&self) -> BTreeMap<String, Option<usize>> {
        let mut out = BTreeMap::new();
        for index in CONTENT_INDEXES {
            let idx = self.client.index(index);
            let mut q = SearchQuery::new(&idx);
            q.with_query("").with_limit(0);
            let n = match q.execute::<RawHit>().await {
                Ok(res) => res.estimated_total_hits,
                // Not created yet is genuinely empty.
                Err(MeiliError::Meilisearch(e)) if e.error_code == ErrorCode::IndexNotFound => {
                    Some(0)
                }
                Err(e) => {
                    tracing::warn!(index, error = %e, "could not count documents");
                    None
                }
            };
            out.insert(index.to_string(), n);
        }
        out
    }
}
