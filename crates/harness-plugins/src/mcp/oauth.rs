//! OAuth 2.1 for Streamable-HTTP MCP servers: authorization-code + PKCE,
//! dynamic client registration (RFC 7591), loopback redirect (RFC 8252),
//! metadata discovery (RFC 9728/8414), token persistence + refresh.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::json;

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
}
