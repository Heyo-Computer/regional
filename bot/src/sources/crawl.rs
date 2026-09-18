//! A polite, region-scoped web crawler.
//!
//! This is the source that *expands* the index: every page it parses is
//! also mined for in-domain links, which go back onto the frontier. It
//! prefers schema.org JSON-LD, which most event calendars, restaurant sites
//! and news CMSes already emit, and falls back to the page's own metadata.
//!
//! A page that cannot be placed inside the region is dropped rather than
//! guessed at. That is the rule that keeps "everything here is in the
//! region" true.

use std::collections::HashSet;
use std::sync::LazyLock;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use regional_core::model::{
    Article, Doc, Envelope, Event, GeoPoint, GeoPrecision, Kind, Place, collapse_ws, truncate_chars,
};
use scraper::{Html, Selector};
use serde_json::Value;
use url::Url;

use super::{Batch, Ctx, Source};
use crate::state::FrontierEntry;

/// File extensions that are never worth fetching as HTML.
const SKIP_EXTENSIONS: &[&str] = &[
    ".pdf", ".jpg", ".jpeg", ".png", ".gif", ".svg", ".webp", ".ico", ".css", ".js", ".json",
    ".xml", ".zip", ".gz", ".mp3", ".mp4", ".mov", ".avi", ".doc", ".docx", ".xls", ".xlsx",
    ".ppt", ".pptx", ".rss", ".woff", ".woff2", ".ttf",
];

static LD_JSON: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse(r#"script[type="application/ld+json"]"#).unwrap());
static LINKS: LazyLock<Selector> = LazyLock::new(|| Selector::parse("a[href]").unwrap());
static TITLE: LazyLock<Selector> = LazyLock::new(|| Selector::parse("title").unwrap());
static META: LazyLock<Selector> = LazyLock::new(|| Selector::parse("meta").unwrap());
static MAIN: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("main, article, [role=main]").unwrap());
static BODY: LazyLock<Selector> = LazyLock::new(|| Selector::parse("body").unwrap());

pub struct Crawl;

#[async_trait]
impl Source for Crawl {
    fn name(&self) -> &'static str {
        "crawl"
    }

    fn default_interval(&self) -> Duration {
        Duration::from_secs(10 * 60)
    }

    async fn fetch(&self, ctx: &Ctx, _cursor: Option<Value>) -> Result<Batch> {
        if ctx.config.crawl_seeds.is_empty() {
            tracing::debug!("no CRAWL_SEEDS configured; the crawl source has nothing to do");
            return Ok(Batch::empty());
        }
        self.seed(ctx).await?;

        let claimed = ctx
            .state
            .frontier_claim(ctx.config.crawl_pages_per_run)
            .await?;
        if claimed.is_empty() {
            // Everything known has been visited; start the cycle again so
            // changed pages are eventually re-read.
            tracing::info!("crawl frontier is empty; re-seeding for another pass");
            self.reseed(ctx).await?;
            return Ok(Batch::empty());
        }

        let mut docs = Vec::new();
        for entry in &claimed {
            match self.visit(ctx, entry).await {
                Ok(mut produced) => {
                    docs.append(&mut produced);
                    let _ = ctx.state.frontier_finish(&entry.url, "done").await;
                }
                Err(e) => {
                    tracing::warn!(url = %entry.url, error = %e, "crawl page failed");
                    let _ = ctx.state.frontier_finish(&entry.url, "error").await;
                }
            }
        }

        let pending = ctx.state.frontier_pending().await;
        tracing::info!(
            visited = claimed.len(),
            produced = docs.len(),
            pending,
            "crawl cycle complete"
        );
        Ok(Batch::new(docs, None))
    }
}

