//! Integration tests for the plugin registry + built-ins.

use harness_core::config::{
    HttpFetchConfig, McpConfig, PluginsConfig, WeatherConfig, WebSearchConfig,
};
use harness_plugins::builtin::web_search::WebSearchPlugin;
use harness_plugins::{registry_from_config, Plugin, PluginRegistry};
use serde_json::{json, Value};

/// Counts warm() calls — verifies warm_all reaches every plugin.
struct WarmCounter {
    count: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl harness_plugins::Plugin for WarmCounter {
    fn manifest(&self) -> &harness_plugins::PluginManifest {
        static M: std::sync::OnceLock<harness_plugins::PluginManifest> = std::sync::OnceLock::new();
        M.get_or_init(|| harness_plugins::PluginManifest {
            name: "counter",
            version: "0.1.0",
            description: "test",
        })
    }
    fn tool_specs(&self) -> Vec<Value> {
        vec![]
    }
    async fn call(&self, _name: &str, _args: Value) -> Result<Value, String> {
        Err("unused".into())
    }
    async fn warm(&self) {
        self.count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

#[tokio::test]
async fn warm_all_warms_every_registered_plugin() {
    let mut registry = PluginRegistry::new();
    let counter = std::sync::Arc::new(WarmCounter {
        count: std::sync::atomic::AtomicUsize::new(0),
    });
    // PluginRegistry owns Box<dyn Plugin>; wrap a clone-sharing adapter.
    struct SharedCounter(std::sync::Arc<WarmCounter>);
    #[async_trait::async_trait]
    impl harness_plugins::Plugin for SharedCounter {
        fn manifest(&self) -> &harness_plugins::PluginManifest {
            self.0.manifest()
        }
        fn tool_specs(&self) -> Vec<Value> {
            self.0.tool_specs()
        }
        async fn call(&self, name: &str, args: Value) -> Result<Value, String> {
            self.0.call(name, args).await
        }
        async fn warm(&self) {
            self.0.warm().await
        }
    }
    registry.register(Box::new(SharedCounter(counter.clone())));
    registry.warm_all().await;
    assert_eq!(
        counter.count.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "warm_all must call warm() on every plugin"
    );
}

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
        web_search: WebSearchConfig::default(),
        weather: WeatherConfig::default(),
        mcp: McpConfig::default(),
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
        web_search: WebSearchConfig::default(),
        weather: WeatherConfig::default(),
        mcp: McpConfig::default(),
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
        web_search: WebSearchConfig::default(),
        weather: WeatherConfig::default(),
        mcp: McpConfig::default(),
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
        web_search: WebSearchConfig::default(),
        weather: WeatherConfig::default(),
        mcp: McpConfig::default(),
    });
    assert!(spec_names(&empty).is_empty());

