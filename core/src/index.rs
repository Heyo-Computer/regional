//! Index names and settings.
//!
//! `ensure_indexes` is idempotent and called on startup by both binaries,
//! so the schema is applied by whichever service boots first and neither
//! can operate against a half-configured index.

use meilisearch_sdk::settings::{FacetingSettings, Settings};

use crate::error::Result;
use crate::meili::{Client, await_task};
use crate::model::Kind;
use crate::region::RegionConfig;

pub const PLACES: &str = "places";
pub const EVENTS: &str = "events";
pub const ARTICLES: &str = "articles";
/// Public submissions: requests to add, change or remove indexed content.
/// Written by the dashboard, consumed by the indexer.
pub const SUBMISSIONS: &str = "submissions";
/// Indexer bookkeeping: per-source cursors, the crawl frontier, the geocode
/// cache and the suppression list.
/// Meilisearch is the only stateful service in the stack, so the indexer
/// keeps its own state here rather than needing a volume of its own.
pub const BOT_STATE: &str = "bot_state";
/// Hashes of the per-user MCP bearer tokens minted on the dashboard. The
/// MCP server's search key must be able to search this index; see
/// `deploy/create-search-key.sh`.
pub const MCP_TOKENS: &str = "mcp_tokens";

pub const CONTENT_INDEXES: [&str; 3] = [PLACES, EVENTS, ARTICLES];

/// Attributes searched, in descending order of weight.
const SEARCHABLE: [&str; 7] = [
    "title",
    "summary",
    "categories",
    "tags",
    "city",
    "address",
    "body",
];

/// Filterable on every content index. `_geo` is what makes the geo filters
/// work at all; it must also appear in [`SORTABLE_BASE`]. `id` is filterable
/// so the indexer can look up a batch of existing documents in one query to
/// decide which ones actually changed.
const FILTERABLE_BASE: [&str; 9] = [
    "id",
    "_geo",
    "kind",
    "city",
    "county",
    "categories",
    "tags",
    "source",
    "geo_precision",
];

const SORTABLE_BASE: [&str; 2] = ["_geo", "updated_at"];

/// Ceiling on how long a single search may run, in milliseconds.
const SEARCH_CUTOFF_MS: u64 = 1_500;

