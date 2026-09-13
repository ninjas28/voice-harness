//! OAuth 2.1 for Streamable-HTTP MCP servers: authorization-code + PKCE,
//! dynamic client registration (RFC 7591), loopback redirect (RFC 8252),
//! metadata discovery (RFC 9728/8414), token persistence + refresh.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use harness_core::config::McpServerConfig;

/// Refresh when the access token is within this margin of expiry.
const EXPIRY_MARGIN_SECS: u64 = 60;

/// A persisted OAuth token for one MCP server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredToken {
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Unix seconds when the access token expires (None = no expiry known).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
}

/// RFC 9728 protected-resource metadata (subset). RFC 9728 spells metadata
/// keys in snake_case; camelCase is accepted as an alias for leniency.
#[derive(Debug, Clone, Deserialize)]
pub struct ProtectedResourceMetadata {
    pub resource: String,
    #[serde(
        default,
        alias = "authorization_servers",
        rename = "authorizationServers"
    )]
    pub authorization_servers: Vec<String>,
    #[serde(default, alias = "scopes_supported", rename = "scopesSupported")]
    pub scopes_supported: Option<Vec<String>>,
}

/// RFC 8414 authorization-server metadata (subset). RFC 8414 spells metadata
/// keys in snake_case; camelCase is accepted as an alias for leniency.
#[derive(Debug, Clone, Deserialize)]
pub struct AuthorizationServerMetadata {
    #[serde(default)]
    pub issuer: String,
    #[serde(alias = "authorization_endpoint", rename = "authorizationEndpoint")]
    pub authorization_endpoint: String,
    #[serde(alias = "token_endpoint", rename = "tokenEndpoint")]
    pub token_endpoint: String,
    #[serde(
        default,
        alias = "registration_endpoint",
        rename = "registrationEndpoint"
    )]
    pub registration_endpoint: Option<String>,
    #[serde(default, alias = "scopes_supported", rename = "scopesSupported")]
    pub scopes_supported: Option<Vec<String>>,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

impl StoredToken {
    pub fn from_response(resp: &TokenResponse) -> Self {
        Self {
            access_token: resp.access_token.clone(),
            refresh_token: resp.refresh_token.clone(),
            expires_at: resp
                .expires_in
                .map(|s| now_secs() + s.saturating_sub(EXPIRY_MARGIN_SECS)),
            client_id: None,
        }
    }

    pub fn needs_refresh(&self) -> bool {
        self.expires_at
            .is_some_and(|exp| now_secs() + EXPIRY_MARGIN_SECS >= exp)
    }
}

/// File-backed store mapping MCP server name → token. Saved with mode 0600.
pub struct TokenStore {
    path: PathBuf,
}

impl TokenStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn load_all(&self) -> HashMap<String, StoredToken> {
        match std::fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
            Err(_) => HashMap::new(),
        }
    }

    pub fn get(&self, server: &str) -> Option<StoredToken> {
        self.load_all().get(server).cloned()
    }

    pub fn put(&self, server: &str, token: StoredToken) -> Result<(), String> {
        let mut all = self.load_all();
        all.insert(server.to_string(), token);
        self.save(&all)
    }

    fn save(&self, map: &HashMap<String, StoredToken>) -> Result<(), String> {
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|e| format!("create token dir: {e}"))?;
            }
        }
        let bytes = serde_json::to_vec_pretty(map).map_err(|e| format!("encode tokens: {e}"))?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, &bytes).map_err(|e| format!("write tokens: {e}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
                .map_err(|e| format!("chmod tokens: {e}"))?;
        }
        std::fs::rename(&tmp, &self.path).map_err(|e| format!("rename tokens: {e}"))?;
        Ok(())
    }
}

