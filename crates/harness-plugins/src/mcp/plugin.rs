//! `mcp` plugin: exposes tools from configured Streamable-HTTP MCP servers to
//! the LLM as `mcp.<server>.<tool>`.

use std::collections::HashMap;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::RwLock;

use harness_core::config::McpServerConfig;

use super::client::McpHttpClient;
use super::oauth::{probe_and_discover, refresh_token, StoredToken, TokenResponse, TokenStore};
use super::types::McpTool;
use crate::{Plugin, PluginManifest};

/// Per-request HTTP timeout for MCP calls.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
/// Per-tool-call overall bound (the LLM waits on this).
const TOOL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// OAuth context discovered at warm-up, for later token refreshes.
#[derive(Debug, Clone)]
struct OAuthCtx {
    token_endpoint: String,
    resource: Option<String>,
    client_id: String,
}

#[derive(Default)]
struct State {
    clients: HashMap<String, McpHttpClient>,
    oauth: HashMap<String, OAuthCtx>,
    specs: Vec<Value>,
}

pub struct McpPlugin {
    servers: Vec<McpServerConfig>,
    token_store_path: String,
    state: RwLock<State>,
}

/// Names of the upstream tools one server may surface, honoring its
/// `tool_allowlist`. Empty allowlist = every tool passes through.
fn allowlisted_tools<'a>(server: &'a McpServerConfig, tools: &'a [McpTool]) -> Vec<&'a McpTool> {
    if server.tool_allowlist.is_empty() {
        return tools.iter().collect();
    }
    tools
        .iter()
        .filter(|t| server.tool_allowlist.iter().any(|a| a == &t.name))
        .collect()
}

impl McpPlugin {
    pub fn new(servers: Vec<McpServerConfig>, token_store_path: String) -> Self {
        Self {
            servers,
            token_store_path,
            state: RwLock::new(State::default()),
        }
    }

    fn server_by_name(&self, name: &str) -> Option<&McpServerConfig> {
        self.servers.iter().find(|s| s.name == name)
    }

    /// Valid server name: no dots (reserved as the tool separator).
    fn valid_name(name: &str) -> bool {
        !name.is_empty()
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    }

    /// Bearer credential for one server, refreshing an expired OAuth token.
    async fn bearer_for(&self, server: &McpServerConfig) -> Result<Option<String>, String> {
        match server.auth.as_str() {
            "bearer" => Ok((!server.api_key.trim().is_empty()).then(|| server.api_key.clone())),
            "oauth" => {
                let store = TokenStore::new(&self.token_store_path);
                let token = store.get(&server.name).ok_or_else(|| {
                    format!(
                        "mcp server '{}' is not authorized; run: harness-server auth {}",
                        server.name, server.name
                    )
                })?;
                if token.needs_refresh() {
                    let fresh = self.refresh(server, token).await?;
                    Ok(Some(fresh.access_token))
                } else {
                    Ok(Some(token.access_token))
                }
            }
            other => Err(format!(
                "mcp server '{}' has unknown auth kind '{other}' (use 'bearer' or 'oauth')",
                server.name
            )),
        }
    }

    /// Refresh one server's token, persisting the rotation.
    async fn refresh(
        &self,
        server: &McpServerConfig,
        token: StoredToken,
    ) -> Result<StoredToken, String> {
        let ctx = {
            let state = self.state.read().await;
            state.oauth.get(&server.name).cloned()
        }
        .ok_or_else(|| {
            format!(
                "mcp server '{}' has no OAuth context (not warmed)",
                server.name
            )
        })?;
        let rt = token.refresh_token.as_deref().ok_or_else(|| {
            format!(
                "mcp server '{}' token expired without refresh_token; run: harness-server auth {}",
                server.name, server.name
            )
        })?;
        let client_id = token
            .client_id
            .clone()
            .filter(|c| !c.is_empty())
            .or_else(|| (!ctx.client_id.is_empty()).then_some(ctx.client_id.clone()))
            .ok_or_else(|| format!("mcp server '{}' has no client_id for refresh", server.name))?;
        let resp: TokenResponse =
            refresh_token(&ctx.token_endpoint, rt, &client_id, ctx.resource.as_deref()).await?;
        let mut fresh = StoredToken::from_response(&resp);
        fresh.client_id = Some(client_id);
        TokenStore::new(&self.token_store_path).put(&server.name, fresh.clone())?;
        Ok(fresh)
    }

    /// Connect every configured server and cache its tool specs. Per-server
    /// failures are logged and skipped — one dead server never blocks boot.
    pub async fn warm(&self) {
        for server in &self.servers {
            if !Self::valid_name(&server.name) {
                tracing::warn!(
                    server = %server.name,
                    "mcp server name invalid (use [A-Za-z0-9_-], no '.'); skipping"
                );
                continue;
            }
            if server.url.trim().is_empty() {
                tracing::warn!(server = %server.name, "mcp server has no url; skipping");
                continue;
            }
            match self.connect(server).await {
                Ok((specs, count)) => {
                    let prefix = format!("mcp.{}.", server.name);
                    let mut state = self.state.write().await;
                    state.specs.retain(|s| {
                        s.get("function")
                            .and_then(|f| f.get("name"))
                            .and_then(Value::as_str)
                            .is_none_or(|n| !n.starts_with(&prefix))
                    });
                    state.specs.extend(specs);
                    tracing::info!(server = %server.name, tools = count, "mcp server connected");
                }
                Err(e) => {
                    tracing::warn!(server = %server.name, err = %e, "mcp server unavailable; skipping")
                }
            }
        }
    }

