//! Everything the dashboard reads and writes.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use meilisearch_sdk::search::{SearchQuery, Selectors};
use regional_core::index::{CONTENT_INDEXES, MCP_TOKENS, SUBMISSIONS};
use regional_core::meili::{self, Client};
use regional_core::model::{Kind, now_ts};
use regional_core::region::RegionConfig;
use regional_core::submission::{Request, Status, Submission};
use regional_core::token::{self, McpToken};
use serde::Deserialize;
use serde_json::Value;

/// Ceiling on a submissions page, so a runaway queue cannot render forever.
pub const PAGE_SIZE: usize = 50;

pub struct Store {
    pub client: Client,
    pub region: RegionConfig,
    http: reqwest::Client,
    bot_health_url: String,
}

/// One row in the index browser, used when triaging an edit request to find
/// the document it refers to.
#[derive(Debug, Clone, Deserialize)]
pub struct BrowseHit {
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub city: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub categories: Vec<String>,
}

#[derive(Debug, Default, Clone)]
pub struct Overview {
    pub documents: BTreeMap<String, usize>,
    pub freshest: BTreeMap<String, Option<i64>>,
    pub submissions: BTreeMap<String, usize>,
    pub top_categories: Vec<(String, usize)>,
    /// The indexer's `/healthz`, or `None` when it cannot be reached.
    pub bot: Option<BotHealth>,
}

