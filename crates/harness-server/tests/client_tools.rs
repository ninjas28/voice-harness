//! Client-announced personal-context catalog: namespaced OpenAI spec building
//! + call routing (Task 3 of the personal-context plan).

use harness_core::types::{ProviderDescriptor, ToolDescriptor};
use harness_server::client_tools::{ClientCatalog, MAX_CLIENT_TOOLS};

fn tool(name: &str, description: &str, parameters: serde_json::Value) -> ToolDescriptor {
    ToolDescriptor {
        name: name.into(),
        description: description.into(),
        parameters,
    }
}

fn provider(id: &str, tools: Vec<ToolDescriptor>) -> ProviderDescriptor {
    ProviderDescriptor {
        id: id.into(),
        tools,
    }
}

fn catalog() -> ClientCatalog {
    ClientCatalog::from_announce(vec![provider(
        "calendar",
        vec![
            tool(
                "events",
                "List events.",
                serde_json::json!({"type":"object","properties":{"days_ahead":{"type":"integer"}}}),
            ),
            tool("reminders", "List reminders.", serde_json::json!({})),
        ],
    )])
}

#[test]
fn specs_are_namespaced_and_openai_shaped() {
    let specs = catalog().openai_tool_specs();
    assert_eq!(specs.len(), 2);
    let names: Vec<&str> = specs
        .iter()
        .map(|s| s["function"]["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        ["personal.calendar.events", "personal.calendar.reminders"]
    );
    for s in &specs {
        assert_eq!(s["type"], "function");
        assert!(s["function"]["parameters"].is_object());
    }
}

#[test]
fn resolve_routes_namespaced_names() {
    let cat = catalog();
    let resolved = cat.resolve("personal.calendar.events").expect("routed");
    assert_eq!(resolved.name, "events");
    assert!(cat.resolve("personal.nosuch.tool").is_none());
    assert!(cat.resolve("time.now_utc").is_none());
    assert!(
        cat.resolve("calendar.events").is_none(),
        "bare name is not routable"
    );
}

#[test]
fn empty_catalog_has_no_specs() {
    assert!(ClientCatalog::default().openai_tool_specs().is_empty());
    assert!(ClientCatalog::default().is_empty());
}

#[test]
fn duplicate_and_oversized_catalogs_are_capped() {
    // Two definitions of the same provider id + tool name in one announce:
    // deduped to one spec, the later definition wins.
    let dup = ClientCatalog::from_announce(vec![
        provider(
            "calendar",
            vec![tool("events", "old description", serde_json::json!({}))],
        ),
        provider(
            "calendar",
            vec![tool("events", "new description", serde_json::json!({}))],
        ),
    ]);
    let specs = dup.openai_tool_specs();
    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0]["function"]["description"], "new description");
    assert_eq!(
        dup.resolve("personal.calendar.events").unwrap().description,
        "new description"
    );

    // Total tool cap: extra tools are silently dropped.
    let many: Vec<ProviderDescriptor> = (0..40)
        .map(|i| {
            provider(
                "cal",
                vec![tool(&format!("tool{i}"), "d", serde_json::json!({}))],
            )
        })
        .collect();
    let specs = ClientCatalog::from_announce(many).openai_tool_specs();
    assert_eq!(specs.len(), MAX_CLIENT_TOOLS);
}

#[test]
fn non_object_parameters_get_object_fallback() {
    let cat = ClientCatalog::from_announce(vec![provider(
        "photos",
        vec![tool("digest", "Photo stats.", serde_json::json!(null))],
    )]);
    let specs = cat.openai_tool_specs();
    assert_eq!(specs.len(), 1);
    let params = &specs[0]["function"]["parameters"];
    assert!(params.is_object());
    assert_eq!(params["type"], "object");
    assert!(params["properties"].is_object());
}