impl Crawl {
    /// Put the configured seeds on the frontier. Existing rows are left
    /// alone, so this is safe to call on every run.
    async fn seed(&self, ctx: &Ctx) -> Result<()> {
        let entries: Vec<FrontierEntry> = ctx
            .config
            .crawl_seeds
            .iter()
            .map(|s| FrontierEntry {
                url: s.url.to_string(),
                depth: 0,
                default_city: s.default_city.clone(),
            })
            .collect();
        let added = ctx.state.frontier_add(&entries).await?;
        if added > 0 {
            tracing::info!(added, "seeded the crawl frontier");
        }
        Ok(())
    }

    /// Reset visited seeds to pending so the crawl starts another pass.
    async fn reseed(&self, ctx: &Ctx) -> Result<()> {
        for seed in &ctx.config.crawl_seeds {
            let _ = ctx
                .state
                .frontier_finish(seed.url.as_str(), "pending")
                .await;
        }
        Ok(())
    }

    async fn visit(&self, ctx: &Ctx, entry: &FrontierEntry) -> Result<Vec<Doc>> {
        if !ctx.http.robots_allow(&entry.url).await {
            tracing::debug!(url = %entry.url, "robots.txt disallows this page");
            return Ok(Vec::new());
        }
        let html = ctx.http.get_text(&entry.url).await?;
        let base = Url::parse(&entry.url)?;

        // Enqueue what this page links to before extracting, so coverage
        // keeps growing even if extraction finds nothing here.
        if entry.depth < ctx.config.crawl_max_depth {
            let links = extract_links(&html, &base);
            let next: Vec<FrontierEntry> = links
                .into_iter()
                .map(|url| FrontierEntry {
                    url,
                    depth: entry.depth + 1,
                    default_city: entry.default_city.clone(),
                })
                .collect();
            match ctx.state.frontier_add(&next).await {
                Ok(n) if n > 0 => {
                    tracing::debug!(url = %entry.url, discovered = n, "new links queued")
                }
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "could not extend the frontier"),
            }
        }

        extract_docs(ctx, &html, &base, entry.default_city.as_deref()).await
    }
}

// ------------------------------------------------------------- extraction

/// Every JSON-LD node on the page, with `@graph` containers flattened out.
#[cfg(test)]
fn json_ld_nodes(html: &str) -> Vec<Value> {
    read_page(html, &Url::parse("https://example.invalid/").unwrap()).nodes
}

fn flatten_ld(value: Value, out: &mut Vec<Value>) {
    match value {
        Value::Array(items) => {
            for item in items {
                flatten_ld(item, out);
            }
        }
        Value::Object(ref map) => {
            if let Some(graph) = map.get("@graph").cloned() {
                flatten_ld(graph, out);
            }
            out.push(value);
        }
        _ => {}
    }
}

/// `@type` can be a string or a list; normalise to lowercase strings.
fn ld_types(node: &Value) -> Vec<String> {
    match node.get("@type") {
        Some(Value::String(s)) => vec![s.to_lowercase()],
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|v| v.as_str().map(str::to_lowercase))
            .collect(),
        _ => Vec::new(),
    }
}

fn ld_str(node: &Value, key: &str) -> Option<String> {
    match node.get(key)? {
        Value::String(s) => Some(collapse_ws(s)).filter(|s| !s.is_empty()),
        // Some publishers wrap a plain value in an object or a list.
        Value::Array(a) => a.first().and_then(|v| v.as_str()).map(collapse_ws),
        Value::Object(o) => o
            .get("name")
            .or_else(|| o.get("@value"))
            .and_then(|v| v.as_str())
            .map(collapse_ws),
        _ => None,
    }
}

/// Coordinates from a schema.org `geo` block, on the node or its `location`.
fn ld_geo(node: &Value) -> Option<GeoPoint> {
    for candidate in [
        node.get("geo"),
        node.get("location").and_then(|l| l.get("geo")),
    ] {
        let Some(geo) = candidate else { continue };
        let lat = num(geo.get("latitude"))?;
        let lng = num(geo.get("longitude"))?;
        let p = GeoPoint::new(lat, lng);
        if p.is_valid() {
            return Some(p);
        }
    }
    None
}

