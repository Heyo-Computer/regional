//! The region: what counts as "inside", and how place names become points.
//!
//! One `RegionConfig` is loaded from TOML at startup by both binaries. It is
//! the only thing that makes this stack Colorado-specific — point it at a
//! different file and the same images serve a different region.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::model::GeoPoint;

pub const DEFAULT_REGION_CONFIG: &str = "/etc/regional/region.toml";

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct BBox {
    pub min_lat: f64,
    pub min_lng: f64,
    pub max_lat: f64,
    pub max_lng: f64,
}

impl BBox {
    pub fn contains(&self, lat: f64, lng: f64) -> bool {
        (self.min_lat..=self.max_lat).contains(&lat) && (self.min_lng..=self.max_lng).contains(&lng)
    }

    /// Meilisearch wants the top-right (north-east) corner first, then the
    /// bottom-left (south-west).
    pub fn filter(&self) -> String {
        format!(
            "_geoBoundingBox([{}, {}], [{}, {}])",
            self.max_lat, self.max_lng, self.min_lat, self.min_lng
        )
    }

    /// Split into a `cols` x `rows` grid. Sources that cannot fetch a whole
    /// state in one request (Overpass, Wikipedia geosearch) walk these tiles
    /// one at a time across runs.
    pub fn tiles(&self, cols: usize, rows: usize) -> Vec<BBox> {
        let cols = cols.max(1);
        let rows = rows.max(1);
        let dlat = (self.max_lat - self.min_lat) / rows as f64;
        let dlng = (self.max_lng - self.min_lng) / cols as f64;
        let mut out = Vec::with_capacity(cols * rows);
        for r in 0..rows {
            for c in 0..cols {
                out.push(BBox {
                    min_lat: self.min_lat + dlat * r as f64,
                    max_lat: self.min_lat + dlat * (r + 1) as f64,
                    min_lng: self.min_lng + dlng * c as f64,
                    max_lng: self.min_lng + dlng * (c + 1) as f64,
                });
            }
        }
        out
    }

    pub fn center(&self) -> GeoPoint {
        GeoPoint::new(
            (self.min_lat + self.max_lat) / 2.0,
            (self.min_lng + self.max_lng) / 2.0,
        )
    }

    /// Roughly how wide and tall this box is, in kilometres.
    ///
    /// Longitude is scaled by the cosine of the mid-latitude, so a box over
    /// Colorado and one over Alaska of the same degree width report very
    /// different widths — which is the whole point of measuring in km.
    pub fn span_km(&self) -> (f64, f64) {
        const KM_PER_DEG_LAT: f64 = 111.0;
        let mid_lat = (self.min_lat + self.max_lat) / 2.0;
        let height = (self.max_lat - self.min_lat) * KM_PER_DEG_LAT;
        let width = (self.max_lng - self.min_lng) * KM_PER_DEG_LAT * mid_lat.to_radians().cos();
        (width.abs(), height.abs())
    }

