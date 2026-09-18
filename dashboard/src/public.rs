//! The public request page.
//!
//! Open to anyone: no account, no token. Someone can ask for something to be
//! added, corrected or taken down, and can check back on what happened using
//! the unguessable reference they get back.

use std::sync::Arc;

use axum::Form;
use axum::Router;
use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use maud::{Markup, html};
use rand::RngExt;
use regional_core::model::now_ts;
use regional_core::submission::{Request, Status, Submission};
use serde::Deserialize;

use crate::state::AppState;
use crate::views::{self, page};

/// Caps on what the form accepts. An open endpoint needs an upper bound on
/// every free-text field or the index becomes the attacker's storage.
const MAX_TITLE: usize = 200;
const MAX_DESCRIPTION: usize = 4_000;
const MAX_SHORT: usize = 300;
const MAX_CATEGORIES: usize = 10;
const MIN_DESCRIPTION: usize = 5;

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(form).post(submit))
        .route("/thanks/{id}", get(thanks))
        .route("/status/{id}", get(status_page))
        .route("/status", post(status_lookup))
        .route("/healthz", get(crate::healthz))
        .with_state(state)
}

fn shell(state: &AppState, title: &str, body: Markup) -> Markup {
    let brand = state
        .config
        .public_title
        .clone()
        .unwrap_or_else(|| format!("{} guide", state.store.region.name));
    page(
        &brand,
        "/",
        None,
        title,
        html! { main.wrap.narrow { (body) } },
    )
}

// ------------------------------------------------------------------- form

#[derive(Debug, Deserialize)]
pub struct FormQuery {
    #[serde(default)]
    request: Option<String>,
}

async fn form(State(state): State<Arc<AppState>>, Query(q): Query<FormQuery>) -> Markup {
    let selected = q
        .request
        .as_deref()
        .and_then(Request::parse)
        .unwrap_or(Request::Add);
    shell(
        &state,
        "Suggest a change",
        form_body(&state, selected, &Draft::default(), None),
    )
}

/// What the submitter typed, kept so a rejected submission comes back filled
/// in rather than blank.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Draft {
    pub request: String,
    pub target_kind: String,
    pub title: String,
    pub description: String,
    pub url: String,
    pub address: String,
    pub city: String,
    pub lat: String,
    pub lng: String,
    pub categories: String,
    pub existing_id: String,
    pub contact: String,
    /// Honeypot. Hidden from people, irresistible to naive bots.
    pub nickname: String,
}

