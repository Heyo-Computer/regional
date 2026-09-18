//! OpenStreetMap places, via the Overpass API.
//!
//! This is the backbone of the `places` index: OSM already has the
//! coordinates, the names, the addresses and the categories, which is
//! exactly the shape of "a vineyard in Grand Junction".
//!
//! Overpass will time out on a whole-state query, so the region bbox is cut
//! into a grid and one tile is fetched per run. The cursor is the tile
//! index, so a full sweep completes over many runs and then starts again —
//! which is also how edits upstream eventually reach us.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use regional_core::model::{Doc, Envelope, GeoPoint, GeoPrecision, Kind, Place};
use regional_core::region::BBox;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Batch, Ctx, Source, cursor_usize};

/// Target size of one Overpass tile, in kilometres per side.
///
/// The number of tiles follows from the region's area rather than being
/// fixed, so the same code serves a small state and a large one. 75 km keeps
/// each query well inside the Overpass timeout even over dense areas; lower
/// it with `OVERPASS_TILE_KM` if queries start timing out.
pub const DEFAULT_TILE_KM: f64 = 75.0;

/// Tags worth indexing, as `(key, value regex)`.
///
/// Deliberately a curated list rather than everything with a name: OSM is
/// full of benches and street lamps, and an index of those is worse than
/// useless because it buries the things people search for.
const WANTED: &[(&str, &str)] = &[
    (
        "amenity",
        "restaurant|cafe|bar|pub|fast_food|biergarten|ice_cream|food_court|marketplace|\
         theatre|cinema|arts_centre|library|museum|community_centre|casino|nightclub|\
         public_bath|spa",
    ),
    ("craft", "winery|brewery|distillery|caterer"),
    (
        "shop",
        "wine|alcohol|bakery|butcher|cheese|chocolate|coffee|deli|farm|greengrocer|\
         pastry|seafood|books|outdoor|sports|bicycle|art|antiques|gift",
    ),
    (
        "tourism",
        "attraction|museum|gallery|artwork|viewpoint|zoo|theme_park|aquarium|\
         hotel|motel|guest_house|hostel|chalet|camp_site|caravan_site|picnic_site|\
         wine_cellar|information",
    ),
    (
        "leisure",
        "park|nature_reserve|garden|water_park|golf_course|sports_centre|\
         swimming_pool|ice_rink|stadium|marina|fishing|dog_park",
    ),
    (
        "natural",
        "peak|hot_spring|spring|waterfall|cave_entrance|glacier|arch",
    ),
    (
        "historic",
        "monument|memorial|ruins|archaeological_site|castle|fort|mine|ghost_town",
    ),
    ("man_made", "lighthouse|observatory|mineshaft"),
];

/// Tag keys whose value becomes a category, in priority order.
const CATEGORY_KEYS: [&str; 9] = [
    "amenity", "craft", "shop", "tourism", "leisure", "natural", "historic", "man_made", "sport",
];

/// Boolean-ish tags worth keeping as searchable facets.
const FLAG_TAGS: [&str; 8] = [
    "outdoor_seating",
    "takeaway",
    "delivery",
    "wheelchair",
    "dog",
    "internet_access",
    "drive_through",
    "reservation",
];

pub struct Overpass;

#[derive(Debug, Deserialize)]
struct OverpassResponse {
    #[serde(default)]
    elements: Vec<Element>,
}

#[derive(Debug, Deserialize)]
struct Element {
    #[serde(rename = "type")]
    kind: String,
    id: i64,
    lat: Option<f64>,
    lon: Option<f64>,
    center: Option<Center>,
    #[serde(default)]
    tags: HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct Center {
    lat: f64,
    lon: f64,
}

impl Element {
    fn geo(&self) -> Option<GeoPoint> {
        match (self.lat, self.lon, &self.center) {
            (Some(lat), Some(lon), _) => Some(GeoPoint::new(lat, lon)),
            // Ways and relations come back with a computed centroid, which
            // is the right point for a building or a park.
            (_, _, Some(c)) => Some(GeoPoint::new(c.lat, c.lon)),
            _ => None,
        }
    }
}

#[async_trait]
impl Source for Overpass {
    fn name(&self) -> &'static str {
        "overpass"
    }

