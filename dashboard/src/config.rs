//! Dashboard configuration.
//!
//! There is deliberately no authentication here. The two surfaces are split
//! across two ports so the load balancer can protect one and leave the other
//! open: the operator views on `ADMIN_PORT`, the public request form on
//! `PUBLIC_PORT`. Anything that changes state beyond filing a new request
//! lives on the admin port only.

use std::net::SocketAddr;

use anyhow::{Context, Result};

#[derive(Debug, Clone)]
pub struct Config {
    pub admin_bind: SocketAddr,
    pub public_bind: SocketAddr,
    /// Where to read the indexer's own health and per-source metrics.
    pub bot_health_url: String,
    /// Shown on the public page so people know what region they are
    /// submitting to; falls back to the region config's name.
    pub public_title: Option<String>,
    /// Optional contact shown on the public page.
    pub public_contact: Option<String>,
    /// Requests allowed per IP per hour on the public form.
    pub submit_per_hour: u32,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let host = std::env::var("BIND_HOST").unwrap_or_else(|_| "0.0.0.0".into());
        let admin_port = env_num("ADMIN_PORT", 8090)?;
        let public_port = env_num("PUBLIC_PORT", 8091)?;
        anyhow::ensure!(
            admin_port != public_port,
            "ADMIN_PORT and PUBLIC_PORT must differ — the split is what lets \
             the load balancer protect the admin views while leaving the \
             request form open"
        );

        Ok(Self {
            admin_bind: format!("{host}:{admin_port}")
                .parse()
                .with_context(|| format!("cannot parse {host}:{admin_port}"))?,
            public_bind: format!("{host}:{public_port}")
                .parse()
                .with_context(|| format!("cannot parse {host}:{public_port}"))?,
            bot_health_url: std::env::var("BOT_HEALTH_URL")
                .unwrap_or_else(|_| "http://bot:8081/healthz".into()),
            public_title: std::env::var("PUBLIC_TITLE").ok().filter(|s| !s.is_empty()),
            public_contact: std::env::var("PUBLIC_CONTACT")
                .ok()
                .filter(|s| !s.is_empty()),
            submit_per_hour: env_num("SUBMIT_PER_HOUR", 10)?,
        })
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
