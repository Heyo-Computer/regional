//! Query execution against Meilisearch, and the shape of what comes back.

use std::collections::HashMap;

use meilisearch_sdk::search::{FederationOptions, SearchQuery, Selectors};
use regional_core::meili::Client;
use regional_core::model::{GeoPoint, Kind};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Attributes we ask Meilisearch to highlight and crop, so a hit carries a
/// snippet showing *why* it matched rather than just its first 200 chars.
const HIGHLIGHT: &[&str] = &["title", "summary", "body"];
const CROP: &[(&str, Option<usize>)] = &[("summary", Some(40)), ("body", Some(40))];
const HIGHLIGHT_PRE: &str = "**";
const HIGHLIGHT_POST: &str = "**";

/// Hard ceiling on `limit`, so one tool call cannot pull the whole index
/// into a model's context window.
pub const MAX_LIMIT: usize = 50;
pub const DEFAULT_LIMIT: usize = 10;

/// How many candidates per index `find_nearby` pulls before re-sorting by
/// true distance. See [`search_nearby`].
const OVERFETCH: usize = 4;

/// A document as it comes back from any of the three indexes.
///
/// Every field beyond the envelope is optional, so one struct can
/// deserialize a place, an event and an article alike.
#[derive(Debug, Clone, Deserialize)]
pub struct RawHit {
    pub id: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub city: Option<String>,
    #[serde(default)]
    pub address: Option<String>,
    #[serde(default)]
    pub categories: Vec<String>,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub geo_precision: String,
    #[serde(rename = "_geo")]
    pub geo: GeoPoint,
    #[serde(rename = "_geoDistance", default)]
    pub geo_distance: Option<f64>,
    #[serde(default)]
    pub updated_at: Option<i64>,

    // place
    #[serde(default)]
    pub phone: Option<String>,
    #[serde(default)]
    pub website: Option<String>,
    #[serde(default)]
    pub opening_hours: Option<String>,

    // event
    #[serde(default)]
    pub start_time: Option<i64>,
    #[serde(default)]
    pub end_time: Option<i64>,
    #[serde(default)]
    pub venue_name: Option<String>,

    // article
    #[serde(default)]
    pub published_at: Option<i64>,
    #[serde(default)]
    pub site_name: Option<String>,
}

/// What a tool returns per result.
///
/// Deliberately not the whole document: `body` is omitted entirely and the
/// snippet is cropped, because list results go straight into a model's
/// context. `get_document` exists for the full record.
#[derive(Debug, Clone, Serialize)]
pub struct Hit {
    pub id: String,
    pub kind: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub city: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub categories: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phone: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub website: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub opening_hours: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub venue: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub site_name: Option<String>,
    pub lat: f64,
    pub lng: f64,
    /// Distance from the query anchor, present only on geo-sorted results.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub distance_m: Option<u64>,
    /// How much to trust `lat`/`lng`: `exact`, `address`, `city`, `region`.
    pub geo_precision: String,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub starts_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ends_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub published_at: Option<String>,
}

pub fn to_hit(raw: RawHit, snippet: Option<String>, city: Option<String>) -> Hit {
    Hit {
        id: raw.id,
        kind: raw.kind,
        title: raw.title,
        snippet: snippet.or_else(|| (!raw.summary.is_empty()).then(|| raw.summary.clone())),
        city,
        categories: raw.categories,
        url: raw.url,
        address: raw.address,
        phone: raw.phone,
        website: raw.website,
        opening_hours: raw.opening_hours,
        venue: raw.venue_name,
        site_name: raw.site_name,
        lat: raw.geo.lat,
        lng: raw.geo.lng,
        distance_m: raw.geo_distance.map(|d| d.round() as u64),
        geo_precision: raw.geo_precision,
        source: raw.source,
        starts_at: raw.start_time.and_then(fmt_ts),
        ends_at: raw.end_time.and_then(fmt_ts),
        published_at: raw.published_at.and_then(fmt_ts),
    }
}

pub fn fmt_ts(ts: i64) -> Option<String> {
    chrono::DateTime::from_timestamp(ts, 0).map(|d| d.format("%Y-%m-%d %H:%M UTC").to_string())
}

