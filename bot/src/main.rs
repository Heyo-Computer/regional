//! Continuous indexer for the regional search stack.
//!
//! Runs forever, walking each source on its own schedule and feeding
//! everything through one pipeline into Meilisearch. Pass `--once` to run a
//! single cycle of every source and exit, which is what CI and the
//! verification steps use.

mod config;
mod geocode;
mod http;
mod metrics;
mod pipeline;
mod sources;
mod state;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::{Json, Router, extract::State, routing::get};
use rand::RngExt;
use regional_core::index::ensure_indexes;
use regional_core::meili;
use regional_core::region::RegionConfig;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::geocode::Geocoder;
use crate::http::Fetcher;
use crate::metrics::SharedMetrics;
use crate::pipeline::Pipeline;
use crate::sources::Ctx;
use crate::state::BotState;

const MEILI_BOOT_WAIT: Duration = Duration::from_secs(180);

/// Nominatim's usage policy is a hard one request per second. The extra
/// 200 ms keeps us clear of it even with clock drift.
const NOMINATIM_INTERVAL: Duration = Duration::from_millis(1_200);
/// Overpass is a small volunteer-run cluster; one query every few seconds
/// is well within what it asks for.
const OVERPASS_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Clone)]
struct Health {
    metrics: SharedMetrics,
    state: Arc<BotState>,
    client: meili::Client,
    region_name: String,
    /// Echoed back so an operator can confirm which contact address the
    /// upstream APIs are actually seeing.
    user_agent: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let config = Arc::new(Config::from_env()?);
    let region = Arc::new(RegionConfig::from_env().context("loading the region config")?);
    tracing::info!(
        region = %region.name,
        cities = region.cities.len(),
        seeds = config.crawl_seeds.len(),
        once = config.once,
        "starting the indexer"
    );

    let client = meili::client_from_env("MEILI_MASTER_KEY")?;
    meili::wait_healthy(&client, MEILI_BOOT_WAIT).await?;
    ensure_indexes(&client, &region)
        .await
        .context("applying index settings")?;

    let bot_state = Arc::new(BotState::new(client.clone()));
    // Anything a previous run claimed but never finished goes back in the
    // pool, so a crash mid-crawl costs one cycle rather than those URLs.
    if let Err(e) = bot_state.frontier_requeue_stale().await {
        tracing::warn!(error = %e, "could not requeue stale frontier entries");
    }

    let fetcher = Arc::new(
        Fetcher::new(&config.user_agent)?
            .with_host_interval(&host_of(&config.nominatim_url), NOMINATIM_INTERVAL)
            .with_host_interval(&host_of(&config.overpass_url), OVERPASS_INTERVAL),
    );
    let geocoder = Arc::new(Geocoder::new(
        fetcher.clone(),
        bot_state.clone(),
        region.clone(),
        config.nominatim_url.clone(),
    ));

    let ctx = Arc::new(Ctx {
        http: fetcher,
        region: region.clone(),
        state: bot_state.clone(),
        geocoder,
        config: config.clone(),
    });
    let pipeline = Arc::new(Pipeline::new(
        client.clone(),
        (*region).clone(),
        bot_state.clone(),
    ));
    let metrics = SharedMetrics::new();
    let ct = CancellationToken::new();

    let health = Health {
        metrics: metrics.clone(),
        state: bot_state.clone(),
        client: client.clone(),
        region_name: region.name.clone(),
        user_agent: config.user_agent.clone(),
    };
    let health_task = tokio::spawn(serve_health(config.clone(), health, ct.clone()));

    let sources: Vec<Arc<dyn sources::Source>> = sources::all()
        .into_iter()
        .map(Arc::from)
        .filter(|s: &Arc<dyn sources::Source>| {
            let on = config.is_enabled(s.name());
            if !on {
                tracing::info!(source = s.name(), "source disabled by DISABLED_SOURCES");
            }
            on
        })
        .collect();

    if config.once {
        // One cycle of everything, then report and exit.
        let mut handles = Vec::new();
        for source in sources {
            handles.push(tokio::spawn(run_once(
                source,
                ctx.clone(),
                pipeline.clone(),
                metrics.clone(),
            )));
        }
        for h in handles {
            let _ = h.await;
        }
        let (_, snapshot) = metrics.snapshot().await;
        tracing::info!(
            summary = %serde_json::to_string(&snapshot).unwrap_or_default(),
            "single cycle complete"
        );
        ct.cancel();
        let _ = health_task.await;
        return Ok(());
    }

    // Each source runs on its own schedule. One failing source backs off on
    // its own and never stalls the others.
    let mut tasks = Vec::new();
    for source in sources {
        let interval = config.interval_for(source.name(), source.default_interval());
        tracing::info!(
            source = source.name(),
            interval_s = interval.as_secs(),
            "scheduling source"
        );
        tasks.push(tokio::spawn(run_forever(
            source,
            interval,
            ctx.clone(),
            pipeline.clone(),
            metrics.clone(),
            ct.clone(),
        )));
    }

