//! Indexer bookkeeping, persisted in Meilisearch.
//!
//! Per-source cursors, the crawl frontier and the geocoding cache all live
//! in the `bot_state` index. Meilisearch is the only stateful service in the
//! stack, so keeping state here means the indexer needs no volume of its own
//! and survives VM replacement — which matters on heyo, where the
//! `firecracker_containerd` backend has no `--mount`.
//!
//! It is not a queue, and it is not pretending to be one: a single indexer
//! owns the frontier, and claims are marked before work starts so a restart
//! does not replay the same page forever.

use std::collections::HashMap;

use anyhow::Result;
use meilisearch_sdk::search::{SearchQuery, Selectors};
use regional_core::id::stable_id;
use regional_core::index::BOT_STATE;
use regional_core::meili::{self, Client};
use regional_core::model::now_ts;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One row in `bot_state`. A single flat shape keeps the index settings
/// simple; `kind` separates the three uses.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateDoc {
    pub id: String,
    /// `cursor`, `frontier` or `geocache`.
    pub kind: String,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub value: Option<Value>,

    // frontier
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub depth: Option<u32>,
    #[serde(default)]
    pub priority: Option<i64>,
    #[serde(default)]
    pub default_city: Option<String>,
    #[serde(default)]
    pub discovered_at: Option<i64>,

    // geocache
    #[serde(default)]
    pub lat: Option<f64>,
    #[serde(default)]
    pub lng: Option<f64>,

    pub updated_at: i64,
}

#[derive(Debug, Clone)]
pub struct FrontierEntry {
    pub url: String,
    pub depth: u32,
    pub default_city: Option<String>,
}

pub struct BotState {
    client: Client,
}

impl BotState {
    pub fn new(client: Client) -> Self {
        Self { client }
    }

    /// The Meilisearch client, for the submissions source, which reads and
    /// writes the submissions index rather than `bot_state`.
    pub fn client(&self) -> &Client {
        &self.client
    }

    fn index(&self) -> meili::Index {
        self.client.index(BOT_STATE)
    }

    // ------------------------------------------------------------- cursors

    pub async fn get_cursor(&self, source: &str) -> Option<Value> {
        let id = format!("cursor-{source}");
        match self.index().get_document::<StateDoc>(&id).await {
            Ok(doc) => doc.value,
            Err(_) => None,
        }
    }

    pub async fn set_cursor(&self, source: &str, value: Option<Value>) -> Result<()> {
        let doc = StateDoc {
            id: format!("cursor-{source}"),
            kind: "cursor".into(),
            source: source.into(),
            value,
            url: None,
            host: None,
            status: None,
            depth: None,
            priority: None,
            default_city: None,
            discovered_at: None,
            lat: None,
            lng: None,
            updated_at: now_ts(),
        };
        meili::upsert_chunked(&self.client, BOT_STATE, &[doc]).await?;
        Ok(())
    }

    // ------------------------------------------------------------ frontier

    /// Add URLs that are not already known. Existing rows are left alone so
    /// a page already crawled is not silently reset to pending.
    pub async fn frontier_add(&self, entries: &[FrontierEntry]) -> Result<usize> {
        if entries.is_empty() {
            return Ok(0);
        }
        let ids: Vec<String> = entries.iter().map(|e| frontier_id(&e.url)).collect();
        let known = self.known_ids(&ids).await?;

        let now = now_ts();
        let fresh: Vec<StateDoc> = entries
            .iter()
            .zip(&ids)
            .filter(|(_, id)| !known.contains(*id))
            .map(|(e, id)| StateDoc {
                id: id.clone(),
                kind: "frontier".into(),
                source: "crawl".into(),
                value: None,
                url: Some(e.url.clone()),
                host: crate::http::host_of(&e.url).ok(),
                status: Some("pending".into()),
                depth: Some(e.depth),
                // Shallower pages first: a site's index pages are where the
                // links are, so breadth-first expands coverage fastest.
                priority: Some(e.depth as i64),
                default_city: e.default_city.clone(),
                discovered_at: Some(now),
                lat: None,
                lng: None,
                updated_at: now,
            })
            .collect();

        let n = fresh.len();
        meili::upsert_chunked(&self.client, BOT_STATE, &fresh).await?;
        Ok(n)
    }