/// Token-endpoint response (subset).
#[derive(Debug, Clone, Deserialize)]
pub struct TokenResponse {
    #[serde(rename = "access_token")]
    pub access_token: String,
    #[serde(default, rename = "refresh_token")]
    pub refresh_token: Option<String>,
    #[serde(default, rename = "expires_in")]
    pub expires_in: Option<u64>,
    #[serde(default, rename = "token_type")]
    pub token_type: Option<String>,
}

/// Discovery + token request timeout.
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(10);
/// Hard bound on waiting for the user to finish the browser flow.
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(300);

fn origin_of(url: &str) -> Result<String, String> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .ok_or_else(|| format!("mcp url must be http(s): {url}"))?;
    let end = rest.find('/').unwrap_or(rest.len());
    Ok(url[..url.len() - rest.len() + end].to_string())
}

/// Split `scheme://host/path?…` into (`scheme://host`, `/path?…`).
fn split_origin_path(url: &str) -> (String, String) {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    let scheme_len = url.len() - rest.len();
    match rest.find(['/', '?']) {
        Some(i) => (url[..scheme_len + i].to_string(), rest[i..].to_string()),
        None => (url.to_string(), String::new()),
    }
}

/// (primary, fallback) well-known URLs for an authorization-server issuer
/// (RFC 8414 path-aware insertion when the issuer has a path).
fn auth_server_well_known(issuer: &str) -> (String, String) {
    let (origin, path) = split_origin_path(issuer.trim_end_matches('/'));
    let base = format!("{origin}/.well-known/oauth-authorization-server");
    if path.is_empty() {
        (base.clone(), base)
    } else {
        (format!("{base}{path}"), base)
    }
}

/// Extract `resource_metadata="…"` from a WWW-Authenticate header, else the
/// default well-known for the server origin.
fn resource_metadata_url(www_authenticate: Option<&str>, server_url: &str) -> String {
    if let Some(h) = www_authenticate {
        const KEY: &str = "resource_metadata=\"";
        if let Some(pos) = h.find(KEY) {
            let rest = &h[pos + KEY.len()..];
            if let Some(end) = rest.find('"') {
                return rest[..end].to_string();
            }
        }
    }
    format!(
        "{}/.well-known/oauth-protected-resource",
        origin_of(server_url).unwrap_or_default()
    )
}

async fn fetch_json(client: &reqwest::Client, url: &str) -> Result<serde_json::Value, String> {
    let resp = tokio::time::timeout(DISCOVERY_TIMEOUT, client.get(url).send())
        .await
        .map_err(|_| format!("discovery timed out: {url}"))?
        .map_err(|e| format!("GET {url} failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("GET {url} -> HTTP {}", resp.status().as_u16()));
    }
    resp.json()
        .await
        .map_err(|e| format!("bad JSON from {url}: {e}"))
}

/// Discover OAuth metadata for `server_url`: POST an unauthenticated
/// `initialize` probe, expect 401, take the `WWW-Authenticate`
/// `resource_metadata` hint (or the default well-known), then fetch the
/// protected-resource and authorization-server metadata documents.
pub async fn probe_and_discover(
    server_url: &str,
) -> Result<(ProtectedResourceMetadata, AuthorizationServerMetadata), String> {
    let client = reqwest::Client::builder()
        .timeout(DISCOVERY_TIMEOUT)
        .build()
        .map_err(|e| format!("http client: {e}"))?;

    let probe_body = json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "voice-harness", "version": env!("CARGO_PKG_VERSION") }
        }
    });
    let resp = tokio::time::timeout(
        DISCOVERY_TIMEOUT,
        client
            .post(server_url)
            .header("Accept", "application/json, text/event-stream")
            .json(&probe_body)
            .send(),
    )
    .await
    .map_err(|_| "authorization probe timed out".to_string())?
    .map_err(|e| format!("authorization probe failed: {e}"))?;

    let prm_url = if resp.status().as_u16() == 401 {
        let header = resp
            .headers()
            .get("www-authenticate")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        resource_metadata_url(header.as_deref(), server_url)
    } else {
        format!(
            "{}/.well-known/oauth-protected-resource",
            origin_of(server_url)?
        )
    };

    let prm: ProtectedResourceMetadata =
        serde_json::from_value(fetch_json(&client, &prm_url).await?)
            .map_err(|e| format!("bad protected-resource metadata: {e}"))?;
    let issuer = prm
        .authorization_servers
        .first()
        .ok_or_else(|| "protected-resource metadata lists no authorization servers".to_string())?
        .clone();
    let (primary, fallback) = auth_server_well_known(&issuer);
    let asm: AuthorizationServerMetadata =
        serde_json::from_value(match fetch_json(&client, &primary).await {
            Ok(v) => v,
            Err(e) => fetch_json(&client, &fallback).await.map_err(|e2| {
                format!("no authorization-server metadata at {primary} ({e}) or {fallback} ({e2})")
            })?,
        })
        .map_err(|e| format!("bad authorization-server metadata: {e}"))?;
    Ok((prm, asm))
}

