//! Per-session conversation state shared by the HTTP and WS surfaces.
//!
//! [`SessionStore`] hands out one `Arc<RwLock<Session>>` per session id so a
//! turn can hold `&mut Session` across awaits without blocking other sessions
//! (or store lookups) — the outer map lock is never held during a turn.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Arc;

use harness_providers::llm::ChatMessage;
use tokio::sync::{mpsc, RwLock};

use crate::client_tools::ClientCatalog;
use harness_core::types::ServerMsg;

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
    /// Canonical user this session belongs to, resolved from
    /// `identity_keys` via the persisted registry on `session.start`.
    /// `None` = anonymous (no keys / federation off): never merges.
    pub canonical_user: Option<String>,
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
            canonical_user: None,
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

    /// OpenAI tool specs of the turn's own session catalog followed by the
    /// catalogs of every OTHER ACTIVE session with the same canonical user,
    /// ascending by session id, deduped by fully-qualified name (the first —
    /// own — definition wins). No canonical user → no merge: the plain own
    /// catalog is the whole world.
    pub async fn federated_specs(
        &self,
        self_id: &str,
        canonical_user: &str,
    ) -> Vec<serde_json::Value> {
        if canonical_user.is_empty() {
            return Vec::new();
        }
        let entries: Vec<(SessionId, Arc<RwLock<Session>>)> = {
            let sessions = self.0.read().await;
            sessions
                .iter()
                .filter(|(id, _)| id.as_str() != self_id)
                .map(|(id, s)| (id.clone(), s.clone()))
                .collect()
        };
        let mut same_user: Vec<SessionId> = Vec::new();
        for (id, session) in entries {
            let s = session.read().await;
            if s.active && s.canonical_user.as_deref() == Some(canonical_user) {
                same_user.push(id);
            }
        }
        same_user.sort();
        let mut out = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for id in same_user {
            let session = self.get(id).await;
            let session = session.read().await;
            for spec in session.context.openai_tool_specs() {
                let name = spec["function"]["name"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                if seen.insert(name) {
                    out.push(spec);
                }
            }
        }
        out
    }
}

/// Cross-session routing for `personal.*` calls: canonical user →
/// (session id → per-connection `ServerMsg` sender). Maintained by ws.rs per
/// connection; the orchestrator queries it when a call's tool lives on a
/// sibling session and as a fallback when the own client cannot serve it.
///
/// A routed call needs an ANSWER back to the turn that asked. The sibling
/// connection learns it from [`FederationRouter::take_pending`]: dispatch
/// stores the call id + originating inbox sender under the sibling session;
/// the sibling's ws layer pops it when its client replies `tool.result` and
/// forwards the (ok, text) into the stored sender. Result: no answer type
/// changes, bounded by the same per-call timeout on the asking side.
/// Where a routed call's answer must land: call id -> the asking turn's inbox.
type PendingReplies = HashMap<u64, mpsc::Sender<(u64, bool, String)>>;

#[derive(Debug, Default)]
pub struct FederationRouter {
    routes: RwLock<HashMap<String, BTreeMap<SessionId, mpsc::Sender<ServerMsg>>>>,
    pending: RwLock<HashMap<SessionId, PendingReplies>>,
}

