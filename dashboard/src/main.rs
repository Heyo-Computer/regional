//! Operator dashboard and public request page for the regional search stack.
//!
//! Two surfaces on two ports, and no authentication in either — the load
//! balancer in front of this VM owns access control. The split is the point:
//! put the admin port behind whatever the balancer offers and leave the
//! public port open, and the access policy is "which port".
//!
//!   ADMIN_PORT  (8090) — index health, indexer status, submission review
//!   PUBLIC_PORT (8091) — the request form, open to anyone

mod admin;
mod config;
mod public;
mod ratelimit;
mod state;
mod store;
mod views;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::Json;
use axum::Router;
use regional_core::index::ensure_indexes;
use regional_core::meili;
use regional_core::region::RegionConfig;
use serde_json::json;
use tower_http::trace::TraceLayer;

use crate::config::Config;
use crate::ratelimit::RateLimiter;
use crate::state::AppState;
use crate::store::Store;

const MEILI_BOOT_WAIT: Duration = Duration::from_secs(120);

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let config = Config::from_env()?;
    let region = RegionConfig::from_env().context("loading the region config")?;

    // The dashboard writes submissions, so it needs a key that can write to
    // the submissions index. It never writes to the content indexes.
    let client = meili::client_from_env("MEILI_MASTER_KEY")?;
    meili::wait_healthy(&client, MEILI_BOOT_WAIT).await?;
    if let Err(e) = ensure_indexes(&client, &region).await {
        tracing::warn!(error = %e, "could not apply index settings; continuing");
    }

    tracing::info!(
        region = %region.name,
        admin = %config.admin_bind,
        public = %config.public_bind,
        "starting the dashboard"
    );

    let state = Arc::new(AppState {
        store: Store::new(client, region, config.bot_health_url.clone()),
        limiter: RateLimiter::new(config.submit_per_hour),
        config,
    });

    let admin_addr = state.config.admin_bind;
    let public_addr = state.config.public_bind;

    let admin = serve("admin", admin_addr, admin::router(state.clone()));
    let public = serve("public", public_addr, public::router(state.clone()));

    // If either listener falls over, the whole service should go with it
    // rather than keep running half-present.
    tokio::select! {
        r = admin => r.context("admin listener")?,
        r = public => r.context("public listener")?,
        _ = shutdown_signal() => tracing::info!("shutting down"),
    }
    Ok(())
}

async fn serve(name: &'static str, addr: SocketAddr, router: Router) -> Result<()> {
    let app = router.layer(TraceLayer::new_for_http());
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("cannot bind the {name} port at {addr}"))?;
    tracing::info!(%addr, surface = name, "listening");
    // ConnectInfo gives the public form a peer address to rate limit on when
    // no forwarded header is present.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .with_context(|| format!("{name} listener failed"))
}

/// Served on both ports so either can be a load-balancer health target.
pub async fn healthz(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let meili_ok = state.store.client.health().await.is_ok();
    Json(json!({
        "status": if meili_ok { "ok" } else { "degraded" },
        "region": state.store.region.name,
        "meilisearch": if meili_ok { "available" } else { "unreachable" },
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}

fn init_tracing() {
    use tracing_subscriber::{EnvFilter, fmt, prelude::*};
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with(fmt::layer())
        .init();
}