    fn default_interval(&self) -> Duration {
        // One tile per run; 48 tiles at 20 minutes is a sweep every ~16
        // hours, which is far more often than OSM POI data meaningfully
        // changes and stays well within Overpass's fair-use expectations.
        Duration::from_secs(20 * 60)
    }

    async fn fetch(&self, ctx: &Ctx, cursor: Option<Value>) -> Result<Batch> {
        let tiles = ctx.region.bbox.tiles_of_about(ctx.config.overpass_tile_km);
        let index = cursor_usize(&cursor, "tile") % tiles.len();
        let tile = tiles[index];

        let query = build_query(&tile);
        // One tile per run, so the sweep time is tiles x interval. Logged so
        // an operator covering a large region can see it and shorten the
        // interval rather than discover it weeks later.
        tracing::info!(
            tile = index,
            of = tiles.len(),
            sweep_hours = (tiles.len() as f64 * self.default_interval().as_secs_f64() / 3600.0)
                .round() as u64,
            "querying overpass for one tile of {}",
            ctx.region.name
        );

        let res: OverpassResponse = ctx
            .http
            .post_form(&ctx.config.overpass_url, &[("data", query.as_str())])
            .await?;

        let docs: Vec<Doc> = res.elements.iter().filter_map(to_place).collect();
        tracing::info!(
            tile = index,
            elements = res.elements.len(),
            usable = docs.len(),
            "overpass tile fetched"
        );

        let next = (index + 1) % tiles.len();
        let batch = Batch::new(docs, Some(json!({ "tile": next })));
        Ok(if next == 0 { batch.swept() } else { batch })
    }
}

/// Build the Overpass QL for one tile.
///
/// Overpass takes its bbox as `(south, west, north, east)`.
fn build_query(tile: &BBox) -> String {
    let bbox = format!(
        "{},{},{},{}",
        tile.min_lat, tile.min_lng, tile.max_lat, tile.max_lng
    );
    let mut clauses = String::new();
    for (key, values) in WANTED {
        // Only named features: an unnamed POI has nothing to search for.
        for element in ["node", "way"] {
            clauses.push_str(&format!(
                "  {element}[\"{key}\"~\"^({values})$\"][\"name\"]({bbox});\n"
            ));
        }
    }
    format!("[out:json][timeout:120];\n(\n{clauses});\nout center tags 3000;\n")
}

fn to_place(el: &Element) -> Option<Doc> {
    let name = el.tags.get("name")?.trim();
    if name.is_empty() {
        return None;
    }
    let geo = el.geo()?;
    if !geo.is_valid() {
        return None;
    }

    let source_id = format!("{}/{}", el.kind, el.id);
    let mut env = Envelope::new(Kind::Place, "osm", &source_id, name, geo);
    env.geo_precision = GeoPrecision::Exact;
    env.url = Some(format!(
        "https://www.openstreetmap.org/{}/{}",
        el.kind, el.id
    ));

    env.categories = categories(&el.tags);
    env.tags = flags(&el.tags);
    env.city = el
        .tags
        .get("addr:city")
        .or_else(|| el.tags.get("addr:town"))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    env.county = el.tags.get("addr:county").map(|s| s.trim().to_string());
    env.address = street_address(&el.tags);

    env.summary = el
        .tags
        .get("description")
        .map(|d| d.trim().to_string())
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| describe(name, &env.categories, env.city.as_deref()));
    // OSM carries no prose, so `body` stays empty rather than repeating the
    // summary — both are searchable, and duplicating would double-weight it.

    Some(Doc::Place(Place {
        env,
        phone: el
            .tags
            .get("phone")
            .or_else(|| el.tags.get("contact:phone"))
            .cloned(),
        website: el
            .tags
            .get("website")
            .or_else(|| el.tags.get("contact:website"))
            .cloned(),
        opening_hours: el.tags.get("opening_hours").cloned(),
        cuisine: el
            .tags
            .get("cuisine")
            .map(|c| split_semis(c))
            .unwrap_or_default(),
        price_level: el.tags.get("price_range").cloned(),
        osm_type: Some(el.kind.clone()),
        osm_id: Some(el.id),
        wikidata_id: el.tags.get("wikidata").cloned(),
    }))
}

