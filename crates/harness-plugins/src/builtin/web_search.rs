//! `web_search` plugin: web search + page fetch via a self-hosted Firecrawl
//! instance (`POST /v2/search`, `POST /v2/scrape`).

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::{Plugin, PluginManifest};

/// Firecrawl API paths, appended to the configured base URL.
const SEARCH_PATH: &str = "/v2/search";
const SCRAPE_PATH: &str = "/v2/scrape";
/// Request timeout: 15 s. Firecrawl fetches and converts the target page, so
/// a scrape legitimately outlasts the 5 s used by plain-GET plugins.
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
/// Response body cap: 64 KiB (repo convention).
const MAX_BODY_BYTES: usize = 64 * 1024;
/// Search results returned when the LLM omits `limit`.
const DEFAULT_LIMIT: u64 = 5;
/// Upper bound on `limit` accepted from the LLM.
const MAX_LIMIT: u64 = 10;

/// Web search + fetch backed by a Firecrawl server. The only network egress
/// is the configured base URL; target URLs are fetched by Firecrawl from its
/// own network position, never by this process.
pub struct WebSearchPlugin {
    base_url: String,
    api_key: String,
}

impl WebSearchPlugin {
    pub fn new(base_url: String, api_key: String) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
        }
    }

    /// POST `body` to `path` on the Firecrawl server, returning parsed JSON.
    /// Errors on transport failure, oversized bodies, non-2xx, and
    /// `success: false` payloads (Firecrawl reports soft failures that way).
    async fn post_json(&self, path: &str, body: Value) -> Result<Value, String> {
        let client = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .build()
            .map_err(|e| format!("http client build failed: {e}"))?;
        let mut req = client.post(format!("{}{path}", self.base_url)).json(&body);
        if !self.api_key.is_empty() {
            req = req.bearer_auth(&self.api_key);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| format!("firecrawl request failed: {e}"))?;
        let status = resp.status();
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| format!("firecrawl body read failed: {e}"))?;
        if bytes.len() > MAX_BODY_BYTES {
            return Err(format!("firecrawl response exceeds {MAX_BODY_BYTES} bytes"));
        }
        let parsed: Value = serde_json::from_slice(&bytes)
            .map_err(|e| format!("firecrawl response not JSON: {e}"))?;
        if !status.is_success() {
            let reason = reason_of(&parsed);
            return Err(format!("firecrawl failed (HTTP {status}): {reason}"));
        }
        if parsed.get("success").and_then(Value::as_bool) == Some(false) {
            return Err(format!("firecrawl failed: {}", reason_of(&parsed)));
        }
        Ok(parsed)
    }

    async fn search(&self, query: &str, limit: u64) -> Result<Value, String> {
        let parsed = self
            .post_json(SEARCH_PATH, json!({ "query": query, "limit": limit }))
            .await?;
        let results = parsed
            .pointer("/data/web")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .map(|r| {
                        json!({
                            "title": r.get("title").and_then(Value::as_str).unwrap_or(""),
                            "url": r.get("url").and_then(Value::as_str).unwrap_or(""),
                            "description": r.get("description").and_then(Value::as_str).unwrap_or(""),
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        Ok(json!({ "results": results }))
    }

    async fn fetch(&self, url: &str) -> Result<Value, String> {
        let parsed = self
            .post_json(SCRAPE_PATH, json!({ "url": url, "formats": ["markdown"] }))
            .await?;
        let markdown = parsed
            .pointer("/data/markdown")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let title = parsed
            .pointer("/data/metadata/title")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        Ok(json!({ "title": title, "markdown": markdown }))
    }
}

/// Firecrawl's human-readable error from a JSON payload, for surfacing.
fn reason_of(parsed: &Value) -> &str {
    parsed
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("unknown error")
}

#[async_trait]
impl Plugin for WebSearchPlugin {
    fn manifest(&self) -> &PluginManifest {
        static MANIFEST: std::sync::OnceLock<PluginManifest> = std::sync::OnceLock::new();
        MANIFEST.get_or_init(|| PluginManifest {
            name: "web_search",
            version: "0.1.0",
            description: "Web search and page fetch via a self-hosted Firecrawl server",
        })
    }

    fn tool_specs(&self) -> Vec<Value> {
        vec![
            json!({
                "type": "function",
                "function": {
                    "name": "web_search.search",
                    "description": "Search the web and return result titles, URLs, and descriptions.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "query": { "type": "string", "description": "Search query" },
                            "limit": { "type": "integer", "description": "How many results (1-10, default 5)" }
                        },
                        "required": ["query"]
                    }
                }
            }),
            json!({
                "type": "function",
                "function": {
                    "name": "web_search.fetch",
                    "description": "Fetch a web page by URL and return its readable text content. Use after web_search.search to read a promising result in full.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "url": { "type": "string", "description": "Absolute http(s) URL" }
                        },
                        "required": ["url"]
                    }
                }
            }),
        ]
    }

    async fn call(&self, name: &str, arguments: Value) -> Result<Value, String> {
        match name {
            "search" => {
                let query = arguments
                    .get("query")
                    .and_then(Value::as_str)
                    .ok_or("missing 'query' argument")?;
                let limit = arguments
                    .get("limit")
                    .and_then(Value::as_u64)
                    .map(|l| l.clamp(1, MAX_LIMIT))
                    .unwrap_or(DEFAULT_LIMIT);
                self.search(query, limit).await
            }
            "fetch" => {
                let url = arguments
                    .get("url")
                    .and_then(Value::as_str)
                    .ok_or("missing 'url' argument")?;
                self.fetch(url).await
            }
            other => Err(format!("web_search plugin has no tool '{other}'")),
        }
    }
}