/// Random 64-hex-char PKCE verifier + its S256 challenge (RFC 7636).
pub fn pkce_pair() -> Result<(String, String), String> {
    use base64::Engine as _;
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).map_err(|e| format!("rng: {e}"))?;
    let verifier: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(verifier.as_bytes());
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest.as_slice());
    Ok((verifier, challenge))
}

/// Random hex state for CSRF protection.
pub fn random_state() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).map_err(|e| format!("rng: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Minimal percent-encoding for query parameters (RFC 3986 unreserved kept).
fn enc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Build the authorization-endpoint URL for the code + PKCE flow.
pub fn authorize_url(
    metadata: &AuthorizationServerMetadata,
    client_id: &str,
    redirect_uri: &str,
    scopes: &[String],
    state: &str,
    code_challenge: &str,
    resource: Option<&str>,
) -> String {
    let mut url = format!(
        "{}?response_type=code&client_id={}&redirect_uri={}&state={}&code_challenge={}&code_challenge_method=S256",
        metadata.authorization_endpoint,
        enc(client_id),
        enc(redirect_uri),
        enc(state),
        enc(code_challenge),
    );
    if !scopes.is_empty() {
        url.push_str(&format!("&scope={}", enc(&scopes.join(" "))));
    }
    if let Some(r) = resource {
        url.push_str(&format!("&resource={}", enc(r)));
    }
    url
}

/// Register an OAuth client via RFC 7591 dynamic client registration
/// (public client: PKCE, no secret).
pub async fn register_client(
    registration_endpoint: &str,
    redirect_uri: &str,
    client_name: &str,
) -> Result<String, String> {
    let client = reqwest::Client::builder()
        .timeout(DISCOVERY_TIMEOUT)
        .build()
        .map_err(|e| format!("http client: {e}"))?;
    let body = json!({
        "client_name": client_name,
        "redirect_uris": [redirect_uri],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
    });
    let resp = tokio::time::timeout(
        DISCOVERY_TIMEOUT,
        client.post(registration_endpoint).json(&body).send(),
    )
    .await
    .map_err(|_| "client registration timed out".to_string())?
    .map_err(|e| format!("client registration failed: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        return Err(format!(
            "client registration -> HTTP {status}: {}",
            truncate(&text, 300)
        ));
    }
    let v: Value = resp
        .json()
        .await
        .map_err(|e| format!("bad registration response: {e}"))?;
    v.get("client_id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| "registration response missing client_id".to_string())
}

fn truncate(s: &str, max_chars: usize) -> String {
    s.chars().take(max_chars).collect()
}

/// One-shot loopback HTTP listener capturing the OAuth redirect (RFC 8252 §7).
pub struct AuthorizationListener {
    listener: tokio::net::TcpListener,
    redirect_uri: String,
}

impl AuthorizationListener {
    /// Bind `127.0.0.1:<ephemeral>`; the redirect URI is
    /// `http://127.0.0.1:<port>/callback`.
    pub async fn bind() -> Result<Self, String> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| format!("bind loopback listener: {e}"))?;
        let port = listener
            .local_addr()
            .map_err(|e| format!("local addr: {e}"))?
            .port();
        Ok(Self {
            listener,
            redirect_uri: format!("http://127.0.0.1:{port}/callback"),
        })
    }

    pub fn redirect_uri(&self) -> &str {
        &self.redirect_uri
    }

    /// Wait (≤300 s, hard-bounded) for the browser redirect; verify `state`;
    /// return the percent-decoded code.
    pub async fn wait_code(&self, expected_state: &str) -> Result<String, String> {
        let fut = async {
            let (mut sock, _) = self
                .listener
                .accept()
                .await
                .map_err(|e| format!("accept: {e}"))?;
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            // Read the request head (request line + headers).
            loop {
                let n = sock
                    .read(&mut chunk)
                    .await
                    .map_err(|e| format!("read: {e}"))?;
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() > 16 * 1024 {
                    break;
                }
            }
            let head = String::from_utf8_lossy(&buf);
            let request_line = head.lines().next().unwrap_or_default();
            // e.g. "GET /callback?code=…&state=… HTTP/1.1"
            let target = request_line.split_whitespace().nth(1).unwrap_or_default();
            let params = target.rsplit_once('?').map(|(_, q)| q).unwrap_or("");
            let mut code = None;
            let mut state = None;
            let mut error = None;
            for kv in params.split('&') {
                let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
                match k {
                    "code" => code = Some(dec(v)),
                    "state" => state = Some(dec(v)),
                    "error" => error = Some(dec(v)),
                    _ => {}
                }
            }

            let body_text = if error.is_some() {
                "<html><body><h3>Authorization failed — you can close this window.</h3></body></html>"
                    .to_string()
            } else {
                "<html><body><h3>Authorization received — you can close this window.</h3></body></html>"
                    .to_string()
            };
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
                body_text.len(),
                body_text
            );
            let _ = sock.write_all(resp.as_bytes()).await;
            let _ = sock.shutdown().await;

            if let Some(err) = error {
                return Err(format!("authorization server returned error: {err}"));
            }
            if state.as_deref() != Some(expected_state) {
                return Err("state mismatch in OAuth callback (possible CSRF)".to_string());
            }
            code.ok_or_else(|| "OAuth callback missing 'code' parameter".to_string())
        };
        tokio::time::timeout(CALLBACK_TIMEOUT, fut)
            .await
            .map_err(|_| "timed out waiting for the OAuth redirect".to_string())?
    }
}

