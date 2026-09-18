//! Geotagged Wikipedia articles, typed by Wikidata.
//!
//! Wikipedia supplies the prose that OSM has none of — the history of a
//! mining town, what a canyon is known for — and every article it returns
//! here is geotagged, so it lands in the region by construction.
//!
//! Types come from Wikidata's `P31` (instance of), but rather than hardcode
//! Q-ids, the entity ids are resolved to their English labels in one extra
//! call. Those labels *are* the categories, so "winery" or "ski resort"
//! appears without anyone maintaining a mapping table.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use regional_core::model::{
    Article, Doc, Envelope, GeoPoint, GeoPrecision, Kind, Place, collapse_ws, truncate_chars,
};
use regional_core::region::BBox;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Batch, Ctx, Source, cursor_usize};

/// Wikipedia's geosearch caps `gsradius` at 10 km. Points spaced 14 km apart
/// overlap enough that no gap is left between the circles.
const RADIUS_M: u32 = 10_000;
const SPACING_KM: f64 = 14.0;
/// Ceiling on pages resolved per run, so a dense grid slice cannot turn
/// into a thousand-request cycle.
const MAX_PAGES_PER_RUN: usize = 200;
/// MediaWiki allows 20 extracts per request.
const EXTRACT_BATCH: usize = 20;
/// wbgetentities allows 50 ids per request.
const ENTITY_BATCH: usize = 50;

/// `P31` labels that mean "this is somewhere you can go".
///
/// Everything geotagged becomes an article; only these also become a place,
/// so the `places` index stays a list of destinations rather than a mirror
/// of the encyclopedia.
const PLACE_LABELS: &[&str] = &[
    "museum",
    "art museum",
    "history museum",
    "national park",
    "state park",
    "park",
    "national monument",
    "national forest",
    "wilderness area",
    "protected area",
    "mountain",
    "mountain pass",
    "mountain range",
    "hill",
    "summit",
    "canyon",
    "valley",
    "lake",
    "reservoir",
    "river",
    "waterfall",
    "hot spring",
    "spring",
    "cave",
    "glacier",
    "ski resort",
    "ski area",
    "winery",
    "vineyard",
    "brewery",
    "distillery",
    "restaurant",
    "hotel",
    "resort",
    "campground",
    "golf course",
    "botanical garden",
    "zoo",
    "amusement park",
    "water park",
    "historic district",
    "historic house",
    "monument",
    "memorial",
    "bridge",
    "tunnel",
    "dam",
    "lighthouse",
    "observatory",
    "mine",
    "ghost town",
    "city",
    "town",
    "village",
    "unincorporated community",
    "census-designated place",
    "neighborhood",
    "theater",
    "concert hall",
    "opera house",
    "stadium",
    "arena",
    "library",
    "university",
    "college",
    "airport",
    "railway station",
    "church building",
    "cathedral",
    "trail",
    "hiking trail",
    "beach",
    "island",
];

pub struct Wikipedia;

// ------------------------------------------------------------ API responses

#[derive(Debug, Deserialize)]
struct GeoSearchResponse {
    #[serde(default)]
    query: Option<GeoSearchQuery>,
}

#[derive(Debug, Deserialize)]
struct GeoSearchQuery {
    #[serde(default)]
    geosearch: Vec<GeoSearchHit>,
}

#[derive(Debug, Deserialize, Clone)]
struct GeoSearchHit {
    pageid: u64,
    lat: f64,
    lon: f64,
}

#[derive(Debug, Deserialize)]
struct PagesResponse {
    #[serde(default)]
    query: Option<PagesQuery>,
}

#[derive(Debug, Deserialize)]
struct PagesQuery {
    #[serde(default)]
    pages: Vec<PageDetail>,
}

#[derive(Debug, Deserialize)]
struct PageDetail {
    pageid: u64,
    title: String,
    #[serde(default)]
    extract: Option<String>,
    #[serde(default)]
    fullurl: Option<String>,
    #[serde(default)]
    pageprops: Option<PageProps>,
}

#[derive(Debug, Deserialize)]
struct PageProps {
    #[serde(default)]
    wikibase_item: Option<String>,
}

#[derive(Debug, Deserialize)]
struct EntitiesResponse {
    #[serde(default)]
    entities: HashMap<String, Entity>,
}

#[derive(Debug, Deserialize, Default)]
struct Entity {
    #[serde(default)]
    claims: HashMap<String, Vec<Claim>>,
    #[serde(default)]
    labels: HashMap<String, LabelValue>,
}