    shutdown_signal().await;
    tracing::info!("shutdown requested; finishing in-flight work");
    ct.cancel();
    for t in tasks {
        let _ = t.await;
    }
    let _ = health_task.await;
    Ok(())
}

/// One pass of one source: fetch, ingest, persist the cursor.
async fn run_once(
    source: Arc<dyn sources::Source>,
    ctx: Arc<Ctx>,
    pipeline: Arc<Pipeline>,
    metrics: SharedMetrics,
) {
    let name = source.name();
    let cursor = ctx.state.get_cursor(name).await;

    match source.fetch(&ctx, cursor).await {
        Ok(batch) => {
            let swept = batch.swept;
            let next = batch.next_cursor.clone();
            match pipeline.ingest(batch.docs).await {
                Ok(stats) => {
                    tracing::info!(
                        source = name,
                        received = stats.received,
                        written = stats.written,
                        unchanged = stats.unchanged,
                        suppressed = stats.suppressed,
                        out_of_region = stats.out_of_region,
                        invalid = stats.invalid,
                        "source cycle complete"
                    );
                    // The cursor only advances once the batch is safely
                    // indexed, so a failed write is retried rather than
                    // silently skipped.
                    if let Some(next) = next
                        && let Err(e) = ctx.state.set_cursor(name, Some(next)).await
                    {
                        tracing::warn!(source = name, error = %e, "could not persist the cursor");
                    }
                    metrics.record_success(name, stats, swept).await;
                }
                Err(e) => {
                    tracing::error!(source = name, error = %e, "ingest failed");
                    metrics.record_error(name, &e.to_string()).await;
                }
            }
        }
        Err(e) => {
            tracing::error!(source = name, error = %e, "fetch failed");
            metrics.record_error(name, &e.to_string()).await;
        }
    }
}

async fn run_forever(
    source: Arc<dyn sources::Source>,
    interval: Duration,
    ctx: Arc<Ctx>,
    pipeline: Arc<Pipeline>,
    metrics: SharedMetrics,
    ct: CancellationToken,
) {
    let name = source.name();
    loop {
        if ct.is_cancelled() {
            return;
        }
        run_once(
            source.clone(),
            ctx.clone(),
            pipeline.clone(),
            metrics.clone(),
        )
        .await;

        let (_, snapshot) = metrics.snapshot().await;
        let errors = snapshot
            .get(name)
            .map(|m| m.consecutive_errors)
            .unwrap_or(0);
        let wait = backoff(interval, errors);
        if errors > 0 {
            tracing::warn!(
                source = name,
                errors,
                wait_s = wait.as_secs(),
                "backing off"
            );
        }

        tokio::select! {
            _ = ct.cancelled() => return,
            _ = tokio::time::sleep(wait) => {}
        }
    }
}

/// The wait before a source's next run: its interval, stretched when it is
/// failing, plus jitter so sources do not synchronise into bursts.
fn backoff(interval: Duration, consecutive_errors: u32) -> Duration {
    let multiplier = 1u32 << consecutive_errors.min(3);
    let base = interval.saturating_mul(multiplier);
    let jitter_ms = rand::rng().random_range(0..=(base.as_millis() / 10).max(1) as u64);
    (base + Duration::from_millis(jitter_ms)).min(Duration::from_secs(6 * 3600))
}

async fn serve_health(config: Arc<Config>, health: Health, ct: CancellationToken) {
    let app = Router::new()
        .route("/healthz", get(healthz))
        .with_state(health);
    let listener = match tokio::net::TcpListener::bind(config.bind).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(addr = %config.bind, error = %e, "could not bind the health endpoint");
            return;
        }
    };
    tracing::info!(addr = %config.bind, "health endpoint listening on /healthz");
    let _ = axum::serve(listener, app)
        .with_graceful_shutdown(async move { ct.cancelled().await })
        .await;
}

async fn healthz(State(h): State<Health>) -> Json<serde_json::Value> {
    let meili_ok = h.client.health().await.is_ok();
    let (started_at, sources) = h.metrics.snapshot().await;
    Json(json!({
        "status": if meili_ok { "ok" } else { "degraded" },
        "region": h.region_name,
        "meilisearch": if meili_ok { "available" } else { "unreachable" },
        "started_at": started_at,
        "user_agent": h.user_agent,
        "crawl_frontier_pending": h.state.frontier_pending().await,
        "sources": sources,
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

fn host_of(url: &str) -> String {
    http::host_of(url).unwrap_or_else(|_| url.to_string())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_with_failures_and_is_capped() {
        let base = Duration::from_secs(600);
        let healthy = backoff(base, 0);
        assert!(healthy >= base && healthy < base * 2, "{healthy:?}");

        let failing = backoff(base, 2);
        assert!(failing >= base * 4, "{failing:?}");

        // The shift is clamped, so a long-dead source retries hourly rather
        // than drifting toward never.
        assert!(backoff(base, 50) <= Duration::from_secs(6 * 3600));
        assert!(backoff(Duration::from_secs(3 * 3600), 50) <= Duration::from_secs(6 * 3600));
    }
}
