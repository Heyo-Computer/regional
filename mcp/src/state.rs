//! Shared, read-only server state.

use std::collections::BTreeMap;

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
    pub async fn doc_counts(&self) -> BTreeMap<String, usize> {
        let mut out = BTreeMap::new();
        for index in CONTENT_INDEXES {
            let n = match self.client.index(index).get_stats().await {
                Ok(s) => s.number_of_documents,
                Err(e) => {
                    tracing::warn!(index, error = %e, "could not read index stats");
                    0
                }
            };
            out.insert(index.to_string(), n);
        }
        out
    }
}