/// schema.org numbers arrive as numbers or as strings, depending on the CMS.
fn num(v: Option<&Value>) -> Option<f64> {
    match v? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// Flatten a `PostalAddress` (or its `location`) into one line.
fn ld_address(node: &Value) -> Option<String> {
    let addr = node
        .get("address")
        .or_else(|| node.get("location").and_then(|l| l.get("address")))?;
    if let Value::String(s) = addr {
        return Some(collapse_ws(s)).filter(|s| !s.is_empty());
    }
    let parts: Vec<String> = [
        "streetAddress",
        "addressLocality",
        "addressRegion",
        "postalCode",
    ]
    .iter()
    .filter_map(|k| addr.get(*k)?.as_str().map(collapse_ws))
    .filter(|s| !s.is_empty())
    .collect();
    (!parts.is_empty()).then(|| parts.join(", "))
}

fn ld_locality(node: &Value) -> Option<String> {
    for base in [node.get("address"), node.get("location")] {
        let Some(b) = base else { continue };
        if let Some(l) = b.get("addressLocality").and_then(|v| v.as_str()) {
            return Some(collapse_ws(l));
        }
        if let Some(l) = b
            .get("address")
            .and_then(|a| a.get("addressLocality"))
            .and_then(|v| v.as_str())
        {
            return Some(collapse_ws(l));
        }
    }
    None
}

/// Parse a schema.org date: full RFC 3339, or a bare `YYYY-MM-DD`.
pub fn parse_schema_date(raw: &str) -> Option<i64> {
    let raw = raw.trim();
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(raw) {
        return Some(dt.timestamp());
    }
    // Local datetimes without a zone are common; read them as UTC.
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S") {
        return Some(dt.and_utc().timestamp());
    }
    if let Ok(d) = chrono::NaiveDate::parse_from_str(raw, "%Y-%m-%d") {
        return Some(d.and_hms_opt(0, 0, 0)?.and_utc().timestamp());
    }
    None
}

fn meta_content(doc: &Html, keys: &[&str]) -> Option<String> {
    for el in doc.select(&META) {
        let v = el.value();
        let name = v.attr("property").or_else(|| v.attr("name")).unwrap_or("");
        if keys.iter().any(|k| k.eq_ignore_ascii_case(name))
            && let Some(content) = v.attr("content")
        {
            let c = collapse_ws(content);
            if !c.is_empty() {
                return Some(c);
            }
        }
    }
    None
}

fn page_text(doc: &Html) -> String {
    let region = doc
        .select(&MAIN)
        .next()
        .or_else(|| doc.select(&BODY).next());
    match region {
        Some(el) => collapse_ws(&el.text().collect::<Vec<_>>().join(" ")),
        None => String::new(),
    }
}

/// In-domain, fetchable links, absolute and de-fragmented.
pub fn extract_links(html: &str, base: &Url) -> Vec<String> {
    let doc = Html::parse_document(html);
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for el in doc.select(&LINKS) {
        let Some(href) = el.value().attr("href") else {
            continue;
        };
        let Ok(mut url) = base.join(href) else {
            continue;
        };
        if !matches!(url.scheme(), "http" | "https") {
            continue;
        }
        // Staying on one host keeps a crawl scoped to the site an operator
        // actually chose, rather than wandering off across the web.
        if url.host_str() != base.host_str() {
            continue;
        }
        url.set_fragment(None);
        let path = url.path().to_lowercase();
        if SKIP_EXTENSIONS.iter().any(|e| path.ends_with(e)) {
            continue;
        }
        let s = url.to_string();
        if seen.insert(s.clone()) {
            out.push(s);
        }
    }
    out
}