impl FederationRouter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add/replace one session's route under `canonical_user`.
    pub async fn register(
        &self,
        canonical_user: &str,
        session_id: &str,
        tx: mpsc::Sender<ServerMsg>,
    ) {
        self.routes
            .write()
            .await
            .entry(canonical_user.to_string())
            .or_default()
            .insert(session_id.to_string(), tx);
    }

    /// Remove one session's route (connection closed / session rebound).
    pub async fn unregister(&self, canonical_user: &str, session_id: &str) {
        let mut routes = self.routes.write().await;
        if let Some(sessions) = routes.get_mut(canonical_user) {
            sessions.remove(session_id);
            if sessions.is_empty() {
                routes.remove(canonical_user);
            }
        }
        self.pending.write().await.remove(session_id);
    }

    /// All routes for `canonical_user` except `self_id`, ascending by
    /// session id — the fallback order ("first" policy: lowest id wins).
    pub async fn routes_for(
        &self,
        canonical_user: &str,
        self_id: &str,
    ) -> Vec<(SessionId, mpsc::Sender<ServerMsg>)> {
        self.routes
            .read()
            .await
            .get(canonical_user)
            .map(|sessions| {
                sessions
                    .iter()
                    .filter(|(id, _)| id.as_str() != self_id)
                    .map(|(id, tx)| (id.clone(), tx.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Record that a call dispatched to `session_id` must send its
    /// `(call_id, ok, text)` result into `reply` when the client answers.
    pub async fn arm_pending(
        &self,
        session_id: &str,
        call_id: u64,
        reply: mpsc::Sender<(u64, bool, String)>,
    ) {
        self.pending
            .write()
            .await
            .entry(session_id.to_string())
            .or_default()
            .insert(call_id, reply);
    }

    /// Pop the reply sender for a `tool.result` arriving on `session_id`.
    /// `None` = no routed call expects this result (the sibling can drop it).
    pub async fn take_pending(
        &self,
        session_id: &str,
        call_id: u64,
    ) -> Option<mpsc::Sender<(u64, bool, String)>> {
        self.pending
            .write()
            .await
            .get_mut(session_id)?
            .remove(&call_id)
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
    use crate::state::FederationRouter as Router;
    use harness_core::types::{ProviderDescriptor, ServerMsg, ToolDescriptor};
    use tokio::sync::mpsc;

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
        assert!(s.canonical_user.is_none());
        assert!(
            !s.active,
            "a session is inactive until a connection binds it"
        );
        assert!(Session::with_device(Some("dev-a".into()))
            .identity_keys
            .is_empty());
    }

    #[tokio::test]
    async fn federated_specs_return_active_same_user_sorted() {
        let store = SessionStore::new();
        // Sibling "b" sorts before "c": the routing "first" policy is lowest
        // session id wins.
        for (id, canonical, active) in [
            ("b", Some("user-1"), true),
            ("c", Some("user-1"), true),
            ("inactive", Some("user-1"), false),
            ("other-user", Some("user-2"), true),
        ] {
            let session = store.get(id).await;
            let mut s = session.write().await;
            s.canonical_user = canonical.map(str::to_string);
            s.active = active;
            s.context = ClientCatalog::from_announce(vec![ProviderDescriptor {
                id: id.into(),
                tools: vec![ToolDescriptor {
                    name: "t".into(),
                    description: "d".into(),
                    parameters: serde_json::json!({"type": "object"}),
                }],
            }]);
        }
        // Sibling specs only: the turn's own catalog is prepended separately
        // by build_request, so this returns the OTHER active same-user
        // sessions' specs, ascending by session id. Inactive and other-user
        // sessions contribute nothing.
        let from_b = store.federated_specs("b", "user-1").await;
        let names: Vec<String> = from_b
            .iter()
            .map(|s| s["function"]["name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            names,
            vec!["personal.c.t".to_string()],
            "active same-user siblings ascending, self excluded: {names:?}"
        );
        // Symmetric: from "c"'s perspective, "b" is the only sibling.
        let from_c = store.federated_specs("c", "user-1").await;
        let names: Vec<String> = from_c
            .iter()
            .map(|s| s["function"]["name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(names, vec!["personal.b.t".to_string()]);
    }

    #[tokio::test]
    async fn federated_specs_empty_without_canonical_user() {
        let store = SessionStore::new();
        let session = store.get("a").await;
        session.write().await.active = true;
        assert!(
            store.federated_specs("a", "").await.is_empty(),
            "no canonical user = never merges"
        );
    }

    #[tokio::test]
    async fn federation_router_registers_and_cleans_up() {
        let router = Router::default();
        let (tx, _rx) = mpsc::channel::<ServerMsg>(8);
        let (tx2, _rx2) = mpsc::channel::<ServerMsg>(8);
        router.register("user-1", "b", tx.clone()).await;
        router.register("user-1", "c", tx2.clone()).await;
        router.register("user-2", "d", tx.clone()).await;
        // Sibling routes for "c": "b" only (self excluded), ascending.
        let routes = router.routes_for("user-1", "c").await;
        assert_eq!(
            routes.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
            vec!["b"],
            "sibling routes ascending, self excluded"
        );
        // Unknown user → empty.
        assert!(router.routes_for("nobody", "b").await.is_empty());
        // Unregister removes exactly the one session.
        router.unregister("user-1", "b").await;
        assert!(
            router.routes_for("user-1", "c").await.is_empty(),
            "b was unregistered: no sibling routes left for c"
        );
        assert!(
            !router
                .routes_for("user-2", "d")
                .await
                .iter()
                .any(|(id, _)| id == "d"),
            "self is excluded even as the only session"
        );
        // Pending reply senders: armed under the sibling session, popped by
        // its tool.result handler, gone after unregister.
        let (reply_tx, mut reply_rx) = mpsc::channel::<(u64, bool, String)>(4);
        router.arm_pending("c", 7, reply_tx).await;
        let popped = router.take_pending("c", 7).await;
        assert!(popped.is_some(), "armed pending reply is retrievable");
        popped
            .unwrap()
            .send((7, true, "digest".into()))
            .await
            .expect("reply sender works");
        let got = tokio::time::timeout(std::time::Duration::from_secs(1), reply_rx.recv())
            .await
            .expect("reply arrives within bound");
        assert_eq!(got, Some((7, true, "digest".to_string())));
        assert!(
            router.take_pending("c", 7).await.is_none(),
            "pending is single-shot"
        );
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
