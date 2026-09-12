//! Per-session conversation state shared by the HTTP and WS surfaces.
//!
//! [`SessionStore`] hands out one `Arc<RwLock<Session>>` per session id so a
//! turn can hold `&mut Session` across awaits without blocking other sessions
//! (or store lookups) — the outer map lock is never held during a turn.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use harness_providers::llm::ChatMessage;
use tokio::sync::RwLock;

pub type SessionId = String;

/// One conversation: rolling message history plus the device that owns it.
#[derive(Debug, Default)]
pub struct Session {
    pub history: VecDeque<ChatMessage>,
    pub device_id: Option<String>,
}

impl Session {
    pub fn with_device(device_id: Option<String>) -> Self {
        Self {
            history: VecDeque::new(),
            device_id,
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
}