    /// Claim up to `limit` pending URLs, marking them in progress before
    /// returning so a crash cannot leave them to be fetched forever.
    pub async fn frontier_claim(&self, limit: usize) -> Result<Vec<FrontierEntry>> {
        let idx = self.index();
        let sort = ["priority:asc", "discovered_at:asc"];
        let mut q = SearchQuery::new(&idx);
        q.with_query("")
            .with_filter("kind = \"frontier\" AND status = \"pending\"")
            .with_sort(&sort)
            .with_limit(limit);
        let res = q.execute::<StateDoc>().await?;

        let entries: Vec<FrontierEntry> = res
            .hits
            .iter()
            .filter_map(|h| {
                Some(FrontierEntry {
                    url: h.result.url.clone()?,
                    depth: h.result.depth.unwrap_or(0),
                    default_city: h.result.default_city.clone(),
                })
            })
            .collect();

        let claimed: Vec<StateDoc> = res
            .hits
            .into_iter()
            .map(|h| StateDoc {
                status: Some("in_progress".into()),
                updated_at: now_ts(),
                ..h.result
            })
            .collect();
        meili::upsert_chunked(&self.client, BOT_STATE, &claimed).await?;

        Ok(entries)
    }

    pub async fn frontier_finish(&self, url: &str, status: &str) -> Result<()> {
        let id = frontier_id(url);
        let mut doc = match self.index().get_document::<StateDoc>(&id).await {
            Ok(d) => d,
            Err(_) => return Ok(()),
        };
        doc.status = Some(status.to_string());
        doc.updated_at = now_ts();
        meili::upsert_chunked(&self.client, BOT_STATE, &[doc]).await?;
        Ok(())
    }

    pub async fn frontier_pending(&self) -> usize {
        let idx = self.index();
        let mut q = SearchQuery::new(&idx);
        q.with_query("")
            .with_filter("kind = \"frontier\" AND status = \"pending\"")
            .with_limit(0);
        match q.execute::<StateDoc>().await {
            Ok(r) => r.estimated_total_hits.unwrap_or(0),
            Err(_) => 0,
        }
    }

    /// Return anything claimed but never finished to the pending pool.
    /// Called once on startup, which is what makes a mid-crawl crash safe.
    pub async fn frontier_requeue_stale(&self) -> Result<usize> {
        let idx = self.index();
        let mut q = SearchQuery::new(&idx);
        q.with_query("")
            .with_filter("kind = \"frontier\" AND status = \"in_progress\"")
            .with_limit(1000);
        let res = q.execute::<StateDoc>().await?;
        let requeued: Vec<StateDoc> = res
            .hits
            .into_iter()
            .map(|h| StateDoc {
                status: Some("pending".into()),
                updated_at: now_ts(),
                ..h.result
            })
            .collect();
        let n = requeued.len();
        if n > 0 {
            meili::upsert_chunked(&self.client, BOT_STATE, &requeued).await?;
            tracing::info!(n, "requeued crawl URLs left in progress by a previous run");
        }
        Ok(n)
    }

    // ----------------------------------------------------------- suppression

    /// Mark a document as one that must never be indexed again.
    ///
    /// Deleting a document is not enough on its own: the source that
    /// produced it still has it, so the next sweep would put it straight
    /// back. An approved removal has to be remembered.
    pub async fn suppress(&self, doc_id: &str, reason: &str) -> Result<()> {
        let doc = StateDoc {
            id: suppression_id(doc_id),
            kind: "suppression".into(),
            source: "submissions".into(),
            value: Some(Value::String(doc_id.to_string())),
            url: None,
            host: None,
            status: Some("suppressed".into()),
            depth: None,
            priority: None,
            default_city: Some(reason.chars().take(200).collect()),
            discovered_at: None,
            lat: None,
            lng: None,
            updated_at: now_ts(),
        };
        meili::upsert_chunked(&self.client, BOT_STATE, &[doc]).await?;
        Ok(())
    }

    /// Which of these document ids are suppressed.
    pub async fn suppressed(
        &self,
        doc_ids: &[String],
    ) -> Result<std::collections::HashSet<String>> {
        if doc_ids.is_empty() {
            return Ok(Default::default());
        }
        let keys: Vec<String> = doc_ids.iter().map(|d| suppression_id(d)).collect();
        let known = self.known_ids(&keys).await?;
        Ok(doc_ids
            .iter()
            .filter(|d| known.contains(&suppression_id(d)))
            .cloned()
            .collect())
    }

    // ------------------------------------------------------------ geocache

    pub async fn geocache_get(&self, key: &str) -> Option<(f64, f64)> {
        let id = geocache_id(key);
        let doc = self.index().get_document::<StateDoc>(&id).await.ok()?;
        Some((doc.lat?, doc.lng?))
    }