fn form_body(state: &AppState, selected: Request, draft: &Draft, error: Option<&str>) -> Markup {
    let region = &state.store.region;
    html! {
        h1 { "Suggest a change" }
        p.lede {
            "This is a search index of places, events and writing located inside "
            strong { (region.name) } ". If something is missing, out of date or wrong, "
            "tell us here — you don't need an account."
        }

        @if let Some(e) = error {
            .note.err { strong { "That didn't go through. " } (e) }
        }

        form.stack method="post" action="/" {
            fieldset {
                legend { "What would you like to do?" }
                .radios {
                    @for r in Request::ALL {
                        label {
                            input type="radio" name="request" value=(r.as_str())
                                checked[r == selected];
                            span {
                                span.t { (r.label()) }
                                br;
                                span.small.muted { (request_help(r)) }
                            }
                        }
                    }
                }
            }

            div {
                label for="title" { "Name" }
                input type="text" id="title" name="title" required
                    maxlength=(MAX_TITLE) value=(draft.title)
                    placeholder="Carlson Vineyards";
                p.hint { "The name of the place, event or article this is about." }
            }

            .row {
                div {
                    label for="target_kind" { "What kind of thing is it?" }
                    select id="target_kind" name="target_kind" {
                        @for (v, l) in [("place", "A place"), ("event", "An event"), ("article", "An article or guide")] {
                            option value=(v) selected[draft.target_kind == v || (draft.target_kind.is_empty() && v == "place")] { (l) }
                        }
                    }
                }
                div {
                    label for="city" { "Town " span.opt { "— optional" } }
                    input type="text" id="city" name="city" maxlength=(MAX_SHORT)
                        value=(draft.city) placeholder="Palisade";
                }
            }

            div {
                label for="description" { "What should we know?" }
                textarea id="description" name="description" required
                    maxlength=(MAX_DESCRIPTION)
                    placeholder="Tell us what to add, what changed, or what's wrong. A link we can check helps a lot." {
                    (draft.description)
                }
                p.hint {
                    "For a correction, say what's wrong and what it should be. "
                    "For something new, anything that helps us find it."
                }
            }

            div {
                label for="url" { "Website " span.opt { "— optional, but the most useful thing you can give us" } }
                input type="url" id="url" name="url" maxlength=(MAX_SHORT)
                    value=(draft.url) placeholder="https://example.com";
                p.hint { "If it's a page we can read, the indexer will pick up the details from it." }
            }

            div {
                label for="address" { "Address " span.opt { "— optional" } }
                input type="text" id="address" name="address" maxlength=(MAX_SHORT)
                    value=(draft.address) placeholder="461 35 Rd, Palisade, CO";
            }

            .row {
                div {
                    label for="lat" { "Latitude " span.opt { "— optional" } }
                    input type="text" id="lat" name="lat" inputmode="decimal"
                        value=(draft.lat) placeholder="39.1103";
                }
                div {
                    label for="lng" { "Longitude " span.opt { "— optional" } }
                    input type="text" id="lng" name="lng" inputmode="decimal"
                        value=(draft.lng) placeholder="-108.3509";
                }
            }
            p.hint {
                "Coordinates are the surest way to place something. If you leave them out "
                "we'll work it out from the address or the town."
            }

            div {
                label for="categories" { "Categories " span.opt { "— optional" } }
                input type="text" id="categories" name="categories" maxlength=(MAX_SHORT)
                    value=(draft.categories) placeholder="winery, tasting room";
                p.hint { "Comma separated." }
            }

            div {
                label for="contact" { "Your email " span.opt { "— optional" } }
                input type="email" id="contact" name="contact" maxlength=(MAX_SHORT)
                    value=(draft.contact);
                p.hint {
                    "Only so we can ask a follow-up question. It is never published, "
                    "never indexed for search, and never shown on any public page."
                }
            }

            div {
                label for="existing_id" { "Entry reference " span.opt { "— optional" } }
                input type="text" id="existing_id" name="existing_id" maxlength=(MAX_SHORT)
                    value=(draft.existing_id) placeholder="e.g. 63276ab7ed80a24f";
                p.hint { "If you already know the id of the entry, paste it here. Otherwise the name above is enough." }
            }

            // Honeypot: off-screen, not tabbable, and no person will fill it.
            div.hp aria-hidden="true" {
                label for="nickname" { "Leave this field empty" }
                input type="text" id="nickname" name="nickname" tabindex="-1"
                    autocomplete="off" value=(draft.nickname);
            }

            .actions {
                button type="submit" { "Send it in" }
                span.small.muted { "You'll get a reference so you can check back." }
            }
        }

        h2 { "Already sent something in?" }
        form.stack method="post" action="/status" {
            div {
                label for="ref" { "Your reference" }
                input type="text" id="ref" name="reference" placeholder="r7k2m…" required;
            }
            .actions { button.secondary type="submit" { "Check status" } }
        }

        (footer(state))
    }
}

fn request_help(r: Request) -> &'static str {
    match r {
        Request::Add => "A place, event or article that isn't in the index yet.",
        Request::Update => "It's listed, but the hours, address or details have changed.",
        Request::Remove => "It's closed, gone, or listed twice.",
        Request::Correction => "A specific fact is wrong.",
    }
}

fn footer(state: &AppState) -> Markup {
    html! {
        footer.foot {
            p {
                "Everything in this index sits inside " (state.store.region.name)
                ". Suggestions are read by a person before anything changes."
            }
            @if let Some(contact) = &state.config.public_contact {
                p { "Questions: " (contact) }
            }
        }
    }
}

// ----------------------------------------------------------------- submit