/// Everything worth having from one page, read out in a single pass.
///
/// `scraper::Html` is not `Send`, and this runs inside an async task, so the
/// document is parsed, drained and dropped before any `await` happens.
#[derive(Debug, Default)]
struct PageFacts {
    nodes: Vec<Value>,
    site_name: Option<String>,
    title: Option<String>,
    description: Option<String>,
    text: String,
    published: Option<String>,
    author: Option<String>,
    meta_lat: Option<f64>,
    meta_lng: Option<f64>,
    meta_locality: Option<String>,
}

fn read_page(html: &str, base: &Url) -> PageFacts {
    let doc = Html::parse_document(html);
    let mut nodes = Vec::new();
    for script in doc.select(&LD_JSON) {
        let raw: String = script.text().collect();
        if let Ok(value) = serde_json::from_str::<Value>(&raw) {
            flatten_ld(value, &mut nodes);
        }
    }
    PageFacts {
        nodes,
        site_name: meta_content(&doc, &["og:site_name"])
            .or_else(|| base.host_str().map(str::to_string)),
        title: meta_content(&doc, &["og:title"]).or_else(|| {
            doc.select(&TITLE)
                .next()
                .map(|t| collapse_ws(&t.text().collect::<String>()))
                .filter(|t| !t.is_empty())
        }),
        description: meta_content(&doc, &["og:description", "description"]),
        text: page_text(&doc),
        published: meta_content(&doc, &["article:published_time"]),
        author: meta_content(&doc, &["author", "article:author"]),
        meta_lat: meta_content(&doc, &["place:location:latitude", "geo.position.latitude"])
            .and_then(|v| v.trim().parse::<f64>().ok()),
        meta_lng: meta_content(
            &doc,
            &["place:location:longitude", "geo.position.longitude"],
        )
        .and_then(|v| v.trim().parse::<f64>().ok()),
        meta_locality: meta_content(&doc, &["og:locality", "geo.placename"]),
    }
}