#[derive(Debug, Deserialize)]
struct Claim {
    #[serde(default)]
    mainsnak: Option<Snak>,
}

#[derive(Debug, Deserialize)]
struct Snak {
    #[serde(default)]
    datavalue: Option<DataValue>,
}

#[derive(Debug, Deserialize)]
struct DataValue {
    #[serde(default)]
    value: Value,
}

#[derive(Debug, Deserialize)]
struct LabelValue {
    #[serde(default)]
    value: String,
}

// -------------------------------------------------------------------- source

#[async_trait]
impl Source for Wikipedia {
    fn name(&self) -> &'static str {
        "wikipedia"
    }

    fn default_interval(&self) -> Duration {
        Duration::from_secs(15 * 60)
    }

    async fn fetch(&self, ctx: &Ctx, cursor: Option<Value>) -> Result<Batch> {
        let grid = grid_points(&ctx.region.bbox);
        let start = cursor_usize(&cursor, "point") % grid.len();
        let take = ctx.config.wikipedia_points_per_run.max(1);

        // Collect candidate pages from this slice of the grid.
        let mut seen: HashMap<u64, GeoSearchHit> = HashMap::new();
        for offset in 0..take {
            let point = grid[(start + offset) % grid.len()];
            match geosearch(ctx, point).await {
                Ok(hits) => {
                    for h in hits {
                        seen.entry(h.pageid).or_insert(h);
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, lat = point.lat, lng = point.lng, "geosearch failed")
                }
            }
            if seen.len() >= MAX_PAGES_PER_RUN {
                break;
            }
        }

        let hits: Vec<GeoSearchHit> = seen.into_values().take(MAX_PAGES_PER_RUN).collect();
        tracing::info!(
            points = take,
            from = start,
            of = grid.len(),
            pages = hits.len(),
            "wikipedia geosearch slice"
        );

        let docs = if hits.is_empty() {
            Vec::new()
        } else {
            build_docs(ctx, &hits).await?
        };

        let next = (start + take) % grid.len();
        let batch = Batch::new(docs, Some(json!({ "point": next })));
        Ok(if next < start { batch.swept() } else { batch })
    }
}

/// Cover the region with search points spaced closely enough that their
/// 10 km circles overlap.
fn grid_points(bbox: &BBox) -> Vec<GeoPoint> {
    bbox.tiles_of_about(SPACING_KM)
        .into_iter()
        .map(|t| t.center())
        .collect()
}

async fn geosearch(ctx: &Ctx, point: GeoPoint) -> Result<Vec<GeoSearchHit>> {
    let url = format!(
        "{}?action=query&list=geosearch&gscoord={}%7C{}&gsradius={}&gslimit=500&format=json&formatversion=2",
        ctx.config.wikipedia_api, point.lat, point.lng, RADIUS_M
    );
    let res: GeoSearchResponse = ctx.http.get_json(&url).await?;
    Ok(res.query.map(|q| q.geosearch).unwrap_or_default())
}

