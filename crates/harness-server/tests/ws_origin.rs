//! WebSocket Origin validation tests (cross-site WebSocket hijacking guard):
//! a browser tab sending an `Origin` header must be allowed only when it
//! matches `server.allowed_origins`; native clients (no Origin) always pass.

mod common;

use common::{recv_json, send_text, spawn_server};
use harness_core::config::Config;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::StatusCode;

type Ws = common::Ws;

/// Build a handshake request for the bound server URL with an Origin header.
fn request_with_origin(
    url: &str,
    origin: Option<&str>,
) -> tokio_tungstenite::tungstenite::http::Request<()> {
    let mut req = url.to_string().into_client_request().unwrap();
    if let Some(o) = origin {
        req.headers_mut()
            .insert("origin", o.parse().expect("origin header value"));
    }
    req
}

#[tokio::test]
async fn origin_from_unlisted_browser_is_rejected_with_403() {
    // Default config: empty allowlist → every Origin-bearing request rejected.
    let (url, _config) = spawn_server(Config::default(), Vec::new(), |_| {}, None, None).await;
    let err =
        tokio_tungstenite::connect_async(request_with_origin(&url, Some("https://evil.example")))
            .await
            .expect_err("unlisted origin must be refused");
    match err {
        tokio_tungstenite::tungstenite::Error::Http(resp) => {
            assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{:?}", resp.status());
        }
        other => panic!("expected HTTP error, got: {other}"),
    }
}

#[tokio::test]
async fn no_origin_header_native_client_upgrade_succeeds() {
    let (url, _config) = spawn_server(Config::default(), Vec::new(), |_| {}, None, None).await;
    // No Origin header at all (native clients) → upgrade allowed.
    let mut ws: Ws = tokio_tungstenite::connect_async(request_with_origin(&url, None))
        .await
        .expect("native client (no Origin) must pass")
        .0;
    send_text(
        &mut ws,
        serde_json::json!({ "type": "session.start" }).to_string(),
    )
    .await;
    let ack = recv_json(&mut ws).await;
    assert_eq!(common::type_of(&ack), "state", "session.start acknowledged");
}

#[tokio::test]
async fn listed_origin_upgrade_succeeds() {
    let (url, _config) = spawn_server(
        Config::default(),
        Vec::new(),
        |cfg| {
            cfg.server.allowed_origins = vec!["https://panel.example.com".to_string()];
        },
        None,
        None,
    )
    .await;
    let mut ws: Ws = tokio_tungstenite::connect_async(request_with_origin(
        &url,
        Some("https://panel.example.com"),
    ))
    .await
    .expect("listed origin must pass")
    .0;
    send_text(
        &mut ws,
        serde_json::json!({ "type": "session.start" }).to_string(),
    )
    .await;
    let ack = recv_json(&mut ws).await;
    assert_eq!(common::type_of(&ack), "state", "session.start acknowledged");
}

#[tokio::test]
async fn listed_origin_mismatch_is_rejected() {
    // An allowlist exists but the request carries a DIFFERENT origin.
    let (url, _config) = spawn_server(
        Config::default(),
        Vec::new(),
        |cfg| {
            cfg.server.allowed_origins = vec!["https://panel.example.com".to_string()];
        },
        None,
        None,
    )
    .await;
    let err =
        tokio_tungstenite::connect_async(request_with_origin(&url, Some("https://evil.example")))
            .await
            .expect_err("mismatched origin must be refused");
    match err {
        tokio_tungstenite::tungstenite::Error::Http(resp) => {
            assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        }
        other => panic!("expected HTTP error, got: {other}"),
    }
}