/// The indexer's health payload.
///
/// Every field is optional or defaulted: this crosses a service boundary,
/// and a dashboard that panics because the indexer added a field is worse
/// than one that renders a blank cell.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct BotHealth {
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub meilisearch: String,
    #[serde(default)]
    pub started_at: Option<i64>,
    #[serde(default)]
    pub crawl_frontier_pending: Option<usize>,
    #[serde(default)]
    pub user_agent: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub sources: BTreeMap<String, SourceHealth>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct SourceHealth {
    #[serde(default)]
    pub runs: u64,
    #[serde(default)]
    pub last_run_at: Option<i64>,
    #[serde(default)]
    pub last_error: Option<String>,
    #[serde(default)]
    pub consecutive_errors: u32,
    #[serde(default)]
    pub swept: u64,
    #[serde(default)]
    pub totals: SourceTotals,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct SourceTotals {
    #[serde(default)]
    pub received: usize,
    #[serde(default)]
    pub invalid: usize,
    #[serde(default)]
    pub out_of_region: usize,
    #[serde(default)]
    pub unchanged: usize,
    #[serde(default)]
    pub written: usize,
}

impl Store {
    pub fn new(client: Client, region: RegionConfig, bot_health_url: String) -> Self {
        Self {
            client,
            region,
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build()
                .unwrap_or_default(),
            bot_health_url,
        }
    }

    // ------------------------------------------------------------- overview

    pub async fn overview(&self) -> Overview {
        let mut o = Overview::default();
        for index in CONTENT_INDEXES {
            let n = match self.client.index(index).get_stats().await {
                Ok(s) => s.number_of_documents,
                Err(e) => {
                    tracing::warn!(index, error = %e, "could not read index stats");
                    0
                }
            };
            o.documents.insert(index.to_string(), n);
            o.freshest
                .insert(index.to_string(), self.newest(index).await);
        }
        o.submissions = self.submission_counts().await;
        o.top_categories = self.top_categories(20).await;
        o.bot = self.bot_health().await;
        o
    }

    async fn newest(&self, index: &str) -> Option<i64> {
        #[derive(Deserialize)]
        struct Row {
            #[serde(default)]
            updated_at: Option<i64>,
        }
        let idx = self.client.index(index);
        let sort = ["updated_at:desc"];
        let mut q = SearchQuery::new(&idx);
        q.with_query("").with_limit(1).with_sort(&sort);
        q.execute::<Row>()
            .await
            .ok()?
            .hits
            .first()
            .and_then(|h| h.result.updated_at)
    }

    async fn top_categories(&self, n: usize) -> Vec<(String, usize)> {
        let idx = self.client.index(regional_core::index::PLACES);
        let facets = ["categories"];
        let mut q = SearchQuery::new(&idx);
        q.with_query("")
            .with_limit(0)
            .with_facets(Selectors::Some(&facets));
        let Ok(res) = q.execute::<Value>().await else {
            return Vec::new();
        };
        let mut out: Vec<(String, usize)> = res
            .facet_distribution
            .and_then(|mut d| d.remove("categories"))
            .unwrap_or_default()
            .into_iter()
            .collect();
        out.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        out.truncate(n);
        out
    }

    /// How many submissions sit in each status. Drives the queue badges.
    pub async fn submission_counts(&self) -> BTreeMap<String, usize> {
        let idx = self.client.index(SUBMISSIONS);
        let facets = ["status"];
        let mut q = SearchQuery::new(&idx);
        q.with_query("")
            .with_limit(0)
            .with_facets(Selectors::Some(&facets));
        let dist = match q.execute::<Value>().await {
            Ok(r) => r
                .facet_distribution
                .and_then(|mut d| d.remove("status"))
                .unwrap_or_default(),
            Err(e) => {
                tracing::warn!(error = %e, "could not read submission counts");
                Default::default()
            }
        };
        // Always report every status, so "0 pending" is visible rather than
        // an absent row that reads as a broken page.
        let mut out = BTreeMap::new();
        for s in Status::ALL {
            out.insert(
                s.as_str().to_string(),
                dist.get(s.as_str()).copied().unwrap_or(0),
            );
        }
        out
    }

    async fn bot_health(&self) -> Option<BotHealth> {
        match self.http.get(&self.bot_health_url).send().await {
            Ok(r) if r.status().is_success() => match r.json::<BotHealth>().await {
                Ok(h) => Some(h),
                Err(e) => {
                    tracing::warn!(error = %e, "indexer health did not parse");
                    None
                }
            },
            Ok(r) => {
                tracing::warn!(status = %r.status(), "indexer health returned an error status");
                None
            }
            Err(e) => {
                tracing::warn!(error = %e, url = %self.bot_health_url, "indexer health unreachable");
                None
            }
        }
    }

    // ---------------------------------------------------------- submissions

    pub async fn create(&self, submission: &Submission) -> Result<()> {
        meili::upsert_chunked(&self.client, SUBMISSIONS, std::slice::from_ref(submission))
            .await
            .context("storing the submission")?;
        Ok(())
    }

    pub async fn get(&self, id: &str) -> Option<Submission> {
        self.client
            .index(SUBMISSIONS)
            .get_document::<Submission>(id)
            .await
            .ok()
    }

    pub async fn list(
        &self,
        status: Option<Status>,
        request: Option<Request>,
        query: &str,
        offset: usize,
    ) -> Result<(Vec<Submission>, usize)> {
        let mut clauses: Vec<String> = Vec::new();
        if let Some(s) = status {
            clauses.push(format!("status = \"{}\"", s.as_str()));
        }
        if let Some(r) = request {
            clauses.push(format!("request = \"{}\"", r.as_str()));
        }
        let filter = clauses.join(" AND ");

        let idx = self.client.index(SUBMISSIONS);
        let sort = ["submitted_at:desc"];
        let mut q = SearchQuery::new(&idx);
        q.with_query(query)
            .with_limit(PAGE_SIZE)
            .with_offset(offset)
            .with_sort(&sort);
        if !filter.is_empty() {
            q.with_filter(&filter);
        }
        let res = q
            .execute::<Submission>()
            .await
            .context("listing submissions")?;
        let total = res.estimated_total_hits.unwrap_or(res.hits.len());
        Ok((res.hits.into_iter().map(|h| h.result).collect(), total))
    }

    /// Record a review decision. Returns the updated submission.
    pub async fn review(
        &self,
        id: &str,
        status: Status,
        note: Option<String>,
    ) -> Result<Submission> {
        let mut sub = self
            .get(id)
            .await
            .with_context(|| format!("no submission with id {id:?}"))?;
        sub.status = status;
        sub.review_note = note.filter(|n| !n.trim().is_empty());
        sub.reviewed_at = Some(now_ts());
        sub.updated_at = now_ts();
        // Re-deciding a submission the indexer already choked on should clear
        // the stale explanation rather than leave it contradicting the state.
        sub.apply_note = None;
        self.create(&sub).await?;
        Ok(sub)
    }

    // ------------------------------------------------------------ mcp tokens

    /// Every minted token, newest first. There are few enough that one page
    /// is the whole list.
    pub async fn tokens(&self) -> Result<Vec<McpToken>> {
        let idx = self.client.index(MCP_TOKENS);
        let sort = ["created_at:desc"];
        let mut q = SearchQuery::new(&idx);
        q.with_query("").with_limit(1000).with_sort(&sort);
        let res = q.execute::<McpToken>().await.context("listing MCP tokens")?;
        Ok(res.hits.into_iter().map(|h| h.result).collect())
    }

    /// Mint a token for `name`. Returns the token itself, which is the only
    /// time it exists anywhere outside the caller's hands.
    pub async fn mint_token(&self, name: String, expires_at: Option<i64>) -> Result<String> {
        let secret = token::format(rand::random());
        let rec = McpToken::new(&secret, name, expires_at);
        // Wait for the write, so the token works the moment it is shown.
        let info = self
            .client
            .index(MCP_TOKENS)
            .add_or_replace(std::slice::from_ref(&rec), Some("id"))
            .await
            .context("storing the MCP token")?;
        meili::await_task(&self.client, info)
            .await
            .context("storing the MCP token")?;
        Ok(secret)
    }

    pub async fn revoke_token(&self, id: &str) -> Result<McpToken> {
        let mut rec = self
            .client
            .index(MCP_TOKENS)
            .get_document::<McpToken>(id)
            .await
            .with_context(|| format!("no MCP token with id {id:?}"))?;
        if rec.revoked_at.is_none() {
            rec.revoked_at = Some(now_ts());
            let info = self
                .client
                .index(MCP_TOKENS)
                .add_or_replace(std::slice::from_ref(&rec), Some("id"))
                .await
                .context("revoking the MCP token")?;
            meili::await_task(&self.client, info)
                .await
                .context("revoking the MCP token")?;
        }
        Ok(rec)
    }

    // -------------------------------------------------------------- browsing

    /// Search the content indexes, so a reviewer can find the document an
    /// edit request is talking about and copy its id.
    pub async fn browse(&self, kind: Option<Kind>, query: &str) -> Result<Vec<BrowseHit>> {
        let indexes: Vec<&str> = match kind {
            Some(k) => vec![k.index()],
            None => CONTENT_INDEXES.to_vec(),
        };
        let mut out = Vec::new();
        for index in indexes {
            let idx = self.client.index(index);
            let mut q = SearchQuery::new(&idx);
            q.with_query(query).with_limit(15);
            match q.execute::<BrowseHit>().await {
                Ok(res) => out.extend(res.hits.into_iter().map(|h| h.result)),
                Err(e) => tracing::warn!(index, error = %e, "browse failed"),
            }
        }
        Ok(out)
    }

    /// Does this document actually exist? Used to validate `existing_id` on
    /// an edit request before a reviewer wastes time on it.
    pub async fn document_exists(&self, kind: Kind, id: &str) -> bool {
        self.client
            .index(kind.index())
            .get_document::<Value>(id)
            .await
            .is_ok()
    }
}

#[cfg(test)]
mod tests {
    use regional_core::submission::{Request, Status};

    #[test]
    fn statuses_and_requests_are_safe_inside_a_filter() {
        // These go into Meilisearch filter expressions unquoted-by-hand, so
        // they must never contain anything that could change the expression.
        for s in Status::ALL {
            assert!(s.as_str().chars().all(|c| c.is_ascii_lowercase()));
        }
        for r in Request::ALL {
            assert!(r.as_str().chars().all(|c| c.is_ascii_lowercase()));
        }
    }
}
