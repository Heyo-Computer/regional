//! Indexer configuration, all from the environment.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use url::Url;

/// A crawl seed, optionally pinned to a town.
///
/// `CRAWL_SEEDS` accepts `https://site.example|Denver`: pages from that site
/// that carry no coordinates of their own are attributed to that town
/// (recorded as `geo_precision: city`). Without it, a page we cannot locate
/// is dropped rather than indexed at an invented position.
#[derive(Debug, Clone)]
pub struct Seed {
    pub url: Url,
    pub default_city: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub bind: SocketAddr,
    pub user_agent: String,
    /// Run one cycle of every source and exit. Used by CI and by the
    /// verification steps in the README.
    pub once: bool,
    pub intervals: HashMap<String, Duration>,
    pub disabled_sources: Vec<String>,
    pub crawl_seeds: Vec<Seed>,
    pub crawl_max_depth: u32,
    pub crawl_pages_per_run: usize,
    pub overpass_url: String,
    /// Target size of one Overpass tile, km per side. The tile count follows
    /// from the region's area; lower this if queries start timing out.
    pub overpass_tile_km: f64,
    pub nominatim_url: String,
    pub wikipedia_api: String,
    pub wikidata_api: String,
    pub wikipedia_points_per_run: usize,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let once = std::env::args().any(|a| a == "--once")
            || matches!(
                std::env::var("BOT_RUN_ONCE").as_deref(),
                Ok("1") | Ok("true")
            );

        let port: u16 = match std::env::var("PORT") {
            Ok(p) => p
                .parse()
                .with_context(|| format!("PORT={p:?} is not a port number"))?,
            Err(_) => 8081,
        };
        let host = std::env::var("BIND_HOST").unwrap_or_else(|_| "0.0.0.0".into());
        let bind: SocketAddr = format!("{host}:{port}").parse()?;

        // OpenStreetMap, Wikimedia and Nominatim all require a real contact
        // in the User-Agent and will block anonymous bulk traffic. Refusing
        // to start is friendlier than getting quietly banned an hour in.
        let contact_email = std::env::var("CONTACT_EMAIL")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty() && s.contains('@'))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "CONTACT_EMAIL must be set to a real address you monitor. OpenStreetMap, \
                     Wikimedia and Nominatim require it in the User-Agent and will block \
                     traffic without it."
                )
            })?;
        let user_agent = format!(
            "regional-indexer/{} (+{contact_email})",
            env!("CARGO_PKG_VERSION")
        );

        let intervals = parse_intervals(std::env::var("SOURCE_INTERVALS").ok().as_deref())?;
        let disabled_sources = split_csv(std::env::var("DISABLED_SOURCES").ok().as_deref());

        let crawl_seeds = parse_seeds(std::env::var("CRAWL_SEEDS").ok().as_deref())?;

        Ok(Self {
            bind,
            user_agent,
            once,
            intervals,
            disabled_sources,
            crawl_seeds,
            crawl_max_depth: env_num("CRAWL_MAX_DEPTH", 3)?,
            crawl_pages_per_run: env_num("CRAWL_PAGES_PER_RUN", 40)?,
            overpass_url: std::env::var("OVERPASS_URL")
                .unwrap_or_else(|_| "https://overpass-api.de/api/interpreter".into()),
            overpass_tile_km: env_num(
                "OVERPASS_TILE_KM",
                crate::sources::overpass::DEFAULT_TILE_KM,
            )?,
            nominatim_url: std::env::var("NOMINATIM_URL")
                .unwrap_or_else(|_| "https://nominatim.openstreetmap.org".into()),
            wikipedia_api: std::env::var("WIKIPEDIA_API")
                .unwrap_or_else(|_| "https://en.wikipedia.org/w/api.php".into()),
            wikidata_api: std::env::var("WIKIDATA_API")
                .unwrap_or_else(|_| "https://www.wikidata.org/w/api.php".into()),
            wikipedia_points_per_run: env_num("WIKIPEDIA_POINTS_PER_RUN", 25)?,
        })
    }

    pub fn interval_for(&self, source: &str, default: Duration) -> Duration {
        self.intervals.get(source).copied().unwrap_or(default)
    }

    pub fn is_enabled(&self, source: &str) -> bool {
        !self.disabled_sources.iter().any(|d| d == source)
    }
}

