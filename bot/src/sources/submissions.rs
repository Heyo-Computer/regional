//! Approved public submissions.
//!
//! This is what makes the request form more than a suggestion box: once a
//! reviewer approves something on the dashboard, this source picks it up on
//! its next pass and acts on it.
//!
//! An approved submission that cannot be acted on is not retried forever.
//! It gets an `apply_note` explaining exactly what was missing and drops out
//! of this source's filter, which puts it back in front of the reviewer on
//! the dashboard. Re-approving clears the note and tries again.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use meilisearch_sdk::search::SearchQuery;
use regional_core::index::SUBMISSIONS;
use regional_core::meili;
use regional_core::model::{
    Article, Doc, Envelope, Event, GeoPoint, GeoPrecision, Kind, Place, now_ts,
};
use regional_core::submission::{Request, Status, Submission};
use serde_json::Value;

use super::{Batch, Ctx, Source};
use crate::state::FrontierEntry;

/// Submissions handled per pass. Approving a batch on the dashboard should
/// drain within a cycle or two.
const PER_RUN: usize = 50;

pub struct Submissions;

#[async_trait]
impl Source for Submissions {
    fn name(&self) -> &'static str {
        "submissions"
    }

    fn default_interval(&self) -> Duration {
        // Short: a reviewer who just approved something should see it appear
        // rather than wonder whether the queue is moving.
        Duration::from_secs(120)
    }

    async fn fetch(&self, ctx: &Ctx, _cursor: Option<Value>) -> Result<Batch> {
        let pending = approved(ctx).await?;
        if pending.is_empty() {
            return Ok(Batch::empty());
        }
        tracing::info!(n = pending.len(), "applying approved submissions");

        let mut docs = Vec::new();
        for mut sub in pending {
            match apply(ctx, &sub, &mut docs).await {
                Ok(note) => {
                    sub.status = Status::Applied;
                    sub.apply_note = note;
                    sub.updated_at = now_ts();
                    tracing::info!(id = %sub.id, request = %sub.request, "submission applied");
                }
                Err(reason) => {
                    // Stays approved, but carries the reason, which both
                    // stops the retry loop and tells the reviewer what to do.
                    sub.apply_note = Some(reason.clone());
                    sub.updated_at = now_ts();
                    tracing::warn!(id = %sub.id, reason, "could not apply submission");
                }
            }
            if let Err(e) =
                meili::upsert_chunked(ctx.state.client(), SUBMISSIONS, std::slice::from_ref(&sub))
                    .await
            {
                tracing::error!(id = %sub.id, error = %e, "could not write the submission back");
            }
        }

        Ok(Batch::new(docs, None))
    }
}

/// Approved submissions the indexer has not already failed on.
async fn approved(ctx: &Ctx) -> Result<Vec<Submission>> {
    let idx = ctx.state.client().index(SUBMISSIONS);
    let sort = ["submitted_at:asc"];
    let mut q = SearchQuery::new(&idx);
    q.with_query("")
        .with_filter("status = \"approved\" AND apply_note IS NULL")
        .with_sort(&sort)
        .with_limit(PER_RUN);
    let res = q.execute::<Submission>().await?;
    Ok(res.hits.into_iter().map(|h| h.result).collect())
}

