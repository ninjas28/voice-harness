//! URL validation tests for the web_search plugin (SSRF guard). The
//! LLM supplies target URLs; they must never reach the Firecrawl service
//! when they point at loopback/private/link-local/ULA literals or use a
//! non-http(s) scheme. Literal-only tests: no DNS in this suite.

use harness_plugins::builtin::web_search::{check_url_literal, ensure_public_host};

#[test]
fn loopback_literal_is_rejected() {
    assert!(check_url_literal("http://127.0.0.1/x").is_err());
    assert!(check_url_literal("http://127.9.9.9/x").is_err());
}

#[test]
fn private_range_literal_is_rejected() {
    assert!(check_url_literal("http://192.168.1.5/x").is_err());
    assert!(check_url_literal("http://10.0.0.3/x").is_err());
    assert!(check_url_literal("http://172.16.0.2/x").is_err());
}

#[test]
fn link_local_metadata_literal_is_rejected() {
    assert!(check_url_literal("http://169.254.169.254/latest/meta-data").is_err());
}

#[test]
fn unique_local_ipv6_literal_is_rejected() {
    assert!(check_url_literal("http://[fd00::1]/x").is_err());
    assert!(check_url_literal("http://[::1]/x").is_err());
}

#[test]
fn non_http_scheme_is_rejected() {
    assert!(check_url_literal("file:///etc/passwd").is_err());
    assert!(check_url_literal("ftp://example.com/x").is_err());
}

#[test]
fn public_https_url_passes_literal_check() {
    assert!(check_url_literal("https://example.com/x").is_ok());
    assert!(check_url_literal("http://example.com/x").is_ok());
}

#[test]
fn unparseable_url_is_rejected() {
    assert!(check_url_literal("not a url").is_err());
}

#[tokio::test]
async fn ensure_public_host_matches_literal_verdict_for_literal_hosts() {
    // Literal hosts need no DNS: ensure_public_host's verdict must agree
    // with the literal check (reject private/loopback, accept public).
    // Name-host resolution paths are deliberately NOT tested here (no DNS
    // in tests, per repo policy); literals exercise the same IP checks.
    assert!(ensure_public_host("http://127.0.0.1/x").await.is_err());
    assert!(ensure_public_host("http://192.168.1.5/x").await.is_err());
    assert!(ensure_public_host("http://169.254.169.254/x")
        .await
        .is_err());
    // A public IP literal passes (no DNS involved).
    assert!(ensure_public_host("http://172.66.147.243/x").await.is_ok());
}
