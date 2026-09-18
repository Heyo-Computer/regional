//! Meilisearch plumbing shared by both binaries.

use std::time::Duration;

use serde::Serialize;

use crate::error::{Error, Result};

pub use meilisearch_sdk::client::Client;
pub use meilisearch_sdk::indexes::Index;
pub use meilisearch_sdk::task_info::TaskInfo;

/// Documents per write request. Bodies are capped at 20k characters, so a
/// chunk this size stays comfortably inside Meilisearch's payload limit.
pub const CHUNK_SIZE: usize = 250;

pub const DEFAULT_URL: &str = "http://127.0.0.1:7700";

/// Build a client from `MEILI_URL` plus whichever key this service uses.
///
/// `bot` passes `MEILI_MASTER_KEY` (it writes); `mcp` passes
/// `MEILI_SEARCH_KEY` (it must not).
pub fn client_from_env(key_var: &str) -> Result<Client> {
    let url = std::env::var("MEILI_URL").unwrap_or_else(|_| DEFAULT_URL.to_string());
    let url = url.trim_end_matches('/').to_string();
    let key = std::env::var(key_var).ok().filter(|k| !k.is_empty());
    if key.is_none() {
        tracing::warn!(
            key_var,
            "no Meilisearch API key set; this only works against an instance started without a master key"
        );
    }
    Ok(Client::new(url, key)?)
}

/// Block until Meilisearch answers, or give up.
///
/// Both services race Meilisearch on boot — under compose and on heyo the
/// database VM may still be starting — so every entry point waits here
/// first rather than crash-looping.
pub async fn wait_healthy(client: &Client, max_wait: Duration) -> Result<()> {
    let started = std::time::Instant::now();
    let mut delay = Duration::from_millis(250);
    let mut last_err: Option<String> = None;
    while started.elapsed() < max_wait {
        match client.health().await {
            Ok(h) if h.status == "available" => {
                tracing::info!(
                    took_ms = started.elapsed().as_millis() as u64,
                    "meilisearch is available"
                );
                return Ok(());
            }
            Ok(h) => last_err = Some(format!("status {}", h.status)),
            Err(e) => last_err = Some(e.to_string()),
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(Duration::from_secs(5));
    }
    tracing::error!(?last_err, "gave up waiting for meilisearch");
    Err(Error::MeiliUnreachable {
        url: client.get_host().to_string(),
        seconds: max_wait.as_secs(),
    })
}

/// Await a task and turn a failed one into an error instead of a silent no-op.
pub async fn await_task(client: &Client, info: TaskInfo) -> Result<()> {
    let task = info
        .wait_for_completion(
            client,
            Some(Duration::from_millis(100)),
            Some(Duration::from_secs(120)),
        )
        .await?;
    if task.is_failure() {
        let uid = task.get_uid();
        let err = task.unwrap_failure();
        return Err(Error::Task {
            task_uid: uid,
            message: err.error_message,
        });
    }
    Ok(())
}

/// Upsert documents in chunks, awaiting each batch.
///
/// Returns the number of documents written. Awaiting each chunk keeps the
/// indexer honest about backpressure: a slow Meilisearch slows ingestion
/// rather than piling up an unbounded task queue.
pub async fn upsert_chunked<T: Serialize + Send + Sync>(
    client: &Client,
    index: &str,
    docs: &[T],
) -> Result<usize> {
    if docs.is_empty() {
        return Ok(0);
    }
    let idx = client.index(index);
    let mut written = 0usize;
    for chunk in docs.chunks(CHUNK_SIZE) {
        let info = idx.add_or_update(chunk, Some("id")).await?;
        await_task(client, info).await?;
        written += chunk.len();
    }
    tracing::debug!(index, written, "upserted documents");
    Ok(written)
}

/// Delete documents by id, in chunks.
pub async fn delete_chunked(client: &Client, index: &str, ids: &[String]) -> Result<usize> {
    if ids.is_empty() {
        return Ok(0);
    }
    let idx = client.index(index);
    let mut deleted = 0usize;
    for chunk in ids.chunks(CHUNK_SIZE) {
        let info = idx.delete_documents(chunk).await?;
        await_task(client, info).await?;
        deleted += chunk.len();
    }
    Ok(deleted)
}
