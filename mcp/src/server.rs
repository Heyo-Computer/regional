//! The MCP tool surface.
//!
//! Tool descriptions are the only documentation a model gets, so they carry
//! the operational detail: what the region is, what a parameter does, and
//! what to call when a guess does not resolve.

use std::sync::Arc;

use regional_core::model::Kind;
use regional_core::region::City;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, ContentBlock, ErrorData, Implementation, ServerCapabilities, ServerConfig,
};
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::filter::Filter;
use crate::geo;
use crate::render;
use crate::search::{self, DEFAULT_LIMIT, Hit, MAX_LIMIT, Params, to_hit};
use crate::state::AppState;

#[derive(Clone)]
pub struct RegionalSearch {
    state: Arc<AppState>,
}

impl RegionalSearch {
    pub fn new(state: Arc<AppState>) -> Self {
        Self { state }
    }
}

// ---------------------------------------------------------------- arguments

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchRegionArgs {
    /// What to look for, in plain words: "wood fired pizza", "vineyard",
    /// "hot springs", "ski rentals". Do not put the place name in here —
    /// use `city` or `near` for that, which filter geographically instead
    /// of just matching text.
    pub query: String,

    /// Restrict to some content kinds: "place", "event", "article".
    /// Omit to search all three and get one merged, relevance-ranked list.
    #[serde(default)]
    pub kinds: Option<Vec<String>>,

    /// A town, neighbourhood or landmark to search around. Resolved against
    /// the region's gazetteer first, then against indexed places. Prefer
    /// this over putting the location in `query`.
    #[serde(default)]
    pub near: Option<String>,

    /// Radius in metres around `near` / `city`. Defaults to a sensible
    /// radius for the resolved place (larger for rural towns than for
    /// dense city centres).
    #[serde(default)]
    pub radius_m: Option<u32>,

    /// A town or city to limit results to. Known towns become a radius
    /// around the town centre, which is far more reliable than matching
    /// the city name on each document. Call `list_locales` for the list.
    #[serde(default)]
    pub city: Option<String>,

    /// A county name to limit results to, matched exactly. `list_locales`
    /// shows the county names this server uses.
    #[serde(default)]
    pub county: Option<String>,

    /// Category values to require, e.g. ["restaurant"], ["winery"]. A hit
    /// needs any one of them. `describe_region` lists the live values.
    #[serde(default)]
    pub categories: Option<Vec<String>>,

    /// How many results to return. Default 10, maximum 50.
    #[serde(default)]
    pub limit: Option<usize>,

    /// How many results to skip, for paging through a large result set.
    #[serde(default)]
    pub offset: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FindNearbyArgs {
    /// A town, neighbourhood or landmark to measure distance from. Give
    /// this or `lat`+`lng`.
    #[serde(default)]
    pub near: Option<String>,

    /// Latitude, if you already have coordinates. Must be inside the region.
    #[serde(default)]
    pub lat: Option<f64>,

    /// Longitude, if you already have coordinates.
    #[serde(default)]
    pub lng: Option<f64>,

    /// Optional text to narrow results, e.g. "coffee". Omit to get whatever
    /// is closest regardless of what it is.
    #[serde(default)]
    pub query: Option<String>,

    /// Search radius in metres. Defaults to 5000 (about 3 miles).
    #[serde(default)]
    pub radius_m: Option<u32>,

    /// Restrict to some content kinds: "place", "event", "article".
    /// Defaults to places only, which is what proximity usually means.
    #[serde(default)]
    pub kinds: Option<Vec<String>>,

    /// Category values to require; a hit needs any one of them.
    #[serde(default)]
    pub categories: Option<Vec<String>>,

    /// How many results to return. Default 10, maximum 50.
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchEventsArgs {
    /// Optional text to match, e.g. "bluegrass", "farmers market".
    #[serde(default)]
    pub query: Option<String>,

    /// A town, neighbourhood or landmark to search around.
    #[serde(default)]
    pub near: Option<String>,

    /// Radius in metres around `near`.
    #[serde(default)]
    pub radius_m: Option<u32>,

    /// A town or city to limit results to.
    #[serde(default)]
    pub city: Option<String>,

    /// Only events starting at or after this time, as an ISO-8601 datetime
    /// ("2026-07-04T00:00:00Z") or a unix timestamp. Defaults to now, so
    /// past events are excluded unless you ask for them.
    #[serde(default)]
    pub starts_after: Option<String>,

    /// Only events starting at or before this time. Same formats as
    /// `starts_after`.
    #[serde(default)]
    pub starts_before: Option<String>,

    /// How many results to return. Default 10, maximum 50.
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListLocalesArgs {
    /// Only list the locales in this county, e.g. "Chaffee" or "Chaffee
    /// County". Omit to list every locale in the region.
    #[serde(default)]
    pub county: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetDocumentArgs {
    /// The `id` from a search result.
    pub id: String,
    /// Which kind it was: "place", "event" or "article". Search results
    /// carry this in their `kind` field.
    pub kind: String,
}

// -------------------------------------------------------------------- tools

#[tool_router]
impl RegionalSearch {
    /// Search everything indexed for this region: places, events and
    /// articles, merged into one ranked list.
    ///
    /// This is the tool to reach for first. Put the subject in `query` and
    /// the location in `city` or `near` — the location parameters filter on
    /// real coordinates, so they find things a text match would miss.
    #[tool(
        name = "search_region",
        description = "Search the region's indexed places, events and articles by text, with optional geographic and category filters. Put the subject in `query` and the location in `city` or `near`. Returns one relevance-ranked list across all content kinds."
    )]
    async fn search_region(
        &self,
        Parameters(args): Parameters<SearchRegionArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let st = &self.state;
        let kinds = match parse_kinds(args.kinds.as_deref(), &Kind::ALL) {
            Ok(k) => k,
            Err(e) => return Ok(user_error(e)),
        };
        let limit = clamp_limit(args.limit);
        let offset = args.offset.unwrap_or(0);

        let mut filter = Filter::within(&st.region.geo_filter());
        let mut anchor_note = None;

        // `city` and `near` both become a radius when we can resolve them.
        let place = args.near.as_deref().or(args.city.as_deref());
        if let Some(name) = place {
            match geo::resolve(st, name).await {
                Ok(a) => {
                    let radius = args.radius_m.unwrap_or(a.default_radius_m);
                    filter.geo_radius(a.geo.lat, a.geo.lng, radius);
                    anchor_note = Some(format!(
                        "within {:.0} km of {} (matched via {})",
                        radius as f64 / 1000.0,
                        a.label,
                        a.via
                    ));
                }
                Err(msg) => {
                    // An unresolvable `city` still has a chance as a plain
                    // string match; an unresolvable `near` does not.
                    if args.near.is_some() {
                        return Ok(user_error(msg));
                    }
                    filter.eq("city", name);
                    anchor_note = Some(format!("with city exactly {name:?}"));
                }
            }
        }
        if let Some(county) = args.county.as_deref() {
            filter.eq("county", county);
        }
        if let Some(cats) = args.categories.as_deref() {
            filter.any_of("categories", cats);
        }

        let params = Params {
            query: args.query.clone(),
            filter: filter.build(),
            sort: vec![],
            limit,
            offset,
        };
        let indexes = search::indexes_for(&kinds);
        let page = match search::search_federated(&st.client, &indexes, &params).await {
            Ok(p) => p,
            Err(e) => return Ok(backend_error("search_region", e)),
        };

        let hits = shape(st, page.hits);
        let header = render::header(
            &st.region.name,
            &format!("{:?}", args.query),
            anchor_note.as_deref(),
            hits.len(),
            page.estimated_total,
            offset,
        );
        Ok(respond(
            render::hits(&header, &hits),
            json!({
                "region": st.region.name,
                "query": args.query,
                "kinds": kinds.iter().map(|k| k.as_str()).collect::<Vec<_>>(),
                "scope": anchor_note,
                "estimated_total": page.estimated_total,
                "offset": offset,
                "results": hits,
            }),
        ))
    }

    /// Find what is physically closest to a point, ordered by distance.
    #[tool(
        name = "find_nearby",
        description = "Find indexed content closest to a point, strictly ordered by true distance. Give `near` (a town or landmark) or `lat`+`lng`. Each result carries `distance_m`. Use this for proximity questions (\"closest coffee to my hotel\"); use search_region when relevance matters more than distance."
    )]
    async fn find_nearby(
        &self,
        Parameters(args): Parameters<FindNearbyArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let st = &self.state;
        let kinds = match parse_kinds(args.kinds.as_deref(), &[Kind::Place]) {
            Ok(k) => k,
            Err(e) => return Ok(user_error(e)),
        };
        let limit = clamp_limit(args.limit);

        let anchor = match geo::resolve_any(st, args.near.as_deref(), args.lat, args.lng).await {
            Ok(Some(a)) => a,
            Ok(None) => {
                return Ok(user_error(
                    "find_nearby needs a location: pass `near` (a town or landmark) or both `lat` and `lng`."
                        .to_string(),
                ));
            }
            Err(msg) => return Ok(user_error(msg)),
        };
        let radius = args.radius_m.unwrap_or(5_000);

        let mut filter = Filter::within(&st.region.geo_filter());
        filter.geo_radius(anchor.geo.lat, anchor.geo.lng, radius);
        if let Some(cats) = args.categories.as_deref() {
            filter.any_of("categories", cats);
        }

        let params = Params {
            query: args.query.clone().unwrap_or_default(),
            filter: filter.build(),
            sort: vec![format!(
                "_geoPoint({}, {}):asc",
                anchor.geo.lat, anchor.geo.lng
            )],
            limit,
            offset: 0,
        };
        let indexes = search::indexes_for(&kinds);
        let page = match search::search_nearby(&st.client, &indexes, &params).await {
            Ok(p) => p,
            Err(e) => return Ok(backend_error("find_nearby", e)),
        };

        let hits = shape(st, page.hits);
        let header = format!(
            "{} result(s) within {:.1} km of {} in {}",
            hits.len(),
            radius as f64 / 1000.0,
            anchor.label,
            st.region.name
        );
        Ok(respond(
            render::hits(&header, &hits),
            json!({
                "region": st.region.name,
                "anchor": { "label": anchor.label, "lat": anchor.geo.lat, "lng": anchor.geo.lng, "via": anchor.via },
                "radius_m": radius,
                "results": hits,
            }),
        ))
    }

    /// Search events by time window and location.
    #[tool(
        name = "search_events",
        description = "Search events in the region within a time window, sorted soonest first. Defaults to upcoming events only. Accepts ISO-8601 datetimes or unix timestamps for `starts_after` / `starts_before`."
    )]
    async fn search_events(
        &self,
        Parameters(args): Parameters<SearchEventsArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let st = &self.state;
        let limit = clamp_limit(args.limit);

        let after = match parse_time(args.starts_after.as_deref()) {
            Ok(v) => v.unwrap_or_else(regional_core::model::now_ts),
            Err(e) => return Ok(user_error(e)),
        };
        let before = match parse_time(args.starts_before.as_deref()) {
            Ok(v) => v,
            Err(e) => return Ok(user_error(e)),
        };
        if let Some(b) = before
            && b < after
        {
            return Ok(user_error(
                "starts_before is earlier than starts_after, so no event can match".to_string(),
            ));
        }

        let mut filter = Filter::within(&st.region.geo_filter());
        filter.gte("start_time", after);
        if let Some(b) = before {
            filter.lte("start_time", b);
        }

        let mut window = format!("from {}", search::fmt_ts(after).unwrap_or_default());
        if let Some(b) = before {
            window.push_str(&format!(" to {}", search::fmt_ts(b).unwrap_or_default()));
        }

        let place = args.near.as_deref().or(args.city.as_deref());
        if let Some(name) = place {
            match geo::resolve(st, name).await {
                Ok(a) => {
                    let radius = args.radius_m.unwrap_or(a.default_radius_m);
                    filter.geo_radius(a.geo.lat, a.geo.lng, radius);
                    window.push_str(&format!(
                        ", within {:.0} km of {}",
                        radius as f64 / 1000.0,
                        a.label
                    ));
                }
                Err(msg) => return Ok(user_error(msg)),
            }
        }

        let params = Params {
            query: args.query.clone().unwrap_or_default(),
            filter: filter.build(),
            sort: vec!["start_time:asc".to_string()],
            limit,
            offset: 0,
        };
        let page = match search::search_one(&st.client, regional_core::index::EVENTS, &params).await
        {
            Ok(p) => p,
            Err(e) => return Ok(backend_error("search_events", e)),
        };

        let hits = shape(st, page.hits);
        let header = format!("{} event(s) in {}, {window}", hits.len(), st.region.name);
        Ok(respond(
            render::hits(&header, &hits),
            json!({
                "region": st.region.name,
                "window": { "starts_after": after, "starts_before": before },
                "estimated_total": page.estimated_total,
                "results": hits,
            }),
        ))
    }

    /// Retrieve one document in full.
    #[tool(
        name = "get_document",
        description = "Fetch the complete record for one document by its `id` and `kind`, including the full body text that search results truncate."
    )]
    async fn get_document(
        &self,
        Parameters(args): Parameters<GetDocumentArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let st = &self.state;
        let Some(kind) = Kind::parse(&args.kind) else {
            return Ok(user_error(format!(
                "unknown kind {:?}; expected one of: place, event, article",
                args.kind
            )));
        };
        match st
            .client
            .index(kind.index())
            .get_document::<Value>(&args.id)
            .await
        {
            Ok(doc) => Ok(respond(render::document(&doc), doc)),
            Err(e) => {
                tracing::debug!(error = %e, id = %args.id, kind = %kind, "document lookup failed");
                Ok(user_error(format!(
                    "no {} with id {:?}. Ids come from search results and are only valid for the kind they were returned under.",
                    kind, args.id
                )))
            }
        }
    }

    /// List the named places `city` and `near` resolve against.
    #[tool(
        name = "list_locales",
        description = "List the locales this server resolves by name — the towns and cities `city` and `near` accept — with each one's aliases, county, coordinates and default search radius, grouped by county. Pass `county` to list one county. Use this to turn a county or a vague area into place names the other tools accept, and to find the county value `search_region` filters on."
    )]
    async fn list_locales(
        &self,
        Parameters(args): Parameters<ListLocalesArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let region = &self.state.region;
        let county = args
            .county
            .as_deref()
            .map(str::trim)
            .filter(|c| !c.is_empty());
        let mut locales: Vec<&City> = match county {
            Some(county) => region.cities_in_county(county),
            None => region.cities.iter().collect(),
        };
        if let Some(county) = county
            && locales.is_empty()
        {
            return Ok(user_error(format!(
                "no locales in a county called {county:?} in {}. Counties with locales: {}.",
                region.name,
                region.county_names().join(", ")
            )));
        }
        // By county, so each county reads as one block; places with no county last.
        locales.sort_by(|a, b| {
            (a.county.is_none(), &a.county, &a.name).cmp(&(b.county.is_none(), &b.county, &b.name))
        });

        let value = json!({
            "region": region.name,
            // Echo the county as the gazetteer spells it, which is the value
            // `search_region` matches, rather than however it was asked for.
            "county": county.and(locales.first().and_then(|c| c.county.clone())),
            "locales": locales
                .iter()
                .map(|c| json!({
                    "name": c.name,
                    "aliases": c.aliases,
                    "county": c.county,
                    "lat": c.lat,
                    "lng": c.lng,
                    "default_radius_m": c.default_radius_m,
                }))
                .collect::<Vec<_>>(),
        });
        Ok(respond(render::locales(&value), value))
    }

    /// Describe the region and what is currently indexed.
    #[tool(
        name = "describe_region",
        description = "Describe this server's region: its name, bounding box, the place names it can resolve, how many documents of each kind are indexed, the live category values, and how fresh the index is. Call this first to discover valid `city` and `categories` values instead of guessing."
    )]
    async fn describe_region(&self) -> Result<CallToolResult, ErrorData> {
        let st = &self.state;
        let region = &st.region;
        let counts = st.doc_counts().await;
        let envelope = region.geo_filter();

        let categories =
            match search::facet_counts(&st.client, regional_core::index::PLACES, "categories", "")
                .await
            {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(error = %e, "facet lookup failed");
                    Default::default()
                }
            };
        let mut top: Vec<(String, usize)> = categories.into_iter().collect();
        top.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        top.truncate(40);

        let mut freshness = serde_json::Map::new();
        for index in regional_core::index::CONTENT_INDEXES {
            let ts = search::newest_update(&st.client, index)
                .await
                .ok()
                .flatten();
            freshness.insert(index.to_string(), json!(ts.and_then(search::fmt_ts)));
        }

        let value = json!({
            "region": region.name,
            "slug": region.slug,
            "admin_level": region.admin_level,
            "timezone": region.timezone,
            "bounding_box": {
                "min_lat": region.bbox.min_lat,
                "min_lng": region.bbox.min_lng,
                "max_lat": region.bbox.max_lat,
                "max_lng": region.bbox.max_lng,
            },
            "centroid": { "lat": region.center().lat, "lng": region.center().lng },
            "geo_filter": envelope,
            "document_counts": counts,
            "last_updated": freshness,
            "resolvable_places": region.city_names(),
            "top_place_categories": top
                .iter()
                .map(|(name, n)| json!({ "category": name, "count": n }))
                .collect::<Vec<_>>(),
        });
        Ok(respond(render::region(&value), value))
    }
}

