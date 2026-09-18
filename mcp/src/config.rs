//! Runtime configuration, all from the environment.
//!
//! heyo passes the listen port on the command line to the VM and injects
//! secrets from HeyoSecret, so nothing here is ever baked into an image.

use std::net::SocketAddr;

use anyhow::{Context, Result, bail};

#[derive(Debug, Clone)]
pub struct Config {
    pub bind: SocketAddr,
    /// Shared secret required on `Authorization: Bearer`. `None` only when
    /// the operator explicitly opted out.
    pub auth_token: Option<String>,
    pub allow_anonymous: bool,
    /// Hostnames the Streamable HTTP transport will answer on.
    ///
    /// rmcp defends against DNS rebinding by refusing any `Host` it does not
    /// recognise, and its default list is loopback only — right for a server
    /// on a laptop, fatal behind a load balancer, which forwards the public
    /// hostname and gets `Forbidden: Host header is not allowed`. Empty
    /// means "keep rmcp's default"; `*` disables the check entirely.
    pub allowed_hosts: Vec<String>,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let port: u16 = match std::env::var("PORT") {
            Ok(p) => p
                .parse()
                .with_context(|| format!("PORT={p:?} is not a port number"))?,
            Err(_) => 8080,
        };
        let host = std::env::var("BIND_HOST").unwrap_or_else(|_| "0.0.0.0".into());
        let bind: SocketAddr = format!("{host}:{port}")
            .parse()
            .with_context(|| format!("cannot parse bind address {host}:{port}"))?;

        let auth_token = std::env::var("MCP_AUTH_TOKEN")
            .ok()
            .filter(|t| !t.is_empty());
        let allow_anonymous = matches!(
            std::env::var("MCP_ALLOW_ANONYMOUS").as_deref(),
            Ok("1") | Ok("true") | Ok("yes")
        );

        // Fail closed. This endpoint is reachable from the internet once
        // it is deployed, so an unset token must stop the server rather
        // than quietly serve the whole index to anyone who finds it.
        if auth_token.is_none() && !allow_anonymous {
            bail!(
                "MCP_AUTH_TOKEN is not set. Set it to a shared secret, or set \
                 MCP_ALLOW_ANONYMOUS=1 to deliberately serve this endpoint without auth."
            );
        }
        if auth_token.is_some() && allow_anonymous {
            tracing::warn!("MCP_ALLOW_ANONYMOUS is set, so MCP_AUTH_TOKEN will not be enforced");
        }

        // Comma-separated, `host` or `host:port`, e.g.
        // `MCP_ALLOWED_HOSTS=regional-mcp.us2.heyo.work`.
        let allowed_hosts: Vec<String> = std::env::var("MCP_ALLOWED_HOSTS")
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();

        Ok(Self {
            bind,
            auth_token,
            allow_anonymous,
            allowed_hosts,
        })
    }

    pub fn requires_auth(&self) -> bool {
        !self.allow_anonymous && self.auth_token.is_some()
    }
}
