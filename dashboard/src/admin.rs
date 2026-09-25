//! The operator dashboard.
//!
//! Deliberately unauthenticated — the load balancer is expected to sit in
//! front of this port. Everything that changes state lives here rather than
//! on the public port, so "protect this port" is the whole access policy.

use std::sync::Arc;

use axum::Form;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use maud::{Markup, html};
use regional_core::index::CONTENT_INDEXES;
use regional_core::model::{Kind, now_ts};
use regional_core::submission::{Request, Status, Submission};
use regional_core::token::McpToken;
use serde::Deserialize;

use crate::state::AppState;
use crate::store::{BotHealth, Overview, PAGE_SIZE};
use crate::views::{self, Nav, page, request_badge, status_badge};

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(overview))
        .route("/submissions", get(submissions))
        .route("/submissions/{id}", get(submission_detail))
        .route("/submissions/{id}/review", post(review))
        .route("/browse", get(browse))
        .route("/tokens", get(tokens).post(mint_token))
        .route("/tokens/{id}/revoke", post(revoke_token))
        .route("/healthz", get(crate::healthz))
        .with_state(state)
}

fn nav(current: &'static str) -> Nav {
    Nav {
        items: vec![
            ("/", "Overview"),
            ("/submissions", "Submissions"),
            ("/browse", "Browse index"),
            ("/tokens", "MCP tokens"),
        ],
        current,
    }
}

/// The page title defaults to the nav entry; `shell_titled` overrides it.
fn shell(state: &AppState, current: &'static str, body: Markup) -> Markup {
    shell_titled(state, current, current, body)
}

fn shell_titled(state: &AppState, current: &'static str, title: &str, body: Markup) -> Markup {
    let brand = format!("{} · index admin", state.store.region.name);
    page(
        &brand,
        "/",
        Some(&nav(current)),
        title,
        html! { main.wrap { (body) } },
    )
}

// --------------------------------------------------------------- overview

async fn overview(State(state): State<Arc<AppState>>) -> Markup {
    let o = state.store.overview().await;
    let (recent, _) = state
        .store
        .list(Some(Status::Pending), None, "", 0)
        .await
        .unwrap_or_default();

    shell(
        &state,
        "Overview",
        html! {
            h1 { (state.store.region.name) }
            p.lede {
                "Everything indexed sits inside the region bounding box "
                span.mono { (state.store.region.geo_filter()) } "."
            }

            .grid {
                @for index in CONTENT_INDEXES {
                    .card {
                        .k { (index) }
                        .n { (o.documents.get(index).copied().unwrap_or(0)) }
                        .sub {
                            @match o.freshest.get(index).copied().flatten() {
                                Some(t) => { "newest " (views::ago(t)) }
                                None => { "nothing indexed yet" }
                            }
                        }
                    }
                }
                .card {
                    .k { "pending review" }
                    a.n href="/submissions?status=pending" {
                        (o.submissions.get("pending").copied().unwrap_or(0))
                    }
                    .sub {
                        (o.submissions.get("approved").copied().unwrap_or(0)) " approved · "
                        (o.submissions.get("applied").copied().unwrap_or(0)) " applied"
                    }
                }
            }

            h2 { "Indexer" }
            (indexer_panel(o.bot.as_ref()))

            @if !recent.is_empty() {
                h2 { "Waiting for review" }
                (submission_table(&recent))
                p.small { a href="/submissions?status=pending" { "All pending →" } }
            }

            (categories_panel(&o))
        },
    )
}

