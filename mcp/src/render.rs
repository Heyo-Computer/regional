//! Rendering results as text.
//!
//! The structured payload is the machine-readable answer; this is the part
//! a model actually reads. It is compact on purpose — every wasted line is
//! context that could have held another result.

use serde_json::Value;

use crate::search::Hit;

pub fn header(
    region: &str,
    subject: &str,
    scope: Option<&str>,
    shown: usize,
    estimated_total: usize,
    offset: usize,
) -> String {
    let mut s = format!("{shown} result(s) in {region} for {subject}");
    if let Some(scope) = scope {
        s.push_str(&format!(" {scope}"));
    }
    if estimated_total > shown + offset {
        s.push_str(&format!(
            " — about {estimated_total} match in total; pass offset={} for more",
            offset + shown
        ));
    }
    s
}

/// Metres read better than "0.0 km" for anything close by.
fn distance(metres: u64) -> String {
    if metres < 1_000 {
        format!("{metres} m")
    } else {
        format!("{:.1} km", metres as f64 / 1000.0)
    }
}

pub fn hits(header: &str, hits: &[Hit]) -> String {
    if hits.is_empty() {
        return format!(
            "{header}\n\nNothing matched. Try a broader `query`, a larger `radius_m`, \
             or call describe_region to see which place names and categories exist."
        );
    }
    let mut out = String::from(header);
    out.push_str("\n\n");
    for (i, h) in hits.iter().enumerate() {
        out.push_str(&format!("{}. {} [{}]", i + 1, h.title, h.kind));
        let mut where_bits = Vec::new();
        if let Some(city) = &h.city {
            where_bits.push(city.clone());
        }
        if let Some(d) = h.distance_m {
            where_bits.push(format!("{} away", distance(d)));
        }
        if !where_bits.is_empty() {
            out.push_str(&format!(" — {}", where_bits.join(" · ")));
        }
        out.push('\n');

        if let Some(s) = &h.starts_at {
            out.push_str(&format!("   when: {s}"));
            if let Some(e) = &h.ends_at {
                out.push_str(&format!(" to {e}"));
            }
            out.push('\n');
        } else if let Some(p) = &h.published_at {
            out.push_str(&format!("   published: {p}\n"));
        }
        if !h.categories.is_empty() {
            out.push_str(&format!("   {}\n", h.categories.join(", ")));
        }
        if let Some(sn) = &h.snippet {
            out.push_str(&format!("   {sn}\n"));
        }
        if let Some(venue) = &h.venue {
            out.push_str(&format!("   venue: {venue}\n"));
        }
        if let Some(addr) = &h.address {
            out.push_str(&format!("   {addr}\n"));
        }
        // Contact detail is the difference between "there is a restaurant"
        // and "you can call it", so it earns its line when present.
        let mut contact = Vec::new();
        if let Some(p) = &h.phone {
            contact.push(p.clone());
        }
        if let Some(w) = &h.website {
            contact.push(w.clone());
        }
        if !contact.is_empty() {
            out.push_str(&format!("   {}\n", contact.join(" · ")));
        }
        if let Some(hours) = &h.opening_hours {
            out.push_str(&format!("   hours: {hours}\n"));
        }
        if let Some(url) = &h.url {
            out.push_str(&format!("   {url}\n"));
        }
        // Location precision matters when deciding whether a distance is
        // meaningful, so it travels with the id rather than being buried.
        out.push_str(&format!(
            "   id: {} · source: {} · location: {}\n",
            h.id, h.source, h.geo_precision
        ));
    }
    out.push_str("\nUse get_document with an id and its kind for the full record.");
    out
}