/// Everything a single index query needs. Kept as owned strings because
/// Meilisearch's query builder borrows them for the life of the request.
#[derive(Debug, Clone, Default)]
pub struct Params {
    pub query: String,
    pub filter: String,
    pub sort: Vec<String>,
    pub limit: usize,
    pub offset: usize,
}

pub struct Page {
    pub hits: Vec<(RawHit, Option<String>)>,
    pub estimated_total: usize,
}

/// Search one index.
pub async fn search_one(client: &Client, index: &str, p: &Params) -> anyhow::Result<Page> {
    let idx = client.index(index);
    let sort: Vec<&str> = p.sort.iter().map(String::as_str).collect();

    let mut q = SearchQuery::new(&idx);
    q.with_query(&p.query)
        .with_limit(p.limit)
        .with_offset(p.offset)
        .with_attributes_to_highlight(Selectors::Some(HIGHLIGHT))
        .with_attributes_to_crop(Selectors::Some(CROP))
        .with_highlight_pre_tag(HIGHLIGHT_PRE)
        .with_highlight_post_tag(HIGHLIGHT_POST);
    if !p.filter.is_empty() {
        q.with_filter(&p.filter);
    }
    if !sort.is_empty() {
        q.with_sort(&sort);
    }

    let res = q.execute::<RawHit>().await?;
    Ok(Page {
        estimated_total: res.estimated_total_hits.unwrap_or(res.hits.len()),
        hits: res
            .hits
            .into_iter()
            .map(|h| {
                let snip = snippet(&h.formatted_result);
                (h.result, snip)
            })
            .collect(),
    })
}

/// Search several indexes and get back one relevance-merged list.
///
/// This is what makes "what's around Durango" answerable in a single call:
/// Meilisearch does the merging by ranking score rather than us stitching
/// three independently-ranked lists together and hoping.
pub async fn search_federated(
    client: &Client,
    indexes: &[&str],
    p: &Params,
) -> anyhow::Result<Page> {
    if indexes.len() == 1 {
        return search_one(client, indexes[0], p).await;
    }
    let handles: Vec<_> = indexes.iter().map(|n| client.index(*n)).collect();

    // No per-query `limit` here: Meilisearch rejects pagination options on
    // a federated sub-query, because `federation.limit`/`offset` page the
    // merged list instead.
    let built: Vec<SearchQuery<'_, _>> = handles
        .iter()
        .map(|idx| {
            let mut q = SearchQuery::new(idx);
            q.with_query(&p.query)
                .with_attributes_to_highlight(Selectors::Some(HIGHLIGHT))
                .with_attributes_to_crop(Selectors::Some(CROP))
                .with_highlight_pre_tag(HIGHLIGHT_PRE)
                .with_highlight_post_tag(HIGHLIGHT_POST);
            if !p.filter.is_empty() {
                q.with_filter(&p.filter);
            }
            q.build()
        })
        .collect();

    let mut multi = client.multi_search();
    for q in built {
        multi.with_search_query(q);
    }
    let federated = multi.with_federation(FederationOptions {
        limit: Some(p.limit),
        offset: Some(p.offset),
        ..Default::default()
    });
    let res = federated.execute::<RawHit>().await?;

    Ok(Page {
        estimated_total: res.estimated_total_hits,
        hits: res
            .hits
            .into_iter()
            .map(|h| {
                let snip = snippet(&h.formatted_result);
                (h.result, snip)
            })
            .collect(),
    })
}

