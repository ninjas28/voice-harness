//! Client-announced personal-context catalog (session-owned).
//!
//! The announcing client sends `context.announce` with bare tool names; this
//! catalog stores them and exposes two server-side views:
//!
//! - [`ClientCatalog::openai_tool_specs`]: OpenAI function specs namespaced
//!   `personal.<provider>.<tool>` (mirrors how `PluginRegistry` namespaces
//!   `plugin.tool`), deduped and capped at [`MAX_CLIENT_TOOLS`].
//! - [`ClientCatalog::resolve`]: maps a namespaced name back to the announced
//!   descriptor for routing.
//!
//! Bare names are deliberately NOT routable — the `personal.` prefix marks a
//! call as client-executed and keeps it collision-free with server plugins.

use harness_core::types::{ProviderDescriptor, ToolDescriptor};
use serde_json::Value;

/// Upper bound on total client tools surfaced to the LLM per session; extras
/// in an announce are silently dropped.
pub const MAX_CLIENT_TOOLS: usize = 32;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ClientCatalog {
    pub providers: Vec<ProviderDescriptor>,
}

impl ClientCatalog {
    /// Build a catalog from an announce, normalizing it: duplicate provider
    /// ids merge (later tool definitions win per tool name) and the total
    /// tool count is capped at [`MAX_CLIENT_TOOLS`] (extras dropped).
    pub fn from_announce(providers: Vec<ProviderDescriptor>) -> Self {
        let mut merged: Vec<ProviderDescriptor> = Vec::new();
        for p in providers {
            match merged.iter_mut().find(|m| m.id == p.id) {
                Some(existing) => {
                    for t in p.tools {
                        match existing.tools.iter_mut().find(|e| e.name == t.name) {
                            Some(slot) => *slot = t, // later definition wins
                            None => existing.tools.push(t),
                        }
                    }
                }
                None => merged.push(p),
            }
        }
        // Cap the total: later providers lose their tools first.
        let mut total = 0usize;
        for p in &mut merged {
            if total >= MAX_CLIENT_TOOLS {
                p.tools.clear();
                continue;
            }
            let room = MAX_CLIENT_TOOLS - total;
            if p.tools.len() > room {
                p.tools.truncate(room);
            }
            total += p.tools.len();
        }
        Self { providers: merged }
    }

    /// OpenAI function specs, namespaced `personal.<provider>.<tool>`.
    /// A tool whose `parameters` is not a JSON object gets an empty-object
    /// schema fallback.
    pub fn openai_tool_specs(&self) -> Vec<Value> {
        let mut out = Vec::new();
        for p in &self.providers {
            for t in &p.tools {
                let fq = format!("personal.{}.{}", p.id, t.name);
                let params = if t.parameters.is_object() {
                    t.parameters.clone()
                } else {
                    serde_json::json!({"type": "object", "properties": {}})
                };
                out.push(serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": fq,
                        "description": t.description,
                        "parameters": params,
                    }
                }));
            }
        }
        out
    }

    /// Resolve `personal.<provider>.<tool>` to the announced descriptor.
    /// Anything else (bare names, unknown providers/tools, wrong shape) →
    /// `None`.
    pub fn resolve(&self, name: &str) -> Option<&ToolDescriptor> {
        let rest = name.strip_prefix("personal.")?;
        let (provider_id, tool_name) = rest.split_once('.')?;
        self.providers
            .iter()
            .find(|p| p.id == provider_id)?
            .tools
            .iter()
            .find(|t| t.name == tool_name)
    }

    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }
}