fn indexer_panel(bot: Option<&BotHealth>) -> Markup {
    let Some(bot) = bot else {
        return html! {
            .note.warn {
                strong { "The indexer is not answering. " }
                "Nothing new will be indexed until it is back. Check "
                span.mono { "docker compose logs bot" }
                " or the bot VM, and confirm BOT_HEALTH_URL points at it."
            }
        };
    };

    html! {
        @if bot.status != "ok" {
            .note.warn {
                strong { "The indexer reports " (bot.status) ". " }
                "Meilisearch is " (bot.meilisearch) "."
            }
        }
        @if bot.sources.is_empty() {
            .note {
                "The indexer is up but no source has completed a run yet. "
                "Sources that are disabled never report."
            }
        } @else {
            table {
                thead {
                    tr {
                        th { "Source" }
                        th.nowrap { "Last run" }
                        th { "Runs" }
                        th { "Written" }
                        th { "Unchanged" }
                        th { "Rejected" }
                        th { "State" }
                    }
                }
                tbody {
                    @for (name, s) in &bot.sources {
                        tr {
                            td { strong { (name) } }
                            td.nowrap {
                                @match s.last_run_at {
                                    Some(t) => { (views::ago(t)) }
                                    None => { "—" }
                                }
                            }
                            td { (s.runs) }
                            td { (s.totals.written) }
                            td { (s.totals.unchanged) }
                            td {
                                // Out-of-region and malformed documents are
                                // dropped on purpose; a rising count here is
                                // usually a mapping bug, not a source problem.
                                (s.totals.out_of_region + s.totals.invalid)
                            }
                            td {
                                @if let Some(err) = &s.last_error {
                                    span."badge"."pending" { "failing" }
                                    div.small.muted { (truncate(err, 140)) }
                                } @else if s.consecutive_errors > 0 {
                                    span."badge"."pending" { (views::plural(s.consecutive_errors as usize, "error")) }
                                } @else {
                                    span."badge"."applied" { "ok" }
                                }
                            }
                        }
                    }
                }
            }
        }
        p.small.muted {
            @if let Some(n) = bot.crawl_frontier_pending { (views::plural(n, "URL")) " queued for crawling. " }
            @if let Some(t) = bot.started_at { "Up since " (views::ts(t)) ". " }
            @if let Some(v) = &bot.version { "v" (v) "." }
        }
    }
}

fn categories_panel(o: &Overview) -> Markup {
    html! {
        @if !o.top_categories.is_empty() {
            h2 { "Most common place categories" }
            p.small.muted {
                "These are the live values callers can filter on. "
                "A category that should exist and does not usually means a source mapping gap."
            }
            .chips {
                @for (name, n) in &o.top_categories {
                    a href={ "/browse?q=" (name) } { (name) " · " (n) }
                }
            }
        }
    }
}

// ------------------------------------------------------------ submissions

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    request: Option<String>,
    #[serde(default)]
    q: Option<String>,
    #[serde(default)]
    offset: Option<usize>,
}

async fn submissions(State(state): State<Arc<AppState>>, Query(query): Query<ListQuery>) -> Markup {
    let status = query.status.as_deref().and_then(Status::parse);
    let request = query.request.as_deref().and_then(Request::parse);
    let q = query.q.clone().unwrap_or_default();
    let offset = query.offset.unwrap_or(0);

    let counts = state.store.submission_counts().await;
    let (rows, total) = state
        .store
        .list(status, request, &q, offset)
        .await
        .unwrap_or_else(|e| {
            tracing::error!(error = %e, "listing submissions failed");
            (Vec::new(), 0)
        });

    let link = |s: Option<Status>| match s {
        Some(s) => format!("/submissions?status={}", s.as_str()),
        None => "/submissions".to_string(),
    };

    shell(
        &state,
        "Submissions",
        html! {
            h1 { "Submissions" }
            p.lede { "Requests from the public form. Nothing reaches the index until it is approved here." }

            .chips {
                @if status.is_none() {
                    a href=(link(None)) aria-current="page" { "All" }
                } @else {
                    a href=(link(None)) { "All" }
                }
                @for s in Status::ALL {
                    @let n = counts.get(s.as_str()).copied().unwrap_or(0);
                    @if status == Some(s) {
                        a href=(link(Some(s))) aria-current="page" { (s.as_str()) " · " (n) }
                    } @else {
                        a href=(link(Some(s))) { (s.as_str()) " · " (n) }
                    }
                }
            }

            form method="get" action="/submissions" style="margin-bottom:18px" {
                @if let Some(s) = status { input type="hidden" name="status" value=(s.as_str()); }
                .actions {
                    input type="text" name="q" value=(q) placeholder="Search submissions…"
                        style="max-width:24rem";
                    button.secondary type="submit" { "Search" }
                }
            }

            @if rows.is_empty() {
                .note {
                    @if q.is_empty() { "Nothing here." }
                    @else { "Nothing matched " span.mono { (q) } "." }
                }
            } @else {
                (submission_table(&rows))
                (pager(&query, offset, rows.len(), total))
            }
        },
    )
}

