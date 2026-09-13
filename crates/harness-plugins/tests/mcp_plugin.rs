//! Integration tests: the `mcp` plugin end-to-end against wiremock servers.

use harness_core::config::{McpConfig, McpServerConfig, PluginsConfig, WebSearchConfig};
use harness_plugins::mcp::oauth::{StoredToken, TokenStore};
use harness_plugins::{registry_from_config, PluginRegistry};
use serde_json::{json, Value};
use wiremock::matchers::{body_partial_json, header, method, path};

fn spec_names(registry: &PluginRegistry) -> Vec<String> {
    registry
        .tool_specs()
        .iter()
        .filter_map(|s| {
            s.get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .map(str::to_string)
        })
        .collect()
}

/// Mounts a full MCP server surface on `server` (initialize, notification,
/// tools/list, tools/call) — the caller adds tools to the tools/list mock.
/// When `bearer` is set, every mock requires that `Authorization` header, so
/// the test proves the plugin actually attaches the credential.
async fn mount_mcp_surface(
    server: &wiremock::MockServer,
    session: &str,
    tools: Value,
    bearer: Option<&str>,
) {
    let mut initialize = wiremock::Mock::given(method("POST"))
        .and(body_partial_json(json!({ "method": "initialize" })));
    let mut notify = wiremock::Mock::given(method("POST")).and(body_partial_json(
        json!({ "method": "notifications/initialized" }),
    ));
    let mut list = wiremock::Mock::given(method("POST"))
        .and(body_partial_json(json!({ "method": "tools/list" })));
    let mut call = wiremock::Mock::given(method("POST"))
        .and(body_partial_json(json!({ "method": "tools/call" })));
    if let Some(expected) = bearer {
        let pattern = format!("Bearer {expected}");
        let make = |m: wiremock::MockBuilder| m.and(header("authorization", pattern.clone()));
        initialize = make(initialize);
        notify = make(notify);
        list = make(list);
        call = make(call);
    }
    initialize
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .insert_header("Mcp-Session-Id", session)
                .set_body_json(json!({
                    "jsonrpc": "2.0", "id": 1,
                    "result": { "protocolVersion": "2025-06-18",
                                "capabilities": {"tools": {}},
                                "serverInfo": {"name": "t", "version": "1"} }
                })),
        )
        .mount(server)
        .await;
    notify
        .respond_with(wiremock::ResponseTemplate::new(202))
        .mount(server)
        .await;
    list.respond_with(
        wiremock::ResponseTemplate::new(200)
            .insert_header("content-type", "application/json")
            .set_body_json(json!({ "jsonrpc": "2.0", "id": 2, "result": { "tools": tools } })),
    )
    .mount(server)
    .await;
    call.respond_with(
        wiremock::ResponseTemplate::new(200)
            .insert_header("content-type", "application/json")
            .set_body_json(json!({
                "jsonrpc": "2.0", "id": 3,
                "result": { "content": [ { "type": "text", "text": "pong" } ], "isError": false }
            })),
    )
    .mount(server)
    .await;
}

/// Mounts OAuth discovery + token endpoint for an oauth-mode server.
async fn mount_oauth(server: &wiremock::MockServer) {
    wiremock::Mock::given(method("POST"))
        .and(body_partial_json(json!({ "method": "initialize" })))
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
            "authorization_servers": [server.uri()]
        })))
        .mount(server)
        .await;
    wiremock::Mock::given(method("GET"))
        .and(path("/.well-known/oauth-authorization-server"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({
            "issuer": server.uri(),
            "authorization_endpoint": format!("{}/authorize", server.uri()),
            "token_endpoint": format!("{}/token", server.uri())
        })))
        .mount(server)
        .await;
}