/// The settings for one content index.
///
/// Takes the region because the searchable vocabulary — synonyms and stop
/// words — is part of the region definition, not a property of the code.
pub fn settings_for(kind: Kind, region: &RegionConfig) -> Settings {
    let mut filterable: Vec<String> = FILTERABLE_BASE.iter().map(|s| s.to_string()).collect();
    let mut sortable: Vec<String> = SORTABLE_BASE.iter().map(|s| s.to_string()).collect();

    // Meilisearch's defaults, which we extend per index.
    let mut ranking: Vec<String> = [
        "words",
        "typo",
        "proximity",
        "attribute",
        "sort",
        "exactness",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();

    match kind {
        Kind::Place => {}
        Kind::Event => {
            // Time-bounded: callers ask for "what's on between X and Y".
            filterable.push("start_time".into());
            filterable.push("end_time".into());
            sortable.push("start_time".into());
            sortable.push("end_time".into());
        }
        Kind::Article => {
            filterable.push("published_at".into());
            sortable.push("published_at".into());
            // Freshness as the final tiebreaker: between two equally
            // relevant articles, prefer the one we saw change most recently.
            ranking.push("updated_at:desc".into());
        }
    }

    let mut settings = Settings::new()
        .with_searchable_attributes(SEARCHABLE)
        .with_filterable_attributes(&filterable)
        .with_sortable_attributes(&sortable)
        .with_ranking_rules(&ranking)
        .with_stop_words(&region.vocabulary.stop_words)
        .with_synonyms(region.vocabulary.synonym_map())
        // Cap how long one search may run. A slow query returns what it
        // has rather than holding the MCP request open.
        .with_search_cutoff(SEARCH_CUTOFF_MS)
        .with_faceting(FacetingSettings {
            max_values_per_facet: 200,
            sort_facet_values_by: None,
        });

    if matches!(kind, Kind::Article) {
        // Crawls reach the same article by several paths; collapse on URL.
        settings = settings.with_distinct_attribute(Some("url"));
    }
    settings
}

/// Settings for the submissions index.
///
/// Not a content index: no `_geo`, because submissions are triaged in a
/// queue rather than searched by location.
fn submission_settings() -> Settings {
    Settings::new()
        .with_searchable_attributes(["title", "description", "city", "address", "url"])
        .with_filterable_attributes([
            "id",
            "status",
            "request",
            "target_kind",
            "city",
            "existing_id",
            // The indexer skips approved submissions it has already failed
            // to apply, so it filters on this being null.
            "apply_note",
        ])
        .with_sortable_attributes(["submitted_at", "updated_at", "reviewed_at"])
}

/// Settings for the indexer's bookkeeping index.
///
/// `id` must be filterable here for the same reason as on the content
/// indexes: the frontier and the geocode cache are read back a batch of ids
/// at a time rather than one document per request.
fn state_settings() -> Settings {
    Settings::new()
        .with_searchable_attributes(["id"])
        .with_filterable_attributes(["id", "kind", "source", "status", "host"])
        .with_sortable_attributes(["priority", "discovered_at", "updated_at"])
}

/// Settings for the MCP token index.
///
/// Looked up by exact `id` filter, never by text, so nothing is searchable
/// beyond the id itself.
fn token_settings() -> Settings {
    Settings::new()
        .with_searchable_attributes(["id"])
        .with_filterable_attributes(["id"])
        .with_sortable_attributes(["created_at"])
}

/// Create any missing index with `id` as its primary key and apply settings.
///
/// Safe to call concurrently from both services: creating an index that
/// already exists is tolerated.
pub async fn ensure_indexes(client: &Client, region: &RegionConfig) -> Result<()> {
    for kind in Kind::ALL {
        ensure_one(client, kind.index(), &settings_for(kind, region)).await?;
    }
    ensure_one(client, SUBMISSIONS, &submission_settings()).await?;
    ensure_one(client, BOT_STATE, &state_settings()).await?;
    ensure_one(client, MCP_TOKENS, &token_settings()).await?;
    tracing::info!(
        region = %region.name,
        indexes = ?CONTENT_INDEXES,
        synonym_terms = region.vocabulary.synonym_map().len(),
        stop_words = region.vocabulary.stop_words.len(),
        "index settings applied"
    );
    Ok(())
}

async fn ensure_one(client: &Client, uid: &str, settings: &Settings) -> Result<()> {
    if client.get_index(uid).await.is_err() {
        match client.create_index(uid, Some("id")).await {
            Ok(info) => {
                // A concurrent creator may win the race; that is fine, the
                // settings write below still lands.
                if let Err(e) = await_task(client, info).await {
                    tracing::debug!(uid, error = %e, "index creation did not complete cleanly");
                }
            }
            Err(e) => tracing::debug!(uid, error = %e, "index creation rejected"),
        }
    }
    let info = client.index(uid).set_settings(settings).await?;
    await_task(client, info).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn region() -> RegionConfig {
        RegionConfig::load(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../region.colorado.toml"
        ))
        .unwrap()
    }

    /// `filterableAttributes` entries can be a bare name or a settings
    /// object; tests only care about the names.
    fn filterable_names(s: &Settings) -> Vec<String> {
        s.filterable_attributes
            .clone()
            .unwrap_or_default()
            .into_iter()
            .map(|a| match a {
                meilisearch_sdk::settings::FilterableAttribute::Attribute(n) => n,
                meilisearch_sdk::settings::FilterableAttribute::Settings(cfg) => {
                    format!("{cfg:?}")
                }
            })
            .collect()
    }

    #[test]
    fn geo_is_both_filterable_and_sortable_on_every_index() {
        // Meilisearch silently returns nothing for a geo filter when `_geo`
        // is missing from filterableAttributes, so assert it explicitly.
        for kind in Kind::ALL {
            let s = settings_for(kind, &region());
            let f = filterable_names(&s);
            let so = s.sortable_attributes.clone().unwrap_or_default();
            assert!(f.contains(&"_geo".to_string()), "{kind} filterable");
            assert!(so.contains(&"_geo".to_string()), "{kind} sortable");
        }
    }

    #[test]
    fn id_is_filterable_so_batches_can_be_looked_up() {
        for kind in Kind::ALL {
            assert!(filterable_names(&settings_for(kind, &region())).contains(&"id".to_string()));
        }
        // The indexer's own bookkeeping index is read back the same way.
        assert!(filterable_names(&state_settings()).contains(&"id".to_string()));
    }

    #[test]
    fn submissions_can_be_triaged_as_a_queue() {
        let s = submission_settings();
        let f = filterable_names(&s);
        for attr in ["id", "status", "request", "existing_id"] {
            assert!(f.contains(&attr.to_string()), "{attr} must be filterable");
        }
        let so = s.sortable_attributes.clone().unwrap_or_default();
        assert!(so.contains(&"submitted_at".to_string()));
        // Submissions are not geo-searched, so `_geo` would be dead weight.
        assert!(!f.contains(&"_geo".to_string()));
    }

    #[test]
    fn the_frontier_can_be_filtered_and_ordered() {
        let s = state_settings();
        let f = filterable_names(&s);
        for attr in ["kind", "status"] {
            assert!(f.contains(&attr.to_string()), "{attr} must be filterable");
        }
        let so = s.sortable_attributes.clone().unwrap_or_default();
        assert!(so.contains(&"priority".to_string()));
        assert!(so.contains(&"discovered_at".to_string()));
    }

    #[test]
    fn tokens_are_looked_up_by_id() {
        let s = token_settings();
        assert!(filterable_names(&s).contains(&"id".to_string()));
        let so = s.sortable_attributes.clone().unwrap_or_default();
        assert!(so.contains(&"created_at".to_string()));
    }

    #[test]
    fn events_can_be_filtered_and_sorted_on_time() {
        let s = settings_for(Kind::Event, &region());
        let f = filterable_names(&s);
        let so = s.sortable_attributes.clone().unwrap_or_default();
        assert!(f.contains(&"start_time".to_string()));
        assert!(so.contains(&"start_time".to_string()));
    }

    #[test]
    fn articles_dedupe_on_url_and_prefer_fresh() {
        let s = settings_for(Kind::Article, &region());
        assert_eq!(s.distinct_attribute.unwrap(), Some("url".to_string()));
        assert!(
            s.ranking_rules
                .unwrap()
                .contains(&"updated_at:desc".to_string())
        );
    }

    #[test]
    fn places_do_not_dedupe_on_url() {
        // Many POIs legitimately share a website (chains, plazas).
        let s = settings_for(Kind::Place, &region());
        assert!(s.distinct_attribute.is_none() || s.distinct_attribute == Some(None));
    }

    #[test]
    fn synonyms_are_bidirectional() {
        let syn = region().vocabulary.synonym_map();
        assert!(syn["vineyard"].contains(&"winery".to_string()));
        assert!(syn["winery"].contains(&"vineyard".to_string()));
        assert!(syn["taproom"].contains(&"brewery".to_string()));
        assert!(!syn["vineyard"].contains(&"vineyard".to_string()));
    }
}
