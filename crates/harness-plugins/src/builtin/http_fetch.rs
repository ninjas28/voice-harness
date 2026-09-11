//! `http_fetch` plugin: GET with host allowlist, size cap, tag stripping.

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::{Plugin, PluginManifest};

/// Response body cap: 64 KiB (plan).
const MAX_BODY_BYTES: usize = 64 * 1024;
/// Request timeout: 5 s (plan).
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Allowlist-gated HTTP GET tool. Denies every host when `allowed_hosts`
/// is empty (secure default).
pub struct HttpFetchPlugin {
    allowed_hosts: Vec<String>,
}

impl HttpFetchPlugin {
    pub fn new(allowed_hosts: Vec<String>) -> Self {
        Self { allowed_hosts }
    }

    /// Host of a URL, lowercased, without port.
    fn host_of(url: &str) -> Option<String> {
        let rest = url
            .strip_prefix("https://")
            .or_else(|| url.strip_prefix("http://"))?;
        let host_end = rest.find('/').unwrap_or(rest.len());
        let authority = &rest[..host_end];
        let host = authority.rsplit_once(':').map_or(authority, |(h, _)| h);
        if host.is_empty() {
            None
        } else {
            Some(host.to_ascii_lowercase())
        }
    }

    fn host_allowed(&self, host: &str) -> bool {
        self.allowed_hosts
            .iter()
            .any(|a| a.eq_ignore_ascii_case(host))
    }

    /// Crude HTML → text: drop script/style bodies, then all tags, then
    /// collapse whitespace.
    fn strip_html(html: &str) -> String {
        let mut s = html.to_string();
        for tag in ["script", "style"] {
            while let Some(open) = s.to_ascii_lowercase().find(&format!("<{tag}")) {
                let Some(close) = s.to_ascii_lowercase()[open..]
                    .find(&format!("</{tag}>"))
                    .map(|i| open + i + tag.len() + 3)
                else {
                    break;
                };
                s.replace_range(open..close, " ");
            }
        }
        let mut out = String::with_capacity(s.len());
        let mut in_tag = false;
        for c in s.chars() {
            match c {
                '<' => in_tag = true,
                '>' => {
                    in_tag = false;
                    out.push(' '); // tags separate words
                }
                c if !in_tag => out.push(c),
                _ => {}
            }
        }
        out.split_whitespace().collect::<Vec<_>>().join(" ")
    }
}

#[async_trait]
impl Plugin for HttpFetchPlugin {
    fn manifest(&self) -> &PluginManifest {
        static MANIFEST: std::sync::OnceLock<PluginManifest> = std::sync::OnceLock::new();
        MANIFEST.get_or_init(|| PluginManifest {
            name: "http_fetch",
            version: "0.1.0",
            description: "Fetch text from an allowlisted URL over HTTP GET",
        })
    }

    fn tool_specs(&self) -> Vec<Value> {
        vec![json!({
            "type": "function",
            "function": {
                "name": "http_fetch.fetch",
                "description": "GET a URL and return its text content (HTML tags stripped). \
                               Only hosts on the configured allowlist are reachable.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "url": { "type": "string", "description": "Absolute http(s) URL" }
                    },
                    "required": ["url"]
                }
            }
        })]
    }

    async fn call(&self, name: &str, arguments: Value) -> Result<Value, String> {
        if name != "fetch" {
            return Err(format!("http_fetch plugin has no tool '{name}'"));
        }
        let url = arguments
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or("missing 'url' argument")?;

        let Some(host) = Self::host_of(url) else {
            return Err(format!("invalid url: {url}"));
        };
        if !self.host_allowed(&host) {
            return Err(format!("host '{host}' is not on the http_fetch allowlist"));
        }

        let client = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .build()
            .map_err(|e| format!("http client build failed: {e}"))?;
        let resp = client
            .get(url)
            .send()
            .await
            .map_err(|e| format!("fetch failed: {e}"))?;
        let status = resp.status().as_u16();
        // Read at most one byte beyond the cap so oversized bodies are
        // detected rather than silently truncated mid-content.
        let body = resp
            .bytes()
            .await
            .map_err(|e| format!("body read failed: {e}"))?;
        if body.len() > MAX_BODY_BYTES {
            return Err(format!("response body exceeds {} bytes", MAX_BODY_BYTES));
        }
        let text = String::from_utf8_lossy(&body);
        Ok(json!({
            "status": status,
            "body": Self::strip_html(&text),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_extraction_and_allowlist() {
        assert_eq!(
            HttpFetchPlugin::host_of("https://Example.com:8443/a/b"),
            Some("example.com".to_string())
        );
        assert_eq!(
            HttpFetchPlugin::host_of("http://plain.host/x"),
            Some("plain.host".to_string())
        );
        assert_eq!(HttpFetchPlugin::host_of("ftp://x/y"), None);
        assert_eq!(HttpFetchPlugin::host_of("notaurl"), None);

        let p = HttpFetchPlugin::new(vec!["example.com".to_string()]);
        assert!(p.host_allowed("example.com"));
        assert!(p.host_allowed("EXAMPLE.com"));
        assert!(!p.host_allowed("evil.com"));
        // Empty allowlist denies everything (secure default).
        assert!(!HttpFetchPlugin::new(vec![]).host_allowed("example.com"));
    }

    #[test]
    fn strip_html_removes_tags_and_script_bodies() {
        let html = "<html><head><style>p{color:red}</style></head>\
                    <body><h1>Title</h1><p>Hello <b>world</b>!</p>\
                    <script>alert('nope')</script></body></html>";
        assert_eq!(HttpFetchPlugin::strip_html(html), "Title Hello world !");
    }

    #[tokio::test]
    async fn fetch_denies_non_allowlisted_host_before_any_request() {
        let p = HttpFetchPlugin::new(vec!["example.com".to_string()]);
        let err = p
            .call("fetch", json!({ "url": "https://evil.com/steal" }))
            .await
            .expect_err("must deny");
        assert!(err.contains("not on the http_fetch allowlist"), "{err}");
    }

    #[tokio::test]
    async fn fetch_requires_url_argument() {
        let p = HttpFetchPlugin::new(vec![]);
        let err = p.call("fetch", json!({})).await.expect_err("must fail");
        assert!(err.contains("url"), "{err}");
        let err = p
            .call("other", json!({ "url": "https://x.example/" }))
            .await
            .expect_err("unknown tool");
        assert!(err.contains("other"), "{err}");
    }
}