async fn build_docs(ctx: &Ctx, hits: &[GeoSearchHit]) -> Result<Vec<Doc>> {
    let coords: HashMap<u64, (f64, f64)> =
        hits.iter().map(|h| (h.pageid, (h.lat, h.lon))).collect();

    // 1. Article text and the Wikidata id, in batches.
    let mut pages: Vec<PageDetail> = Vec::new();
    for chunk in hits.chunks(EXTRACT_BATCH) {
        let ids: Vec<String> = chunk.iter().map(|h| h.pageid.to_string()).collect();
        let url = format!(
            "{}?action=query&pageids={}&prop=extracts%7Cpageprops%7Cinfo&explaintext=1&exlimit=max&inprop=url&format=json&formatversion=2",
            ctx.config.wikipedia_api,
            ids.join("%7C")
        );
        match ctx.http.get_json::<PagesResponse>(&url).await {
            Ok(r) => pages.extend(r.query.map(|q| q.pages).unwrap_or_default()),
            Err(e) => tracing::warn!(error = %e, "wikipedia extract batch failed"),
        }
    }

    // 2. P31 for each entity, then the labels of those P31 targets. Two
    //    cheap batched calls replace a hardcoded Q-id table.
    let entity_ids: Vec<String> = pages
        .iter()
        .filter_map(|p| p.pageprops.as_ref()?.wikibase_item.clone())
        .collect();
    let instance_of = fetch_instance_of(ctx, &entity_ids).await;
    let type_ids: HashSet<String> = instance_of.values().flatten().cloned().collect();
    let labels = fetch_labels(ctx, &type_ids.into_iter().collect::<Vec<_>>()).await;

    // 3. Build the documents.
    let mut docs = Vec::new();
    for page in pages {
        let Some(&(lat, lng)) = coords.get(&page.pageid) else {
            continue;
        };
        let geo = GeoPoint::new(lat, lng);
        if !geo.is_valid() {
            continue;
        }
        let wikidata_id = page
            .pageprops
            .as_ref()
            .and_then(|p| p.wikibase_item.clone());
        let categories: Vec<String> = wikidata_id
            .as_ref()
            .and_then(|q| instance_of.get(q))
            .map(|types| {
                types
                    .iter()
                    .filter_map(|t| labels.get(t).cloned())
                    .collect()
            })
            .unwrap_or_default();

        // Derive the fallback from the configured API so pointing
        // WIKIPEDIA_API at another language wiki produces links to it.
        let url = page
            .fullurl
            .clone()
            .unwrap_or_else(|| wiki_page_url(&ctx.config.wikipedia_api, page.pageid));
        let text = page.extract.clone().unwrap_or_default();
        let summary = first_paragraph(&text);

        let mut env = Envelope::new(
            Kind::Article,
            "wikipedia",
            page.pageid.to_string(),
            &page.title,
            geo,
        );
        env.geo_precision = GeoPrecision::Exact;
        env.url = Some(url.clone());
        env.summary = summary.clone();
        env.body = text.clone();
        env.categories = categories.clone();

        let word_count = text.split_whitespace().count() as u32;
        docs.push(Doc::Article(Article {
            env,
            published_at: None,
            author: None,
            site_name: Some("Wikipedia".into()),
            lang: Some("en".into()),
            word_count: Some(word_count),
        }));

        // Somewhere you can actually go also belongs in `places`, where the
        // geo tools look first.
        if is_a_place(&categories) {
            let mut env = Envelope::new(
                Kind::Place,
                "wikipedia",
                format!("place/{}", page.pageid),
                &page.title,
                geo,
            );
            env.geo_precision = GeoPrecision::Exact;
            env.url = Some(url);
            env.summary = summary;
            env.body = text;
            env.categories = categories;
            docs.push(Doc::Place(Place {
                env,
                phone: None,
                website: None,
                opening_hours: None,
                cuisine: vec![],
                price_level: None,
                osm_type: None,
                osm_id: None,
                wikidata_id,
            }));
        }
    }
    Ok(docs)
}

/// `entity id -> the Q-ids it is an instance of`.
async fn fetch_instance_of(ctx: &Ctx, ids: &[String]) -> HashMap<String, Vec<String>> {
    let mut out = HashMap::new();
    for chunk in ids.chunks(ENTITY_BATCH) {
        let url = format!(
            "{}?action=wbgetentities&ids={}&props=claims&format=json&formatversion=2",
            ctx.config.wikidata_api,
            chunk.join("%7C")
        );
        let res: EntitiesResponse = match ctx.http.get_json(&url).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "wikidata claims batch failed");
                continue;
            }
        };
        for (id, entity) in res.entities {
            let types: Vec<String> = entity
                .claims
                .get("P31")
                .map(|claims| claims.iter().filter_map(claim_entity_id).collect())
                .unwrap_or_default();
            if !types.is_empty() {
                out.insert(id, types);
            }
        }
    }
    out
}

/// `Q-id -> English label`.
async fn fetch_labels(ctx: &Ctx, ids: &[String]) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for chunk in ids.chunks(ENTITY_BATCH) {
        let url = format!(
            "{}?action=wbgetentities&ids={}&props=labels&languages=en&format=json&formatversion=2",
            ctx.config.wikidata_api,
            chunk.join("%7C")
        );
        let res: EntitiesResponse = match ctx.http.get_json(&url).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "wikidata label batch failed");
                continue;
            }
        };
        for (id, entity) in res.entities {
            if let Some(label) = entity.labels.get("en")
                && !label.value.is_empty()
            {
                out.insert(id, label.value.to_lowercase());
            }
        }
    }
    out
}

