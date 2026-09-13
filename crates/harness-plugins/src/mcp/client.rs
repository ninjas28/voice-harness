//! Streamable-HTTP MCP client: JSON-RPC 2.0 over HTTP POST, accepting JSON or
//! SSE responses, tracking `Mcp-Session-Id`, re-initializing once on session
//! expiry (404). Every network wait is timeout-bounded.

use std::time::Duration;

use eventsource_stream::Eventsource;
use futures::StreamExt;
use serde_json::{json, Value};

use super::types::{JsonRpcResponse, McpTool};

/// Cap on a single MCP response body (256 KiB).
const MAX_RESPONSE_BYTES: usize = 256 * 1024;
/// The protocol version we request; the server's answer (if any) overrides it.
const REQUESTED_PROTOCOL_VERSION: &str = "2025-06-18";
/// Sentinel error: the server answered 401 (auth required).
pub(crate) const ERR_AUTH_REQUIRED: &str = "mcp-auth-required";
/// Sentinel error: the server's session expired (404).
pub(crate) const ERR_SESSION_EXPIRED: &str = "mcp-session-expired";

/// An HTTP connection to one MCP server (Streamable HTTP transport).
pub struct McpHttpClient {
    http: reqwest::Client,
    url: String,
    timeout: Duration,
    session_id: Option<String>,
    protocol_version: String,
    next_id: u64,
}

impl McpHttpClient {
    pub fn new(url: impl Into<String>, timeout: Duration) -> Self {
        Self {
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .build()
                .expect("reqwest client builds"),
            url: url.into(),
            timeout,
            session_id: None,
            protocol_version: REQUESTED_PROTOCOL_VERSION.to_string(),
            next_id: 0,
        }
    }

    fn next_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    /// POST a JSON-RPC payload. Returns the parsed response for requests that
    /// expect one; `Ok(None)` after notifications. Handles `application/json`
    /// and `text/event-stream` bodies (SSE is read until the matching id).
    async fn post_rpc(
        &mut self,
        bearer: Option<&str>,
        body: Value,
        expect_response: bool,
    ) -> Result<Option<JsonRpcResponse>, String> {
        let sent_id = body.get("id").and_then(Value::as_u64);
        let mut req = self
            .http
            .post(&self.url)
            .header("Accept", "application/json, text/event-stream")
            .header("MCP-Protocol-Version", self.protocol_version.clone());
        if let Some(token) = bearer {
            req = req.bearer_auth(token);
        }
        if let Some(sid) = &self.session_id {
            req = req.header("Mcp-Session-Id", sid);
        }

        let resp = tokio::time::timeout(self.timeout, req.json(&body).send())
            .await
            .map_err(|_| "mcp request timed out".to_string())?
            .map_err(|e| format!("mcp request failed: {e}"))?;

        if let Some(sid) = resp
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
        {
            self.session_id = Some(sid.to_string());
        }

        let status = resp.status();
        if status.as_u16() == 401 {
            return Err(ERR_AUTH_REQUIRED.to_string());
        }
        if status.as_u16() == 404 {
            // Session expired server-side; the caller re-initializes and retries.
            self.session_id = None;
            return Err(ERR_SESSION_EXPIRED.to_string());
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(format!(
                "mcp http {}: {}",
                status.as_u16(),
                truncate(&body, 500)
            ));
        }
        if !expect_response {
            return Ok(None);
        }
        let Some(sent_id) = sent_id else {
            return Err("mcp request without id cannot expect a response".to_string());
        };

        let ct = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_ascii_lowercase();

        if ct.contains("text/event-stream") {
            let mut stream = Box::pin(resp.bytes_stream().eventsource());
            loop {
                let ev = tokio::time::timeout(self.timeout, stream.next())
                    .await
                    .map_err(|_| "mcp sse read timed out".to_string())?
                    .ok_or("mcp sse stream ended without a response")?
                    .map_err(|e| format!("mcp sse error: {e}"))?;
                if ev.data.trim().is_empty() {
                    continue; // keepalive comment
                }
                let msg: JsonRpcResponse = serde_json::from_str(&ev.data)
                    .map_err(|e| format!("bad mcp sse payload: {e}"))?;
                if msg.id.as_u64() == Some(sent_id) {
                    return Ok(Some(msg));
                }
                // Notifications interleaved on the stream are ignored.
            }
        }

        let text = tokio::time::timeout(self.timeout, resp.text())
            .await
            .map_err(|_| "mcp body read timed out".to_string())?
            .map_err(|e| format!("mcp body read failed: {e}"))?;
        if text.len() > MAX_RESPONSE_BYTES {
            return Err(format!("mcp response exceeds {MAX_RESPONSE_BYTES} bytes"));
        }
        let msg: JsonRpcResponse =
            serde_json::from_str(&text).map_err(|e| format!("bad mcp response: {e}"))?;
        Ok(Some(msg))
    }