/// Turn one fetched page into documents.
async fn extract_docs(
    ctx: &Ctx,
    html: &str,
    base: &Url,
    default_city: Option<&str>,
) -> Result<Vec<Doc>> {
    let facts = read_page(html, base);
    let nodes = &facts.nodes;
    let site_name = facts.site_name.clone();

    let mut out = Vec::new();
    let mut structured = false;

    for node in nodes {
        let types = ld_types(node);
        let Some(title) = ld_str(node, "name").or_else(|| ld_str(node, "headline")) else {
            continue;
        };
        let url = ld_str(node, "url").unwrap_or_else(|| base.to_string());
        let description = ld_str(node, "description").unwrap_or_default();

        let located = locate(ctx, node, default_city).await;

        if types.iter().any(|t| t.contains("event")) {
            let Some(start) = ld_str(node, "startDate").and_then(|d| parse_schema_date(&d)) else {
                // An event with no start time cannot be filtered by time,
                // which is the only thing the events index is for.
                continue;
            };
            let Some((geo, precision, city)) = located.clone() else {
                continue;
            };
            let mut env = Envelope::new(Kind::Event, "crawl", format!("event:{url}"), &title, geo);
            env.geo_precision = precision;
            env.url = Some(url.clone());
            env.summary = truncate_chars(&description, 500);
            env.body = description.clone();
            env.city = city;
            env.address = ld_address(node);
            env.categories = vec!["event".into()];
            out.push(Doc::Event(Event {
                env,
                start_time: start,
                end_time: ld_str(node, "endDate").and_then(|d| parse_schema_date(&d)),
                venue_name: node
                    .get("location")
                    .and_then(|l| l.get("name"))
                    .and_then(|v| v.as_str())
                    .map(collapse_ws),
                organizer: ld_str(node, "organizer"),
                price: node
                    .get("offers")
                    .and_then(|o| o.get("price"))
                    .map(|p| p.to_string().trim_matches('"').to_string()),
            }));
            structured = true;
        } else if is_business(&types) {
            let Some((geo, precision, city)) = located.clone() else {
                continue;
            };
            let mut env = Envelope::new(Kind::Place, "crawl", format!("place:{url}"), &title, geo);
            env.geo_precision = precision;
            env.url = Some(url.clone());
            env.summary = truncate_chars(&description, 500);
            env.body = description.clone();
            env.city = city;
            env.address = ld_address(node);
            env.categories = types.clone();
            out.push(Doc::Place(Place {
                env,
                phone: ld_str(node, "telephone"),
                website: Some(url),
                opening_hours: ld_str(node, "openingHours"),
                cuisine: ld_str(node, "servesCuisine").into_iter().collect(),
                price_level: ld_str(node, "priceRange"),
                osm_type: None,
                osm_id: None,
                wikidata_id: None,
            }));
            structured = true;
        } else if is_article(&types) {
            let Some((geo, precision, city)) = located.clone() else {
                continue;
            };
            let body = facts.text.clone();
            let mut env = Envelope::new(
                Kind::Article,
                "crawl",
                format!("article:{url}"),
                &title,
                geo,
            );
            env.geo_precision = precision;
            env.url = Some(url);
            env.summary = truncate_chars(&description, 500);
            env.body = body.clone();
            env.city = city;
            out.push(Doc::Article(Article {
                env,
                published_at: ld_str(node, "datePublished").and_then(|d| parse_schema_date(&d)),
                author: ld_str(node, "author"),
                site_name: site_name.clone(),
                lang: None,
                word_count: Some(body.split_whitespace().count() as u32),
            }));
            structured = true;
        }
    }

    // No usable JSON-LD: fall back to the page's own metadata. Plenty of
    // useful regional content is published by sites that emit none.
    if !structured {
        let title = facts.title.clone().unwrap_or_default();
        if !title.is_empty()
            && let Some((geo, precision, city)) = locate_page(ctx, &facts, default_city)
        {
            let body = facts.text.clone();
            let description = facts
                .description
                .clone()
                .unwrap_or_else(|| truncate_chars(&body, 500));
            let url = base.to_string();
            let mut env = Envelope::new(
                Kind::Article,
                "crawl",
                format!("article:{url}"),
                &title,
                geo,
            );
            env.geo_precision = precision;
            env.url = Some(url);
            env.summary = truncate_chars(&description, 500);
            env.body = body.clone();
            env.city = city;
            out.push(Doc::Article(Article {
                env,
                published_at: facts.published.as_deref().and_then(parse_schema_date),
                author: facts.author.clone(),
                site_name,
                lang: None,
                word_count: Some(body.split_whitespace().count() as u32),
            }));
        }
    }

    Ok(out)
}

fn is_business(types: &[String]) -> bool {
    const BUSINESS: &[&str] = &[
        "localbusiness",
        "restaurant",
        "cafe",
        "bar",
        "brewery",
        "winery",
        "foodestablishment",
        "bakery",
        "store",
        "shop",
        "touristattraction",
        "touristdestination",
        "lodgingbusiness",
        "hotel",
        "campground",
        "museum",
        "park",
        "place",
        "civicstructure",
        "landmarksorhistoricalbuildings",
        "performingartstheater",
        "entertainmentbusiness",
        "sportsactivitylocation",
        "wineryvisit",
    ];
    types.iter().any(|t| BUSINESS.contains(&t.as_str()))
}

fn is_article(types: &[String]) -> bool {
    types.iter().any(|t| {
        t.contains("article") || t == "blogposting" || t == "newsarticle" || t == "webpage"
    })
}