    /// A grid whose tiles are about `target_km` on a side.
    ///
    /// This is what lets one region config serve a small state and a large
    /// one: the number of tiles follows the region's actual area instead of
    /// being fixed to whatever suited the region it was first written for.
    pub fn tiles_of_about(&self, target_km: f64) -> Vec<BBox> {
        let target = if target_km.is_finite() && target_km > 0.0 {
            target_km
        } else {
            75.0
        };
        let (w, h) = self.span_km();
        let cols = ((w / target).ceil() as usize).max(1);
        let rows = ((h / target).ceil() as usize).max(1);
        self.tiles(cols, rows)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct City {
    pub name: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    pub lat: f64,
    pub lng: f64,
    #[serde(default)]
    pub county: Option<String>,
    #[serde(default = "default_radius")]
    pub default_radius_m: u32,
}

fn default_radius() -> u32 {
    12_000
}

impl City {
    pub fn geo(&self) -> GeoPoint {
        GeoPoint::new(self.lat, self.lng)
    }
}

/// Language-dependent search settings.
///
/// These live with the region because they are the part of the index
/// configuration that does not travel: the synonyms that make "vineyard"
/// find a winery are English, and slanted towards what this region actually
/// has. A value given here **replaces** the built-in default rather than
/// adding to it, so edit the shipped list rather than appending to it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Vocabulary {
    /// Words ignored when matching. Language-specific.
    #[serde(default = "default_stop_words")]
    pub stop_words: Vec<String>,
    /// Groups of interchangeable terms. Every term in a group is made a
    /// synonym of every other, in both directions.
    #[serde(default = "default_synonyms")]
    pub synonyms: Vec<Vec<String>>,
}

impl Default for Vocabulary {
    fn default() -> Self {
        Self {
            stop_words: default_stop_words(),
            synonyms: default_synonyms(),
        }
    }
}

impl Vocabulary {
    /// Expand the groups into the one-directional map Meilisearch wants.
    pub fn synonym_map(&self) -> std::collections::HashMap<String, Vec<String>> {
        let mut map: std::collections::HashMap<String, Vec<String>> = Default::default();
        for group in &self.synonyms {
            let terms: Vec<String> = group
                .iter()
                .map(|t| t.trim().to_lowercase())
                .filter(|t| !t.is_empty())
                .collect();
            for term in &terms {
                let others: Vec<String> = terms.iter().filter(|o| *o != term).cloned().collect();
                map.entry(term.clone()).or_default().extend(others);
            }
        }
        // A term repeated across groups accumulates all of them; keep the
        // list tidy and free of self-references.
        for (term, list) in map.iter_mut() {
            list.sort();
            list.dedup();
            list.retain(|o| o != term);
        }
        map
    }
}

fn default_stop_words() -> Vec<String> {
    [
        "a", "an", "and", "at", "for", "in", "is", "it", "near", "of", "on", "or", "the", "to",
        "with", "where",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

fn default_synonyms() -> Vec<Vec<String>> {
    [
        &["vineyard", "winery", "wine tasting", "tasting room"][..],
        &["restaurant", "eatery", "dining", "diner"][..],
        &["bar", "pub", "tavern"][..],
        &["brewery", "taproom", "brewpub", "beer garden"][..],
        &["cafe", "coffee shop", "coffeehouse", "espresso"][..],
        &["trail", "trailhead", "hiking"][..],
        &["hotel", "lodging", "motel", "inn"][..],
        &["museum", "gallery"][..],
        &["campground", "camping", "campsite"][..],
        &["hot springs", "hot spring", "thermal springs"][..],
        &["ski area", "ski resort", "ski hill"][..],
    ]
    .iter()
    .map(|g| g.iter().map(|t| t.to_string()).collect())
    .collect()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegionConfig {
    pub name: String,
    pub slug: String,
    #[serde(default)]
    pub admin_level: Option<String>,
    #[serde(default)]
    pub timezone: Option<String>,
    pub bbox: BBox,
    #[serde(default)]
    pub centroid: Option<GeoPoint>,
    /// Optional precise outline as `[[lat, lng], ...]`. When present it is
    /// used for both the query envelope and the ingest gate; the bbox then
    /// only serves as the coarse tiling grid.
    #[serde(default)]
    pub polygon: Option<Vec<[f64; 2]>>,
    #[serde(default)]
    pub cities: Vec<City>,
    /// Search vocabulary. Omit the whole table to take the English defaults.
    #[serde(default)]
    pub vocabulary: Vocabulary,
}

impl RegionConfig {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let raw = std::fs::read_to_string(path)
            .map_err(|e| Error::Region(format!("cannot read {}: {e}", path.display())))?;
        let cfg: RegionConfig = toml::from_str(&raw)
            .map_err(|e| Error::Region(format!("cannot parse {}: {e}", path.display())))?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Load from `REGION_CONFIG`, falling back to the baked-in image path.
    pub fn from_env() -> Result<Self> {
        let path = std::env::var("REGION_CONFIG").unwrap_or_else(|_| DEFAULT_REGION_CONFIG.into());
        Self::load(path)
    }

    fn validate(&self) -> Result<()> {
        let b = &self.bbox;
        if b.min_lat >= b.max_lat || b.min_lng >= b.max_lng {
            return Err(Error::Region(format!(
                "bbox is inverted or empty: lat {}..{}, lng {}..{}",
                b.min_lat, b.max_lat, b.min_lng, b.max_lng
            )));
        }
        if let Some(p) = &self.polygon
            && p.len() < 3
        {
            return Err(Error::Region(format!(
                "polygon needs at least 3 vertices, got {}",
                p.len()
            )));
        }
        for c in &self.cities {
            if !b.contains(c.lat, c.lng) {
                return Err(Error::Region(format!(
                    "gazetteer city {:?} at ({}, {}) falls outside the region bbox",
                    c.name, c.lat, c.lng
                )));
            }
        }
        Ok(())
    }

    /// The filter clause constraining every query to the region. Applied on
    /// every search so nothing outside the region can ever surface, even if
    /// something outside it slipped into the index.
    pub fn geo_filter(&self) -> String {
        match &self.polygon {
            Some(p) => {
                let pts: Vec<String> = p
                    .iter()
                    .map(|[lat, lng]| format!("[{lat}, {lng}]"))
                    .collect();
                format!("_geoPolygon({})", pts.join(", "))
            }
            None => self.bbox.filter(),
        }
    }

    /// The ingest gate: is this point actually inside the region?
    pub fn contains(&self, lat: f64, lng: f64) -> bool {
        if !self.bbox.contains(lat, lng) {
            return false;
        }
        match &self.polygon {
            Some(p) => point_in_polygon(lat, lng, p),
            None => true,
        }
    }

    pub fn center(&self) -> GeoPoint {
        self.centroid.unwrap_or_else(|| self.bbox.center())
    }

    /// Resolve a human place name to a gazetteer entry.
    ///
    /// Tries exact match on name or alias, then prefix, then a bounded edit
    /// distance. Returns `None` rather than guessing wildly — the caller
    /// surfaces [`RegionConfig::suggest`] instead of silently dropping the
    /// geographic constraint the user asked for.
    pub fn resolve_place(&self, query: &str) -> Option<&City> {
        let q = normalize(query);
        if q.is_empty() {
            return None;
        }

        if let Some(c) = self
            .cities
            .iter()
            .find(|c| normalize(&c.name) == q || c.aliases.iter().any(|a| normalize(a) == q))
        {
            return Some(c);
        }

        // "Denver CO" / "Denver downtown" should still land on Denver.
        if let Some(c) = self
            .cities
            .iter()
            .filter(|c| {
                let n = normalize(&c.name);
                n.len() >= 4 && (q.starts_with(&n) || n.starts_with(&q))
            })
            .max_by_key(|c| normalize(&c.name).len())
        {
            return Some(c);
        }

        // Typos, but only for names long enough that an edit is unambiguous.
        let budget = if q.len() >= 8 {
            2
        } else if q.len() >= 5 {
            1
        } else {
            return None;
        };
        self.cities
            .iter()
            .filter_map(|c| {
                let d = std::iter::once(&c.name)
                    .chain(c.aliases.iter())
                    .map(|n| edit_distance(&q, &normalize(n)))
                    .min()
                    .unwrap_or(usize::MAX);
                (d <= budget).then_some((d, c))
            })
            .min_by_key(|(d, _)| *d)
            .map(|(_, c)| c)
    }

    /// The closest gazetteer names to a query, for "did you mean" errors.
    pub fn suggest(&self, query: &str, n: usize) -> Vec<String> {
        let q = normalize(query);
        let mut scored: Vec<(usize, &str)> = self
            .cities
            .iter()
            .map(|c| (edit_distance(&q, &normalize(&c.name)), c.name.as_str()))
            .collect();
        scored.sort_by_key(|(d, name)| (*d, *name));
        scored
            .into_iter()
            .take(n)
            .map(|(_, s)| s.to_string())
            .collect()
    }

    pub fn city_names(&self) -> Vec<String> {
        self.cities.iter().map(|c| c.name.clone()).collect()
    }

    /// The gazetteer city nearest a point, used to backfill `city` on
    /// documents whose source gave coordinates but no place name.
    pub fn nearest_city(&self, geo: GeoPoint) -> Option<&City> {
        self.cities
            .iter()
            .map(|c| (c.geo().distance_m(&geo), c))
            .min_by(|a, b| a.0.total_cmp(&b.0))
            .map(|(_, c)| c)
    }
}

/// Lowercase, drop punctuation and diacritics, collapse whitespace, so
/// "Cañon City", "canon city" and "Cañon  City," all compare equal.
fn normalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut pending_space = false;
    for ch in s.chars() {
        let ch = fold_diacritic(ch);
        if ch.is_alphanumeric() {
            if pending_space && !out.is_empty() {
                out.push(' ');
            }
            pending_space = false;
            out.extend(ch.to_lowercase());
        } else {
            pending_space = true;
        }
    }
    out
}

/// Fold the Latin-1 accented characters that show up in US place names.
fn fold_diacritic(c: char) -> char {
    match c {
        'á' | 'à' | 'â' | 'ä' | 'ã' | 'å' | 'Á' | 'À' | 'Â' | 'Ä' | 'Ã' | 'Å' => 'a',
        'é' | 'è' | 'ê' | 'ë' | 'É' | 'È' | 'Ê' | 'Ë' => 'e',
        'í' | 'ì' | 'î' | 'ï' | 'Í' | 'Ì' | 'Î' | 'Ï' => 'i',
        'ó' | 'ò' | 'ô' | 'ö' | 'õ' | 'Ó' | 'Ò' | 'Ô' | 'Ö' | 'Õ' => 'o',
        'ú' | 'ù' | 'û' | 'ü' | 'Ú' | 'Ù' | 'Û' | 'Ü' => 'u',
        'ñ' | 'Ñ' => 'n',
        'ç' | 'Ç' => 'c',
        other => other,
    }
}

/// Levenshtein distance over chars, two-row variant.
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// Ray casting. Vertices are `[lat, lng]`; we treat lng as x, lat as y.
fn point_in_polygon(lat: f64, lng: f64, poly: &[[f64; 2]]) -> bool {
    let mut inside = false;
    let mut j = poly.len() - 1;
    for i in 0..poly.len() {
        let (yi, xi) = (poly[i][0], poly[i][1]);
        let (yj, xj) = (poly[j][0], poly[j][1]);
        if (yi > lat) != (yj > lat) && lng < (xj - xi) * (lat - yi) / (yj - yi) + xi {
            inside = !inside;
        }
        j = i;
    }
    inside
}

#[cfg(test)]
mod tests {
    use super::*;

    fn colorado() -> RegionConfig {
        RegionConfig::load(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../region.colorado.toml"
        ))
        .expect("the shipped Colorado config must load and validate")
    }

    #[test]
    fn shipped_config_is_valid() {
        let co = colorado();
        assert_eq!(co.name, "Colorado");
        assert!(co.cities.len() >= 40);
    }

    #[test]
    fn bbox_filter_puts_the_top_right_corner_first() {
        // Meilisearch: _geoBoundingBox([topRightLat, topRightLng], [bottomLeftLat, bottomLeftLng])
        let b = BBox {
            min_lat: 36.0,
            min_lng: -109.0,
            max_lat: 41.0,
            max_lng: -102.0,
        };
        assert_eq!(b.filter(), "_geoBoundingBox([41, -102], [36, -109])");
    }

    #[test]
    fn contains_gates_on_the_region_edges() {
        let co = colorado();
        assert!(co.contains(39.7392, -104.9903)); // Denver
        assert!(co.contains(39.0639, -108.5506)); // Grand Junction
        assert!(!co.contains(40.7128, -74.0060)); // New York
        assert!(!co.contains(41.5, -104.9)); // just into Wyoming
        assert!(!co.contains(39.7, -111.0)); // just into Utah
        // Exact corners count as inside.
        assert!(co.contains(co.bbox.min_lat, co.bbox.min_lng));
        assert!(co.contains(co.bbox.max_lat, co.bbox.max_lng));
    }

    #[test]
    fn gazetteer_resolves_names_aliases_and_typos() {
        let co = colorado();
        assert_eq!(co.resolve_place("Denver").unwrap().name, "Denver");
        assert_eq!(co.resolve_place("  denver  ").unwrap().name, "Denver");
        assert_eq!(co.resolve_place("Mile High City").unwrap().name, "Denver");
        assert_eq!(co.resolve_place("Denver, CO").unwrap().name, "Denver");
        assert_eq!(
            co.resolve_place("grand junction").unwrap().name,
            "Grand Junction"
        );
        assert_eq!(
            co.resolve_place("Grand Juncton").unwrap().name,
            "Grand Junction"
        );
        assert_eq!(co.resolve_place("Canon City").unwrap().name, "Cañon City");
        assert_eq!(co.resolve_place("Breck").unwrap().name, "Breckenridge");
        assert!(co.resolve_place("Chicago").is_none());
        assert!(co.resolve_place("").is_none());
    }

    #[test]
    fn suggest_offers_close_names() {
        let co = colorado();
        let s = co.suggest("Bolder", 3);
        assert!(s.contains(&"Boulder".to_string()), "got {s:?}");
    }

    #[test]
    fn nearest_city_backfills_from_coordinates() {
        let co = colorado();
        // A point in the Palisade vineyards, east of Grand Junction.
        let c = co.nearest_city(GeoPoint::new(39.108, -108.35)).unwrap();
        assert_eq!(c.name, "Palisade");
    }

    #[test]
    fn tiles_cover_the_bbox_without_gaps() {
        let co = colorado();
        let tiles = co.bbox.tiles(4, 3);
        assert_eq!(tiles.len(), 12);
        assert!((tiles[0].min_lat - co.bbox.min_lat).abs() < 1e-9);
        assert!((tiles[11].max_lat - co.bbox.max_lat).abs() < 1e-9);
        assert!((tiles[11].max_lng - co.bbox.max_lng).abs() < 1e-9);
        // Every gazetteer city must land in exactly one tile.
        for city in &co.cities {
            let hits = tiles
                .iter()
                .filter(|t| t.contains(city.lat, city.lng))
                .count();
            assert!(hits >= 1, "{} landed in no tile", city.name);
        }
    }

    #[test]
    fn polygon_region_filters_and_gates_on_the_outline() {
        let mut co = colorado();
        // A triangle over the western slope only.
        co.polygon = Some(vec![[39.5, -109.0], [39.5, -107.0], [38.0, -108.0]]);
        assert!(co.geo_filter().starts_with("_geoPolygon("));
        assert!(co.contains(39.0, -108.0));
        assert!(!co.contains(39.7392, -104.9903)); // Denver is outside the triangle
    }
}

#[cfg(test)]
mod shipped_regions {
    use super::*;

    /// Every region config in the repo must load, validate, and produce a
    /// sane Overpass grid — the whole promise of "swap one file".
    #[test]
    fn every_shipped_region_config_is_usable() {
        for file in ["region.colorado.toml", "region.vermont.toml"] {
            let path = format!("{}/../{file}", env!("CARGO_MANIFEST_DIR"));
            let r = RegionConfig::load(&path).unwrap_or_else(|e| panic!("{file}: {e}"));

            let (w, h) = r.bbox.span_km();
            let tiles = r.bbox.tiles_of_about(75.0).len();
            assert!(tiles >= 1, "{file} produced no tiles");
            assert!(
                r.geo_filter().starts_with("_geoBoundingBox(")
                    || r.geo_filter().starts_with("_geoPolygon("),
                "{file} produced no geo envelope"
            );
            assert!(
                !r.vocabulary.synonym_map().is_empty(),
                "{file} has no synonyms"
            );

            // Its own gazetteer must resolve, or `city`/`near` is dead.
            for city in &r.cities {
                assert_eq!(
                    r.resolve_place(&city.name).map(|c| &c.name),
                    Some(&city.name),
                    "{file}: {} does not resolve",
                    city.name
                );
                assert!(r.contains(city.lat, city.lng));
            }
            println!(
                "  {file}: {w:.0}x{h:.0} km, {tiles} tiles, {} cities",
                r.cities.len()
            );
        }
    }
}