/// `https://xx.wikipedia.org/w/api.php` -> `https://xx.wikipedia.org/?curid=N`.
fn wiki_page_url(api: &str, pageid: u64) -> String {
    match url::Url::parse(api) {
        Ok(u) => {
            let host = u.host_str().unwrap_or("en.wikipedia.org");
            format!("{}://{host}/?curid={pageid}", u.scheme())
        }
        Err(_) => format!("https://en.wikipedia.org/?curid={pageid}"),
    }
}

fn claim_entity_id(claim: &Claim) -> Option<String> {
    let v = claim.mainsnak.as_ref()?.datavalue.as_ref()?;
    v.value.get("id")?.as_str().map(str::to_string)
}

fn is_a_place(categories: &[String]) -> bool {
    categories
        .iter()
        .any(|c| PLACE_LABELS.contains(&c.trim().to_lowercase().as_str()))
}

/// Wikipedia extracts open with the lead paragraph, which is exactly the
/// summary we want.
fn first_paragraph(text: &str) -> String {
    let para = text
        .split("\n\n")
        .map(str::trim)
        .find(|p| !p.is_empty())
        .unwrap_or("");
    truncate_chars(&collapse_ws(para), 500)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_grid_covers_the_region_with_overlapping_circles() {
        let co = BBox {
            min_lat: 36.992426,
            min_lng: -109.060253,
            max_lat: 41.003444,
            max_lng: -102.041524,
        };
        let points = grid_points(&co);
        assert!(points.len() > 500, "got {}", points.len());
        // Every point must sit inside the region it is meant to cover.
        for p in &points {
            assert!(co.contains(p.lat, p.lng));
        }
        // Neighbouring points must be closer than twice the search radius,
        // or the circles leave gaps between them.
        let d = points[0].distance_m(&points[1]);
        assert!(d < 2.0 * RADIUS_M as f64, "spacing {d} m leaves a gap");
    }

    #[test]
    fn page_urls_follow_the_configured_wiki() {
        assert_eq!(
            wiki_page_url("https://de.wikipedia.org/w/api.php", 7),
            "https://de.wikipedia.org/?curid=7"
        );
        assert_eq!(
            wiki_page_url("https://en.wikipedia.org/w/api.php", 7),
            "https://en.wikipedia.org/?curid=7"
        );
        assert!(wiki_page_url("not a url", 7).ends_with("?curid=7"));
    }

    #[test]
    fn place_labels_separate_destinations_from_everything_else() {
        assert!(is_a_place(&["winery".into()]));
        assert!(is_a_place(&["human".into(), "ski resort".into()]));
        assert!(!is_a_place(&["human".into()]));
        assert!(!is_a_place(&["album".into(), "song".into()]));
        assert!(!is_a_place(&[]));
    }

    #[test]
    fn the_lead_paragraph_becomes_the_summary() {
        let text = "Palisade is a town in Mesa County.\n\nIt is known for peaches.\n\nMore.";
        assert_eq!(first_paragraph(text), "Palisade is a town in Mesa County.");
        assert_eq!(first_paragraph(""), "");
    }

    #[test]
    fn claims_yield_their_entity_ids() {
        let claim: Claim = serde_json::from_value(serde_json::json!({
            "mainsnak": { "datavalue": { "value": { "id": "Q33506", "entity-type": "item" } } }
        }))
        .unwrap();
        assert_eq!(claim_entity_id(&claim).as_deref(), Some("Q33506"));

        // Claims with no value at all ("unknown value" snaks) must not panic.
        let empty: Claim = serde_json::from_value(serde_json::json!({ "mainsnak": {} })).unwrap();
        assert_eq!(claim_entity_id(&empty), None);
    }

    #[test]
    fn a_geosearch_response_parses() {
        let res: GeoSearchResponse = serde_json::from_value(serde_json::json!({
            "batchcomplete": true,
            "query": { "geosearch": [
                { "pageid": 123, "ns": 0, "title": "Palisade, Colorado",
                  "lat": 39.11, "lon": -108.35, "dist": 1200.0, "primary": true }
            ]}
        }))
        .unwrap();
        let hits = res.query.unwrap().geosearch;
        assert_eq!(hits[0].pageid, 123);
        assert_eq!(hits[0].lat, 39.11);
        assert_eq!(hits[0].lon, -108.35);
    }

    #[test]
    fn an_empty_query_block_is_not_an_error() {
        let res: GeoSearchResponse =
            serde_json::from_value(serde_json::json!({ "batchcomplete": true })).unwrap();
        assert!(res.query.is_none());
    }
}