/// Percent-decode a query parameter value (`+` → space).
fn dec(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && b.len() >= i + 3 {
            if let Ok(v) =
                u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("zz"), 16)
            {
                out.push(v);
                i += 3;
                continue;
            }
        }
        if b[i] == b'+' {
            out.push(b' ');
            i += 1;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

async fn post_form(
    client: &reqwest::Client,
    url: &str,
    form: &[(&str, &str)],
) -> Result<TokenResponse, String> {
    let resp = tokio::time::timeout(DISCOVERY_TIMEOUT, client.post(url).form(form).send())
        .await
        .map_err(|_| "token request timed out".to_string())?
        .map_err(|e| format!("token request failed: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!(
            "token endpoint -> HTTP {status}: {}",
            truncate(&body, 300)
        ));
    }
    resp.json()
        .await
        .map_err(|e| format!("bad token response: {e}"))
}

fn http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(DISCOVERY_TIMEOUT)
        .build()
        .map_err(|e| format!("http client: {e}"))
}

/// Exchange an authorization code for tokens (PKCE verifier attached,
/// RFC 8707 `resource` included when known).
pub async fn exchange_code(
    token_endpoint: &str,
    code: &str,
    verifier: &str,
    client_id: &str,
    redirect_uri: &str,
    resource: Option<&str>,
) -> Result<TokenResponse, String> {
    let client = http_client()?;
    let mut form = vec![
        ("grant_type", "authorization_code"),
        ("code", code),
        ("code_verifier", verifier),
        ("client_id", client_id),
        ("redirect_uri", redirect_uri),
    ];
    if let Some(r) = resource {
        form.push(("resource", r));
    }
    post_form(&client, token_endpoint, &form).await
}