#[tool_handler]
impl ServerHandler for RegionalSearch {
    fn get_info(&self) -> ServerConfig {
        let r = &self.state.region;
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                format!("regional-search-{}", r.slug),
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(format!(
                "Search engine for content geographically located inside {region}. \
                 Everything indexed — places, events and articles — has coordinates \
                 inside the region, so results are always local to {region} and a \
                 query about anywhere else will correctly return nothing.\n\n\
                 Put the subject of the search in `query` and the location in `city` \
                 or `near`; those resolve to real coordinates and filter on distance, \
                 which finds things that a text match on the place name would miss. \
                 Start with `describe_region` to see what is indexed and which \
                 category values exist, and `list_locales` for the place names \
                 `city` and `near` accept.",
                region = r.name
            ))
    }
}

// ------------------------------------------------------------------ helpers

fn clamp_limit(limit: Option<usize>) -> usize {
    limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)
}

fn parse_kinds(raw: Option<&[String]>, default: &[Kind]) -> Result<Vec<Kind>, String> {
    let Some(raw) = raw else {
        return Ok(default.to_vec());
    };
    if raw.is_empty() {
        return Ok(default.to_vec());
    }
    let mut out = Vec::new();
    for k in raw {
        match Kind::parse(k) {
            Some(kind) if !out.contains(&kind) => out.push(kind),
            Some(_) => {}
            None => {
                return Err(format!(
                    "unknown kind {k:?}; expected any of: place, event, article"
                ));
            }
        }
    }
    Ok(out)
}

