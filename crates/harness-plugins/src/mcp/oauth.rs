//! OAuth 2.1 for Streamable-HTTP MCP servers: authorization-code + PKCE,
//! dynamic client registration (RFC 7591), loopback redirect (RFC 8252),
//! metadata discovery (RFC 9728/8414), token persistence + refresh.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

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

/// RFC 9728 protected-resource metadata (subset).
#[derive(Debug, Clone, Deserialize)]
pub struct ProtectedResourceMetadata {
    pub resource: String,
    #[serde(default, rename = "authorizationServers")]
    pub authorization_servers: Vec<String>,
    #[serde(default, rename = "scopesSupported")]
    pub scopes_supported: Option<Vec<String>>,
}

/// RFC 8414 authorization-server metadata (subset).
#[derive(Debug, Clone, Deserialize)]
pub struct AuthorizationServerMetadata {
    #[serde(default)]
    pub issuer: String,
    #[serde(rename = "authorizationEndpoint")]
    pub authorization_endpoint: String,
    #[serde(rename = "tokenEndpoint")]
    pub token_endpoint: String,
    #[serde(default, rename = "registrationEndpoint")]
    pub registration_endpoint: Option<String>,
    #[serde(default, rename = "scopesSupported")]
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
}
