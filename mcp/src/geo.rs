//! Turning a place name into a point.
//!
//! The read path never calls an external geocoder — it has to stay fast and
//! must not depend on a third party being up. Instead it resolves against
//! the region gazetteer, and falls back to the index itself, which by
//! construction only contains things inside the region.

use regional_core::model::GeoPoint;

use crate::search::RawHit;
use crate::state::AppState;

/// A resolved geographic anchor for a query.
#[derive(Debug, Clone)]
pub struct Anchor {
    pub geo: GeoPoint,
    /// Radius to use when the caller did not specify one.
    pub default_radius_m: u32,
    /// Human label for the resolved place, echoed back so the caller can
    /// see what its fuzzy string actually matched.
    pub label: String,
    pub via: &'static str,
}

/// Resolve a free-text place name.
///
/// Gazetteer first (instant, curated), then the `places` index (covers
/// neighbourhoods and landmarks the gazetteer does not list). Returns a
/// caller-facing message on failure rather than silently dropping the
/// geographic constraint — a search for "vineyards near Palisade" that
/// quietly becomes "vineyards anywhere" is worse than an error.
pub async fn resolve(state: &AppState, name: &str) -> Result<Anchor, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("empty place name".to_string());
    }

    if let Some(city) = state.region.resolve_place(name) {
        return Ok(Anchor {
            geo: city.geo(),
            default_radius_m: city.default_radius_m,
            label: city.name.clone(),
            via: "gazetteer",
        });
    }

    // Not a known city — ask the index. Constrained to the region envelope
    // so a stray document cannot drag the anchor out of the region.
    let region_filter = state.region.geo_filter();
    if let Some(hit) = state.top_place(name, &region_filter).await {
        return Ok(Anchor {
            geo: hit.geo,
            default_radius_m: 5_000,
            label: hit.title.clone(),
            via: "index",
        });
    }

    let suggestions = state.region.suggest(name, 5);
    Err(format!(
        "could not locate {name:?} within {}. Closest known places: {}. \
         Pass explicit lat/lng, or call list_locales to list every place name this server knows.",
        state.region.name,
        suggestions.join(", ")
    ))
}

/// Resolve whichever of `near` / `lat`+`lng` the caller supplied.
pub async fn resolve_any(
    state: &AppState,
    near: Option<&str>,
    lat: Option<f64>,
    lng: Option<f64>,
) -> Result<Option<Anchor>, String> {
    match (near, lat, lng) {
        (_, Some(lat), Some(lng)) => {
            let geo = GeoPoint::new(lat, lng);
            if !geo.is_valid() {
                return Err(format!("({lat}, {lng}) is not a valid coordinate"));
            }
            if !state.region.contains(lat, lng) {
                return Err(format!(
                    "({lat}, {lng}) is outside {} — this server only indexes content inside the region",
                    state.region.name
                ));
            }
            Ok(Some(Anchor {
                geo,
                default_radius_m: 10_000,
                label: format!("{lat:.4}, {lng:.4}"),
                via: "coordinates",
            }))
        }
        (_, Some(_), None) | (_, None, Some(_)) => {
            Err("lat and lng must be given together".to_string())
        }
        (Some(name), None, None) => resolve(state, name).await.map(Some),
        (None, None, None) => Ok(None),
    }
}

/// Backfill the `city` label on a hit that has coordinates but no city.
pub fn label_city(state: &AppState, hit: &RawHit) -> Option<String> {
    hit.city
        .clone()
        .or_else(|| state.region.nearest_city(hit.geo).map(|c| c.name.clone()))
}