fn env_num<T: std::str::FromStr>(key: &str, default: T) -> Result<T>
where
    T::Err: std::fmt::Display,
{
    match std::env::var(key) {
        Ok(v) => v
            .trim()
            .parse::<T>()
            .map_err(|e| anyhow::anyhow!("{key}={v:?} is not a number: {e}")),
        Err(_) => Ok(default),
    }
}

fn split_csv(raw: Option<&str>) -> Vec<String> {
    raw.unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Parse `overpass=6h,wikipedia=30m,crawl=15m`.
fn parse_intervals(raw: Option<&str>) -> Result<HashMap<String, Duration>> {
    let mut out = HashMap::new();
    for entry in split_csv(raw) {
        let (name, value) = entry
            .split_once('=')
            .with_context(|| format!("SOURCE_INTERVALS entry {entry:?} is not name=duration"))?;
        out.insert(name.trim().to_string(), parse_duration(value.trim())?);
    }
    Ok(out)
}

/// Parse `45s`, `30m`, `6h`, `2d`. A bare number is seconds.
pub fn parse_duration(raw: &str) -> Result<Duration> {
    let raw = raw.trim();
    if raw.is_empty() {
        bail!("empty duration");
    }
    let (digits, unit) = raw.split_at(raw.find(|c: char| !c.is_ascii_digit()).unwrap_or(raw.len()));
    let n: u64 = digits
        .parse()
        .with_context(|| format!("{raw:?} does not start with a number"))?;
    let secs = match unit.trim() {
        "" | "s" | "sec" | "secs" => n,
        "m" | "min" | "mins" => n * 60,
        "h" | "hr" | "hrs" => n * 3600,
        "d" | "day" | "days" => n * 86400,
        other => bail!("unknown duration unit {other:?} in {raw:?}; use s, m, h or d"),
    };
    if secs == 0 {
        bail!("duration {raw:?} is zero, which would spin the scheduler");
    }
    Ok(Duration::from_secs(secs))
}

fn parse_seeds(raw: Option<&str>) -> Result<Vec<Seed>> {
    let mut out = Vec::new();
    for entry in split_csv(raw) {
        let (url_part, city) = match entry.split_once('|') {
            Some((u, c)) => (
                u.trim(),
                Some(c.trim().to_string()).filter(|c| !c.is_empty()),
            ),
            None => (entry.as_str(), None),
        };
        let url = Url::parse(url_part)
            .with_context(|| format!("CRAWL_SEEDS entry {url_part:?} is not a valid URL"))?;
        if !matches!(url.scheme(), "http" | "https") {
            bail!("CRAWL_SEEDS entry {url_part:?} must be http or https");
        }
        out.push(Seed {
            url,
            default_city: city,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_parse_with_and_without_units() {
        assert_eq!(parse_duration("45").unwrap(), Duration::from_secs(45));
        assert_eq!(parse_duration("30m").unwrap(), Duration::from_secs(1800));
        assert_eq!(parse_duration("6h").unwrap(), Duration::from_secs(21600));
        assert_eq!(parse_duration("2d").unwrap(), Duration::from_secs(172800));
        assert!(parse_duration("0m").is_err(), "a zero interval would spin");
        assert!(parse_duration("6 weeks").is_err());
        assert!(parse_duration("").is_err());
    }

    #[test]
    fn intervals_parse_as_a_map() {
        let m = parse_intervals(Some("overpass=6h, crawl=15m")).unwrap();
        assert_eq!(m["overpass"], Duration::from_secs(21600));
        assert_eq!(m["crawl"], Duration::from_secs(900));
        assert!(parse_intervals(Some("bogus")).is_err());
    }

    #[test]
    fn seeds_carry_an_optional_default_city() {
        let s = parse_seeds(Some("https://a.test/news|Denver, https://b.test")).unwrap();
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].default_city.as_deref(), Some("Denver"));
        assert_eq!(s[1].default_city, None);
        assert!(parse_seeds(Some("ftp://a.test")).is_err());
        assert!(parse_seeds(Some("not a url")).is_err());
    }
}