    // Explicitly both built-ins enabled → two specs.
    let both = registry_from_config(&PluginsConfig {
        enabled: vec!["time".to_string(), "http_fetch".to_string()],
        http_fetch: HttpFetchConfig::default(),
        web_search: WebSearchConfig::default(),
        weather: WeatherConfig::default(),
        mcp: McpConfig::default(),
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
        web_search: WebSearchConfig::default(),
        weather: WeatherConfig::default(),
        mcp: McpConfig::default(),
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
        web_search: WebSearchConfig::default(),
        weather: WeatherConfig::default(),
        mcp: McpConfig::default(),
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
        web_search: WebSearchConfig::default(),
        weather: WeatherConfig::default(),
        mcp: McpConfig::default(),
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

#[tokio::test]
async fn weather_spec_surfaces_and_unknown_tool_errors() {
    let cfg = PluginsConfig {
        enabled: vec!["weather".to_string()],
        http_fetch: HttpFetchConfig::default(),
        web_search: WebSearchConfig::default(),
        weather: WeatherConfig::default(),
        mcp: McpConfig::default(),
    };
    let registry = registry_from_config(&cfg);
    let names = spec_names(&registry);
    assert_eq!(names, vec!["weather.get_forecast".to_string()]);
    for spec in registry.tool_specs() {
        assert_eq!(spec["type"], "function");
        assert!(spec["function"]["parameters"].is_object());
    }
    let err = registry
        .dispatch("weather.nothing", json!({}))
        .await
        .expect_err("unknown tool must error");
    assert!(err.contains("nothing"), "{err}");
}

#[tokio::test]
async fn weather_round_trip_through_registry_with_test_endpoints() {
    let geo = wiremock::MockServer::start().await;
    let api = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_string(
            r#"{"results":[{"name":"Paris","latitude":48.85341,"longitude":2.3488,
                "country":"France","admin1":"Île-de-France","timezone":"Europe/Paris"}]}"#,
        ))
        .mount(&geo)
        .await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_string(
            r#"{"timezone":"Europe/Paris",
                "current":{"time":"2026-09-12T14:30","temperature_2m":22.4,
                    "apparent_temperature":23.1,"relative_humidity_2m":58,
                    "is_day":1,"weather_code":2,"wind_speed_10m":11.2},
                "daily":{"time":["2026-09-12"],"weather_code":[2],
                    "temperature_2m_max":[24.1],"temperature_2m_min":[15.3],
                    "precipitation_probability_max":[10]}}"#,
        ))
        .mount(&api)
        .await;

    let cfg = PluginsConfig {
        enabled: vec!["weather".to_string()],
        http_fetch: HttpFetchConfig::default(),
        web_search: WebSearchConfig::default(),
        weather: WeatherConfig {
            default_location: String::new(),
            api_base: api.uri(),
            geocoding_base: geo.uri(),
        },
        mcp: McpConfig::default(),
    };
    let registry = registry_from_config(&cfg);
    assert!(spec_names(&registry).contains(&"weather.get_forecast".to_string()));
    let out = registry
        .dispatch("weather.get_forecast", json!({ "location": "Paris" }))
        .await
        .expect("dispatch ok");
    assert_eq!(out["location"]["name"], "Paris");
    assert_eq!(out["current"]["condition"], "Partly cloudy");
}

#[tokio::test]
async fn web_search_registers_tools_when_configured() {
    let cfg = PluginsConfig {
        enabled: vec!["web_search".to_string()],
        web_search: WebSearchConfig {
            base_url: "http://127.0.0.1:3002".to_string(),
            api_key: String::new(),
        },
        ..Default::default()
    };
    let registry = registry_from_config(&cfg);
    let names = spec_names(&registry);
    assert!(
        names.contains(&"web_search.search".to_string()),
        "{names:?}"
    );
    assert!(names.contains(&"web_search.fetch".to_string()), "{names:?}");
}

#[tokio::test]
async fn web_search_without_base_url_registers_nothing() {
    let cfg = PluginsConfig {
        enabled: vec!["web_search".to_string()],
        web_search: WebSearchConfig::default(),
        ..Default::default()
    };
    let registry = registry_from_config(&cfg);
    assert!(
        spec_names(&registry).is_empty(),
        "enabled web_search without base_url must contribute no tools"
    );
}

#[tokio::test]
async fn web_search_search_parses_results_and_sends_query() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/v2/search"))
        .and(wiremock::matchers::body_partial_json(
            json!({ "query": "rust async", "limit": 5 }),
        ))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({
            "success": true,
            "data": { "web": [
                { "url": "https://a.example/", "title": "A", "description": "first" },
                { "url": "https://b.example/", "title": "B", "description": "second" }
            ]}
        })))
        .expect(1)
        .mount(&server)
        .await;

    let plugin = WebSearchPlugin::new(server.uri(), String::new());
    let out = plugin
        .call("search", json!({ "query": "rust async" }))
        .await
        .expect("search succeeds");
    let results = out
        .get("results")
        .and_then(Value::as_array)
        .expect("results array");
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].get("title").and_then(Value::as_str), Some("A"));
    assert_eq!(
        results[0].get("url").and_then(Value::as_str),
        Some("https://a.example/")
    );
}

