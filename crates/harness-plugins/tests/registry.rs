//! Integration tests for the plugin registry + built-ins.

use harness_core::config::{HttpFetchConfig, PluginsConfig};
use harness_plugins::{registry_from_config, PluginRegistry};
use serde_json::json;

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

#[tokio::test]
async fn dispatch_round_trip_and_unknown_tool_error() {
    let cfg = PluginsConfig {
        enabled: vec!["time".to_string()],
        http_fetch: HttpFetchConfig::default(),
    };
    let registry = registry_from_config(&cfg);

    // Namespaced dispatch reaches the plugin, which receives the bare name.
    let out = registry
        .dispatch("time.get_time", json!({ "tz": "UTC" }))
        .await
        .expect("dispatch ok");
    assert!(out["datetime"].as_str().expect("string").contains('T'));
    assert_eq!(out["timezone"], "UTC");

    // Unknown plugin → Err (never panic), message says unknown.
    let err = registry
        .dispatch("weather.lookup", json!({}))
        .await
        .expect_err("unknown tool must error");
    assert!(err.contains("unknown tool"), "{err}");

    // Unknown tool inside a known plugin → Err too.
    let err = registry
        .dispatch("time.nothing", json!({}))
        .await
        .expect_err("unknown tool must error");
    assert!(
        err.contains("unknown tool") || err.contains("nothing"),
        "{err}"
    );
}

#[tokio::test]
async fn tool_specs_are_concatenated_and_namespaced() {
    let cfg = PluginsConfig {
        enabled: vec!["time".to_string(), "http_fetch".to_string()],
        http_fetch: HttpFetchConfig {
            allowed_hosts: vec!["example.com".to_string()],
        },
    };
    let registry = registry_from_config(&cfg);
    let names = spec_names(&registry);
    assert_eq!(
        names,
        vec!["time.get_time".to_string(), "http_fetch.fetch".to_string(),]
    );
    // Specs are valid OpenAI function specs.
    for spec in registry.tool_specs() {
        assert_eq!(spec["type"], "function");
        assert!(spec["function"]["parameters"].is_object());
    }
}

#[tokio::test]
async fn registry_from_config_honors_enabled_list() {
    let cfg = PluginsConfig {
        enabled: vec!["time".to_string()],
        http_fetch: HttpFetchConfig::default(),
    };
    let registry = registry_from_config(&cfg);
    let names = spec_names(&registry);
    assert_eq!(names, vec!["time.get_time".to_string()]);
    assert!(
        !names.iter().any(|n| n.starts_with("http_fetch")),
        "disabled plugin must not contribute specs: {names:?}"
    );

    // Empty enabled list → empty registry.
    let empty = registry_from_config(&PluginsConfig {
        enabled: vec![],
        http_fetch: HttpFetchConfig::default(),
    });
    assert!(spec_names(&empty).is_empty());

    // Explicitly both built-ins enabled → two specs.
    let both = registry_from_config(&PluginsConfig {
        enabled: vec!["time".to_string(), "http_fetch".to_string()],
        http_fetch: HttpFetchConfig::default(),
    });
    assert_eq!(spec_names(&both).len(), 2);
}

#[tokio::test]
async fn http_fetch_fetches_allowlisted_host_and_strips_html() {
    let server = wiremock::MockServer::start().await;
    // MockServer binds 127.0.0.1:<random port>; allowlist the host without port.
    let host = server
        .uri()
        .trim_start_matches("http://")
        .split(':')
        .next()
        .unwrap_or_default()
        .to_string();
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .insert_header("content-type", "text/html")
                .set_body_string("<html><body><h1>Hi</h1><p>Fetched ok</p></body></html>"),
        )
        .mount(&server)
        .await;

    let cfg = PluginsConfig {
        enabled: vec!["http_fetch".to_string()],
        http_fetch: HttpFetchConfig {
            allowed_hosts: vec![host.clone()],
        },
    };
    let registry = registry_from_config(&cfg);
    let out = registry
        .dispatch(
            "http_fetch.fetch",
            json!({ "url": format!("{}/page", server.uri()) }),
        )
        .await
        .expect("fetch ok");
    assert_eq!(out["status"], 200);
    assert_eq!(out["body"], "Hi Fetched ok");
}

#[tokio::test]
async fn http_fetch_denies_non_allowlisted_host_without_requesting() {
    let server = wiremock::MockServer::start().await;
    // No mocks: a request would 404. The deny must happen before any I/O.
    let cfg = PluginsConfig {
        enabled: vec!["http_fetch".to_string()],
        http_fetch: HttpFetchConfig {
            allowed_hosts: vec!["example.com".to_string()],
        },
    };
    let registry = registry_from_config(&cfg);
    let err = registry
        .dispatch(
            "http_fetch.fetch",
            json!({ "url": format!("{}/steal", server.uri()) }),
        )
        .await
        .expect_err("must deny");
    assert!(err.contains("not on the http_fetch allowlist"), "{err}");
    assert!(
        server
            .received_requests()
            .await
            .expect("received requests")
            .is_empty(),
        "denied host must not be contacted"
    );
}

#[tokio::test]
async fn http_fetch_reports_upstream_error_status() {
    let server = wiremock::MockServer::start().await;
    let host = server
        .uri()
        .trim_start_matches("http://")
        .split(':')
        .next()
        .unwrap_or_default()
        .to_string();
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("boom"))
        .mount(&server)
        .await;

    let cfg = PluginsConfig {
        enabled: vec!["http_fetch".to_string()],
        http_fetch: HttpFetchConfig {
            allowed_hosts: vec![host],
        },
    };
    let registry = registry_from_config(&cfg);
    // 5xx still returns a result object with the status — the LLM decides.
    let out = registry
        .dispatch(
            "http_fetch.fetch",
            json!({ "url": format!("{}/down", server.uri()) }),
        )
        .await
        .expect("result object");
    assert_eq!(out["status"], 500);
}
