//! Abuse controls for the public form.
//!
//! This is not authentication — the load balancer owns that — it is the
//! minimum an open, unauthenticated POST endpoint needs so that one script
//! cannot fill the review queue.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use axum::http::HeaderMap;
use tokio::sync::Mutex;

const WINDOW: Duration = Duration::from_secs(3_600);

pub struct RateLimiter {
    per_window: u32,
    hits: Mutex<HashMap<String, Vec<Instant>>>,
}

impl RateLimiter {
    pub fn new(per_window: u32) -> Self {
        Self {
            per_window: per_window.max(1),
            hits: Mutex::new(HashMap::new()),
        }
    }

    /// Record an attempt. `false` means the caller is over its budget.
    pub async fn check(&self, key: &str) -> bool {
        let now = Instant::now();
        let mut map = self.hits.lock().await;

        // Drop keys that have gone quiet, so the map cannot grow without
        // bound across a long-running deployment.
        map.retain(|_, times| {
            times.retain(|t| now.duration_since(*t) < WINDOW);
            !times.is_empty()
        });

        let entry = map.entry(key.to_string()).or_default();
        if entry.len() >= self.per_window as usize {
            return false;
        }
        entry.push(now);
        true
    }
}

/// The caller's address, as best we can tell.
///
/// Behind the load balancer the socket address is the balancer's, so the
/// forwarded header is the useful one. It is caller-controlled and therefore
/// spoofable — which is fine here, because this limits accidental floods and
/// casual abuse, not a determined attacker.
pub fn client_key(headers: &HeaderMap, peer: Option<SocketAddr>) -> String {
    for name in ["x-forwarded-for", "x-real-ip"] {
        if let Some(v) = headers.get(name).and_then(|v| v.to_str().ok()) {
            // X-Forwarded-For is a chain; the original client is first.
            if let Some(first) = v.split(',').next() {
                let first = first.trim();
                if !first.is_empty() {
                    return first.to_string();
                }
            }
        }
    }
    peer.map(|p| p.ip().to_string())
        .unwrap_or_else(|| "unknown".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[tokio::test]
    async fn the_budget_is_per_key_and_runs_out() {
        let rl = RateLimiter::new(2);
        assert!(rl.check("a").await);
        assert!(rl.check("a").await);
        assert!(!rl.check("a").await, "third attempt must be refused");
        assert!(rl.check("b").await, "a different caller is unaffected");
    }

    #[tokio::test]
    async fn a_zero_budget_still_allows_one() {
        // Misconfiguring SUBMIT_PER_HOUR=0 should not silently close the form.
        let rl = RateLimiter::new(0);
        assert!(rl.check("a").await);
        assert!(!rl.check("a").await);
    }

    #[test]
    fn the_forwarded_client_wins_over_the_socket() {
        let mut h = HeaderMap::new();
        let peer: SocketAddr = "10.0.0.1:5555".parse().unwrap();
        assert_eq!(client_key(&h, Some(peer)), "10.0.0.1");

        h.insert(
            "x-forwarded-for",
            HeaderValue::from_static("203.0.113.9, 10.0.0.1"),
        );
        assert_eq!(client_key(&h, Some(peer)), "203.0.113.9");

        h.insert("x-forwarded-for", HeaderValue::from_static("  "));
        assert_eq!(
            client_key(&h, Some(peer)),
            "10.0.0.1",
            "a blank header falls through"
        );
        assert_eq!(client_key(&HeaderMap::new(), None), "unknown");
    }
}
