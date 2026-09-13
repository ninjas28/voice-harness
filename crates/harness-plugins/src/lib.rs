//! harness-plugins: plugin trait, registry, built-in plugins (time, weather, http_fetch).
//!
//! Plugins supply OpenAI-style tool specs and handlers; the registry
//! namespaces tool names as `<plugin>.<tool>` so specs can be concatenated
//! and dispatched unambiguously. Unknown tool names produce an `Err` (the
//! LLM can recover), never a panic.

pub mod builtin;
pub mod mcp;
pub mod registry;

use async_trait::async_trait;
use serde_json::Value;

pub use registry::{registry_from_config, PluginRegistry};

/// Plugin metadata.
#[derive(Debug, Clone)]
pub struct PluginManifest {
    pub name: &'static str,
    pub version: &'static str,
    pub description: &'static str,
}

/// A tool-providing plugin. The registry namespaces specs as
/// `<plugin>.<tool>`; [`Plugin::call`] receives the bare tool name.
#[async_trait]
pub trait Plugin: Send + Sync {
    fn manifest(&self) -> &PluginManifest;
    fn tool_specs(&self) -> Vec<Value>;
    async fn call(&self, name: &str, arguments: Value) -> Result<Value, String>;
    /// One-time async setup (fetch remote tool lists, warm caches).
    /// Default: nothing to do.
    async fn warm(&self) {}
}