fn categories(tags: &HashMap<String, String>) -> Vec<String> {
    let mut out = Vec::new();
    for key in CATEGORY_KEYS {
        if let Some(v) = tags.get(key) {
            out.extend(split_semis(v));
        }
    }
    // Cuisine is what people actually search for ("thai", "pizza"), so it
    // belongs alongside the structural category rather than buried.
    if let Some(c) = tags.get("cuisine") {
        out.extend(split_semis(c));
    }
    if tags.get("microbrewery").map(String::as_str) == Some("yes") {
        out.push("brewery".into());
    }
    out
}

fn flags(tags: &HashMap<String, String>) -> Vec<String> {
    FLAG_TAGS
        .iter()
        .filter(|k| {
            matches!(
                tags.get(**k).map(String::as_str),
                Some("yes") | Some("designated")
            )
        })
        .map(|k| k.replace('_', " "))
        .collect()
}

fn street_address(tags: &HashMap<String, String>) -> Option<String> {
    let street = tags.get("addr:street")?;
    let number = tags.get("addr:housenumber");
    Some(match number {
        Some(n) => format!("{n} {street}"),
        None => street.clone(),
    })
}

/// A one-line description for a POI that has none, so search results are
/// not just a bare name.
fn describe(name: &str, categories: &[String], city: Option<&str>) -> String {
    let what = categories
        .first()
        .map(|c| c.replace('_', " "))
        .unwrap_or_else(|| "place".to_string());
    match city {
        Some(city) => format!("{name} — {what} in {city}."),
        None => format!("{name} — {what}."),
    }
}