    /// One request round; used by [`Self::rpc`] for its retry-once logic.
    async fn rpc_once(
        &mut self,
        bearer: Option<&str>,
        method: &str,
        params: Value,
    ) -> Result<Value, String> {
        let id = self.next_id();
        let body = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        let Some(resp) = self.post_rpc(bearer, body, true).await? else {
            return Err(format!("mcp {method}: empty response"));
        };
        if let Some(err) = resp.error {
            return Err(format!("mcp {method} error {}: {}", err.code, err.message));
        }
        resp.result
            .ok_or_else(|| format!("mcp {method} missing result"))
    }

    /// Run one JSON-RPC request; on session expiry (404) re-initialize and
    /// retry exactly once.
    async fn rpc(
        &mut self,
        bearer: Option<&str>,
        method: &str,
        params: Value,
    ) -> Result<Value, String> {
        match self.rpc_once(bearer, method, params.clone()).await {
            Err(e) if e == ERR_SESSION_EXPIRED => {
                tracing::debug!("mcp session expired; re-initializing once");
                self.initialize(bearer).await?;
                self.rpc_once(bearer, method, params).await
            }
            other => other,
        }
    }

    /// Perform the MCP `initialize` handshake + `notifications/initialized`.
    /// Resets any previous session.
    pub async fn initialize(&mut self, bearer: Option<&str>) -> Result<(), String> {
        self.session_id = None;
        let body = json!({
            "jsonrpc": "2.0",
            "id": self.next_id(),
            "method": "initialize",
            "params": {
                "protocolVersion": REQUESTED_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": { "name": "voice-harness", "version": env!("CARGO_PKG_VERSION") }
            }
        });
        let Some(resp) = self.post_rpc(bearer, body, true).await? else {
            return Err("mcp initialize: empty response".to_string());
        };
        if let Some(err) = resp.error {
            return Err(format!(
                "mcp initialize error {}: {}",
                err.code, err.message
            ));
        }
        let result = resp.result.ok_or("mcp initialize missing result")?;
        self.protocol_version = result
            .get("protocolVersion")
            .and_then(Value::as_str)
            .unwrap_or(REQUESTED_PROTOCOL_VERSION)
            .to_string();

        let note = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        let _ = self.post_rpc(bearer, note, false).await;
        Ok(())
    }

    /// `tools/list` → advertised tools.
    pub async fn list_tools(&mut self, bearer: Option<&str>) -> Result<Vec<McpTool>, String> {
        let result = self.rpc(bearer, "tools/list", json!({})).await?;
        let Some(tools) = result.get("tools").and_then(Value::as_array) else {
            return Ok(Vec::new());
        };
        tools
            .iter()
            .map(|t| serde_json::from_value(t.clone()).map_err(|e| format!("bad tool spec: {e}")))
            .collect()
    }
}