/// Where is this JSON-LD node?
///
/// In descending order of confidence: explicit coordinates, a geocoded
/// street address, the named locality, and finally the seed's configured
/// town. Returning `None` means we genuinely do not know, and the caller
/// drops the document rather than inventing a position for it.
async fn locate(
    ctx: &Ctx,
    node: &Value,
    default_city: Option<&str>,
) -> Option<(GeoPoint, GeoPrecision, Option<String>)> {
    if let Some(geo) = ld_geo(node)
        && ctx.region.contains(geo.lat, geo.lng)
    {
        let city = ld_locality(node);
        return Some((geo, GeoPrecision::Exact, city));
    }
    if let Some(address) = ld_address(node)
        && let Some(geo) = ctx.geocoder.geocode(&address).await
    {
        return Some((geo, GeoPrecision::Address, ld_locality(node)));
    }
    if let Some(locality) = ld_locality(node)
        && let Some(city) = ctx.region.resolve_place(&locality)
    {
        return Some((city.geo(), GeoPrecision::City, Some(city.name.clone())));
    }
    from_default_city(ctx, default_city)
}

/// The same question for a page with no JSON-LD, using its metadata.
fn locate_page(
    ctx: &Ctx,
    facts: &PageFacts,
    default_city: Option<&str>,
) -> Option<(GeoPoint, GeoPrecision, Option<String>)> {
    if let (Some(lat), Some(lng)) = (facts.meta_lat, facts.meta_lng) {
        let p = GeoPoint::new(lat, lng);
        if p.is_valid() && ctx.region.contains(lat, lng) {
            return Some((p, GeoPrecision::Exact, None));
        }
    }
    if let Some(locality) = &facts.meta_locality
        && let Some(city) = ctx.region.resolve_place(locality)
    {
        return Some((city.geo(), GeoPrecision::City, Some(city.name.clone())));
    }
    from_default_city(ctx, default_city)
}

fn from_default_city(
    ctx: &Ctx,
    default_city: Option<&str>,
) -> Option<(GeoPoint, GeoPrecision, Option<String>)> {
    let city = ctx.region.resolve_place(default_city?)?;
    Some((city.geo(), GeoPrecision::City, Some(city.name.clone())))
}

#[cfg(test)]
mod tests {
    use super::*;

    const EVENT_PAGE: &str = r#"<html><head><title>Peach Festival</title>
      <script type="application/ld+json">
      {"@context":"https://schema.org","@type":"Event",
       "name":"Palisade Peach Festival","description":"Three days of peaches.",
       "startDate":"2026-08-14T09:00:00-06:00","endDate":"2026-08-16T18:00:00-06:00",
       "url":"https://example.test/peach",
       "location":{"@type":"Place","name":"Riverbend Park",
         "address":{"@type":"PostalAddress","streetAddress":"451 Pendleton St",
                    "addressLocality":"Palisade","addressRegion":"CO"},
         "geo":{"@type":"GeoCoordinates","latitude":39.1103,"longitude":-108.3509}},
       "offers":{"@type":"Offer","price":"25.00"}}
      </script></head><body><main>Peaches.</main></body></html>"#;

    #[test]
    fn json_ld_is_found_and_typed() {
        let nodes = json_ld_nodes(EVENT_PAGE);
        assert_eq!(nodes.len(), 1);
        assert_eq!(ld_types(&nodes[0]), vec!["event"]);
        assert_eq!(
            ld_str(&nodes[0], "name").as_deref(),
            Some("Palisade Peach Festival")
        );
        let geo = ld_geo(&nodes[0]).unwrap();
        assert_eq!(geo.lat, 39.1103);
        assert_eq!(ld_locality(&nodes[0]).as_deref(), Some("Palisade"));
        assert_eq!(
            ld_address(&nodes[0]).as_deref(),
            Some("451 Pendleton St, Palisade, CO")
        );
    }

    #[test]
    fn a_graph_container_is_flattened() {
        let html = r#"<script type="application/ld+json">
          {"@context":"https://schema.org","@graph":[
            {"@type":"Restaurant","name":"A"},{"@type":["NewsArticle","Article"],"name":"B"}]}
          </script>"#;
        let nodes = json_ld_nodes(html);
        let names: Vec<String> = nodes.iter().filter_map(|n| ld_str(n, "name")).collect();
        assert!(names.contains(&"A".to_string()));
        assert!(names.contains(&"B".to_string()));
        let article = nodes
            .iter()
            .find(|n| ld_str(n, "name").as_deref() == Some("B"))
            .unwrap();
        assert!(is_article(&ld_types(article)));
    }