/// Accept either an ISO-8601 datetime or a bare unix timestamp, because
/// models reliably produce both.
fn parse_time(raw: Option<&str>) -> Result<Option<i64>, String> {
    let Some(raw) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    if let Ok(ts) = raw.parse::<i64>() {
        return Ok(Some(ts));
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(raw) {
        return Ok(Some(dt.timestamp()));
    }
    // A bare date is a common and unambiguous shape; treat it as midnight UTC.
    if let Ok(d) = chrono::NaiveDate::parse_from_str(raw, "%Y-%m-%d") {
        return Ok(Some(d.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp()));
    }
    Err(format!(
        "could not read {raw:?} as a time; use ISO-8601 (2026-07-04T00:00:00Z), a date (2026-07-04), or a unix timestamp"
    ))
}

fn shape(st: &AppState, raw: Vec<(crate::search::RawHit, Option<String>)>) -> Vec<Hit> {
    raw.into_iter()
        .map(|(r, snip)| {
            let city = geo::label_city(st, &r);
            to_hit(r, snip, city)
        })
        .collect()
}

/// A result the caller can act on: readable text plus the structured
/// equivalent, so both kinds of MCP client get something useful.
fn respond(text: String, value: Value) -> CallToolResult {
    let mut result = CallToolResult::structured(value);
    result.content = vec![ContentBlock::text(text)];
    result
}

/// The caller asked for something we cannot do. Returned as a tool-level
/// error so the message actually reaches them.
fn user_error(message: String) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(message)])
}