fn pager(q: &ListQuery, offset: usize, shown: usize, total: usize) -> Markup {
    let base = |off: usize| {
        let mut s = format!("/submissions?offset={off}");
        if let Some(v) = &q.status {
            s.push_str(&format!("&status={v}"));
        }
        if let Some(v) = &q.request {
            s.push_str(&format!("&request={v}"));
        }
        if let Some(v) = &q.q {
            s.push_str(&format!("&q={v}"));
        }
        s
    };
    html! {
        p.small.muted style="margin-top:14px" {
            "Showing " (offset + 1) "–" (offset + shown) " of about " (total) ". "
            @if offset > 0 {
                a href=(base(offset.saturating_sub(PAGE_SIZE))) { "← previous" }
                " "
            }
            @if offset + shown < total {
                a href=(base(offset + PAGE_SIZE)) { "next →" }
            }
        }
    }
}

fn submission_table(rows: &[Submission]) -> Markup {
    html! {
        table {
            thead {
                tr {
                    th { "What" }
                    th { "Request" }
                    th { "Where" }
                    th.nowrap { "Received" }
                    th { "Status" }
                }
            }
            tbody {
                @for s in rows {
                    tr {
                        td {
                            a href={ "/submissions/" (s.id) } { strong { (s.title) } }
                            div.small.muted { (truncate(&s.description, 110)) }
                        }
                        td { (request_badge(s.request)) " " span.small.muted { (s.target_kind) } }
                        td.small {
                            @match &s.city {
                                Some(c) => { (c) }
                                None => { span.muted { "—" } }
                            }
                        }
                        td.nowrap.small { (views::ago(s.submitted_at)) }
                        td { (status_badge(s.status)) }
                    }
                }
            }
        }
    }
}

