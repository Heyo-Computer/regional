//! The document schema.
//!
//! All three indexes share one `Envelope` of geo + provenance fields and
//! add their own payload on top. The envelope is `#[serde(flatten)]`ed, so
//! what lands in Meilisearch is a flat document and every index can be
//! filtered on the same geo/city/category attributes.

use serde::{Deserialize, Serialize};

use crate::id;

/// Maximum number of characters kept in `body`. Long pages are truncated
/// on a character boundary; Meilisearch only indexes the first ~65k
/// positions of an attribute anyway, and huge bodies bloat the index.
pub const MAX_BODY_CHARS: usize = 20_000;

/// Maximum number of characters kept in `summary`.
pub const MAX_SUMMARY_CHARS: usize = 600;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Place,
    Event,
    Article,
}

impl Kind {
    pub const ALL: [Kind; 3] = [Kind::Place, Kind::Event, Kind::Article];

    /// The Meilisearch index a document of this kind lives in.
    pub fn index(&self) -> &'static str {
        match self {
            Kind::Place => crate::index::PLACES,
            Kind::Event => crate::index::EVENTS,
            Kind::Article => crate::index::ARTICLES,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Kind::Place => "place",
            Kind::Event => "event",
            Kind::Article => "article",
        }
    }

    pub fn parse(s: &str) -> Option<Kind> {
        match s.trim().to_ascii_lowercase().as_str() {
            "place" | "places" => Some(Kind::Place),
            "event" | "events" => Some(Kind::Event),
            "article" | "articles" => Some(Kind::Article),
            _ => None,
        }
    }
}

impl std::fmt::Display for Kind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How much to trust a document's coordinates.
///
/// Sources differ: OSM gives us a node, a crawled page might only yield a
/// street address we geocoded, or nothing better than the city it was
/// published in. Keeping this explicit lets a caller ask for precise hits
/// only rather than silently ranking a city centroid next to a real address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GeoPrecision {
    /// Coordinates came straight from the source (OSM node, Wikidata P625).
    Exact,
    /// Geocoded from a street address.
    Address,
    /// Fell back to the centroid of a gazetteer city.
    City,
    /// Fell back to the region centroid. Barely located at all.
    Region,
}

impl GeoPrecision {
    pub fn as_str(&self) -> &'static str {
        match self {
            GeoPrecision::Exact => "exact",
            GeoPrecision::Address => "address",
            GeoPrecision::City => "city",
            GeoPrecision::Region => "region",
        }
    }
}

/// Meilisearch requires this exact shape under the key `_geo`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GeoPoint {
    pub lat: f64,
    pub lng: f64,
}

impl GeoPoint {
    pub fn new(lat: f64, lng: f64) -> Self {
        Self { lat, lng }
    }

    pub fn is_valid(&self) -> bool {
        self.lat.is_finite()
            && self.lng.is_finite()
            && (-90.0..=90.0).contains(&self.lat)
            && (-180.0..=180.0).contains(&self.lng)
    }

    /// Great-circle distance in metres. Used for local sanity checks and
    /// tests; live queries let Meilisearch compute `_geoDistance`.
    pub fn distance_m(&self, other: &GeoPoint) -> f64 {
        const R: f64 = 6_371_008.8;
        let (lat1, lat2) = (self.lat.to_radians(), other.lat.to_radians());
        let dlat = lat2 - lat1;
        let dlng = (other.lng - self.lng).to_radians();
        let a = (dlat / 2.0).sin().powi(2) + lat1.cos() * lat2.cos() * (dlng / 2.0).sin().powi(2);
        2.0 * R * a.sqrt().asin()
    }
}