/// Refresh an access token (rotating refresh tokens are stored by the caller).
pub async fn refresh_token(
    token_endpoint: &str,
    refresh_token: &str,
    client_id: &str,
    resource: Option<&str>,
) -> Result<TokenResponse, String> {
    let client = http_client()?;
    let mut form = vec![
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", client_id),
    ];
    if let Some(r) = resource {
        form.push(("resource", r));
    }
    post_form(&client, token_endpoint, &form).await
}

/// Interactive end-to-end flow: discover → register (or static client_id) →
/// open the browser → capture the loopback callback → exchange → persist.
/// Prints the URL it opened so a headless operator can copy-paste it.
pub async fn run_authorization_flow(
    server: &McpServerConfig,
    token_store_path: &str,
) -> Result<(), String> {
    let (prm, asm) = probe_and_discover(&server.url).await?;
    let listener = AuthorizationListener::bind().await?;
    let redirect_uri = listener.redirect_uri().to_string();

    let client_id = if server.client_id.is_empty() {
        let Some(endpoint) = &asm.registration_endpoint else {
            return Err(
                "server supports neither dynamic registration nor a configured client_id"
                    .to_string(),
            );
        };
        register_client(
            endpoint,
            &redirect_uri,
            &format!("voice-harness ({})", server.name),
        )
        .await?
    } else {
        server.client_id.clone()
    };

    let (verifier, challenge) = pkce_pair()?;
    let state = random_state()?;
    let url = authorize_url(
        &asm,
        &client_id,
        &redirect_uri,
        &server.scopes,
        &state,
        &challenge,
        Some(prm.resource.as_str()),
    );

    println!("Open this URL to authorize '{}':\n  {url}", server.name);
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(&url).spawn();
    #[cfg(not(target_os = "macos"))]
    let _ = std::process::Command::new("xdg-open").arg(&url).spawn();

    let code = listener.wait_code(&state).await?;
    let resp = exchange_code(
        &asm.token_endpoint,
        &code,
        &verifier,
        &client_id,
        &redirect_uri,
        Some(prm.resource.as_str()),
    )
    .await?;
    let mut token = StoredToken::from_response(&resp);
    token.client_id = Some(client_id);
    TokenStore::new(token_store_path).put(&server.name, token)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store(tag: &str) -> (TokenStore, PathBuf) {
        let dir = std::env::temp_dir().join(format!("vh-oauth-{}-{}", tag, std::process::id()));
        let path = dir.join("tokens.json");
        (TokenStore::new(&path), path)
    }

    #[test]
    fn token_store_round_trips_and_defaults_to_empty() {
        let (store, path) = temp_store("roundtrip");
        assert!(store.get("weather").is_none(), "missing file → empty map");

        store
            .put(
                "weather",
                StoredToken {
                    access_token: "at".into(),
                    refresh_token: Some("rt".into()),
                    expires_at: Some(1_000),
                    client_id: Some("cid".into()),
                },
            )
            .expect("put ok");
        let t = store.get("weather").expect("token stored");
        assert_eq!(t.access_token, "at");
        assert_eq!(t.refresh_token.as_deref(), Some("rt"));

        // A fresh store over the same file sees the persisted token.
        let reopened = TokenStore::new(&path);
        assert_eq!(
            reopened.get("weather").expect("persisted").access_token,
            "at"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).expect("meta").permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "token file must be owner-only");
        }
    }

    #[test]
    fn expiry_margin_triggers_refresh() {
        let now = now_secs();
        let fresh = StoredToken {
            access_token: "a".into(),
            refresh_token: None,
            expires_at: Some(now + 600),
            client_id: None,
        };
        let stale = StoredToken {
            access_token: "b".into(),
            refresh_token: None,
            expires_at: Some(now + 10),
            client_id: None,
        };
        assert!(!fresh.needs_refresh());
        assert!(stale.needs_refresh());
    }

    #[test]
    fn stored_token_from_response_applies_margin() {
        let resp = TokenResponse {
            access_token: "x".into(),
            refresh_token: Some("r".into()),
            expires_in: Some(3600),
            token_type: Some("Bearer".into()),
        };
        let t = StoredToken::from_response(&resp);
        assert_eq!(t.expires_at, Some(now_secs() + 3600 - EXPIRY_MARGIN_SECS));
    }

    use serde_json::json;
    use wiremock::matchers::{method, path};

    async fn mount_oauth_metadata(server: &wiremock::MockServer) {
        // Unauthenticated initialize probe → 401 with a resource_metadata hint.
        wiremock::Mock::given(method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(401).insert_header(
                "WWW-Authenticate",
                format!(
                    "Bearer resource_metadata=\"{}/.well-known/oauth-protected-resource\"",
                    server.uri()
                ),
            ))
            .mount(server)
            .await;
        wiremock::Mock::given(method("GET"))
            .and(path("/.well-known/oauth-protected-resource"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({
                "resource": "https://mcp.example.com/mcp",
                "authorization_servers": [server.uri()],
                "scopes_supported": ["tools.read"]
            })))
            .mount(server)
            .await;
        wiremock::Mock::given(method("GET"))
            .and(path("/.well-known/oauth-authorization-server"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({
                "issuer": server.uri(),
                "authorization_endpoint": format!("{}/authorize", server.uri()),
                "token_endpoint": format!("{}/token", server.uri()),
                "registration_endpoint": format!("{}/register", server.uri()),
                "code_challenge_methods_supported": ["S256"]
            })))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn probe_and_discover_walks_both_metadata_documents() {
        let server = wiremock::MockServer::start().await;
        mount_oauth_metadata(&server).await;
        let (prm, asm) = probe_and_discover(&server.uri()).await.expect("discovered");
        assert_eq!(prm.resource, "https://mcp.example.com/mcp");
        assert_eq!(
            asm.authorization_endpoint,
            format!("{}/authorize", server.uri())
        );
        assert_eq!(asm.token_endpoint, format!("{}/token", server.uri()));
        assert_eq!(
            asm.registration_endpoint.as_deref(),
            Some(format!("{}/register", server.uri()).as_str())
        );
    }

    #[tokio::test]
    async fn discovery_falls_back_to_default_well_known_without_www_authenticate() {
        let server = wiremock::MockServer::start().await;
        // Probe returns 401 with NO WWW-Authenticate header.
        wiremock::Mock::given(method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(401))
            .mount(&server)
            .await;
        mount_oauth_metadata(&server).await; // GET documents still mounted
                                             // (The 401 mock above is mounted first, so the probe hits it; the
                                             // discovery GETs fall through to the metadata mocks.)
        let (prm, _) = probe_and_discover(&server.uri()).await.expect("discovered");
        assert!(!prm.authorization_servers.is_empty());
    }

    #[tokio::test]
    async fn discovery_fails_cleanly_without_metadata() {
        let server = wiremock::MockServer::start().await; // no mocks at all
        let err = probe_and_discover(&server.uri())
            .await
            .expect_err("must fail");
        assert!(!err.is_empty());
    }

    #[test]
    fn pkce_pair_is_random_and_challenge_is_s256_of_verifier() {
        let (v1, c1) = pkce_pair().expect("pair");
        let (v2, c2) = pkce_pair().expect("pair");
        assert_ne!(v1, v2, "verifiers must be random");
        assert_eq!(v1.len(), 64, "64 hex chars");
        // Challenge = base64url-no-pad(SHA256(verifier)) — verify the wiring
        // with a known input via the same primitives.
        use base64::Engine as _;
        use sha2::{Digest, Sha256};
        let expected =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(v1.as_bytes()));
        assert_eq!(c1, expected);
        assert_ne!(c1, c2);
        assert!(!c1.contains('+') && !c1.contains('/'), "url-safe alphabet");
    }

    #[test]
    fn random_state_differs_between_calls() {
        let s1 = random_state().expect("state");
        let s2 = random_state().expect("state");
        assert_ne!(s1, s2);
        assert_eq!(s1.len(), 32);
    }

    #[test]
    fn authorize_url_carries_all_required_params() {
        let asm = AuthorizationServerMetadata {
            issuer: "https://auth.example.com".into(),
            authorization_endpoint: "https://auth.example.com/authorize".into(),
            token_endpoint: "https://auth.example.com/token".into(),
            registration_endpoint: None,
            scopes_supported: None,
        };
        let url = authorize_url(
            &asm,
            "client-1",
            "http://127.0.0.1:54321/callback",
            &["tools.read".to_string(), "tools.write".to_string()],
            "state123",
            "challengeXYZ",
            Some("https://mcp.example.com/mcp"),
        );
        assert!(
            url.starts_with("https://auth.example.com/authorize?"),
            "{url}"
        );
        assert!(url.contains("response_type=code"), "{url}");
        assert!(url.contains("client_id=client-1"), "{url}");
        assert!(
            url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A54321%2Fcallback"),
            "{url}"
        );
        assert!(url.contains("state=state123"), "{url}");
        assert!(
            url.contains("code_challenge=challengeXYZ&code_challenge_method=S256"),
            "{url}"
        );
        assert!(url.contains("scope=tools.read%20tools.write"), "{url}");
        assert!(
            url.contains("resource=https%3A%2F%2Fmcp.example.com%2Fmcp"),
            "{url}"
        );
    }

    #[tokio::test]
    async fn register_client_posts_rfc7591_body_and_returns_client_id() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(method("POST"))
            .and(path("/register"))
            .respond_with(
                wiremock::ResponseTemplate::new(201)
                    .set_body_json(json!({ "client_id": "cid-42" })),
            )
            .mount(&server)
            .await;

        let cid = register_client(
            &format!("{}/register", server.uri()),
            "http://127.0.0.1:5000/callback",
            "voice-harness (test)",
        )
        .await
        .expect("registered");
        assert_eq!(cid, "cid-42");

        let req = server
            .received_requests()
            .await
            .expect("requests")
            .into_iter()
            .next()
            .expect("a request");
        let body: Value = serde_json::from_slice(&req.body).expect("json body");
        assert_eq!(body["grant_types"][0], "authorization_code");
        assert_eq!(body["grant_types"][1], "refresh_token");
        assert_eq!(body["token_endpoint_auth_method"], "none");
        assert_eq!(body["redirect_uris"][0], "http://127.0.0.1:5000/callback");
    }

    #[tokio::test]
    async fn register_client_errors_without_client_id_in_response() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(201).set_body_json(json!({})))
            .mount(&server)
            .await;
        let err = register_client(
            &format!("{}/register", server.uri()),
            "http://127.0.0.1:1/callback",
            "x",
        )
        .await
        .expect_err("missing client_id must error");
        assert!(err.contains("client_id"), "{err}");
    }

    #[tokio::test]
    async fn wait_code_captures_code_and_verifies_state() {
        // Arc: TcpListener is not Clone and tokio::spawn needs 'static.
        let listener = std::sync::Arc::new(AuthorizationListener::bind().await.expect("bind"));
        let redirect = listener.redirect_uri().to_string();
        assert!(redirect.starts_with("http://127.0.0.1:"), "{redirect}");

        let handle = tokio::spawn({
            let listener = listener.clone();
            async move { listener.wait_code("expected-state").await }
        });
        // Simulate the browser redirect (percent-encoded code included).
        let browser_url = format!("{redirect}?code=abc%2Fdef.XY&state=expected-state");
        let resp = reqwest::get(&browser_url).await.expect("browser GET");
        assert!(resp.status().is_success());

        let code = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("no test hang")
            .expect("join")
            .expect("code captured");
        assert_eq!(code, "abc/def.XY", "percent-encoding must be decoded");
    }

    #[tokio::test]
    async fn wait_code_rejects_state_mismatch() {
        let listener = AuthorizationListener::bind().await.expect("bind");
        let redirect = listener.redirect_uri().to_string();
        let handle = tokio::spawn(async move { listener.wait_code("right").await });
        let browser_url = format!("{redirect}?code=x&state=wrong");
        let _ = reqwest::get(&browser_url).await;
        let err = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("no test hang")
            .expect("join")
            .expect_err("state mismatch must error");
        assert!(err.contains("state mismatch"), "{err}");
    }

    #[tokio::test]
    async fn wait_code_surfaces_authorization_error() {
        let listener = AuthorizationListener::bind().await.expect("bind");
        let redirect = listener.redirect_uri().to_string();
        let handle = tokio::spawn(async move { listener.wait_code("s").await });
        let browser_url = format!("{redirect}?error=access_denied");
        let _ = reqwest::get(&browser_url).await;
        let err = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("no test hang")
            .expect("join")
            .expect_err("error param must error");
        assert!(err.contains("access_denied"), "{err}");
    }

    #[tokio::test]
    async fn exchange_code_posts_pkce_verifier_and_stores_token() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "at-1", "refresh_token": "rt-1",
                "expires_in": 3600, "token_type": "Bearer"
            })))
            .mount(&server)
            .await;

        let resp = exchange_code(
            &format!("{}/token", server.uri()),
            "the-code",
            "the-verifier",
            "cid",
            "http://127.0.0.1:5000/callback",
            Some("https://mcp.example.com/mcp"),
        )
        .await
        .expect("exchanged");
        assert_eq!(resp.access_token, "at-1");

        let req = server
            .received_requests()
            .await
            .expect("requests")
            .remove(0);
        let body = String::from_utf8_lossy(&req.body).to_string();
        assert!(body.contains("grant_type=authorization_code"), "{body}");
        assert!(body.contains("code=the-code"), "{body}");
        assert!(body.contains("code_verifier=the-verifier"), "{body}");
        assert!(
            body.contains("resource=https%3A%2F%2Fmcp.example.com%2Fmcp"),
            "{body}"
        );
    }

    #[tokio::test]
    async fn refresh_posts_refresh_grant_and_rotates() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "at-2", "refresh_token": "rt-2",
                "expires_in": 1800
            })))
            .mount(&server)
            .await;

        let resp = refresh_token(&format!("{}/token", server.uri()), "rt-1", "cid", None)
            .await
            .expect("refreshed");
        assert_eq!(resp.access_token, "at-2");
        assert_eq!(resp.refresh_token.as_deref(), Some("rt-2"));

        let body = String::from_utf8_lossy(
            &server
                .received_requests()
                .await
                .expect("requests")
                .remove(0)
                .body,
        )
        .to_string();
        assert!(body.contains("grant_type=refresh_token"), "{body}");
        assert!(body.contains("refresh_token=rt-1"), "{body}");
    }

    #[tokio::test]
    async fn token_endpoint_error_is_mapped() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(method("POST"))
            .respond_with(
                wiremock::ResponseTemplate::new(400)
                    .set_body_string("{\"error\":\"invalid_grant\"}"),
            )
            .mount(&server)
            .await;
        let err = refresh_token(&format!("{}/token", server.uri()), "rt", "cid", None)
            .await
            .expect_err("must fail");
        assert!(err.contains("400"), "{err}");
    }
}