pub fn document(doc: &Value) -> String {
    let get = |k: &str| doc.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let mut out = String::new();
    let title = get("title");
    out.push_str(&format!(
        "{}\n",
        if title.is_empty() {
            "(untitled)"
        } else {
            &title
        }
    ));
    out.push_str(&format!(
        "kind: {}  ·  source: {}\n",
        get("kind"),
        get("source")
    ));

    for (label, key) in [
        ("city", "city"),
        ("county", "county"),
        ("address", "address"),
        ("url", "url"),
        ("phone", "phone"),
        ("website", "website"),
        ("opening hours", "opening_hours"),
        ("venue", "venue_name"),
        ("organizer", "organizer"),
        ("author", "author"),
        ("site", "site_name"),
    ] {
        let v = get(key);
        if !v.is_empty() {
            out.push_str(&format!("{label}: {v}\n"));
        }
    }
    if let Some(geo) = doc.get("_geo") {
        let lat = geo.get("lat").and_then(Value::as_f64).unwrap_or_default();
        let lng = geo.get("lng").and_then(Value::as_f64).unwrap_or_default();
        out.push_str(&format!(
            "coordinates: {lat}, {lng} ({})\n",
            get("geo_precision")
        ));
    }
    for (label, key) in [
        ("starts", "start_time"),
        ("ends", "end_time"),
        ("published", "published_at"),
    ] {
        if let Some(ts) = doc.get(key).and_then(Value::as_i64)
            && let Some(s) = crate::search::fmt_ts(ts)
        {
            out.push_str(&format!("{label}: {s}\n"));
        }
    }
    if let Some(cats) = doc.get("categories").and_then(Value::as_array)
        && !cats.is_empty()
    {
        let names: Vec<String> = cats
            .iter()
            .filter_map(|c| c.as_str().map(String::from))
            .collect();
        out.push_str(&format!("categories: {}\n", names.join(", ")));
    }

    let summary = get("summary");
    if !summary.is_empty() {
        out.push_str(&format!("\n{summary}\n"));
    }
    let body = get("body");
    // Sources without prose reuse the summary as the body; print it once.
    if !body.is_empty() && body != summary {
        out.push_str(&format!("\n{body}\n"));
    }
    out
}

pub fn region(v: &Value) -> String {
    let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let mut out = format!("Region: {}", s("region"));
    if let Some(tz) = v.get("timezone").and_then(Value::as_str) {
        out.push_str(&format!("  ·  timezone {tz}"));
    }
    out.push('\n');

    if let Some(b) = v.get("bounding_box") {
        let f = |k: &str| b.get(k).and_then(Value::as_f64).unwrap_or_default();
        out.push_str(&format!(
            "Bounds: lat {}..{}, lng {}..{}\n",
            f("min_lat"),
            f("max_lat"),
            f("min_lng"),
            f("max_lng")
        ));
    }

    out.push_str("\nIndexed documents:\n");
    if let Some(counts) = v.get("document_counts").and_then(Value::as_object) {
        let empty = counts.values().all(|n| n.as_u64() == Some(0));
        for (k, n) in counts {
            let fresh = v
                .get("last_updated")
                .and_then(|f| f.get(k))
                .and_then(Value::as_str)
                .map(|s| format!(" (newest {s})"))
                .unwrap_or_default();
            match n.as_u64() {
                Some(n) => out.push_str(&format!("  {k}: {n}{fresh}\n")),
                None => out.push_str(&format!(
                    "  {k}: unknown (the count could not be read){fresh}\n"
                )),
            }
        }
        if empty {
            out.push_str(
                "  The index is empty — the indexer has not written anything yet, so \
                 every search will correctly return nothing.\n",
            );
        }
    }

    if let Some(places) = v.get("resolvable_places").and_then(Value::as_array) {
        let names: Vec<String> = places
            .iter()
            .filter_map(|p| p.as_str().map(String::from))
            .collect();
        out.push_str(&format!(
            "\nPlace names that resolve to coordinates ({}):\n  {}\n",
            names.len(),
            names.join(", ")
        ));
    }

    if let Some(cats) = v.get("top_place_categories").and_then(Value::as_array)
        && !cats.is_empty()
    {
        out.push_str("\nMost common place categories:\n  ");
        let rendered: Vec<String> = cats
            .iter()
            .filter_map(|c| {
                Some(format!(
                    "{} ({})",
                    c.get("category")?.as_str()?,
                    c.get("count")?.as_u64()?
                ))
            })
            .collect();
        out.push_str(&rendered.join(", "));
        out.push('\n');
    }
    out
}