    /// Initialize + list tools for one server; returns its tool specs.
    async fn connect(&self, server: &McpServerConfig) -> Result<(Vec<Value>, usize), String> {
        let mut client = McpHttpClient::new(server.url.clone(), REQUEST_TIMEOUT);
        let bearer = match server.auth.as_str() {
            "bearer" => (!server.api_key.trim().is_empty()).then(|| server.api_key.clone()),
            "oauth" => {
                let (prm, asm) = probe_and_discover(&server.url).await?;
                let ctx = OAuthCtx {
                    token_endpoint: asm.token_endpoint.clone(),
                    resource: Some(prm.resource.clone()),
                    client_id: server.client_id.clone(),
                };
                let token = TokenStore::new(&self.token_store_path)
                    .get(&server.name)
                    .ok_or_else(|| {
                        format!("not authorized; run: harness-server auth {}", server.name)
                    })?;
                let bearer = token.access_token.clone();
                let mut state = self.state.write().await;
                state.oauth.insert(server.name.clone(), ctx);
                Some(bearer)
            }
            other => return Err(format!("unknown auth kind '{other}'")),
        };

        client.initialize(bearer.as_deref()).await?;
        let tools = client.list_tools(bearer.as_deref()).await?;
        let tools = allowlisted_tools(server, &tools);
        let specs: Vec<Value> = tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": format!("mcp.{}.{}", server.name, t.name),
                        "description": format!(
                            "{} (MCP server '{}')",
                            t.description.as_deref().unwrap_or(&t.name),
                            server.name
                        ),
                        "parameters": t.input_schema.clone(),
                    }
                })
            })
            .collect();
        let count = specs.len();
        let mut state = self.state.write().await;
        state.clients.insert(server.name.clone(), client);
        Ok((specs, count))
    }
}

/// MCP tool result → JSON payload for the LLM. `isError: true` → Err with the
/// text content (flows back as a recoverable tool error).
fn convert_result(result: Value) -> Result<Value, String> {
    let is_error = result
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let text = result
        .get("content")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|c| c.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    if is_error {
        return Err(if text.is_empty() {
            format!("mcp tool failed: {result}")
        } else {
            text
        });
    }
    let mut out = json!({ "text": text });
    if let Some(structured) = result.get("structuredContent") {
        out["structured"] = structured.clone();
    }
    Ok(out)
}

#[async_trait]
impl Plugin for McpPlugin {
    fn manifest(&self) -> &PluginManifest {
        static MANIFEST: std::sync::OnceLock<PluginManifest> = std::sync::OnceLock::new();
        MANIFEST.get_or_init(|| PluginManifest {
            name: "mcp",
            version: "0.1.0",
            description: "Tools from configured MCP servers (Streamable HTTP)",
        })
    }

    async fn warm(&self) {
        McpPlugin::warm(self).await
    }

    fn tool_specs(&self) -> Vec<Value> {
        // Synchronous snapshot of warmed specs; empty until warm() runs.
        self.state
            .try_read()
            .map(|s| s.specs.clone())
            .unwrap_or_default()
    }

    async fn call(&self, name: &str, arguments: Value) -> Result<Value, String> {
        let Some((server_name, tool)) = name.split_once('.') else {
            return Err(format!("mcp tool '{name}' must be '<server>.<tool>'"));
        };
        let server = self.server_by_name(server_name).ok_or_else(|| {
            format!(
                "unknown mcp server '{server_name}' (configured: {:?})",
                self.servers
                    .iter()
                    .map(|s| s.name.as_str())
                    .collect::<Vec<_>>()
            )
        })?;
        // Dispatch-time allowlist check: pruning at warm-up keeps the tool off
        // the LLM's spec list, but the model can still hallucinate a call to a
        // pruned name — refuse it instead of forwarding to the server.
        if !server.tool_allowlist.is_empty() && !server.tool_allowlist.iter().any(|a| a == tool) {
            return Err(format!(
                "tool '{tool}' is not allowlisted on mcp server '{server_name}'"
            ));
        }

        let bearer = self.bearer_for(server).await?;
        let result = {
            let mut state = self.state.write().await;
            let Some(client) = state.clients.get_mut(server_name) else {
                return Err(format!(
                    "mcp server '{server_name}' is not connected (see server logs)"
                ));
            };
            tokio::time::timeout(
                TOOL_TIMEOUT,
                client.call_tool(bearer.as_deref(), tool, arguments.clone()),
            )
            .await
            .map_err(|_| {
                format!(
                    "mcp tool '{name}' timed out after {}s",
                    TOOL_TIMEOUT.as_secs()
                )
            })?
        };

        // One transparent token refresh + retry on 401 for OAuth servers.
        let result = match result {
            Err(e) if e == super::client::ERR_AUTH_REQUIRED && server.auth == "oauth" => {
                tracing::info!(server = %server_name, "mcp 401; refreshing token once");
                let token = TokenStore::new(&self.token_store_path)
                    .get(&server.name)
                    .ok_or_else(|| {
                        format!(
                            "mcp server '{server_name}' unauthorized; run: harness-server auth {}",
                            server_name
                        )
                    })?;
                let fresh = self.refresh(server, token).await?;
                let mut state = self.state.write().await;
                let Some(client) = state.clients.get_mut(server_name) else {
                    return Err(format!("mcp server '{server_name}' is not connected"));
                };
                tokio::time::timeout(
                    TOOL_TIMEOUT,
                    client.call_tool(Some(&fresh.access_token), tool, arguments),
                )
                .await
                .map_err(|_| {
                    format!(
                        "mcp tool '{name}' timed out after {}s",
                        TOOL_TIMEOUT.as_secs()
                    )
                })?
            }
            other => other,
        };

        convert_result(result?)
    }
}
