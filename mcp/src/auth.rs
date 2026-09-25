//! Bearer-token gate for the MCP endpoint.
//!
//! Two kinds of token get in: the shared `MCP_AUTH_TOKEN`, and per-user
//! tokens minted on the dashboard, which are checked against the hashes in
//! the `mcp_tokens` index.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use meilisearch_sdk::search::SearchQuery;
use regional_core::index::MCP_TOKENS;
use regional_core::meili::Client;
use regional_core::model::now_ts;
use regional_core::token::{self, McpToken};
use subtle::ConstantTimeEq;

use crate::config::Config;

/// How long a minted token that checked out is trusted without asking
/// Meilisearch again. This is also the longest a revocation takes to bite.
const CACHE_TTL: Duration = Duration::from_secs(60);

/// Past this many cached tokens the cache is simply dropped. Only valid
/// tokens are cached, so reaching it means a lot of real users, not an
/// attack.
const CACHE_MAX: usize = 10_000;

pub struct Auth {
    pub cfg: Arc<Config>,
    client: Client,
    /// Token hash → when it was last confirmed, and the record then.
    cache: Mutex<HashMap<String, (Instant, McpToken)>>,
}

impl Auth {
    pub fn new(cfg: Arc<Config>, client: Client) -> Self {
        Self {
            cfg,
            client,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Is this a live minted token? `Err` when the answer is unknowable
    /// because the lookup itself failed.
    async fn minted_token(&self, presented: &str) -> Result<Option<McpToken>, String> {
        if !token::looks_minted(presented) {
            return Ok(None);
        }
        let id = token::hash(presented);
        let now = now_ts();

        if let Some((at, rec)) = self.cache.lock().unwrap().get(&id)
            && at.elapsed() < CACHE_TTL
        {
            return Ok(rec.is_active(now).then(|| rec.clone()));
        }

        let idx = self.client.index(MCP_TOKENS);
        // `id` is a hex digest, so it is safe inside the filter expression.
        let filter = format!("id = \"{id}\"");
        let mut q = SearchQuery::new(&idx);
        q.with_query("").with_limit(1).with_filter(&filter);
        let rec = q
            .execute::<McpToken>()
            .await
            .map_err(|e| e.to_string())?
            .hits
            .into_iter()
            .next()
            .map(|h| h.result)
            .filter(|r| r.id == id && r.is_active(now));

        if let Some(rec) = &rec {
            let mut cache = self.cache.lock().unwrap();
            if cache.len() >= CACHE_MAX {
                cache.clear();
            }
            cache.insert(id, (Instant::now(), rec.clone()));
        }
        Ok(rec)
    }
}

/// Reject anything under the MCP route without a valid bearer token.
///
/// `/healthz` is deliberately not behind this layer: heyo's `--health-path`
/// probe has no credentials.
pub async fn require_bearer(
    State(auth): State<Arc<Auth>>,
    req: Request<Body>,
    next: Next,
) -> Response {
    if !auth.cfg.requires_auth() {
        return next.run(req).await;
    }
    let expected = auth.cfg.auth_token.as_deref().unwrap_or_default();

    let presented = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .unwrap_or_default();

    if token_matches(presented, expected) {
        return next.run(req).await;
    }
    match auth.minted_token(presented).await {
        Ok(Some(rec)) => {
            tracing::debug!(token = %rec.name, "MCP request on a minted token");
            next.run(req).await
        }
        Ok(None) => {
            tracing::warn!(path = %req.uri().path(), "rejected unauthenticated MCP request");
            unauthorized()
        }
        Err(e) => {
            // Most likely a search key created before the tokens index
            // existed; re-run deploy/create-search-key.sh.
            tracing::error!(error = %e, "could not check a minted MCP token");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "could not verify the token right now",
            )
                .into_response()
        }
    }
}

/// Constant-time comparison, so a caller cannot learn the token by timing
/// how long a wrong guess takes to be rejected.
fn token_matches(presented: &str, expected: &str) -> bool {
    if presented.is_empty() || expected.is_empty() {
        return false;
    }
    // `ct_eq` is only constant-time for equal-length inputs; the length
    // check leaks the token length, which is not a secret.
    presented.len() == expected.len() && presented.as_bytes().ct_eq(expected.as_bytes()).into()
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Bearer realm=\"regional-mcp\"")],
        "missing or invalid bearer token",
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_exact_token_matches() {
        assert!(token_matches("s3cret", "s3cret"));
        assert!(!token_matches("s3cret", "s3crey"));
        assert!(!token_matches("s3cre", "s3cret"));
        assert!(!token_matches("s3cretx", "s3cret"));
        assert!(!token_matches("", "s3cret"));
        assert!(!token_matches("s3cret", ""));
    }
}