fn backend_error(tool: &str, e: anyhow::Error) -> CallToolResult {
    tracing::error!(tool, error = %e, "search backend error");
    CallToolResult::error(vec![ContentBlock::text(format!(
        "the search backend failed while handling {tool}: {e}"
    ))])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limit_is_clamped_to_a_sane_window() {
        assert_eq!(clamp_limit(None), DEFAULT_LIMIT);
        assert_eq!(clamp_limit(Some(0)), 1);
        assert_eq!(clamp_limit(Some(5)), 5);
        assert_eq!(clamp_limit(Some(10_000)), MAX_LIMIT);
    }

    #[test]
    fn kinds_parse_with_defaults_and_dedupe() {
        assert_eq!(
            parse_kinds(None, &[Kind::Place]).unwrap(),
            vec![Kind::Place]
        );
        assert_eq!(
            parse_kinds(Some(&[]), &Kind::ALL).unwrap(),
            Kind::ALL.to_vec()
        );
        assert_eq!(
            parse_kinds(Some(&["place".into(), "places".into()]), &Kind::ALL).unwrap(),
            vec![Kind::Place]
        );
        assert!(parse_kinds(Some(&["restaurant".into()]), &Kind::ALL).is_err());
    }

    #[test]
    fn times_parse_from_every_shape_a_model_produces() {
        assert_eq!(parse_time(None).unwrap(), None);
        assert_eq!(parse_time(Some("  ")).unwrap(), None);
        assert_eq!(parse_time(Some("0")).unwrap(), Some(0));
        assert_eq!(
            parse_time(Some("1970-01-02T00:00:00Z")).unwrap(),
            Some(86_400)
        );
        assert_eq!(parse_time(Some("1970-01-02")).unwrap(), Some(86_400));
        assert!(parse_time(Some("next tuesday")).is_err());
    }
}
