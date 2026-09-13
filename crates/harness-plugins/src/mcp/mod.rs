//! Streamable-HTTP MCP support: JSON-RPC client, OAuth 2.1 auth, and the
//! `mcp` plugin that exposes remote tools to the LLM as `mcp.<server>.<tool>`.

pub mod client;
pub mod oauth;
pub mod plugin;
pub mod types;