pub fn locales(v: &Value) -> String {
    let region = v.get("region").and_then(Value::as_str).unwrap_or("");
    let list = v
        .get("locales")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let mut out = format!(
        "{} locale(s) in {region}. Pass a name or any alias as `city` or `near`; \
         pass the county as `county` to search_region.\n",
        list.len()
    );

    // The list arrives sorted by county, so a new heading starts a new block.
    let mut current: Option<Option<&str>> = None;
    for c in list {
        let county = c.get("county").and_then(Value::as_str);
        if current != Some(county) {
            out.push_str(&match county {
                Some(k) => format!("\ncounty: {k}\n"),
                None => "\nno county\n".to_string(),
            });
            current = Some(county);
        }
        let mut line = format!("  {}", c.get("name").and_then(Value::as_str).unwrap_or(""));
        let aliases: Vec<String> = c
            .get("aliases")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .filter_map(|a| a.as_str().map(|a| format!("{a:?}")))
            .collect();
        if !aliases.is_empty() {
            line.push_str(&format!("  ·  aka {}", aliases.join(", ")));
        }
        let f = |k: &str| c.get(k).and_then(Value::as_f64).unwrap_or_default();
        line.push_str(&format!("  ·  {:.4}, {:.4}", f("lat"), f("lng")));
        if let Some(r) = c.get("default_radius_m").and_then(Value::as_u64) {
            line.push_str(&format!("  ·  {} default radius", distance(r)));
        }
        out.push_str(&line);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit() -> Hit {
        Hit {
            id: "abc123".into(),
            kind: "place".into(),
            title: "Carlson Vineyards".into(),
            snippet: Some("Family **winery** on East Orchard Mesa".into()),
            city: Some("Palisade".into()),
            categories: vec!["winery".into()],
            url: Some("https://example.test/carlson".into()),
            address: Some("461 35 Rd".into()),
            phone: Some("+1-970-555-0123".into()),
            website: None,
            opening_hours: Some("Mo-Su 11:00-18:00".into()),
            venue: None,
            site_name: None,
            lat: 39.11,
            lng: -108.35,
            distance_m: Some(12_400),
            geo_precision: "exact".into(),
            source: "osm".into(),
            starts_at: None,
            ends_at: None,
            published_at: None,
        }
    }

    #[test]
    fn empty_results_say_what_to_try_next() {
        let out = hits("0 result(s)", &[]);
        assert!(out.contains("Nothing matched"));
        assert!(out.contains("describe_region"));
    }

    #[test]
    fn a_hit_renders_its_location_distance_and_id() {
        let out = hits("1 result", &[hit()]);
        assert!(out.contains("1. Carlson Vineyards [place]"));
        assert!(out.contains("Palisade"));
        assert!(out.contains("12.4 km away"));
        assert!(out.contains("id: abc123"));
        assert!(out.contains("+1-970-555-0123"));
        assert!(out.contains("hours: Mo-Su 11:00-18:00"));
        assert!(out.contains("location: exact"));
    }

    #[test]
    fn close_distances_read_in_metres() {
        assert_eq!(distance(29), "29 m");
        assert_eq!(distance(999), "999 m");
        assert_eq!(distance(1_000), "1.0 km");
        assert_eq!(distance(12_400), "12.4 km");
    }

    #[test]
    fn a_body_that_only_repeats_the_summary_prints_once() {
        let doc = serde_json::json!({
            "title": "X", "kind": "place", "source": "osm",
            "summary": "X is a cafe.", "body": "X is a cafe.",
        });
        assert_eq!(document(&doc).matches("X is a cafe.").count(), 1);
    }

    #[test]
    fn header_offers_paging_only_when_more_remain() {
        assert!(header("Colorado", "\"x\"", None, 10, 250, 0).contains("offset=10"));
        assert!(!header("Colorado", "\"x\"", None, 10, 10, 0).contains("offset"));
    }

    #[test]
    fn locales_group_under_their_county_with_aliases() {
        let v = serde_json::json!({
            "region": "Colorado",
            "locales": [
                { "name": "Buena Vista", "aliases": [], "county": "Chaffee",
                  "lat": 38.8422, "lng": -106.1311, "default_radius_m": 12000 },
                { "name": "Salida", "aliases": ["Salida, CO"], "county": "Chaffee",
                  "lat": 38.5347, "lng": -105.9989, "default_radius_m": 12000 },
                { "name": "Nowhere", "aliases": [], "county": null,
                  "lat": 39.0, "lng": -105.0, "default_radius_m": 5000 },
            ],
        });
        let out = locales(&v);
        assert!(out.starts_with("3 locale(s) in Colorado."));
        assert_eq!(out.matches("county: Chaffee").count(), 1);
        assert!(out.contains(
            "  Salida  ·  aka \"Salida, CO\"  ·  38.5347, -105.9989  ·  12.0 km default radius"
        ));
        assert!(out.contains("no county\n  Nowhere"));
    }

    #[test]
    fn an_unreadable_count_is_not_mistaken_for_an_empty_index() {
        let v = serde_json::json!({
            "region": "Colorado",
            "document_counts": { "places": null, "events": 0, "articles": 0 },
        });
        let out = region(&v);
        assert!(!out.contains("index is empty"));
        assert!(out.contains("places: unknown"));
    }

    #[test]
    fn an_empty_index_is_called_out_rather_than_looking_like_no_matches() {
        let v = serde_json::json!({
            "region": "Colorado",
            "document_counts": { "places": 0, "events": 0, "articles": 0 },
        });
        assert!(region(&v).contains("index is empty"));
    }
}