async fn submit(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    Form(draft): Form<Draft>,
) -> Response {
    let selected = Request::parse(&draft.request).unwrap_or(Request::Add);

    // A bot filled the hidden field. Accept it as far as the bot can tell,
    // and store nothing — no feedback to tune against.
    if !draft.nickname.trim().is_empty() {
        tracing::info!("dropped a honeypot submission");
        return Redirect::to("/thanks/none").into_response();
    }

    let key = crate::ratelimit::client_key(&headers, Some(peer));
    if !state.limiter.check(&key).await {
        tracing::warn!(client = %key, "public submission rate limited");
        return (
            StatusCode::TOO_MANY_REQUESTS,
            shell(
                &state,
                "Too many requests",
                html! {
                    h1 { "Slow down a moment" }
                    p.lede {
                        "That's more suggestions than we accept from one place in an hour. "
                        "Try again later — nothing you sent earlier was lost."
                    }
                    p { a.btn.secondary href="/" { "Back to the form" } }
                },
            ),
        )
            .into_response();
    }

    let submission = match validate(&state, selected, &draft) {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                shell(
                    &state,
                    "Check the form",
                    form_body(&state, selected, &draft, Some(&e)),
                ),
            )
                .into_response();
        }
    };

    let id = submission.id.clone();
    if let Err(e) = state.store.create(&submission).await {
        tracing::error!(error = %e, "could not store a submission");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            shell(
                &state,
                "Something broke",
                html! {
                    h1 { "We couldn't save that" }
                    p.lede {
                        "Something on our side failed, not anything you did. "
                        "Please try again in a minute."
                    }
                    p { a.btn.secondary href="/" { "Back to the form" } }
                },
            ),
        )
            .into_response();
    }

    tracing::info!(id = %id, request = %submission.request, "submission received");
    // See Other, so a refresh of the confirmation does not resubmit.
    Redirect::to(&format!("/thanks/{id}")).into_response()
}

fn validate(state: &AppState, request: Request, d: &Draft) -> Result<Submission, String> {
    let title = d.title.trim();
    if title.len() < 2 {
        return Err("Please give the name of the place, event or article.".into());
    }
    if title.chars().count() > MAX_TITLE {
        return Err(format!(
            "The name is too long (limit {MAX_TITLE} characters)."
        ));
    }

    let description = d.description.trim();
    if description.chars().count() < MIN_DESCRIPTION {
        return Err("Please say a little more about what you'd like changed.".into());
    }
    if description.chars().count() > MAX_DESCRIPTION {
        return Err(format!(
            "That description is too long (limit {MAX_DESCRIPTION} characters)."
        ));
    }

    let url = match trimmed(&d.url) {
        Some(u) => {
            let parsed = url::Url::parse(&u)
                .map_err(|_| "That website address doesn't look like a URL.".to_string())?;
            if !matches!(parsed.scheme(), "http" | "https") {
                return Err("The website address needs to start with http:// or https://.".into());
            }
            Some(parsed.to_string())
        }
        None => None,
    };

    // Coordinates are optional, but half a coordinate is not a location.
    let (lat, lng) = match (trimmed(&d.lat), trimmed(&d.lng)) {
        (Some(a), Some(b)) => {
            let lat: f64 = a
                .parse()
                .map_err(|_| "Latitude should be a number like 39.1103.".to_string())?;
            let lng: f64 = b
                .parse()
                .map_err(|_| "Longitude should be a number like -108.3509.".to_string())?;
            if !state.store.region.contains(lat, lng) {
                return Err(format!(
                    "Those coordinates are outside {}. This index only covers {}.",
                    state.store.region.name, state.store.region.name
                ));
            }
            (Some(lat), Some(lng))
        }
        (None, None) => (None, None),
        _ => return Err("Please give both latitude and longitude, or neither.".into()),
    };

    let target_kind = match d.target_kind.trim() {
        "" => "place".to_string(),
        k => regional_core::model::Kind::parse(k)
            .ok_or_else(|| "Pick what kind of thing this is.".to_string())?
            .to_string(),
    };

    let categories: Vec<String> = d
        .categories
        .split(',')
        .map(|c| c.trim().to_lowercase())
        .filter(|c| !c.is_empty())
        .take(MAX_CATEGORIES)
        .collect();

    let now = now_ts();
    Ok(Submission {
        id: reference(),
        status: Status::Pending,
        request,
        target_kind,
        title: title.to_string(),
        description: description.to_string(),
        url,
        address: trimmed(&d.address),
        city: trimmed(&d.city),
        lat,
        lng,
        categories,
        existing_id: trimmed(&d.existing_id),
        contact: trimmed(&d.contact),
        submitted_at: now,
        updated_at: now,
        reviewed_at: None,
        review_note: None,
        apply_note: None,
    })
}