fn truncate(s: &str, max_chars: usize) -> String {
    s.chars().take(max_chars).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_partial_json, header, method};

    fn client(url: &str) -> McpHttpClient {
        McpHttpClient::new(url, Duration::from_secs(5))
    }

    /// Mount the initialize handshake (assigning `session_id`) and the
    /// `notifications/initialized` ack on a wiremock server.
    async fn mount_initialize(server: &wiremock::MockServer, session_id: &str) {
        wiremock::Mock::given(method("POST"))
            .and(body_partial_json(json!({ "method": "initialize" })))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .insert_header("Mcp-Session-Id", session_id)
                    .set_body_json(json!({
                        "jsonrpc": "2.0", "id": 1,
                        "result": {
                            "protocolVersion": "2025-06-18",
                            "capabilities": { "tools": {} },
                            "serverInfo": { "name": "test", "version": "0.0.1" }
                        }
                    })),
            )
            .mount(server)
            .await;
        wiremock::Mock::given(method("POST"))
            .and(body_partial_json(
                json!({ "method": "notifications/initialized" }),
            ))
            .respond_with(wiremock::ResponseTemplate::new(202))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn initialize_handshake_stores_session_and_sends_it_on_followups() {
        let server = wiremock::MockServer::start().await;
        mount_initialize(&server, "sess-123").await;
        wiremock::Mock::given(method("POST"))
            .and(body_partial_json(json!({ "method": "tools/list" })))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_json(json!({
                        "jsonrpc": "2.0", "id": 2,
                        "result": { "tools": [ { "name": "t1", "description": "d" } ] }
                    })),
            )
            .mount(&server)
            .await;

        let mut c = client(&server.uri());
        c.initialize(None).await.expect("init ok");
        let tools = c.list_tools(None).await.expect("list ok");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "t1");

        let received = server.received_requests().await.expect("requests");
        let tools_req = received
            .iter()
            .find(|r| {
                serde_json::from_slice::<Value>(&r.body)
                    .map(|b| b["method"] == "tools/list")
                    .unwrap_or(false)
            })
            .expect("tools/list request captured");
        // Session id from initialize is echoed on subsequent requests.
        assert_eq!(
            tools_req
                .headers
                .get("mcp-session-id")
                .and_then(|v| v.to_str().ok()),
            Some("sess-123")
        );
        // Accept header allows both JSON and SSE responses.
        assert_eq!(
            tools_req
                .headers
                .get("accept")
                .and_then(|v| v.to_str().ok()),
            Some("application/json, text/event-stream")
        );
    }

    #[tokio::test]
    async fn initialize_sends_bearer_token_when_given() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(method("POST"))
            .and(header("authorization", "Bearer sk-test"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_json(json!({
                        "jsonrpc": "2.0", "id": 1,
                        "result": { "protocolVersion": "2025-06-18",
                                    "capabilities": {}, "serverInfo": {"name": "s", "version": "1"} }
                    })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(202))
            .mount(&server)
            .await;

        let mut c = client(&server.uri());
        c.initialize(Some("sk-test")).await.expect("init ok");
        let received = server.received_requests().await.expect("requests");
        assert!(
            received
                .iter()
                .all(|r| r.headers.get("authorization").is_some()),
            "every request must carry the bearer token"
        );
    }

    #[tokio::test]
    async fn initialize_adopts_server_protocol_version() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(method("POST"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_json(json!({
                        "jsonrpc": "2.0", "id": 1,
                        "result": { "protocolVersion": "2025-03-26",
                                    "capabilities": {}, "serverInfo": {"name": "s", "version": "1"} }
                    })),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(202))
            .mount(&server)
            .await;

        let mut c = client(&server.uri());
        c.initialize(None).await.expect("init ok");
        // A follow-up request must carry the adopted version header.
        wiremock::Mock::given(method("POST"))
            .and(header("mcp-protocol-version", "2025-03-26"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_json(json!({ "jsonrpc": "2.0", "id": 2, "result": { "tools": [] } })),
            )
            .mount(&server)
            .await;
        c.list_tools(None).await.expect("list ok");
    }

    #[tokio::test]
    async fn list_tools_maps_rpc_error_to_err() {
        let server = wiremock::MockServer::start().await;
        mount_initialize(&server, "s").await;
        wiremock::Mock::given(method("POST"))
            .and(body_partial_json(json!({ "method": "tools/list" })))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_json(json!({
                        "jsonrpc": "2.0", "id": 2,
                        "error": { "code": -32601, "message": "Method not found" }
                    })),
            )
            .mount(&server)
            .await;

        let mut c = client(&server.uri());
        c.initialize(None).await.expect("init ok");
        let err = c.list_tools(None).await.expect_err("rpc error must Err");
        assert!(err.contains("-32601"), "{err}");
    }
}
