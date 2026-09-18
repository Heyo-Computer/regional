//! Bearer-token gate for the MCP endpoint.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use subtle::ConstantTimeEq;

use crate::config::Config;

/// Reject anything under the MCP route without a valid bearer token.
///
/// `/healthz` is deliberately not behind this layer: heyo's `--health-path`
/// probe has no credentials.
pub async fn require_bearer(
    State(cfg): State<Arc<Config>>,
    req: Request<Body>,
    next: Next,
) -> Response {
    if !cfg.requires_auth() {
        return next.run(req).await;
    }
    let expected = cfg.auth_token.as_deref().unwrap_or_default();

    let presented = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .unwrap_or_default();

    if !token_matches(presented, expected) {
        tracing::warn!(path = %req.uri().path(), "rejected unauthenticated MCP request");
        return unauthorized();
    }
    next.run(req).await
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
