//! Streamable-HTTP MCP client: JSON-RPC 2.0 over HTTP POST, accepting JSON or
//! SSE responses, tracking `Mcp-Session-Id`, re-initializing once on session
//! expiry (404). Every network wait is timeout-bounded.