    #[test]
    fn malformed_json_ld_does_not_break_the_page() {
        let html = r#"<script type="application/ld+json">{not json</script>
                      <script type="application/ld+json">{"@type":"Restaurant","name":"OK"}</script>"#;
        let nodes = json_ld_nodes(html);
        assert_eq!(nodes.len(), 1);
        assert_eq!(ld_str(&nodes[0], "name").as_deref(), Some("OK"));
    }

    #[test]
    fn coordinates_given_as_strings_still_parse() {
        let html = r#"<script type="application/ld+json">
          {"@type":"Winery","name":"W","geo":{"latitude":"39.11","longitude":"-108.35"}}
        </script>"#;
        let nodes = json_ld_nodes(html);
        assert_eq!(ld_geo(&nodes[0]).unwrap().lat, 39.11);
    }

    #[test]
    fn schema_dates_parse_in_every_common_shape() {
        assert_eq!(parse_schema_date("1970-01-02"), Some(86_400));
        assert_eq!(parse_schema_date("1970-01-02T00:00:00Z"), Some(86_400));
        assert_eq!(parse_schema_date("1970-01-02T00:00:00+00:00"), Some(86_400));
        assert_eq!(parse_schema_date("1970-01-02T00:00:00"), Some(86_400));
        assert_eq!(parse_schema_date("sometime next August"), None);
    }

    #[test]
    fn business_and_article_types_are_recognised_case_insensitively() {
        assert!(is_business(&["restaurant".into()]));
        assert!(is_business(&["winery".into()]));
        assert!(is_business(&["thing".into(), "localbusiness".into()]));
        assert!(!is_business(&["person".into()]));
        assert!(is_article(&["newsarticle".into()]));
        assert!(is_article(&["blogposting".into()]));
        assert!(!is_article(&["restaurant".into()]));
    }

    #[test]
    fn links_stay_on_the_host_and_skip_non_html() {
        let base = Url::parse("https://a.test/news/").unwrap();
        let html = r#"
            <a href="/news/one">one</a>
            <a href="two.html#section">two</a>
            <a href="https://a.test/three">three</a>
            <a href="https://other.test/four">off-site</a>
            <a href="/doc.pdf">pdf</a>
            <a href="mailto:x@a.test">mail</a>
            <a href="/news/one">duplicate</a>
        "#;
        let links = extract_links(html, &base);
        assert!(links.contains(&"https://a.test/news/one".to_string()));
        assert!(
            links.contains(&"https://a.test/news/two.html".to_string()),
            "{links:?}"
        );
        assert!(links.contains(&"https://a.test/three".to_string()));
        assert!(!links.iter().any(|l| l.contains("other.test")));
        assert!(!links.iter().any(|l| l.ends_with(".pdf")));
        assert!(!links.iter().any(|l| l.starts_with("mailto")));
        assert_eq!(
            links.iter().filter(|l| l.ends_with("/news/one")).count(),
            1,
            "duplicates must collapse"
        );
    }

    #[test]
    fn meta_and_page_text_are_read_from_the_main_region() {
        let html = r#"<html><head>
            <meta property="og:title" content="  Best  Tacos ">
            <meta name="description" content="A guide.">
            </head><body><nav>menu junk</nav><main>The actual content.</main></body></html>"#;
        let doc = Html::parse_document(html);
        assert_eq!(
            meta_content(&doc, &["og:title"]).as_deref(),
            Some("Best Tacos")
        );
        assert_eq!(
            meta_content(&doc, &["description"]).as_deref(),
            Some("A guide.")
        );
        assert_eq!(page_text(&doc), "The actual content.");
        assert_eq!(meta_content(&doc, &["og:site_name"]), None);
    }
}
