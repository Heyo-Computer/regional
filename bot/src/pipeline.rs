//! The path every document takes into the index.
//!
//! Sources produce candidate documents; this decides which of them are
//! real, in the region, and actually different from what is already stored.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use regional_core::meili::{self, Client};
use regional_core::model::{Doc, GeoPrecision};
use regional_core::region::RegionConfig;

use crate::state::{BotState, existing_hashes};

#[derive(Debug, Default, Clone, Copy, serde::Serialize)]
pub struct IngestStats {
    pub received: usize,
    /// Dropped because the coordinates were missing or nonsensical.
    pub invalid: usize,
    /// Dropped because the coordinates fall outside the region. This is the
    /// guarantee the whole stack rests on: everything indexed is here.
    pub out_of_region: usize,
    /// Already indexed with identical content, so not rewritten.
    pub unchanged: usize,
    /// Dropped because a reviewer approved a request to remove them. Without
    /// this, the next sweep of the originating source would put them back.
    pub suppressed: usize,
    pub written: usize,
}

impl IngestStats {
    pub fn merge(&mut self, other: IngestStats) {
        self.received += other.received;
        self.invalid += other.invalid;
        self.out_of_region += other.out_of_region;
        self.unchanged += other.unchanged;
        self.suppressed += other.suppressed;
        self.written += other.written;
    }
}

pub struct Pipeline {
    client: Client,
    region: RegionConfig,
    state: Arc<BotState>,
}

impl Pipeline {
    pub fn new(client: Client, region: RegionConfig, state: Arc<BotState>) -> Self {
        Self {
            client,
            region,
            state,
        }
    }

    pub async fn ingest(&self, docs: Vec<Doc>) -> Result<IngestStats> {
        let mut stats = IngestStats {
            received: docs.len(),
            ..Default::default()
        };
        if docs.is_empty() {
            return Ok(stats);
        }

        // 1. Gate on geography, and normalise while we are here.
        let mut kept: Vec<Doc> = Vec::with_capacity(docs.len());
        for mut doc in docs {
            let geo = doc.geo();
            if !geo.is_valid() || doc.env().title.trim().is_empty() {
                stats.invalid += 1;
                continue;
            }
            if !self.region.contains(geo.lat, geo.lng) {
                stats.out_of_region += 1;
                continue;
            }
            self.backfill(&mut doc);
            kept.push(doc.finalize());
        }

        // 2. Collapse duplicates inside this batch. Two sources can reach
        //    the same record in one cycle; last write wins.
        let mut by_id: HashMap<String, Doc> = HashMap::with_capacity(kept.len());
        for doc in kept {
            by_id.insert(doc.id().to_string(), doc);
        }

        // 3. Drop anything a reviewer has had removed. The source that
        //    produced it still has it, so this has to be checked on every
        //    pass, not just at deletion time.
        let ids: Vec<String> = by_id.keys().cloned().collect();
        match self.state.suppressed(&ids).await {
            Ok(blocked) => {
                for id in &blocked {
                    by_id.remove(id);
                }
                stats.suppressed = blocked.len();
                if !blocked.is_empty() {
                    tracing::info!(n = blocked.len(), "dropped suppressed documents");
                }
            }
            // Failing open would resurrect removed content, so fail closed:
            // skip the whole batch and let the next cycle retry.
            Err(e) => {
                tracing::error!(error = %e, "could not read the suppression list; skipping this batch");
                return Ok(stats);
            }
        }

        // 4. Group by destination index.
        let mut by_index: HashMap<&'static str, Vec<Doc>> = HashMap::new();
        for doc in by_id.into_values() {
            by_index.entry(doc.index()).or_default().push(doc);
        }

        // 5. Skip anything whose content did not change, then write.
        for (index, docs) in by_index {
            let ids: Vec<String> = docs.iter().map(|d| d.id().to_string()).collect();
            let existing = match existing_hashes(&self.client, index, &ids).await {
                Ok(h) => h,
                Err(e) => {
                    // Losing the comparison costs writes, not correctness.
                    tracing::warn!(index, error = %e, "could not read existing hashes; rewriting the batch");
                    HashMap::new()
                }
            };
            let changed: Vec<Doc> = docs
                .into_iter()
                .filter(|d| existing.get(d.id()) != Some(&d.env().content_hash))
                .collect();
            stats.unchanged += ids.len() - changed.len();

            if changed.is_empty() {
                continue;
            }
            let written = meili::upsert_chunked(&self.client, index, &changed).await?;
            stats.written += written;
            tracing::info!(
                index,
                written,
                skipped = ids.len() - changed.len(),
                "indexed"
            );
        }

        Ok(stats)
    }

    /// Fill in what the source could not: a city label for a document that
    /// has coordinates but no place name, and the county that goes with it.
    fn backfill(&self, doc: &mut Doc) {
        let geo = doc.geo();
        let nearest = self.region.nearest_city(geo);
        let env = doc.env_mut();
        if env.city.as_deref().unwrap_or("").trim().is_empty()
            && let Some(c) = nearest
        {
            env.city = Some(c.name.clone());
        }
        if env.county.is_none()
            && let Some(c) = nearest
            && let Some(county) = &c.county
        {
            env.county = Some(county.clone());
        }
        // A city-centroid document that claims exact coordinates would sort
        // ahead of genuinely precise ones; keep the claim honest.
        if matches!(env.geo_precision, GeoPrecision::Exact)
            && let Some(c) = nearest
            && c.geo().distance_m(&geo) < 1.0
        {
            env.geo_precision = GeoPrecision::City;
        }
    }
}

#[cfg(test)]
mod tests {
    use regional_core::model::{Envelope, GeoPoint, Kind, Place};

    use super::*;

    fn region() -> RegionConfig {
        RegionConfig::load(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../region.colorado.toml"
        ))
        .unwrap()
    }

    fn pipeline() -> Pipeline {
        let client =
            regional_core::meili::Client::new("http://localhost:7700", None::<String>).unwrap();
        Pipeline::new(client.clone(), region(), Arc::new(BotState::new(client)))
    }

    fn place(title: &str, lat: f64, lng: f64) -> Doc {
        Doc::Place(Place {
            env: Envelope::new(Kind::Place, "osm", title, title, GeoPoint::new(lat, lng)),
            phone: None,
            website: None,
            opening_hours: None,
            cuisine: vec![],
            price_level: None,
            osm_type: None,
            osm_id: None,
            wikidata_id: None,
        })
    }

    #[test]
    fn backfill_names_the_city_and_county_from_coordinates() {
        let p = pipeline();
        let mut doc = place("Somewhere", 39.108, -108.35);
        p.backfill(&mut doc);
        assert_eq!(doc.env().city.as_deref(), Some("Palisade"));
        assert_eq!(doc.env().county.as_deref(), Some("Mesa"));
    }

    #[test]
    fn a_document_sitting_exactly_on_a_city_centroid_loses_its_exact_claim() {
        let p = pipeline();
        let mut doc = place("Denver thing", 39.7392, -104.9903);
        assert!(matches!(doc.env().geo_precision, GeoPrecision::Exact));
        p.backfill(&mut doc);
        assert!(matches!(doc.env().geo_precision, GeoPrecision::City));
    }

    #[test]
    fn stats_merge_additively() {
        let mut a = IngestStats {
            received: 1,
            written: 1,
            ..Default::default()
        };
        a.merge(IngestStats {
            received: 2,
            out_of_region: 2,
            ..Default::default()
        });
        assert_eq!(a.received, 3);
        assert_eq!(a.written, 1);
        assert_eq!(a.out_of_region, 2);
    }
}