/// Search several indexes sorted by distance, merged on true distance.
///
/// Federated search merges on relevance score, which is the wrong order for
/// a proximity question, so this runs the indexes separately and merges on
/// the `_geoDistance` Meilisearch returns. That ordering is exact.
pub async fn search_nearby(client: &Client, indexes: &[&str], p: &Params) -> anyhow::Result<Page> {
    let mut all: Vec<(RawHit, Option<String>)> = Vec::new();
    let mut estimated = 0usize;
    // Over-fetch before merging. Meilisearch applies `sort` as one ranking
    // rule among several, so with a non-empty query it orders by relevance
    // tier first and only breaks ties by distance. Pulling a wider candidate
    // set means the genuinely nearest matches are there to be re-sorted,
    // rather than cut off by a more "relevant" but more distant one.
    let per_index = ((p.limit + p.offset) * OVERFETCH).min(MAX_LIMIT * OVERFETCH);
    for index in indexes {
        let per = Params {
            limit: per_index,
            offset: 0,
            ..p.clone()
        };
        let page = search_one(client, index, &per).await?;
        estimated += page.estimated_total;
        all.extend(page.hits);
    }
    all.sort_by(|a, b| {
        let da = a.0.geo_distance.unwrap_or(f64::MAX);
        let db = b.0.geo_distance.unwrap_or(f64::MAX);
        da.total_cmp(&db)
    });
    let hits = all.into_iter().skip(p.offset).take(p.limit).collect();
    Ok(Page {
        hits,
        estimated_total: estimated,
    })
}

/// Facet counts for one attribute, used by `describe_region` to tell the
/// caller which category values actually exist.
pub async fn facet_counts(
    client: &Client,
    index: &str,
    field: &str,
    filter: &str,
) -> anyhow::Result<HashMap<String, usize>> {
    let idx = client.index(index);
    let fields = [field];
    let mut q = SearchQuery::new(&idx);
    q.with_query("")
        .with_limit(0)
        .with_facets(Selectors::Some(&fields));
    if !filter.is_empty() {
        q.with_filter(filter);
    }
    let res = q.execute::<RawHit>().await?;
    Ok(res
        .facet_distribution
        .and_then(|mut d| d.remove(field))
        .unwrap_or_default())
}

/// The most recently updated document in an index, for freshness reporting.
pub async fn newest_update(client: &Client, index: &str) -> anyhow::Result<Option<i64>> {
    let idx = client.index(index);
    let sort = ["updated_at:desc"];
    let mut q = SearchQuery::new(&idx);
    q.with_query("").with_limit(1).with_sort(&sort);
    let res = q.execute::<RawHit>().await?;
    Ok(res.hits.first().and_then(|h| h.result.updated_at))
}

/// Pull a readable snippet out of Meilisearch's `_formatted` payload,
/// preferring the cropped summary and falling back to the cropped body.
fn snippet(formatted: &Option<Map<String, Value>>) -> Option<String> {
    let f = formatted.as_ref()?;
    for key in ["summary", "body"] {
        if let Some(Value::String(s)) = f.get(key) {
            let s = s.trim();
            if !s.is_empty() {
                return Some(s.to_string());
            }
        }
    }
    None
}

/// Which indexes a set of kinds maps to.
pub fn indexes_for(kinds: &[Kind]) -> Vec<&'static str> {
    let mut out: Vec<&'static str> = kinds.iter().map(|k| k.index()).collect();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snippet_prefers_summary_then_body() {
        let mut m = Map::new();
        m.insert(
            "body".into(),
            Value::String("…a **winery** in Palisade…".into()),
        );
        assert_eq!(
            snippet(&Some(m.clone())).as_deref(),
            Some("…a **winery** in Palisade…")
        );
        m.insert("summary".into(), Value::String("Family **winery**".into()));
        assert_eq!(snippet(&Some(m)).as_deref(), Some("Family **winery**"));
        assert_eq!(snippet(&None), None);
    }

    #[test]
    fn blank_formatted_values_fall_through() {
        let mut m = Map::new();
        m.insert("summary".into(), Value::String("   ".into()));
        m.insert("body".into(), Value::String("real text".into()));
        assert_eq!(snippet(&Some(m)).as_deref(), Some("real text"));
    }

    #[test]
    fn indexes_for_maps_kinds() {
        assert_eq!(indexes_for(&[Kind::Place]), vec!["places"]);
        assert_eq!(
            indexes_for(&Kind::ALL),
            vec!["places", "events", "articles"]
        );
    }

    #[test]
    fn timestamps_render_readably() {
        assert_eq!(fmt_ts(0).as_deref(), Some("1970-01-01 00:00 UTC"));
    }
}
