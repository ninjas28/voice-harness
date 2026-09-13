//! Tool registry: resolves namespaced `plugin.tool` names to plugins.

use harness_core::config::PluginsConfig;
use serde_json::Value;

use crate::builtin;
use crate::Plugin;

/// Resolves `<plugin>.<tool>` function names to plugin handlers.
pub struct PluginRegistry {
    plugins: Vec<Box<dyn Plugin>>,
}

impl PluginRegistry {
    pub fn new() -> Self {
        Self {
            plugins: Vec::new(),
        }
    }

    pub fn register(&mut self, plugin: Box<dyn Plugin>) {
        self.plugins.push(plugin);
    }

    /// Concatenated OpenAI tool specs, namespaced `<plugin>.<tool>`.
    pub fn tool_specs(&self) -> Vec<Value> {
        let mut out = Vec::new();
        for plugin in &self.plugins {
            let ns = plugin.manifest().name;
            for mut spec in plugin.tool_specs() {
                if let Some(name) = spec
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
                    .map(str::to_string)
                {
                    // Skip names already carrying a namespace prefix.
                    if let Some(bare) = name.strip_prefix(&format!("{ns}.")) {
                        spec["function"]["name"] = Value::String(format!("{ns}.{bare}"));
                    } else {
                        spec["function"]["name"] = Value::String(format!("{ns}.{name}"));
                    }
                }
                out.push(spec);
            }
        }
        out
    }

    /// Find the plugin owning `name` (`plugin.tool`), returning it plus the
    /// bare tool name. Unknown plugins/tools → `Err` the LLM can recover from.
    fn resolve<'a>(&'a self, name: &'a str) -> Result<(&'a dyn Plugin, &'a str), String> {
        let Some((plugin_name, tool)) = name.split_once('.') else {
            return Err(format!(
                "unknown tool '{name}': expected the form '<plugin>.<tool>'"
            ));
        };
        let plugin = self
            .plugins
            .iter()
            .map(|p| p.as_ref())
            .find(|p| p.manifest().name == plugin_name)
            .ok_or_else(|| format!("unknown tool '{name}': no plugin named '{plugin_name}'"))?;
        // A tool name that belongs to another plugin is also unknown here.
        let owned = plugin.tool_specs().iter().any(|spec| {
            spec.get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .is_some_and(|n| n == tool || n == name)
        });
        if !owned {
            return Err(format!("unknown tool '{name}'"));
        }
        Ok((plugin, tool))
    }

    /// Dispatch a tool call by namespaced name; unknown → `Err`.
    pub async fn dispatch(&self, name: &str, args: Value) -> Result<Value, String> {
        let (plugin, tool) = self.resolve(name)?;
        plugin.call(tool, args).await
    }

    /// One-time async initialization of every plugin. Never fails: plugins
    /// log their own problems and continue.
    pub async fn warm_all(&self) {
        for plugin in &self.plugins {
            plugin.warm().await;
        }
    }
}

impl Default for PluginRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Build a registry from config, honoring `enabled` (unknown names skipped
/// with a warning).
pub fn registry_from_config(cfg: &PluginsConfig) -> PluginRegistry {
    let mut registry = PluginRegistry::new();
    for name in &cfg.enabled {
        match name.as_str() {
            "time" => registry.register(Box::new(builtin::time::TimePlugin)),
            "web_search" => {
                if cfg.web_search.base_url.trim().is_empty() {
                    tracing::warn!(
                        "web_search plugin enabled but no [plugins.web_search].base_url configured; skipping"
                    );
                } else {
                    registry.register(Box::new(builtin::web_search::WebSearchPlugin::new(
                        cfg.web_search.base_url.clone(),
                        cfg.web_search.api_key.clone(),
                    )));
                }
            }
            "weather" => registry.register(Box::new(
                builtin::weather::WeatherPlugin::new(cfg.weather.default_location.clone())
                    .with_endpoints(
                        cfg.weather.api_base.clone(),
                        cfg.weather.geocoding_base.clone(),
                    ),
            )),
            "mcp" => {
                if cfg.mcp.servers.is_empty() {
                    tracing::warn!("mcp plugin enabled but no [plugins.mcp.servers] configured");
                }
                registry.register(Box::new(crate::mcp::plugin::McpPlugin::new(
                    cfg.mcp.servers.clone(),
                    cfg.mcp.token_store.clone(),
                )));
            }
            other => tracing::warn!(plugin = %other, "unknown plugin in enabled list; skipping"),
        }
    }
    registry
}