/// Fields every document carries, regardless of index.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    /// Stable, deterministic primary key. See [`crate::id::stable_id`].
    pub id: String,
    pub kind: Kind,
    pub title: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub url: Option<String>,
    /// Which adapter produced this: `osm`, `wikipedia`, `wikidata`, `crawl`.
    pub source: String,
    /// The source's own identifier, unique within `source`.
    pub source_id: String,
    #[serde(default)]
    pub categories: Vec<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(rename = "_geo")]
    pub geo: GeoPoint,
    #[serde(default)]
    pub city: Option<String>,
    #[serde(default)]
    pub county: Option<String>,
    #[serde(default)]
    pub address: Option<String>,
    pub geo_precision: GeoPrecision,
    /// When the upstream record last changed, unix seconds.
    pub updated_at: i64,
    /// When we last wrote it, unix seconds. Excluded from `content_hash`.
    pub indexed_at: i64,
    /// Hash of the meaningful fields, used to skip no-op writes.
    #[serde(default)]
    pub content_hash: String,
    /// Distance from the query point, in metres. Only present on search
    /// results sorted by `_geoPoint(..)` — never written to the index.
    #[serde(rename = "_geoDistance", default, skip_serializing)]
    pub geo_distance: Option<f64>,
}

impl Envelope {
    /// Start a new envelope with sane defaults and a derived stable id.
    pub fn new(
        kind: Kind,
        source: impl Into<String>,
        source_id: impl Into<String>,
        title: impl Into<String>,
        geo: GeoPoint,
    ) -> Self {
        let source = source.into();
        let source_id = source_id.into();
        let now = now_ts();
        Self {
            id: id::stable_id(&source, &source_id),
            kind,
            title: title.into(),
            summary: String::new(),
            body: String::new(),
            url: None,
            source,
            source_id,
            categories: Vec::new(),
            tags: Vec::new(),
            geo,
            city: None,
            county: None,
            address: None,
            geo_precision: GeoPrecision::Exact,
            updated_at: now,
            indexed_at: now,
            content_hash: String::new(),
            geo_distance: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Place {
    #[serde(flatten)]
    pub env: Envelope,
    #[serde(default)]
    pub phone: Option<String>,
    #[serde(default)]
    pub website: Option<String>,
    #[serde(default)]
    pub opening_hours: Option<String>,
    #[serde(default)]
    pub cuisine: Vec<String>,
    #[serde(default)]
    pub price_level: Option<String>,
    #[serde(default)]
    pub osm_type: Option<String>,
    #[serde(default)]
    pub osm_id: Option<i64>,
    #[serde(default)]
    pub wikidata_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    #[serde(flatten)]
    pub env: Envelope,
    /// Unix seconds. Filterable and sortable so callers can ask for
    /// "what's on next weekend near Durango".
    pub start_time: i64,
    #[serde(default)]
    pub end_time: Option<i64>,
    #[serde(default)]
    pub venue_name: Option<String>,
    #[serde(default)]
    pub organizer: Option<String>,
    #[serde(default)]
    pub price: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Article {
    #[serde(flatten)]
    pub env: Envelope,
    #[serde(default)]
    pub published_at: Option<i64>,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub site_name: Option<String>,
    #[serde(default)]
    pub lang: Option<String>,
    #[serde(default)]
    pub word_count: Option<u32>,
}

/// A document on its way into the index, tagged with where it belongs.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum Doc {
    Place(Place),
    Event(Event),
    Article(Article),
}

impl Doc {
    pub fn env(&self) -> &Envelope {
        match self {
            Doc::Place(d) => &d.env,
            Doc::Event(d) => &d.env,
            Doc::Article(d) => &d.env,
        }
    }

    pub fn env_mut(&mut self) -> &mut Envelope {
        match self {
            Doc::Place(d) => &mut d.env,
            Doc::Event(d) => &mut d.env,
            Doc::Article(d) => &mut d.env,
        }
    }

    pub fn id(&self) -> &str {
        &self.env().id
    }

    pub fn kind(&self) -> Kind {
        self.env().kind
    }

    pub fn index(&self) -> &'static str {
        self.kind().index()
    }

    pub fn geo(&self) -> GeoPoint {
        self.env().geo
    }

    /// Normalise the text fields and stamp `content_hash`.
    ///
    /// Call this once, last, before handing a document to the pipeline.
    pub fn finalize(mut self) -> Self {
        {
            let env = self.env_mut();
            env.title = collapse_ws(&env.title);
            env.summary = truncate_chars(&collapse_ws(&env.summary), MAX_SUMMARY_CHARS);
            env.body = truncate_chars(&collapse_ws(&env.body), MAX_BODY_CHARS);
            env.categories = dedupe_lower(&env.categories);
            env.tags = dedupe_lower(&env.tags);
            env.indexed_at = now_ts();
        }
        let hash = id::content_hash(&self);
        self.env_mut().content_hash = hash;
        self
    }
}

pub fn now_ts() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Collapse all runs of whitespace to a single space and trim.
pub fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut space = false;
    for c in s.chars() {
        if c.is_whitespace() {
            space = !out.is_empty();
        } else {
            if space {
                out.push(' ');
                space = false;
            }
            out.push(c);
        }
    }
    out
}

/// Truncate to `max` characters on a char boundary, appending an ellipsis
/// when anything was actually cut.
pub fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    // Prefer to break at the last word boundary so we don't cut mid-word.
    if let Some(sp) = out.rfind(' ')
        && sp > max.saturating_sub(1) * 4 / 5
    {
        out.truncate(sp);
    }
    out.push('…');
    out
}