#[tokio::test]
async fn bearer_and_oauth_servers_surface_tools_and_dispatch() {
    let alpha = wiremock::MockServer::start().await; // bearer
    mount_mcp_surface(
        &alpha,
        "sess-a",
        json!([ { "name": "ping", "description": "Ping the alpha server",
                  "inputSchema": { "type": "object", "properties": {} } } ]),
        Some("sk-alpha"),
    )
    .await;

    let beta = wiremock::MockServer::start().await; // oauth
    mount_mcp_surface(
        &beta,
        "sess-b",
        json!([ { "name": "echo", "inputSchema": { "type": "object", "properties": {} } } ]),
        Some("tok-beta"),
    )
    .await;
    mount_oauth(&beta).await;
    // Token endpoint (only hit on refresh — not in this test).
    wiremock::Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_body_json(json!({ "access_token": "at-x", "expires_in": 3600 })),
        )
        .mount(&beta)
        .await;

    // Pre-seed the OAuth token for beta.
    let dir = std::env::temp_dir().join(format!("vh-mcp-plugin-{}", std::process::id()));
    let token_path = dir.join("tokens.json");
    TokenStore::new(&token_path)
        .put(
            "beta",
            StoredToken {
                access_token: "tok-beta".into(),
                refresh_token: Some("rt".into()),
                expires_at: None, // never expires in this test
                client_id: Some("cid".into()),
            },
        )
        .expect("seed token");

    let cfg = PluginsConfig {
        enabled: vec!["mcp".to_string()],
        web_search: WebSearchConfig::default(),
        mcp: McpConfig {
            token_store: token_path.to_string_lossy().into_owned(),
            servers: vec![
                McpServerConfig {
                    name: "alpha".into(),
                    url: alpha.uri(),
                    auth: "bearer".into(),
                    api_key: "sk-alpha".into(),
                    ..Default::default()
                },
                McpServerConfig {
                    name: "beta".into(),
                    url: beta.uri(),
                    auth: "oauth".into(),
                    ..Default::default()
                },
            ],
        },
        ..Default::default()
    };
    let registry = registry_from_config(&cfg);
    registry.warm_all().await;

    // Specs from both servers are namespaced and visible to the LLM.
    let names = spec_names(&registry);
    assert!(names.contains(&"mcp.alpha.ping".to_string()), "{names:?}");
    assert!(names.contains(&"mcp.beta.echo".to_string()), "{names:?}");

    // Dispatch reaches the right server with the right auth.
    let out = registry
        .dispatch("mcp.alpha.ping", json!({}))
        .await
        .expect("alpha dispatch");
    assert_eq!(out["text"], "pong");
    let out = registry
        .dispatch("mcp.beta.echo", json!({}))
        .await
        .expect("beta dispatch");
    assert_eq!(out["text"], "pong");

    // Unknown server → recoverable error for the LLM.
    let err = registry
        .dispatch("mcp.nosuch.tool", json!({}))
        .await
        .expect_err("unknown server");
    assert!(err.contains("nosuch"), "{err}");
}

#[tokio::test]
async fn oauth_server_without_token_is_skipped_with_warning() {
    let gamma = wiremock::MockServer::start().await;
    mount_mcp_surface(&gamma, "sess-g", json!([]), None).await;
    mount_oauth(&gamma).await;

    let dir = std::env::temp_dir().join(format!("vh-mcp-plugin-empty-{}", std::process::id()));
    let token_path = dir.join("tokens.json");

    let cfg = PluginsConfig {
        enabled: vec!["mcp".to_string()],
        web_search: WebSearchConfig::default(),
        mcp: McpConfig {
            token_store: token_path.to_string_lossy().into_owned(),
            servers: vec![McpServerConfig {
                name: "gamma".into(),
                url: gamma.uri(),
                auth: "oauth".into(),
                ..Default::default()
            }],
        },
        ..Default::default()
    };
    let registry = registry_from_config(&cfg);
    registry.warm_all().await; // must not fail the process
    assert!(
        spec_names(&registry).is_empty(),
        "unauthorized server contributes no tools"
    );
}

#[tokio::test]
async fn is_error_tool_result_maps_to_err() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(method("POST"))
        .and(body_partial_json(json!({ "method": "initialize" })))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_json(json!({
                    "jsonrpc": "2.0", "id": 1,
                    "result": { "protocolVersion": "2025-06-18", "capabilities": {},
                                "serverInfo": {"name": "t", "version": "1"} }
                })),
        )
        .mount(&server)
        .await;
    wiremock::Mock::given(method("POST"))
        .and(body_partial_json(json!({ "method": "tools/list" })))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_json(
                    json!({ "jsonrpc": "2.0", "id": 2, "result": { "tools": [ { "name": "nothing" } ] } }),
                ),
        )
        .mount(&server)
        .await;
    wiremock::Mock::given(method("POST"))
        .and(body_partial_json(json!({ "method": "tools/call" })))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_json(json!({
                    "jsonrpc": "2.0", "id": 3,
                    "result": { "content": [ { "type": "text", "text": "boom" } ], "isError": true }
                })),
        )
        .mount(&server)
        .await;

    let dir = std::env::temp_dir().join(format!("vh-mcp-plugin-err-{}", std::process::id()));
    let token_path = dir.join("tokens.json");
    let cfg = PluginsConfig {
        enabled: vec!["mcp".to_string()],
        web_search: WebSearchConfig::default(),
        mcp: McpConfig {
            token_store: token_path.to_string_lossy().into_owned(),
            servers: vec![McpServerConfig {
                name: "err".into(),
                url: server.uri(),
                auth: "bearer".into(),
                api_key: "k".into(),
                ..Default::default()
            }],
        },
        ..Default::default()
    };
    let registry = registry_from_config(&cfg);
    registry.warm_all().await;
    let err = registry
        .dispatch("mcp.err.nothing", json!({}))
        .await
        .expect_err("isError must map to Err");
    assert!(err.contains("boom"), "{err}");
}
