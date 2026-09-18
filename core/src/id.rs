//! Deterministic identity and change detection.

use crate::model::Doc;

/// A stable primary key derived from the source and its own identifier.
///
/// Deterministic by design: re-running a source upserts the same documents
/// instead of duplicating them, which is what lets the indexer run forever
/// without the index growing without bound.
///
/// The output is hex, which satisfies Meilisearch's primary-key charset
/// (`[a-zA-Z0-9_-]`) for any input, including URLs and OSM ids.
pub fn stable_id(source: &str, source_id: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(source.as_bytes());
    hasher.update(b"\x00");
    hasher.update(source_id.as_bytes());
    hasher.finalize().to_hex()[..16].to_string()
}

/// Fields excluded from the content hash because they change on every run
/// even when nothing meaningful did.
///
/// `updated_at` is bookkeeping, not content: most sources do not tell us
/// when a record last changed upstream, so it defaults to "now" and would
/// otherwise make every document look different on every pass. Leaving it
/// out is also what gives it its real meaning — an unchanged document is
/// never rewritten, so the stored `updated_at` stays at the moment the
/// content actually last changed.
const VOLATILE: [&str; 4] = ["indexed_at", "updated_at", "content_hash", "_geoDistance"];

/// Hash of a document's meaningful content.
///
/// The pipeline compares this against what is already indexed and skips the
/// write when they match, so Meilisearch write volume tracks actual change
/// rather than crawl volume.
pub fn content_hash(doc: &Doc) -> String {
    let mut value = match serde_json::to_value(doc) {
        Ok(v) => v,
        // A document that will not serialise cannot be indexed either; the
        // caller will fail on the write. Fall back to a non-matching hash so
        // we never silently treat it as unchanged.
        Err(_) => return String::from("unhashable"),
    };
    if let Some(obj) = value.as_object_mut() {
        for key in VOLATILE {
            obj.remove(key);
        }
    }
    // serde_json's default map is a BTreeMap, so this rendering is stable
    // across runs regardless of struct field order.
    let canonical = value.to_string();
    blake3::hash(canonical.as_bytes()).to_hex()[..32].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Doc, Envelope, GeoPoint, Kind, Place};

    fn doc(title: &str, indexed_at: i64) -> Doc {
        let mut env = Envelope::new(
            Kind::Place,
            "osm",
            "node/1",
            title,
            GeoPoint::new(39.0, -105.0),
        );
        env.indexed_at = indexed_at;
        Doc::Place(Place {
            env,
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
    fn ids_are_stable_and_distinct() {
        assert_eq!(stable_id("osm", "node/1"), stable_id("osm", "node/1"));
        assert_ne!(stable_id("osm", "node/1"), stable_id("osm", "node/2"));
        // The separator keeps ("a","bc") from colliding with ("ab","c").
        assert_ne!(stable_id("a", "bc"), stable_id("ab", "c"));
        assert_eq!(stable_id("osm", "node/1").len(), 16);
        assert!(
            stable_id("crawl", "https://x.test/a?b=1")
                .chars()
                .all(|c| c.is_ascii_alphanumeric())
        );
    }

    #[test]
    fn content_hash_ignores_the_indexing_timestamp() {
        assert_eq!(
            content_hash(&doc("Same", 1_000)),
            content_hash(&doc("Same", 9_999)),
        );
    }

    #[test]
    fn content_hash_ignores_updated_at() {
        // Sources that carry no upstream timestamp stamp `updated_at` with
        // the current time on every pass. If that fed the hash, nothing
        // would ever compare equal and the indexer would rewrite the entire
        // index on every cycle.
        let mut a = doc("Same", 1);
        let mut b = doc("Same", 1);
        a.env_mut().updated_at = 1_000;
        b.env_mut().updated_at = 9_999;
        assert_eq!(content_hash(&a), content_hash(&b));
    }

    #[test]
    fn content_hash_tracks_real_changes() {
        assert_ne!(
            content_hash(&doc("Before", 1)),
            content_hash(&doc("After", 1))
        );
    }
}