fn dedupe_lower(v: &[String]) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::with_capacity(v.len());
    for item in v {
        let c = collapse_ws(item).to_lowercase();
        if !c.is_empty() && seen.insert(c.clone()) {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn place() -> Doc {
        let mut env = Envelope::new(
            Kind::Place,
            "osm",
            "node/123",
            "  Carlson   Vineyards ",
            GeoPoint::new(39.11, -108.35),
        );
        env.categories = vec!["Winery".into(), "winery".into(), " Vineyard ".into()];
        Doc::Place(Place {
            env,
            phone: None,
            website: None,
            opening_hours: None,
            cuisine: vec![],
            price_level: None,
            osm_type: Some("node".into()),
            osm_id: Some(123),
            wikidata_id: None,
        })
    }

    #[test]
    fn finalize_normalises_text_and_categories() {
        let d = place().finalize();
        assert_eq!(d.env().title, "Carlson Vineyards");
        assert_eq!(d.env().categories, vec!["winery", "vineyard"]);
        assert!(!d.env().content_hash.is_empty());
    }

    #[test]
    fn kind_routes_to_its_index() {
        assert_eq!(Kind::Place.index(), "places");
        assert_eq!(Kind::Event.index(), "events");
        assert_eq!(Kind::Article.index(), "articles");
        assert_eq!(Kind::parse("Places"), Some(Kind::Place));
        assert_eq!(Kind::parse("nope"), None);
    }

    #[test]
    fn geo_serialises_under_the_underscore_geo_key() {
        let d = place().finalize();
        let v = serde_json::to_value(&d).unwrap();
        assert_eq!(v["_geo"]["lat"], 39.11);
        assert_eq!(v["_geo"]["lng"], -108.35);
        // The envelope must be flattened, not nested under "env".
        assert_eq!(v["title"], "Carlson Vineyards");
        assert!(v.get("env").is_none());
        // _geoDistance is a read-side annotation and must never be written.
        assert!(v.get("_geoDistance").is_none());
    }

    #[test]
    fn truncate_breaks_on_a_word_boundary_and_marks_the_cut() {
        let s = "alpha beta gamma delta epsilon zeta";
        let out = truncate_chars(s, 20);
        assert!(out.chars().count() <= 20);
        assert!(out.ends_with('…'));
        assert!(out.starts_with("alpha beta"));
        assert_eq!(truncate_chars("short", 20), "short");
    }

    #[test]
    fn distance_is_roughly_right() {
        // Denver -> Grand Junction is about 340 km as the crow flies.
        let denver = GeoPoint::new(39.7392, -104.9903);
        let gj = GeoPoint::new(39.0639, -108.5506);
        let km = denver.distance_m(&gj) / 1000.0;
        assert!((300.0..380.0).contains(&km), "got {km} km");
    }
}
