//! One polite HTTP client for every outbound request.
//!
//! Everything the indexer fetches goes through here so that rate limiting,
//! retries and robots.txt are enforced in one place rather than per source.
//! The sources we pull from are free, volunteer-run services; getting this
//! wrong gets the deployment blocked, not just slowed.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use rand::RngExt;
use reqwest::{Response, StatusCode};
use texting_robots::Robot;
use tokio::sync::Mutex;
use tokio::time::Instant;
use url::Url;

/// Minimum gap between requests to the same host, unless overridden.
const DEFAULT_HOST_INTERVAL: Duration = Duration::from_millis(1_000);
const MAX_ATTEMPTS: u32 = 4;
/// Cap on a single response body, so one enormous page cannot exhaust memory.
const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

pub struct Fetcher {
    client: reqwest::Client,
    user_agent: String,
    /// Next time each host may be contacted.
    next_allowed: Mutex<HashMap<String, Instant>>,
    /// Per-host overrides, e.g. Nominatim's hard 1 req/s policy.
    host_intervals: HashMap<String, Duration>,
    robots: Mutex<HashMap<String, Option<Robot>>>,
}

impl Fetcher {
    pub fn new(user_agent: &str) -> Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent(user_agent)
            .timeout(Duration::from_secs(60))
            .connect_timeout(Duration::from_secs(15))
            .gzip(true)
            .build()
            .context("building the HTTP client")?;
        Ok(Self {
            client,
            user_agent: user_agent.to_string(),
            next_allowed: Mutex::new(HashMap::new()),
            host_intervals: HashMap::new(),
            robots: Mutex::new(HashMap::new()),
        })
    }

    /// Require a longer gap between requests to one host.
    pub fn with_host_interval(mut self, host: &str, interval: Duration) -> Self {
        self.host_intervals.insert(host.to_string(), interval);
        self
    }

    fn interval_for(&self, host: &str) -> Duration {
        self.host_intervals
            .get(host)
            .copied()
            .unwrap_or(DEFAULT_HOST_INTERVAL)
    }

    /// Block until this host may be contacted again.
    async fn throttle(&self, host: &str) {
        let wait = {
            let mut map = self.next_allowed.lock().await;
            let now = Instant::now();
            let slot = map.entry(host.to_string()).or_insert(now);
            let wait = slot.saturating_duration_since(now);
            // Reserve the next slot before releasing the lock, so concurrent
            // callers queue up rather than all racing through at once.
            *slot = (*slot).max(now) + self.interval_for(host);
            wait
        };
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
    }

    /// GET with throttling, retries and backoff.
    ///
    /// Retries 429 and 5xx (honouring `Retry-After`), and gives up on 4xx,
    /// which will not get better by asking again.
    pub async fn get(&self, url: &str) -> Result<Response> {
        let host = host_of(url)?;
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            self.throttle(&host).await;

            let result = self.client.get(url).send().await;
            let retry_after = match &result {
                Ok(r) if r.status().is_success() => return Ok(result.unwrap()),
                Ok(r) if should_retry(r.status()) => {
                    let hinted = parse_retry_after(r);
                    tracing::warn!(url, status = %r.status(), attempt, "retrying");
                    hinted
                }
                Ok(r) => {
                    let status = r.status();
                    bail!("GET {url} failed with {status}");
                }
                Err(e) => {
                    tracing::warn!(url, error = %e, attempt, "request error, retrying");
                    None
                }
            };

            if attempt >= MAX_ATTEMPTS {
                bail!("GET {url} still failing after {MAX_ATTEMPTS} attempts");
            }
            let backoff = retry_after.unwrap_or_else(|| {
                // Exponential with jitter, so a fleet of sources coming back
                // after an outage does not stampede in lockstep.
                let base = Duration::from_secs(2u64.pow(attempt));
                let jitter = rand::rng().random_range(0..500);
                base + Duration::from_millis(jitter)
            });
            tokio::time::sleep(backoff.min(Duration::from_secs(120))).await;
        }
    }

    pub async fn get_json<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T> {
        let text = self.get_text(url).await?;
        serde_json::from_str(&text)
            .with_context(|| format!("GET {url} did not return the expected JSON"))
    }

    /// POST a form body, which is how Overpass takes its query language.
    pub async fn post_form<T: serde::de::DeserializeOwned>(
        &self,
        url: &str,
        form: &[(&str, &str)],
    ) -> Result<T> {
        let host = host_of(url)?;
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            self.throttle(&host).await;

            let res = self.client.post(url).form(form).send().await;
            match res {
                Ok(r) if r.status().is_success() => {
                    let text = r.text().await.context("reading the response body")?;
                    return serde_json::from_str(&text)
                        .with_context(|| format!("POST {url} did not return the expected JSON"));
                }
                Ok(r) if should_retry(r.status()) => {
                    let hinted = parse_retry_after(&r);
                    tracing::warn!(url, status = %r.status(), attempt, "retrying POST");
                    if attempt >= MAX_ATTEMPTS {
                        bail!("POST {url} still failing after {MAX_ATTEMPTS} attempts");
                    }
                    let backoff = hinted.unwrap_or(Duration::from_secs(5 * attempt as u64));
                    tokio::time::sleep(backoff.min(Duration::from_secs(180))).await;
                }
                Ok(r) => bail!("POST {url} failed with {}", r.status()),
                Err(e) => {
                    if attempt >= MAX_ATTEMPTS {
                        return Err(e).with_context(|| format!("POST {url}"));
                    }
                    tracing::warn!(url, error = %e, attempt, "POST error, retrying");
                    tokio::time::sleep(Duration::from_secs(2u64.pow(attempt))).await;
                }
            }
        }
    }

    /// Fetch a body as text, refusing anything oversized or non-textual.
    pub async fn get_text(&self, url: &str) -> Result<String> {
        let res = self.get(url).await?;
        if let Some(len) = res.content_length()
            && len as usize > MAX_BODY_BYTES
        {
            bail!("GET {url} body is {len} bytes, over the {MAX_BODY_BYTES} limit");
        }
        let bytes = res.bytes().await.context("reading the response body")?;
        if bytes.len() > MAX_BODY_BYTES {
            bail!("GET {url} body exceeded the {MAX_BODY_BYTES} limit");
        }
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// Is this URL crawlable according to the host's robots.txt?
    ///
    /// A host whose robots.txt cannot be fetched is treated as permissive,
    /// matching the convention; a robots.txt that parses and disallows is
    /// always respected.
    pub async fn robots_allow(&self, url: &str) -> bool {
        let Ok(parsed) = Url::parse(url) else {
            return false;
        };
        if parsed.host_str().is_none() {
            return false;
        }
        // The origin, not just the host: robots.txt is per scheme+host+port,
        // and dropping the port asks the wrong server entirely.
        let key = parsed.origin().ascii_serialization();

        {
            let cache = self.robots.lock().await;
            if let Some(entry) = cache.get(&key) {
                return entry.as_ref().is_none_or(|r| r.allowed(url));
            }
        }

        let robots_url = format!("{key}/robots.txt");
        tracing::debug!(robots_url, "fetching robots.txt");
        let parsed_robot = match self.get_text(&robots_url).await {
            Ok(body) => Robot::new(&self.user_agent, body.as_bytes()).ok(),
            Err(e) => {
                tracing::debug!(robots_url, error = %e, "no usable robots.txt; treating as permissive");
                None
            }
        };
        let allowed = parsed_robot.as_ref().is_none_or(|r| r.allowed(url));
        self.robots.lock().await.insert(key, parsed_robot);
        allowed
    }
}