    pub async fn geocache_put(&self, key: &str, coords: Option<(f64, f64)>) -> Result<()> {
        // A miss is cached too: re-asking Nominatim for an address it could
        // not resolve wastes the one request per second we are allowed.
        let doc = StateDoc {
            id: geocache_id(key),
            kind: "geocache".into(),
            source: "nominatim".into(),
            value: Some(Value::String(key.to_string())),
            url: None,
            host: None,
            status: Some(if coords.is_some() { "hit" } else { "miss" }.into()),
            depth: None,
            priority: None,
            default_city: None,
            discovered_at: None,
            lat: coords.map(|c| c.0),
            lng: coords.map(|c| c.1),
            updated_at: now_ts(),
        };
        meili::upsert_chunked(&self.client, BOT_STATE, &[doc]).await?;
        Ok(())
    }

    /// True when we have already asked about this key, hit or miss.
    pub async fn geocache_known(&self, key: &str) -> bool {
        self.index()
            .get_document::<StateDoc>(&geocache_id(key))
            .await
            .is_ok()
    }

    // --------------------------------------------------------------- utils

    async fn known_ids(&self, ids: &[String]) -> Result<std::collections::HashSet<String>> {
        let mut out = std::collections::HashSet::new();
        for chunk in ids.chunks(200) {
            let filter = id_in_filter(chunk);
            let idx = self.index();
            let fields = ["id"];
            let mut q = SearchQuery::new(&idx);
            q.with_query("")
                .with_filter(&filter)
                .with_limit(chunk.len())
                .with_attributes_to_retrieve(Selectors::Some(&fields));
            let res = q.execute::<IdOnly>().await?;
            out.extend(res.hits.into_iter().map(|h| h.result.id));
        }
        Ok(out)
    }
}

#[derive(Debug, Deserialize)]
struct IdOnly {
    id: String,
}

pub fn frontier_id(url: &str) -> String {
    format!("f{}", stable_id("frontier", url))
}

/// Derived directly from the document id, so a batch of documents can be
/// checked for suppression with one `id IN [...]` lookup.
fn suppression_id(doc_id: &str) -> String {
    format!("sup-{}", stable_id("suppression", doc_id))
}

fn geocache_id(key: &str) -> String {
    format!("g{}", stable_id("geocache", key))
}

/// `id IN ["a", "b"]`. Ids are hex from [`stable_id`], so no escaping is
/// needed, but quoting keeps the expression well-formed regardless.
pub fn id_in_filter(ids: &[String]) -> String {
    let quoted: Vec<String> = ids.iter().map(|i| format!("\"{i}\"")).collect();
    format!("id IN [{}]", quoted.join(", "))
}

/// Look up the `content_hash` already indexed for a batch of ids.
///
/// This is what lets the indexer run continuously without rewriting the
/// whole index every cycle: unchanged documents never reach Meilisearch.
pub async fn existing_hashes(
    client: &Client,
    index: &str,
    ids: &[String],
) -> Result<HashMap<String, String>> {
    let mut out = HashMap::new();
    for chunk in ids.chunks(200) {
        let filter = id_in_filter(chunk);
        let idx = client.index(index);
        let fields = ["id", "content_hash"];
        let mut q = SearchQuery::new(&idx);
        q.with_query("")
            .with_filter(&filter)
            .with_limit(chunk.len())
            .with_attributes_to_retrieve(Selectors::Some(&fields));
        let res = q.execute::<HashRow>().await?;
        for hit in res.hits {
            out.insert(hit.result.id, hit.result.content_hash);
        }
    }
    Ok(out)
}

#[derive(Debug, Deserialize)]
struct HashRow {
    id: String,
    #[serde(default)]
    content_hash: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontier_ids_are_stable_and_url_safe() {
        let a = frontier_id("https://a.test/x");
        assert_eq!(a, frontier_id("https://a.test/x"));
        assert_ne!(a, frontier_id("https://a.test/y"));
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric()));
    }

    #[test]
    fn suppression_ids_are_derived_and_key_safe() {
        let a = suppression_id("63276ab7ed80a24f");
        assert_eq!(a, suppression_id("63276ab7ed80a24f"));
        assert_ne!(a, suppression_id("other"));
        // Meilisearch primary keys allow only [a-zA-Z0-9_-].
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
    }

    #[test]
    fn id_filters_are_well_formed() {
        assert_eq!(
            id_in_filter(&["aa".into(), "bb".into()]),
            r#"id IN ["aa", "bb"]"#
        );
    }
}