fn trimmed(s: &str) -> Option<String> {
    let t = s.trim();
    (!t.is_empty())
        .then(|| t.chars().take(MAX_SHORT).collect::<String>())
        .filter(|t| !t.is_empty())
}

/// An unguessable reference. It is the document id and the only thing
/// standing between a stranger and someone else's submission, so it comes
/// from a CSPRNG rather than a counter or a timestamp.
fn reference() -> String {
    const ALPHABET: &[u8] = b"abcdefghijkmnpqrstuvwxyz23456789";
    let mut rng = rand::rng();
    (0..24)
        .map(|_| ALPHABET[rng.random_range(0..ALPHABET.len())] as char)
        .collect()
}

// ------------------------------------------------------------ confirmation

async fn thanks(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Markup {
    // The honeypot path lands here too, and must look identical.
    let known = id != "none" && state.store.get(&id).await.is_some();
    shell(
        &state,
        "Thanks",
        html! {
            h1 { "Thanks — that's with us" }
            p.lede { "A person reads every suggestion before anything changes in the index." }
            @if known {
                .note {
                    p { "Your reference:" }
                    p.mono style="font-size:18px" { (id) }
                    p.small.muted {
                        "Keep it if you want to check back. Anyone with this reference "
                        "can see the status of this suggestion, so treat it as private."
                    }
                }
                p { a.btn href={ "/status/" (id) } { "Check its status" } }
                " "
                a.btn.secondary href="/" { "Send another" }
            } @else {
                p { a.btn href="/" { "Back to the form" } }
            }
            (footer(&state))
        },
    )
}

#[derive(Debug, Deserialize)]
pub struct Lookup {
    reference: String,
}

async fn status_lookup(Form(l): Form<Lookup>) -> Redirect {
    let r = l.reference.trim();
    // Keep the reference out of the path when it is obviously not one.
    if r.is_empty() || r.len() > 64 {
        return Redirect::to("/status/unknown");
    }
    Redirect::to(&format!("/status/{r}"))
}

async fn status_page(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    let Some(sub) = state.store.get(&id).await else {
        return (
            StatusCode::NOT_FOUND,
            shell(
                &state,
                "Not found",
                html! {
                    h1 { "We can't find that reference" }
                    p.lede {
                        "Check for a typo — references are 24 characters, no capitals. "
                        "If you never got one, the suggestion may not have gone through."
                    }
                    p { a.btn.secondary href="/" { "Back to the form" } }
                },
            ),
        )
            .into_response();
    };

    shell(
        &state,
        "Suggestion status",
        html! {
            h1 { "Your suggestion" }
            p.lede { (sub.title) }

            .note[matches!(sub.status, Status::Applied)] {
                p {
                    strong { (sub.status.public_label()) }
                }
                @if let Some(note) = &sub.review_note {
                    p { (note) }
                }
            }

            dl.facts {
                dt { "Reference" }   dd.mono { (sub.id) }
                dt { "Request" }     dd { (sub.request.label()) }
                dt { "Sent" }        dd { (views::ts(sub.submitted_at)) " (" (views::ago(sub.submitted_at)) ")" }
                dt { "Reviewed" }    dd { (views::opt_ts(sub.reviewed_at)) }
            }

            p { a.btn.secondary href="/" { "Send another suggestion" } }
            (footer(&state))
        },
    )
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn references_are_long_unguessable_and_unambiguous() {
        let a = reference();
        assert_eq!(a.chars().count(), 24);
        assert_ne!(a, reference());
        // No look-alike characters, so a reference can be read aloud or
        // retyped from a screenshot without ambiguity.
        assert!(!a.contains('l') && !a.contains('o') && !a.contains('0') && !a.contains('1'));
        assert!(
            a.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        );
    }

    #[test]
    fn trimming_bounds_every_short_field() {
        assert_eq!(trimmed("  hi  ").as_deref(), Some("hi"));
        assert_eq!(trimmed("   "), None);
        assert_eq!(trimmed(""), None);
        let long = "x".repeat(10_000);
        assert_eq!(trimmed(&long).unwrap().chars().count(), MAX_SHORT);
    }
}