async fn submission_detail(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    let Some(s) = state.store.get(&id).await else {
        return (
            StatusCode::NOT_FOUND,
            shell(
                &state,
                "Submissions",
                html! {
                    h1 { "No such submission" }
                    p { a.btn.secondary href="/submissions" { "Back to the queue" } }
                },
            ),
        )
            .into_response();
    };

    // If the submitter gave an id, say whether it actually resolves before
    // the reviewer goes looking for it.
    let existing = match (&s.existing_id, Kind::parse(&s.target_kind)) {
        (Some(id), Some(kind)) => Some((id.clone(), state.store.document_exists(kind, id).await)),
        _ => None,
    };

    let body = html! {
        p.small { a href="/submissions" { "← Queue" } }
        h1 { (s.title) }
        p.lede { (status_badge(s.status)) " " (request_badge(s.request)) " " span.muted { (s.target_kind) } }

        @if let Some(note) = &s.apply_note {
            .note.warn {
                strong { "The indexer could not apply this. " } (note)
            }
        }

        h2 { "What they said" }
        .card { p style="white-space:pre-wrap;margin:0" { (s.description) } }

        h2 { "Details" }
        dl.facts {
            dt { "Reference" }  dd.mono { (s.id) }
            dt { "Received" }   dd { (views::ts(s.submitted_at)) " (" (views::ago(s.submitted_at)) ")" }
            @if let Some(u) = &s.url {
                dt { "Website" } dd { a href=(u) rel="nofollow noopener noreferrer" target="_blank" { (u) } }
            }
            @if let Some(a) = &s.address { dt { "Address" } dd { (a) } }
            @if let Some(c) = &s.city { dt { "Town" } dd { (c) } }
            @if s.has_coords() {
                dt { "Coordinates" }
                dd.mono { (s.lat.unwrap_or_default()) ", " (s.lng.unwrap_or_default()) }
            } @else {
                dt { "Coordinates" }
                dd.muted {
                    "none given — "
                    @if s.url.is_some() { "the crawler will try the website" }
                    @else if s.address.is_some() { "will be geocoded from the address" }
                    @else if s.city.is_some() { "will fall back to the town centre" }
                    @else { "there is nothing to place this by, so it cannot be indexed" }
                }
            }
            @if !s.categories.is_empty() {
                dt { "Categories" } dd { (s.categories.join(", ")) }
            }
            @if let Some((id, exists)) = &existing {
                dt { "Existing entry" }
                dd {
                    span.mono { (id) } " "
                    @if *exists { span."badge"."applied" { "found" } }
                    @else { span."badge"."pending" { "no such document" } }
                }
            }
            @if let Some(c) = &s.contact {
                dt { "Contact" }
                dd { (c) " " span.small.muted { "(private — never published)" } }
            }
            @if let Some(t) = s.reviewed_at { dt { "Reviewed" } dd { (views::ts(t)) } }
            @if let Some(n) = &s.review_note { dt { "Review note" } dd { (n) } }
        }

        @if s.request.needs_existing() && s.existing_id.is_none() {
            .note.warn {
                "No entry id was given. Find the document in "
                a href={ "/browse?q=" (s.title) } { "the index browser" }
                " to confirm what this refers to."
            }
        }

        h2 { "Decide" }
        p.small.muted {
            "Approving queues it for the indexer, which acts on it within one "
            "cycle of the submissions source. The note is shown to the submitter."
        }
        form.stack method="post" action={ "/submissions/" (s.id) "/review" } {
            div {
                label for="note" { "Note to the submitter " span.opt { "— optional" } }
                textarea id="note" name="note" style="min-height:70px"
                    placeholder="Added, thanks. / We couldn't confirm this one." {
                    @if let Some(n) = &s.review_note { (n) }
                }
            }
            .actions {
                button type="submit" name="decision" value="approve" { "Approve" }
                button.secondary type="submit" name="decision" value="reject" { "Reject" }
                @if !matches!(s.status, Status::Pending) {
                    button.secondary type="submit" name="decision" value="pending" { "Back to pending" }
                }
            }
        }
    };
    shell_titled(&state, "Submissions", &s.title, body).into_response()
}

#[derive(Debug, Deserialize)]
pub struct ReviewForm {
    decision: String,
    #[serde(default)]
    note: String,
}

async fn review(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Form(form): Form<ReviewForm>,
) -> Response {
    let status = match form.decision.as_str() {
        "approve" => Status::Approved,
        "reject" => Status::Rejected,
        "pending" => Status::Pending,
        other => {
            tracing::warn!(decision = other, "unknown review decision");
            return (StatusCode::BAD_REQUEST, "unknown decision").into_response();
        }
    };

    match state.store.review(&id, status, Some(form.note)).await {
        Ok(s) => {
            tracing::info!(id = %s.id, status = %s.status, "submission reviewed");
            Redirect::to(&format!("/submissions/{id}")).into_response()
        }
        Err(e) => {
            tracing::error!(error = %e, id = %id, "review failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("could not save: {e}"),
            )
                .into_response()
        }
    }
}

// ----------------------------------------------------------------- browse

#[derive(Debug, Deserialize)]
pub struct BrowseQuery {
    #[serde(default)]
    q: Option<String>,
    #[serde(default)]
    kind: Option<String>,
}

