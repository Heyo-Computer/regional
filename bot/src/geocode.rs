//! Address -> coordinates, via Nominatim, cached hard.
//!
//! Nominatim's usage policy allows one request per second and expects a
//! real contact address. Every lookup is cached — including the misses,
//! since re-asking about an address that has no answer spends the same
//! budget as asking about one that does.

use std::sync::Arc;

use regional_core::model::GeoPoint;
use regional_core::region::RegionConfig;
use serde::Deserialize;

use crate::http::Fetcher;
use crate::state::BotState;

#[derive(Debug, Deserialize)]
struct NominatimHit {
    lat: String,
    lon: String,
}

pub struct Geocoder {
    http: Arc<Fetcher>,
    state: Arc<BotState>,
    region: Arc<RegionConfig>,
    base: String,
}

impl Geocoder {
    pub fn new(
        http: Arc<Fetcher>,
        state: Arc<BotState>,
        region: Arc<RegionConfig>,
        base: String,
    ) -> Self {
        Self {
            http,
            state,
            region,
            base: base.trim_end_matches('/').to_string(),
        }
    }

    /// Resolve a free-text address to a point inside the region.
    ///
    /// The query is bounded to the region's bbox, so an ambiguous address
    /// ("Main Street") resolves locally or not at all — never to the same
    /// street name in another state.
    pub async fn geocode(&self, address: &str) -> Option<GeoPoint> {
        let key = normalize_key(address);
        if key.is_empty() {
            return None;
        }
        if let Some((lat, lng)) = self.state.geocache_get(&key).await {
            return Some(GeoPoint::new(lat, lng));
        }
        if self.state.geocache_known(&key).await {
            // A cached miss.
            return None;
        }

        let b = &self.region.bbox;
        let url = format!(
            "{}/search?q={}&format=jsonv2&limit=1&bounded=1&viewbox={},{},{},{}",
            self.base,
            urlencode(&key),
            b.min_lng,
            b.max_lat,
            b.max_lng,
            b.min_lat,
        );

        let found = match self.http.get_json::<Vec<NominatimHit>>(&url).await {
            Ok(hits) => hits.into_iter().next().and_then(|h| {
                let lat = h.lat.parse::<f64>().ok()?;
                let lng = h.lon.parse::<f64>().ok()?;
                let p = GeoPoint::new(lat, lng);
                (p.is_valid() && self.region.contains(lat, lng)).then_some(p)
            }),
            Err(e) => {
                // Do not cache a transport failure as a miss — that would
                // poison the cache for an address that is actually fine.
                tracing::warn!(error = %e, "nominatim lookup failed");
                return None;
            }
        };

        if let Err(e) = self
            .state
            .geocache_put(&key, found.map(|p| (p.lat, p.lng)))
            .await
        {
            tracing::warn!(error = %e, "could not write the geocode cache");
        }
        found
    }
}

/// Collapse an address to a stable cache key, so trivially different
/// spellings of the same address share one lookup.
fn normalize_key(address: &str) -> String {
    address
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_matches(|c: char| c == ',' || c.is_whitespace())
        .to_lowercase()
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_stable_across_spacing_and_case() {
        assert_eq!(
            normalize_key("  461  35 Rd,  Palisade, CO "),
            normalize_key("461 35 Rd, Palisade, CO")
        );
        assert_eq!(normalize_key("A B"), "a b");
        assert_eq!(normalize_key("   "), "");
    }

    #[test]
    fn urlencoding_escapes_what_a_query_string_cannot_carry() {
        assert_eq!(urlencode("461 35 Rd, Palisade"), "461+35+Rd%2C+Palisade");
        assert_eq!(urlencode("caf\u{e9}"), "caf%C3%A9");
        assert_eq!(urlencode("a&b=c"), "a%26b%3Dc");
    }
}
