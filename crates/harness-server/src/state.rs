//! Per-session conversation state shared by the HTTP and WS surfaces.
//!
//! [`SessionStore`] hands out one `Arc<RwLock<Session>>` per session id so a
//! turn can hold `&mut Session` across awaits without blocking other sessions
//! (or store lookups) — the outer map lock is never held during a turn.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use harness_providers::llm::ChatMessage;
use tokio::sync::RwLock;

use crate::client_tools::ClientCatalog;

pub type SessionId = String;

/// One conversation: rolling message history plus the device that owns it.
#[derive(Debug, Default)]
pub struct Session {
    pub history: VecDeque<ChatMessage>,
    pub device_id: Option<String>,
    /// Client-announced personal-context catalog (empty = none). Set by
    /// `context.announce` on the WS; cleared when a fresh `session.start`
    /// rebinding the id arrives.
    pub context: ClientCatalog,
    /// Identity keys the client announced with its catalog (iCloud record
    /// name, platform UUID, me-contact email — client-chosen precedence).
    /// Empty = anonymous: the session never merges with another.
    pub identity_keys: Vec<String>,
    /// Whether a live WS connection is currently bound to this session.
    /// Updated by ws.rs connect/disconnect; only ACTIVE sessions contribute
    /// catalogs to a federation merge or receive routed tool calls.
    pub active: bool,
}

impl Session {
    pub fn with_device(device_id: Option<String>) -> Self {
        Self {
            history: VecDeque::new(),
            device_id,
            context: ClientCatalog::default(),
            identity_keys: Vec::new(),
            active: false,
        }
    }
}

/// Session registry keyed by session id (device id / connection id).
#[derive(Debug, Default, Clone)]
pub struct SessionStore(Arc<RwLock<HashMap<SessionId, Arc<RwLock<Session>>>>>);

impl SessionStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Get (creating if absent) the session for `id`.
    pub async fn get(&self, id: impl Into<SessionId>) -> Arc<RwLock<Session>> {
        self.0
            .write()
            .await
            .entry(id.into())
            .or_insert_with(|| Arc::new(RwLock::new(Session::default())))
            .clone()
    }
}

/// Drop oldest turns until at most `max_turns` user messages remain. A turn is
/// a user message plus all assistant/tool traffic that follows it, so whole
/// turns leave the front together (never an orphaned assistant reply).
pub fn trim_history(history: &mut VecDeque<ChatMessage>, max_turns: usize) {
    let users = history.iter().filter(|m| m.role == "user").count();
    let mut excess = users.saturating_sub(max_turns);
    while excess > 0 {
        // Drop leading non-user leftovers (defensive; turns start with user).
        while matches!(history.front(), Some(m) if m.role != "user") {
            history.pop_front();
        }
        if history.pop_front().is_none() {
            break;
        }
        // Drop the reply traffic belonging to that user turn.
        while matches!(history.front(), Some(m) if m.role != "user") {
            history.pop_front();
        }
        excess -= 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: &str) -> ChatMessage {
        ChatMessage::text(role, "x")
    }

    #[test]
    fn trim_history_keeps_last_turns() {
        let mut h: VecDeque<ChatMessage> = ["user", "assistant", "user", "assistant", "user"]
            .iter()
            .map(|r| msg(r))
            .collect();
        trim_history(&mut h, 2);
        assert_eq!(h.len(), 3, "trailing user turn without reply is kept");
        assert_eq!(h[0].role, "user");
        assert_eq!(h.iter().filter(|m| m.role == "user").count(), 2);
    }

    #[test]
    fn trim_history_noop_when_under_limit() {
        let mut h: VecDeque<ChatMessage> = vec![msg("user"), msg("assistant")].into();
        trim_history(&mut h, 8);
        assert_eq!(h.len(), 2);
    }

    #[test]
    fn session_defaults_to_empty_identity_keys() {
        let s = Session::default();
        assert!(
            s.identity_keys.is_empty(),
            "a session without an announce has no identity keys"
        );
        assert!(Session::with_device(Some("dev-a".into()))
            .identity_keys
            .is_empty());
    }

    #[test]
    fn session_defaults_to_empty_context_catalog() {
        let s = Session::default();
        assert!(
            s.context.is_empty(),
            "a session without an announce has no client tools"
        );
        assert!(s.context.openai_tool_specs().is_empty());
    }

    #[test]
    fn with_device_has_empty_context_catalog() {
        let s = Session::with_device(Some("dev-a".into()));
        assert!(
            s.context.is_empty(),
            "a fresh session never inherits a client catalog"
        );
    }
}