async fn browse(State(state): State<Arc<AppState>>, Query(query): Query<BrowseQuery>) -> Markup {
    let q = query.q.clone().unwrap_or_default();
    let kind = query.kind.as_deref().and_then(Kind::parse);
    let hits = if q.is_empty() {
        Vec::new()
    } else {
        state.store.browse(kind, &q).await.unwrap_or_default()
    };

    shell(
        &state,
        "Browse index",
        html! {
            h1 { "Browse the index" }
            p.lede {
                "Find a document and copy its id, which is what an edit request "
                "needs to point at."
            }

            form method="get" action="/browse" style="margin-bottom:18px" {
                .actions {
                    input type="text" name="q" value=(q) placeholder="Search everything indexed…"
                        style="max-width:26rem" autofocus;
                    select name="kind" style="max-width:10rem" {
                        option value="" selected[kind.is_none()] { "All kinds" }
                        @for k in Kind::ALL {
                            option value=(k.as_str()) selected[kind == Some(k)] { (k.as_str()) }
                        }
                    }
                    button.secondary type="submit" { "Search" }
                }
            }

            @if q.is_empty() {
                .note { "Type something to search." }
            } @else if hits.is_empty() {
                .note { "Nothing matched " span.mono { (q) } "." }
            } @else {
                table {
                    thead {
                        tr { th { "Title" } th { "Kind" } th { "Where" } th { "Source" } th { "Id" } }
                    }
                    tbody {
                        @for h in &hits {
                            tr {
                                td {
                                    @match &h.url {
                                        Some(u) => {
                                            a href=(u) rel="nofollow noopener noreferrer" target="_blank" { (h.title) }
                                        }
                                        None => { (h.title) }
                                    }
                                    @if !h.categories.is_empty() {
                                        div.small.muted { (h.categories.join(", ")) }
                                    }
                                }
                                td.small { (h.kind) }
                                td.small { (h.city.clone().unwrap_or_else(|| "—".into())) }
                                td.small { (h.source) }
                                td.mono { (h.id) }
                            }
                        }
                    }
                }
            }
        },
    )
}

// ------------------------------------------------------------- mcp tokens

/// Longest a token name may be; it is a label, not a description.
const MAX_TOKEN_NAME: usize = 120;

#[derive(Debug, Deserialize)]
pub struct MintForm {
    name: String,
    /// Days until expiry; blank or 0 means it never expires.
    #[serde(default)]
    expires_days: String,
}

async fn tokens(State(state): State<Arc<AppState>>) -> Markup {
    tokens_page(&state, None, None).await
}

async fn mint_token(State(state): State<Arc<AppState>>, Form(form): Form<MintForm>) -> Response {
    let name: String = form.name.trim().chars().take(MAX_TOKEN_NAME).collect();
    if name.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            tokens_page(&state, None, Some("Give the token a name, so you know whose it is later.")).await,
        )
            .into_response();
    }
    let expires_at = match form.expires_days.trim() {
        "" | "0" => None,
        d => match d.parse::<u32>() {
            Ok(days) => Some(now_ts() + i64::from(days) * 86_400),
            Err(_) => {
                return (
                    StatusCode::BAD_REQUEST,
                    tokens_page(&state, None, Some("Expiry must be a whole number of days.")).await,
                )
                    .into_response();
            }
        },
    };

    match state.store.mint_token(name.clone(), expires_at).await {
        Ok(token) => {
            tracing::info!(name = %name, ?expires_at, "minted an MCP token");
            // Rendered straight back rather than redirected: the token is
            // shown this once and must not end up in a URL or a cache.
            (
                [(header::CACHE_CONTROL, "no-store")],
                tokens_page(&state, Some((&name, &token)), None).await,
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!(error = %e, "minting an MCP token failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                tokens_page(&state, None, Some(&format!("Could not mint the token: {e}"))).await,
            )
                .into_response()
        }
    }
}