/// OSM packs multiple values into one tag with semicolons.
fn split_semis(v: &str) -> Vec<String> {
    v.split(';')
        .map(|s| s.trim().replace('_', " "))
        .filter(|s| !s.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn element(tags: &[(&str, &str)]) -> Element {
        Element {
            kind: "node".into(),
            id: 42,
            lat: Some(39.11),
            lon: Some(-108.35),
            center: None,
            tags: tags
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    #[test]
    fn a_winery_becomes_a_searchable_place() {
        let doc = to_place(&element(&[
            ("name", "Carlson Vineyards"),
            ("craft", "winery"),
            ("addr:city", "Palisade"),
            ("addr:street", "35 Rd"),
            ("addr:housenumber", "461"),
            ("opening_hours", "Mo-Su 11:00-18:00"),
            ("website", "https://example.test"),
        ]))
        .expect("a named winery must produce a document");

        let env = doc.env();
        assert_eq!(env.title, "Carlson Vineyards");
        assert!(env.categories.contains(&"winery".to_string()));
        assert_eq!(env.city.as_deref(), Some("Palisade"));
        assert_eq!(env.address.as_deref(), Some("461 35 Rd"));
        assert_eq!(env.source_id, "node/42");
        assert!(
            env.url
                .as_deref()
                .unwrap()
                .contains("openstreetmap.org/node/42")
        );
        assert!(matches!(env.geo_precision, GeoPrecision::Exact));
        let Doc::Place(p) = &doc else {
            panic!("expected a place")
        };
        assert_eq!(p.opening_hours.as_deref(), Some("Mo-Su 11:00-18:00"));
    }

    #[test]
    fn cuisine_and_multi_valued_tags_all_become_categories() {
        let doc = to_place(&element(&[
            ("name", "Two Rivers"),
            ("amenity", "restaurant"),
            ("cuisine", "pizza;italian"),
            ("outdoor_seating", "yes"),
            ("takeaway", "no"),
        ]))
        .unwrap();
        let env = doc.env();
        // finalize() lowercases and dedupes; check the raw mapping here.
        assert!(env.categories.contains(&"restaurant".to_string()));
        assert!(env.categories.contains(&"pizza".to_string()));
        assert!(env.categories.contains(&"italian".to_string()));
        assert_eq!(env.tags, vec!["outdoor seating".to_string()]);
    }

    #[test]
    fn an_unnamed_or_unplaced_element_is_dropped() {
        assert!(to_place(&element(&[("amenity", "restaurant")])).is_none());
        let mut el = element(&[("name", "X"), ("amenity", "cafe")]);
        el.lat = None;
        el.lon = None;
        assert!(to_place(&el).is_none());
    }

    #[test]
    fn a_way_uses_its_centroid() {
        let mut el = element(&[("name", "City Park"), ("leisure", "park")]);
        el.kind = "way".into();
        el.lat = None;
        el.lon = None;
        el.center = Some(Center {
            lat: 39.75,
            lon: -104.95,
        });
        let doc = to_place(&el).unwrap();
        assert_eq!(doc.geo().lat, 39.75);
        assert_eq!(doc.env().source_id, "way/42");
    }

    #[test]
    fn a_poi_without_a_description_still_gets_a_sentence() {
        let doc = to_place(&element(&[
            ("name", "Hot Springs Pool"),
            ("leisure", "swimming_pool"),
            ("addr:city", "Glenwood Springs"),
        ]))
        .unwrap();
        assert_eq!(
            doc.env().summary,
            "Hot Springs Pool — swimming pool in Glenwood Springs."
        );
    }

    #[test]
    fn the_tile_count_follows_the_region_size() {
        use regional_core::region::BBox;
        // Colorado: ~610 x 445 km, so roughly 9 x 6 tiles at 75 km.
        let co = BBox {
            min_lat: 36.992426,
            min_lng: -109.060253,
            max_lat: 41.003444,
            max_lng: -102.041524,
        };
        let n = co.tiles_of_about(DEFAULT_TILE_KM).len();
        assert!((40..=70).contains(&n), "colorado produced {n} tiles");

        // Rhode Island is ~70 x 80 km: a couple of tiles, not dozens.
        let ri = BBox {
            min_lat: 41.146,
            min_lng: -71.862,
            max_lat: 42.018,
            max_lng: -71.120,
        };
        let small = ri.tiles_of_about(DEFAULT_TILE_KM).len();
        assert!(small <= 4, "rhode island produced {small} tiles");

        // Texas is ~1300 x 1100 km and must produce many more than Colorado,
        // or each query would be far too big for Overpass.
        let tx = BBox {
            min_lat: 25.84,
            min_lng: -106.65,
            max_lat: 36.50,
            max_lng: -93.51,
        };
        assert!(tx.tiles_of_about(DEFAULT_TILE_KM).len() > n * 3);

        // A nonsense target must not divide by zero or hang.
        assert!(!co.tiles_of_about(0.0).is_empty());
        assert!(!co.tiles_of_about(f64::NAN).is_empty());
    }

    #[test]
    fn the_query_covers_the_tile_and_asks_only_for_named_features() {
        let tile = BBox {
            min_lat: 39.0,
            min_lng: -108.6,
            max_lat: 39.2,
            max_lng: -108.3,
        };
        let q = build_query(&tile);
        assert!(q.contains("[out:json][timeout:120]"));
        assert!(
            q.contains("39,-108.6,39.2,-108.3"),
            "south,west,north,east order"
        );
        assert!(q.contains("out center tags"), "ways need their centroid");
        assert!(q.contains("[\"name\"]"), "unnamed features are noise");
        assert!(q.contains("craft"), "wineries are tagged craft=winery");
        // Both nodes and ways, for every wanted key.
        assert_eq!(q.matches("  node[").count(), WANTED.len());
        assert_eq!(q.matches("  way[").count(), WANTED.len());
    }
}