/// Act on one submission.
///
/// `Ok(note)` means done, with an optional explanation for the record.
/// `Err(reason)` means it could not be applied and why.
async fn apply(ctx: &Ctx, sub: &Submission, docs: &mut Vec<Doc>) -> Result<Option<String>, String> {
    match sub.request {
        Request::Add => {
            let (geo, precision) = locate(ctx, sub).await.ok_or_else(|| {
                "Nothing to place this by: no coordinates, no address we could geocode, \
                 and no town we recognise. Ask the submitter for coordinates or an address."
                    .to_string()
            })?;
            docs.push(build(sub, geo, precision));

            // A website is worth more than the form fields: queue it so the
            // crawler can replace this stub with the real page content.
            let queued = queue_url(ctx, sub).await;
            Ok(match (precision, queued) {
                (GeoPrecision::City, true) => Some(
                    "Indexed at the town centre for now; the website is queued for crawling."
                        .into(),
                ),
                (GeoPrecision::City, false) => {
                    Some("Indexed at the town centre — no exact location was given.".into())
                }
                (_, true) => Some("Indexed; the website is queued for crawling.".into()),
                (_, false) => None,
            })
        }

        Request::Remove => {
            let kind = target_kind(sub)?;
            let id = sub.existing_id.as_deref().ok_or_else(|| {
                "No entry id. Find the document in the index browser and add its id \
                 before approving."
                    .to_string()
            })?;

            // Suppress first, then delete. The other order leaves a window
            // where a concurrent source sweep could re-add it.
            ctx.state
                .suppress(id, &format!("removal requested: {}", sub.id))
                .await
                .map_err(|e| format!("could not record the suppression: {e}"))?;
            meili::delete_chunked(ctx.state.client(), kind.index(), &[id.to_string()])
                .await
                .map_err(|e| format!("could not delete the document: {e}"))?;
            Ok(Some(
                "Removed, and suppressed so the sources cannot re-add it.".into(),
            ))
        }

        Request::Update | Request::Correction => {
            // We do not overwrite fields on a source-derived document: the
            // next sweep of that source would simply undo it. Re-reading the
            // website is the change that actually sticks.
            if queue_url(ctx, sub).await {
                Ok(Some(
                    "The website is queued for re-reading; the entry updates once it is crawled."
                        .into(),
                ))
            } else {
                Err("Nothing to act on automatically: no website to re-read. \
                     Fix this at the source (OpenStreetMap, for an `osm` entry), \
                     or ask the submitter for a URL."
                    .to_string())
            }
        }
    }
}

/// Put the submission's website on the crawl frontier.
async fn queue_url(ctx: &Ctx, sub: &Submission) -> bool {
    let Some(url) = sub.url.as_deref() else {
        return false;
    };
    let entry = FrontierEntry {
        url: url.to_string(),
        depth: 0,
        default_city: sub.city.clone(),
    };
    match ctx.state.frontier_add(&[entry]).await {
        Ok(_) => true,
        Err(e) => {
            tracing::warn!(error = %e, url, "could not queue the submitted URL");
            false
        }
    }
}

/// Work out where the submission is, in descending order of confidence.
async fn locate(ctx: &Ctx, sub: &Submission) -> Option<(GeoPoint, GeoPrecision)> {
    if sub.has_coords() {
        let geo = GeoPoint::new(sub.lat?, sub.lng?);
        if ctx.region.contains(geo.lat, geo.lng) {
            return Some((geo, GeoPrecision::Exact));
        }
        // The form rejects out-of-region coordinates, so this only happens
        // if the region config changed after the submission was filed.
        tracing::warn!(id = %sub.id, "submitted coordinates fall outside the region");
    }
    if let Some(address) = sub.address.as_deref()
        && let Some(geo) = ctx.geocoder.geocode(address).await
    {
        return Some((geo, GeoPrecision::Address));
    }
    if let Some(city) = sub.city.as_deref()
        && let Some(found) = ctx.region.resolve_place(city)
    {
        return Some((found.geo(), GeoPrecision::City));
    }
    None
}

fn target_kind(sub: &Submission) -> Result<Kind, String> {
    Kind::parse(&sub.target_kind)
        .ok_or_else(|| format!("unknown target kind {:?}", sub.target_kind))
}