fn should_retry(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

/// Honour `Retry-After`, in seconds. The HTTP-date form is rare enough from
/// these APIs that falling back to our own backoff is fine.
fn parse_retry_after(res: &Response) -> Option<Duration> {
    res.headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
}

pub fn host_of(url: &str) -> Result<String> {
    Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .with_context(|| format!("{url:?} has no host"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retryable_statuses_are_transient_only() {
        assert!(should_retry(StatusCode::TOO_MANY_REQUESTS));
        assert!(should_retry(StatusCode::BAD_GATEWAY));
        assert!(should_retry(StatusCode::SERVICE_UNAVAILABLE));
        assert!(!should_retry(StatusCode::NOT_FOUND));
        assert!(!should_retry(StatusCode::FORBIDDEN));
        assert!(!should_retry(StatusCode::OK));
    }

    #[test]
    fn robots_are_scoped_to_the_full_origin() {
        // A non-default port is part of the origin; asking 127.0.0.1:80 for
        // the rules that govern 127.0.0.1:9932 is a different server.
        let origin = |u: &str| Url::parse(u).unwrap().origin().ascii_serialization();
        assert_eq!(origin("http://127.0.0.1:9932/a/b"), "http://127.0.0.1:9932");
        assert_eq!(origin("https://a.test/x"), "https://a.test");
        assert_ne!(origin("http://a.test/x"), origin("https://a.test/x"));
    }

    #[test]
    fn host_extraction() {
        assert_eq!(host_of("https://a.test/x?y=1").unwrap(), "a.test");
        assert!(host_of("not a url").is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn the_same_host_is_throttled_but_different_hosts_are_not() {
        let f = Fetcher::new("test/1.0").unwrap();
        let start = Instant::now();
        f.throttle("a.test").await;
        f.throttle("b.test").await;
        assert_eq!(
            start.elapsed(),
            Duration::ZERO,
            "different hosts must not queue"
        );
        f.throttle("a.test").await;
        assert!(
            start.elapsed() >= DEFAULT_HOST_INTERVAL,
            "a second hit on one host must wait"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn per_host_overrides_apply() {
        let f = Fetcher::new("test/1.0")
            .unwrap()
            .with_host_interval("slow.test", Duration::from_secs(5));
        let start = Instant::now();
        f.throttle("slow.test").await;
        f.throttle("slow.test").await;
        assert!(start.elapsed() >= Duration::from_secs(5));
    }
}