#[tokio::test]
async fn web_search_search_clamps_limit_and_passes_it() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/v2/search"))
        .and(wiremock::matchers::body_partial_json(
            json!({ "limit": 10 }),
        ))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({
            "success": true, "data": { "web": [] }
        })))
        .expect(1)
        .mount(&server)
        .await;
    let plugin = WebSearchPlugin::new(server.uri(), String::new());
    let out = plugin
        .call("search", json!({ "query": "x", "limit": 99 }))
        .await
        .expect("search succeeds");
    assert_eq!(
        out.get("results").and_then(Value::as_array).map(Vec::len),
        Some(0)
    );
}

#[tokio::test]
async fn web_search_search_surfaces_firecrawl_error() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/v2/search"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({
            "success": false, "error": "No search backend configured"
        })))
        .mount(&server)
        .await;
    let plugin = WebSearchPlugin::new(server.uri(), String::new());
    let err = plugin
        .call("search", json!({ "query": "x" }))
        .await
        .expect_err("must fail");
    assert!(err.contains("No search backend configured"), "{err}");
}

#[tokio::test]
async fn web_search_fetch_returns_markdown_and_title() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/v2/scrape"))
        .and(wiremock::matchers::body_partial_json(json!({
            "url": "https://example.com/",
            "formats": ["markdown"]
        })))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({
            "success": true,
            "data": {
                "markdown": "# Example Domain\n\nHello.",
                "metadata": { "title": "Example Domain", "statusCode": 200 }
            }
        })))
        .expect(1)
        .mount(&server)
        .await;
    let plugin = WebSearchPlugin::new(server.uri(), String::new());
    let out = plugin
        .call("fetch", json!({ "url": "https://example.com/" }))
        .await
        .expect("fetch succeeds");
    assert_eq!(
        out.get("title").and_then(Value::as_str),
        Some("Example Domain")
    );
    assert!(out
        .get("markdown")
        .and_then(Value::as_str)
        .expect("markdown present")
        .contains("Hello."));
}

#[tokio::test]
async fn web_search_fetch_rejects_oversized_body() {
    let server = wiremock::MockServer::start().await;
    let big = "x".repeat(65 * 1024);
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/v2/scrape"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_string(big))
        .mount(&server)
        .await;
    let plugin = WebSearchPlugin::new(server.uri(), String::new());
    let err = plugin
        .call("fetch", json!({ "url": "https://example.com/" }))
        .await
        .expect_err("must fail");
    assert!(err.contains("exceeds"), "{err}");
}

#[tokio::test]
async fn web_search_fetch_surfaces_upstream_http_error() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/v2/scrape"))
        .respond_with(wiremock::ResponseTemplate::new(500).set_body_json(json!({
            "success": false, "error": "scrape failed"
        })))
        .mount(&server)
        .await;
    let plugin = WebSearchPlugin::new(server.uri(), String::new());
    let err = plugin
        .call("fetch", json!({ "url": "https://example.com/" }))
        .await
        .expect_err("must fail");
    assert!(
        err.contains("500") && err.contains("scrape failed"),
        "{err}"
    );
}

#[tokio::test]
async fn web_search_sends_bearer_key_when_configured() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/v2/search"))
        .and(wiremock::matchers::header(
            "Authorization",
            "Bearer fc-test-key",
        ))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({
            "success": true, "data": { "web": [] }
        })))
        .expect(1)
        .mount(&server)
        .await;
    let plugin = WebSearchPlugin::new(server.uri(), "fc-test-key".to_string());
    plugin
        .call("search", json!({ "query": "x" }))
        .await
        .expect("search succeeds");
}

#[tokio::test]
async fn web_search_unknown_tool_errors() {
    let plugin = WebSearchPlugin::new("http://127.0.0.1:1".to_string(), String::new());
    let err = plugin
        .call("nope", json!({}))
        .await
        .expect_err("unknown tool");
    assert!(err.contains("nope"), "{err}");
}