/// Turn an approved `add` into a document.
fn build(sub: &Submission, geo: GeoPoint, precision: GeoPrecision) -> Doc {
    let kind = Kind::parse(&sub.target_kind).unwrap_or(Kind::Place);
    let mut env = Envelope::new(kind, "submission", &sub.id, &sub.title, geo);
    env.geo_precision = precision;
    env.url = sub.url.clone();
    env.summary = sub.description.clone();
    env.body = sub.description.clone();
    env.city = sub.city.clone();
    env.address = sub.address.clone();
    env.categories = sub.categories.clone();
    // Tagged so an operator can tell submitted entries from crawled ones,
    // and so they can be found and re-reviewed later.
    env.tags = vec!["submitted".into()];

    match kind {
        Kind::Event => Doc::Event(Event {
            env,
            // Events need a start time to be filterable, and the form does
            // not collect one; today keeps it visible rather than hiding it
            // in the past.
            start_time: now_ts(),
            end_time: None,
            venue_name: None,
            organizer: None,
            price: None,
        }),
        Kind::Article => Doc::Article(Article {
            env,
            published_at: Some(sub.submitted_at),
            author: None,
            site_name: None,
            lang: None,
            word_count: Some(sub.description.split_whitespace().count() as u32),
        }),
        Kind::Place => Doc::Place(Place {
            env,
            phone: None,
            website: sub.url.clone(),
            opening_hours: None,
            cuisine: vec![],
            price_level: None,
            osm_type: None,
            osm_id: None,
            wikidata_id: None,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regional_core::submission::{Request, Status};

    fn sub(request: Request) -> Submission {
        Submission {
            id: "abc123".into(),
            status: Status::Approved,
            request,
            target_kind: "place".into(),
            title: "Mesa Vista Winery".into(),
            description: "New tasting room on East Orchard Mesa.".into(),
            url: Some("https://example.test/winery".into()),
            address: None,
            city: Some("Palisade".into()),
            lat: Some(39.1103),
            lng: Some(-108.3509),
            categories: vec!["winery".into()],
            existing_id: None,
            contact: Some("someone@example.test".into()),
            submitted_at: 1_700_000_000,
            updated_at: 1_700_000_000,
            reviewed_at: None,
            review_note: None,
            apply_note: None,
        }
    }

    #[test]
    fn an_approved_addition_becomes_a_tagged_document() {
        let s = sub(Request::Add);
        let doc = build(&s, GeoPoint::new(39.1103, -108.3509), GeoPrecision::Exact).finalize();
        let env = doc.env();
        assert_eq!(env.title, "Mesa Vista Winery");
        assert_eq!(env.source, "submission");
        assert_eq!(env.source_id, "abc123");
        assert_eq!(env.city.as_deref(), Some("Palisade"));
        assert!(env.categories.contains(&"winery".to_string()));
        assert!(env.tags.contains(&"submitted".to_string()));
        assert_eq!(doc.index(), "places");
    }

    #[test]
    fn the_submitters_contact_never_reaches_the_index() {
        // The form promises the address is never published; the document
        // built from it is the one place that promise could be broken.
        let s = sub(Request::Add);
        let doc = build(&s, GeoPoint::new(39.1103, -108.3509), GeoPrecision::Exact).finalize();
        let json = serde_json::to_string(&doc).unwrap();
        assert!(
            !json.contains("someone@example.test"),
            "contact leaked into the indexed document: {json}"
        );
    }

    #[test]
    fn an_event_submission_gets_a_start_time_so_it_can_be_filtered() {
        let mut s = sub(Request::Add);
        s.target_kind = "event".into();
        let doc = build(&s, GeoPoint::new(39.1, -108.3), GeoPrecision::Exact);
        let Doc::Event(e) = &doc else {
            panic!("expected an event")
        };
        assert!(e.start_time > 0);
        assert_eq!(doc.index(), "events");
    }

    #[test]
    fn an_unknown_target_kind_is_reported_not_guessed() {
        let mut s = sub(Request::Remove);
        s.target_kind = "widget".into();
        assert!(target_kind(&s).is_err());
        s.target_kind = "article".into();
        assert_eq!(target_kind(&s).unwrap(), Kind::Article);
    }
}