async fn revoke_token(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    match state.store.revoke_token(&id).await {
        Ok(t) => {
            tracing::info!(name = %t.name, hint = %t.hint, "revoked an MCP token");
            Redirect::to("/tokens").into_response()
        }
        Err(e) => {
            tracing::error!(error = %e, id = %id, "revoking an MCP token failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("could not revoke: {e}"),
            )
                .into_response()
        }
    }
}

async fn tokens_page(state: &AppState, minted: Option<(&str, &str)>, error: Option<&str>) -> Markup {
    let rows = state.store.tokens().await.unwrap_or_else(|e| {
        tracing::error!(error = %e, "listing MCP tokens failed");
        Vec::new()
    });
    let now = now_ts();

    shell(
        state,
        "MCP tokens",
        html! {
            h1 { "MCP tokens" }
            p.lede {
                "Bearer tokens for the MCP endpoint, one per person, so access can be "
                "handed out and taken back individually. The shared "
                span.mono { "MCP_AUTH_TOKEN" } " keeps working alongside these."
            }

            @if let Some((name, token)) = minted {
                .note {
                    strong { "Token for " (name) ". " }
                    "Copy it now — only its hash is stored, so it cannot be shown again."
                    pre.mono style="white-space:pre-wrap;word-break:break-all;margin:10px 0 6px" { (token) }
                    div.small { "They send it as " span.mono { "Authorization: Bearer " (token) } }
                }
            }
            @if let Some(err) = error {
                .note.err { (err) }
            }

            h2 { "Mint a token" }
            form.stack method="post" action="/tokens" {
                div {
                    label for="name" { "Who is it for" }
                    input type="text" id="name" name="name" required maxlength=(MAX_TOKEN_NAME)
                        placeholder="Jane Doe — trail guide app";
                }
                div {
                    label for="expires_days" { "Expires after " span.opt { "— days, blank for never" } }
                    input type="number" id="expires_days" name="expires_days" min="0"
                        style="max-width:10rem";
                }
                .actions { button type="submit" { "Mint token" } }
            }

            h2 { "Issued" }
            @if rows.is_empty() {
                .note { "No tokens minted yet." }
            } @else {
                (token_table(&rows, now))
            }
            p.small.muted {
                "The MCP server caches a valid token for up to a minute, so a "
                "revocation can take that long to bite."
            }
        },
    )
}

fn token_table(rows: &[McpToken], now: i64) -> Markup {
    html! {
        table {
            thead {
                tr {
                    th { "Name" }
                    th { "Token" }
                    th.nowrap { "Created" }
                    th.nowrap { "Expires" }
                    th { "State" }
                    th {}
                }
            }
            tbody {
                @for t in rows {
                    tr {
                        td { strong { (t.name) } }
                        td.mono { (t.hint) "…" }
                        td.nowrap.small { (views::ts(t.created_at)) }
                        td.nowrap.small { (views::opt_ts(t.expires_at)) }
                        td {
                            @if let Some(r) = t.revoked_at {
                                span."badge"."rejected" { "revoked" }
                                div.small.muted { (views::ago(r)) }
                            } @else if !t.is_active(now) {
                                span."badge"."pending" { "expired" }
                            } @else {
                                span."badge"."applied" { "active" }
                            }
                        }
                        td {
                            @if t.revoked_at.is_none() {
                                form method="post" action={ "/tokens/" (t.id) "/revoke" }
                                    onsubmit="return confirm('Revoke this token? Whoever holds it loses access.')" {
                                    button.secondary type="submit" { "Revoke" }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_is_char_safe_and_marks_the_cut() {
        assert_eq!(truncate("  short  ", 20), "short");
        let out = truncate(&"é".repeat(50), 10);
        assert_eq!(out.chars().count(), 11);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn every_nav_entry_points_at_a_real_route() {
        // The route table and the nav are edited separately; keep them honest.
        let routes = ["/", "/submissions", "/browse", "/tokens"];
        for (href, _) in nav("Overview").items {
            assert!(routes.contains(&href), "{href} is not a route");
        }
    }
}
