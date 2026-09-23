//! Realtime WebSocket sessions: `GET /v1/realtime`.
//!
//! One task per connection. The read loop parses `ClientMsg` frames, feeds a
//! per-connection [`UtteranceAssembler`] (server-side VAD), emits `State`
//! transitions, and spawns the turn pipeline on an abortable task when an
//! utterance finalizes. All writes funnel through ONE writer task draining an
//! mpsc — serialization by construction. `session.stop` (or a fresh
//! `session.start`) aborts any in-flight turn: the v1 interruption story.
//! Idle timeout comes from config via `tokio::time::timeout` on the read.

use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use futures::{SinkExt, StreamExt};
use harness_core::sentence::{ends_sentence, join_transcripts};
use harness_core::types::{ClientMsg, ServerMsg, SessionState};
use harness_core::vad::{Utterance, UtteranceAssembler, VadPolicy, WebrtcVad};
use harness_providers::stt_realtime::{spawn_link, RealtimeSttLink, SttRealtimeEvent};
use tokio::sync::mpsc;

use crate::client_tools::ClientCatalog;
use crate::http::RouterDeps;
use crate::identity::IdentityRegistry;
use crate::orchestrator::{run_text_turn, Deps};
use crate::state::{FederationRouter, SessionStore};

/// Write-channel bound per connection. The pipeline's own sends are awaited,
/// so a full channel applies backpressure rather than dropping events.
const WRITE_CHANNEL_BOUND: usize = 256;

/// A connection-scoped entry point: builds the orchestrator deps for a turn.
pub type DepsFactory = Arc<dyn Fn() -> Deps + Send + Sync>;

/// Everything the WS route needs from the router state.
#[derive(Clone)]
pub struct WsState {
    pub config: harness_core::config::Config,
    pub deps_factory: DepsFactory,
    pub sessions: Arc<SessionStore>,
    /// Streaming STT client for the ASR server's realtime transcription WS.
    /// `None` = batch mode (harness-side VAD + WAV upload).
    pub stt_realtime: Option<Arc<harness_providers::stt_realtime::RealtimeSttClient>>,
    /// Cross-session `personal.*` routes (canonical user → per-connection
    /// senders), shared with the orchestrator for federated routing.
    pub router: Arc<FederationRouter>,
    /// Persisted identity-key → canonical-user map for `session.start`.
    pub identities: Arc<tokio::sync::RwLock<IdentityRegistry>>,
}

impl WsState {
    /// Derive from the HTTP router state (same deps, one wiring path).
    pub fn from_router_deps(deps: &RouterDeps) -> Self {
        Self {
            config: deps.config.clone(),
            sessions: deps.sessions.clone(),
            stt_realtime: deps.stt_realtime.clone(),
            router: deps.router.clone(),
            identities: deps.identities.clone(),
            deps_factory: Arc::new({
                let deps = deps.clone();
                move || Deps {
                    config: deps.config.clone(),
                    llm: deps.llm.clone(),
                    tts: deps.tts.clone(),
                    stt: deps.stt.clone(),
                    plugins: deps.plugins.clone(),
                    store: Some(deps.sessions.clone()),
                    router: Some(deps.router.clone()),
                    self_session_id: None, // set per turn by dispatch_turn
                }
            }),
        }
    }
}

/// WebSocket upgrade limits: a legitimate 20 ms PCM16 @ 16 kHz audio chunk is
/// ~640 raw bytes (~1.3 KB base64) and control frames are tiny JSON, so 1 MiB
/// per message / 256 KiB per frame is far above anything a real client sends
/// while capping the damage a malicious peer can cause (memory exhaustion via
/// huge frames). tungstenite enforces these on read and fails the connection
/// when exceeded.
const WS_MAX_MESSAGE_SIZE: usize = 1024 * 1024;
const WS_MAX_FRAME_SIZE: usize = 256 * 1024;

/// `GET /v1/realtime` handler: validate the `Origin` header (cross-site
/// WebSocket hijacking guard), then upgrade and serve the connection loop.
///
/// Policy: no `Origin` header (native clients) → allowed. An `Origin` header
/// (browser contexts) is allowed only when it exactly matches an entry in
/// `server.allowed_origins`; an empty allowlist rejects every browser origin.
pub async fn realtime_handler(
    State(ws): State<WsState>,
    headers: axum::http::HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    if let Some(origin) = headers
        .get(axum::http::header::ORIGIN)
        .and_then(|v| v.to_str().ok())
    {
        if !ws.config.server.allowed_origins.iter().any(|o| o == origin) {
            tracing::warn!(origin, "websocket upgrade rejected: origin not allowed");
            return (axum::http::StatusCode::FORBIDDEN, "origin not allowed").into_response();
        }
    }
    upgrade
        .max_message_size(WS_MAX_MESSAGE_SIZE)
        .max_frame_size(WS_MAX_FRAME_SIZE)
        .on_upgrade(move |socket| handle_socket(socket, ws))
}

/// Serve one upgraded WebSocket connection until it closes or idles out.
pub async fn handle_socket(socket: WebSocket, session: WsState) {
    let (mut ws_tx, mut ws_rx) = socket.split();

    // ONE writer task: everything the pipeline/state machine emits goes
    // through this mpsc, and only this task touches the socket's send half.
    let (out_tx, mut out_rx) = mpsc::channel::<ServerMsg>(WRITE_CHANNEL_BOUND);
    let writer = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            let json = match serde_json::to_string(&msg) {
                Ok(j) => j,
                Err(e) => {
                    tracing::warn!("serialize server msg: {e}");
                    break;
                }
            };
            if ws_tx.send(Message::Text(json.into())).await.is_err() {
                break; // client gone
            }
        }
    });

    let mut conn = ConnState::new(&session, out_tx.clone());
    let idle = std::time::Duration::from_secs(session.config.session.idle_timeout_secs);

    // Read loop: idle timeout on every receive. Ends on close frame, idle
    // timeout, socket error, or a clean `session.stop`. While a held
    // (sentence-incomplete) transcript waits for its continuation, a flush
    // deadline joins the select: expiring dispatches the held text as a turn.
    // In realtime-STT mode an upstream-event poll also joins: deltas surface
    // as transcript messages, pump failures tear the link down (lazy
    // reconnect on the next audio chunk).
    loop {
        // Snapshot the flush deadline before the select: awaiting it inline
        // would pin an immutable borrow of `conn` across the whole select,
        // colliding with the `&mut conn` the other branches' handlers need.
        let flush_at = conn.hold_deadline().await;
        tokio::select! {
            incoming = tokio::time::timeout(idle, ws_rx.next()) => {
                let Ok(Some(msg)) = incoming else {
                    break;
                };
                let msg = match msg {
                    Ok(m) => m,
                    Err(e) => {
                        tracing::debug!("ws read error: {e}");
                        break;
                    }
                };
                match msg {
                    Message::Text(text) => {
                        if conn.on_text(&text).await {
                            break; // session.stop: clean disconnect
                        }
                    }
                    Message::Close(_) => break,
                    Message::Ping(_) | Message::Pong(_) | Message::Binary(_) => {}
                }
            }
            _ = async {
                match flush_at {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending::<()>().await,
                }
            } => {
                // The wait elapsed with no new speech: dispatch the held
                // transcript so a punctuation-free command can't hang.
                if conn.flush_held().await {
                    break;
                }
            }
            ev = async { conn.recv_upstream_event().await } => {
                conn.on_upstream_event(ev).await;
            }
        }
    }

    conn.abort_turn();
    // The connection is gone: its session is no longer ACTIVE (no catalog
    // contribution, no routing target) and its federated route is removed.
    if let Some(id) = conn.session_id.clone() {
        conn.deactivate_and_unregister(&id).await;
    }
    drop(conn);
    drop(out_tx); // writer drains whatever is queued, then exits
    let _ = writer.await;
}

/// Per-connection STT strategy. Chosen at connect time from
/// `WsState.stt_realtime`; batch mode is the default and the fallback.
enum SttMode {
    /// Harness VAD + batch transcription (default).
    Batch,
    /// Stream frames to the ASR server's realtime WS; upstream endpointing
    /// owns segmentation. VAD remains for UI state only. `link` is lazily
    /// connected on the first audio chunk and dropped on failure (the next
    /// chunk reconnects); `events` is the pump's event half. Both are Options
    /// so teardown can `take()` them.
    Realtime {
        client: Arc<harness_providers::stt_realtime::RealtimeSttClient>,
        link: Option<RealtimeSttLink>,
        events: Option<mpsc::Receiver<Result<SttRealtimeEvent, harness_core::error::HarnessError>>>,
    },
}

/// Everything one connection owns.
struct ConnState {
    config: harness_core::config::Config,
    deps_factory: DepsFactory,
    sessions: Arc<SessionStore>,
    /// Cross-session routes (shared with the orchestrator via WsState).
    router: Arc<FederationRouter>,
    /// Persisted identity-key → canonical-user map (shared via WsState).
    identities: Arc<tokio::sync::RwLock<IdentityRegistry>>,
    /// Outbound event channel feeding the single writer task.
    out: mpsc::Sender<ServerMsg>,
    /// Server-side VAD + utterance segmentation (one per connection). In
    /// realtime mode it drives UI state only — finalized utterances are
    /// discarded there.
    assembler: Option<UtteranceAssembler<WebrtcVad>>,
    /// Session id bound by `session.start` (device_id or generated).
    session_id: Option<String>,
    /// Sample rate announced by `session.start` (default 16000); the upstream
    /// realtime session is configured with it on (re)connect.
    sample_rate: u32,
    /// STT strategy for this connection.
    stt: SttMode,
    /// In-flight turn task; aborted by session.stop / new session.start.
    turn: Option<tokio::task::JoinHandle<()>>,
    /// Sender half of the per-turn client tool-result inbox; the orchestrator
    /// holds the receiver. `Some` only while a turn is in flight — a
    /// `tool.result` with no inbox is late and dropped.
    client_results: Option<mpsc::Sender<(u64, bool, String)>>,
    /// Sentence-end gate: transcript finalized but not yet sentence-terminal,
    /// waiting for the next utterance (or the flush deadline). Empty = none.
    held_transcript: String,
    /// Running partial transcript in realtime mode: upstream deltas are append
    /// increments, so they accumulate here and the client receives the
    /// cumulative text. Reset on completed/cleared/teardown/new session so
    /// each utterance starts fresh.
    upstream_partial: String,
    /// When the held transcript dispatches even without punctuation (a tokio
    /// Instant is only meaningful when `held_transcript` is non-empty).
    hold_deadline: Option<tokio::time::Instant>,
}

impl ConnState {
    /// VAD sub-frame size: 30 ms @ 16 kHz — the largest frame WebRTC VAD
    /// accepts. Client chunks are split into pieces of at most this size.
    const VAD_FRAME_SAMPLES: usize = 480;
    fn new(session: &WsState, out: mpsc::Sender<ServerMsg>) -> Self {
        let stt = match session.stt_realtime.clone() {
            Some(client) => SttMode::Realtime {
                client,
                link: None,
                events: None,
            },
            None => SttMode::Batch,
        };
        Self {
            config: session.config.clone(),
            deps_factory: session.deps_factory.clone(),
            sessions: session.sessions.clone(),
            router: session.router.clone(),
            identities: session.identities.clone(),
            out,
            assembler: None,
            session_id: None,
            sample_rate: 16_000,
            stt,
            turn: None,
            client_results: None,
            held_transcript: String::new(),
            upstream_partial: String::new(),
            hold_deadline: None,
        }
    }

    /// Handle one text frame. Returns `true` to end the connection (only on
    /// `session.stop`). Malformed frames produce an `Error` reply; the
    /// connection stays open.
    async fn on_text(&mut self, text: &str) -> bool {
        let msg: ClientMsg = match serde_json::from_str(text) {
            Ok(m) => m,
            Err(e) => {
                let _ = self
                    .out
                    .send(ServerMsg::Error {
                        code: "protocol".into(),
                        message: format!("unparseable client message: {e}"),
                    })
                    .await;
                return false;
            }
        };
        match msg {
            ClientMsg::SessionStart {
                device_id,
                sample_rate,
                ..
            } => {
                // A fresh session aborts any in-flight turn (interruption
                // story) and starts a new VAD stream.
                self.abort_turn();
                self.assembler = Some(self.new_assembler());
                self.session_id = Some(device_id.unwrap_or_else(next_session_id));
                // The upstream realtime session is configured with the
                // client's announced rate on the next (re)connect.
                if sample_rate > 0 {
                    self.sample_rate = sample_rate;
                }
                // New session = fresh conversation; any held fragment is
                // stale (its device has moved on).
                self.held_transcript.clear();
                self.hold_deadline = None;
                // The announced client catalog + identity belong to the
                // previous connection too: a fresh `session.start` (including
                // a reconnect) resets them — the client re-announces.
                if let Some(id) = self.session_id.clone() {
                    // The old session is no longer bound to this connection.
                    self.deactivate_and_unregister(&id).await;
                    let session = self.sessions.get(id).await;
                    let mut session = session.write().await;
                    session.context = ClientCatalog::default();
                    session.identity_keys = Vec::new();
                    session.canonical_user = None;
                }
                // Identity keys arrive with the announce (resolution happens
                // there); session.start only marks the session ACTIVE.
                if let Some(id) = self.session_id.clone() {
                    let session = self.sessions.get(id).await;
                    session.write().await.active = true;
                }
                // Register this connection as the session's route so sibling
                // sessions of the same canonical user can route to it.
                if let Some(id) = self.session_id.clone() {
                    let canonical = {
                        let session = self.sessions.get(id.clone()).await;
                        let guard = session.read().await;
                        guard.canonical_user.clone()
                    };
                    if let Some(canonical) = canonical {
                        self.router
                            .register(&canonical, &id, self.out.clone())
                            .await;
                    }
                }
                // Any per-turn inbox is stale with the aborted turn.
                self.client_results = None;
                // The upstream link belongs to the previous session: tear it
                // down so the upstream sees the socket close (lazy reconnect
                // re-dials with the new session's sample rate on the next
                // audio chunk).
                self.teardown_upstream();
                self.send_state(SessionState::Listening).await;
            }
            ClientMsg::AudioData { pcm } => {
                if self.assembler.is_none() {
                    let _ = self
                        .out
                        .send(ServerMsg::Error {
                            code: "protocol".into(),
                            message: "audio.data before session.start".into(),
                        })
                        .await;
                    return false;
                }
                let decoded = base64::engine::general_purpose::STANDARD.decode(&pcm).ok();
                let Some(bytes) = decoded else {
                    let _ = self
                        .out
                        .send(ServerMsg::Error {
                            code: "protocol".into(),
                            message: "audio.data payload is not base64 PCM16".into(),
                        })
                        .await;
                    return false;
                };
                let samples = samples_from_le_bytes(&bytes);
                let Some(samples) = samples else {
                    let _ = self
                        .out
                        .send(ServerMsg::Error {
                            code: "protocol".into(),
                            message: "audio.data payload is not base64 PCM16".into(),
                        })
                        .await;
                    return false;
                };
                // Borrow dance: feed_audio needs &mut self and &mut assembler.
                let mut assembler = self.assembler.take().unwrap();
                let result = self.feed_audio(&mut assembler, &samples).await;
                self.assembler = Some(assembler);
                // Realtime mode: forward the decoded bytes upstream (they are
                // already LE PCM16 — zero re-encode). Batch mode: transcribe
                // each finalized utterance.
                match &mut self.stt {
                    SttMode::Batch => {
                        for utt in result.utterances {
                            self.ingest_utterance(utt).await;
                        }
                    }
                    SttMode::Realtime {
                        client,
                        link,
                        events,
                    } => {
                        // VAD already ran for UI state above; finalized
                        // utterances are discarded — upstream endpointing
                        // owns segmentation.
                        if link.is_none() {
                            match spawn_link(client, self.sample_rate).await {
                                Ok((l, ev)) => {
                                    *link = Some(l);
                                    *events = Some(ev);
                                }
                                Err(e) => {
                                    // Keep the connection alive; retry on the
                                    // next chunk (lazy reconnect).
                                    let _ = self
                                        .out
                                        .send(ServerMsg::Error {
                                            code: "stt".into(),
                                            message: e.to_string(),
                                        })
                                        .await;
                                    return false;
                                }
                            }
                        }
                        // The decoded base64 payload, verbatim (LE PCM16).
                        if let Err(e) = link.as_ref().unwrap().send_pcm(bytes).await {
                            // Pump died: drop the link; the next chunk
                            // reconnects.
                            *link = None;
                            let _ = self
                                .out
                                .send(ServerMsg::Error {
                                    code: "stt".into(),
                                    message: e.to_string(),
                                })
                                .await;
                        }
                    }
                }
            }
            ClientMsg::SpeechEnd => match &mut self.stt {
                SttMode::Realtime { link, .. } => {
                    // Upstream owns segmentation: `speech.end` flushes the
                    // buffered audio (`input_audio_buffer.commit`) instead
                    // of transcribing locally. A dead pump drops the link
                    // (the next chunk reconnects lazily).
                    let committed = match link.as_ref() {
                        Some(l) => l.commit().await.is_ok(),
                        None => true,
                    };
                    if !committed {
                        *link = None;
                    }
                    // Same UI tail as batch, minus transcription: the
                    // force-ended utterance is discarded — upstream
                    // endpointing owns the text.
                    if let Some(assembler) = self.assembler.as_mut() {
                        if assembler.force_end().is_some() {
                            self.send_state(SessionState::Listening).await;
                        }
                    }
                    // "That's all": a held fragment dispatches now, deadline
                    // or not — the identical tail batch mode runs.
                    if !self.held_transcript.is_empty() {
                        self.flush_held().await;
                    }
                }
                SttMode::Batch => {
                    if let Some(assembler) = self.assembler.as_mut() {
                        if let Some(utt) = assembler.force_end() {
                            self.send_state(SessionState::Listening).await;
                            self.ingest_utterance(utt).await;
                        } else if !self.held_transcript.is_empty() {
                            // No utterance open, but a held fragment is
                            // waiting: the user (or client logic) says
                            // "that's all" — dispatch it now, don't wait out
                            // the timer.
                            self.flush_held().await;
                        }
                    }
                }
            },
            ClientMsg::SessionStop => {
                self.abort_turn();
                // The upstream link belongs to the session: stop = tear it
                // down (the pump exits when the command channel closes, the
                // upstream sees the socket close).
                self.teardown_upstream();
                if let Some(id) = self.session_id.clone() {
                    self.deactivate_and_unregister(&id).await;
                }
                self.assembler = None;
                self.session_id = None;
                self.held_transcript.clear();
                self.hold_deadline = None;
                // No turn can be in flight after the abort: late results
                // would have nowhere to go.
                self.client_results = None;
                return true; // clean disconnect
            }
            // Client context catalog: upsert on the bound session. No session
            // yet (announce before `session.start`) → warn + ignore: the
            // client re-announces after the handshake per the protocol order.
            ClientMsg::ContextAnnounce {
                providers,
                identity_keys,
            } => match self.session_id.clone() {
                Some(id) => {
                    let session = self.sessions.get(id.clone()).await;
                    let canonical = if self.config.personal_context.federation.enabled
                        && !identity_keys.is_empty()
                    {
                        let mut registry = self.identities.write().await;
                        let user = registry.canonical_user_for(&identity_keys);
                        let _ = registry.save();
                        Some(user)
                    } else {
                        None
                    };
                    let mut session = session.write().await;
                    session.context = ClientCatalog::from_announce(providers);
                    session.identity_keys = identity_keys;
                    let previous = session.canonical_user.clone();
                    session.canonical_user = canonical.clone();
                    // Re-register the route under the resolved canonical user.
                    if previous != canonical {
                        if let Some(old) = previous {
                            self.router.unregister(&old, &id).await;
                        }
                        if let Some(canonical) = canonical {
                            self.router
                                .register(&canonical, &id, self.out.clone())
                                .await;
                        }
                    }
                }
                None => {
                    tracing::warn!(
                        "context.announce before session.start: ignored ({} providers)",
                        providers.len()
                    );
                }
            },
            // A client tool result: either the answer to a call routed here
            // from a SIBLING session's turn (the pending-reply registry
            // matches first), or the answer to this connection's own turn
            // (forwarded into the per-turn inbox). No match → late result,
            // warn + drop.
            ClientMsg::ToolResult { call_id, ok, text } => {
                if let Some(reply) = self
                    .router
                    .take_pending(&self.session_id.clone().unwrap_or_default(), call_id)
                    .await
                {
                    if let Err(e) = reply.try_send((call_id, ok, text.clone())) {
                        let why = match e {
                            mpsc::error::TrySendError::Full(_) => "reply inbox full",
                            mpsc::error::TrySendError::Closed(_) => "reply inbox closed",
                        };
                        tracing::warn!("routed tool.result for call {call_id} dropped: {why}");
                    }
                } else {
                    Self::forward_own_result(self, call_id, ok, text).await;
                }
            }
        }
        false
    }

    /// Forward a tool result into THIS connection's per-turn inbox (the
    /// own-turn path). No inbox (turn already finished / timed out / never
    /// started) → late result, warn + drop. Full inbox (capacity 4) → drop
    /// the newest so a flooding client can never backpressure the read loop.
    async fn forward_own_result(&mut self, call_id: u64, ok: bool, text: String) {
        match self.client_results.clone() {
            Some(tx) => {
                if let Err(e) = tx.try_send((call_id, ok, text)) {
                    // Full = the orchestrator is not draining fast enough
                    // (or is stuck): drop the newest. Closed = the turn
                    // task finished between the clone and this send.
                    let why = match e {
                        mpsc::error::TrySendError::Full(_) => "turn inbox full",
                        mpsc::error::TrySendError::Closed(_) => "turn inbox closed",
                    };
                    tracing::warn!("tool.result for call {call_id} dropped: {why}");
                }
            }
            None => {
                tracing::warn!("late tool.result for call {call_id}: no turn is waiting");
            }
        }
    }

    /// Feed one chunk through the assembler; emit State transitions. Client
    /// chunks arrive at arbitrary sizes (the macOS tap emits ~300 ms batches,
    /// an ESP32 may send 20 ms or 500 ms), but WebRTC VAD only accepts
    /// 10/20/30 ms frames — so every chunk is split into ≤30 ms sub-frames
    /// before the assembler. All utterances finalized within the chunk are
    /// returned (a 480 ms chunk can close one utterance and open another).
    async fn feed_audio(
        &mut self,
        assembler: &mut UtteranceAssembler<WebrtcVad>,
        samples: &[i16],
    ) -> FeedResult {
        let was_active = assembler.is_active();
        let mut utterances = Vec::new();
        for sub in samples.chunks(Self::VAD_FRAME_SAMPLES) {
            if let Some(utt) = assembler.push(sub, 0) {
                self.send_state(SessionState::Listening).await;
                utterances.push(utt);
            }
        }
        if !was_active && assembler.is_active() {
            self.send_state(SessionState::Speech).await;
        }
        FeedResult { utterances }
    }

    /// Transcribe one finalized utterance: forward the transcript to the
    /// client immediately, then apply the shared sentence-end gate.
    async fn ingest_utterance(&mut self, utt: Utterance) {
        let deps = (self.deps_factory)();
        let text = match deps.stt.transcribe(&utt.pcm).await {
            Ok(t) => t,
            Err(e) => {
                let _ = self
                    .out
                    .send(ServerMsg::Error {
                        code: "stt".into(),
                        message: e.to_string(),
                    })
                    .await;
                return;
            }
        };
        let _ = self
            .out
            .send(ServerMsg::Transcript { text: text.clone() })
            .await;
        self.gate_transcript(&text).await;
    }

    /// Sentence-end gate for a finalized transcript. Empty text is ignored;
    /// sentence-terminal text (or gate off) dispatches the accumulated text as
    /// a turn; otherwise the fragment is held with a flush deadline. Used by
    /// BOTH the batch path (after STT returns) and the realtime path (on
    /// completed).
    ///
    /// With `session.require_sentence_end` on, a transcript that does not end
    /// sentence-terminal (`. ! ? …`) is treated as a mid-sentence VAD pause:
    /// it is concatenated onto any previously held fragment and held with a
    /// flush deadline instead of dispatching. A sentence-terminal transcript
    /// dispatches the whole accumulated text as one turn.
    async fn gate_transcript(&mut self, text: &str) {
        if text.trim().is_empty() {
            return; // silent audio / STT found nothing: nothing to hold
        }
        if !self.config.session.require_sentence_end || ends_sentence(text) {
            // Sentence complete (or the gate is off): dispatch everything.
            let held = std::mem::take(&mut self.held_transcript);
            self.hold_deadline = None;
            let full = join_transcripts(&held, text);
            if full != text {
                // The fragment already echoed itself when it was held; send
                // the joined view so the client's transcript shows the whole
                // sentence, then dispatch.
                let _ = self
                    .out
                    .send(ServerMsg::Transcript { text: full.clone() })
                    .await;
            }
            self.dispatch_turn(&full);
            return;
        }
        // Mid-sentence fragment: hold it and arm the flush deadline so a
        // complete command can never hang forever if no continuation comes.
        self.held_transcript = join_transcripts(&self.held_transcript, text);
        self.hold_deadline = Some(
            tokio::time::Instant::now()
                + std::time::Duration::from_millis(self.config.session.sentence_end_wait_ms),
        );
        tracing::debug!(held = %self.held_transcript, "sentence gate: holding fragment");
    }

    /// The flush deadline the read loop's select should await, if any. While
    /// the VAD has an utterance open (speech in progress) the deadline is
    /// deferred — the timer flushes silence, it must not cut off the
    /// continuation of the very sentence it is holding.
    async fn hold_deadline(&self) -> Option<tokio::time::Instant> {
        if self.held_transcript.is_empty() {
            return None;
        }
        let speech_open = self.assembler.as_ref().is_some_and(|a| a.is_active());
        if speech_open {
            None
        } else {
            self.hold_deadline
        }
    }

    /// Dispatch the held transcript now (flush deadline expired, or an
    /// explicit `speech.end`). Returns `true` to end the connection — never,
    /// mirroring the `on_text` contract.
    async fn flush_held(&mut self) -> bool {
        if self.held_transcript.trim().is_empty() {
            self.hold_deadline = None;
            return false;
        }
        let text = std::mem::take(&mut self.held_transcript);
        self.hold_deadline = None;
        self.dispatch_turn(&text);
        false
    }

    /// Start the turn pipeline on an abortable task for a final (possibly
    /// accumulated) user text. The task owns the orchestrator deps and the
    /// session lock; events flow out via `out`.
    fn dispatch_turn(&mut self, text: &str) {
        // One turn at a time: while a turn task is still running (echoing
        // audio can produce finals mid-answer in both STT modes), a new
        // dispatch is dropped instead of racing the session lock.
        if self.turn.as_ref().is_some_and(|t| !t.is_finished()) {
            tracing::warn!("turn already in flight; dropping dispatch of {text:?}");
            return;
        }
        let mut deps = (self.deps_factory)();
        deps.self_session_id = self.session_id.clone();
        let out = self.out.clone();
        let session_id = self.session_id.clone().unwrap_or_else(next_session_id);
        let store = self.sessions.clone();
        let text = text.to_string();
        // Per-turn inbox of client tool results: the WS read loop forwards
        // `tool.result` frames into `tx` while this turn runs; the
        // orchestrator waits on `rx` for the call it dispatched.
        let (result_tx, mut result_rx) = mpsc::channel(4);
        self.client_results = Some(result_tx.clone());
        let turn = tokio::spawn(async move {
            let session = store.get(session_id).await;
            let mut session = session.write().await;
            // `Thinking` goes out before the LLM round starts.
            let _ = out
                .send(ServerMsg::State {
                    state: SessionState::Thinking,
                })
                .await;
            if let Err(e) = run_text_turn(
                &deps,
                &mut session,
                &text,
                out.clone(),
                Some(&mut result_rx),
            )
            .await
            {
                let _ = out
                    .send(ServerMsg::Error {
                        code: "turn".into(),
                        message: e.to_string(),
                    })
                    .await;
            }
            // Speaking while the client drains the audio, then back to listening.
            let _ = out
                .send(ServerMsg::State {
                    state: SessionState::Speaking,
                })
                .await;
            let _ = out
                .send(ServerMsg::State {
                    state: SessionState::Listening,
                })
                .await;
            // Turn over: results arriving from here on are late (dropped by
            // the read loop once this clears).
            drop(result_tx);
        });
        self.turn = Some(turn);
    }

    /// Abort the in-flight turn task, if any.
    fn abort_turn(&mut self) {
        if let Some(turn) = self.turn.take() {
            turn.abort();
        }
    }

    /// Poll the upstream realtime-STT event channel, if one is live. `None`
    /// (no channel or channel closed) maps to `None` here so the read loop's
    /// select branch never spins: a closed channel is a one-shot teardown.
    async fn recv_upstream_event(
        &mut self,
    ) -> Option<Result<SttRealtimeEvent, harness_core::error::HarnessError>> {
        match &mut self.stt {
            SttMode::Batch => std::future::pending().await,
            SttMode::Realtime { events, .. } => match events {
                Some(ev) => ev.recv().await,
                None => std::future::pending().await,
            },
        }
    }

    /// Handle one upstream realtime-STT event (or channel closure). Deltas
    /// surface to the client as transcript messages; finals surface their
    /// text and then run the shared sentence-end gate (sentence-terminal
    /// text dispatches the turn immediately). Pump failures/closure tear the
    /// link down — the next audio chunk reconnects lazily.
    async fn on_upstream_event(
        &mut self,
        ev: Option<Result<SttRealtimeEvent, harness_core::error::HarnessError>>,
    ) {
        match ev {
            Some(Ok(SttRealtimeEvent::Delta { text })) => {
                // Upstream deltas are append increments (VERIFIED from
                // http_server.cpp): accumulate so the client sees the running
                // partial and its transcript line grows instead of flashing
                // word-by-word.
                self.upstream_partial.push_str(&text);
                let _ = self
                    .out
                    .send(ServerMsg::Transcript {
                        text: self.upstream_partial.clone(),
                    })
                    .await;
            }
            Some(Ok(SttRealtimeEvent::Completed { text })) => {
                // The client sees the final transcript (same as batch mode),
                // then the gate decides: dispatch now, or hold the fragment.
                self.upstream_partial.clear();
                let _ = self
                    .out
                    .send(ServerMsg::Transcript { text: text.clone() })
                    .await;
                self.gate_transcript(&text).await;
            }
            Some(Ok(other)) => {
                // A cleared upstream buffer discards the partial too.
                if matches!(other, SttRealtimeEvent::Cleared) {
                    self.upstream_partial.clear();
                }
                tracing::debug!(?other, "stt realtime event");
            }
            Some(Err(e)) => {
                self.teardown_upstream();
                let _ = self
                    .out
                    .send(ServerMsg::Error {
                        code: "stt".into(),
                        message: e.to_string(),
                    })
                    .await;
            }
            None => self.teardown_upstream(),
        }
    }

    /// Drop the upstream link + event receiver (pump exits when the command
    /// channel closes). The next audio chunk reconnects lazily.
    fn teardown_upstream(&mut self) {
        // A fresh (re)connect starts a new upstream session with an empty
        // buffer — the running partial is stale from here on.
        self.upstream_partial.clear();
        match &mut self.stt {
            SttMode::Batch => {}
            SttMode::Realtime { link, events, .. } => {
                *link = None;
                *events = None;
            }
        }
    }

    /// Mark the bound session INACTIVE and drop its federated route. The
    /// session itself (history, catalog) survives: a reconnect rebinds it.
    async fn deactivate_and_unregister(&self, id: &str) {
        let session = self.sessions.get(id).await;
        let canonical = {
            let mut session = session.write().await;
            let canonical = session.canonical_user.clone();
            session.active = false;
            canonical
        };
        if let Some(canonical) = canonical {
            self.router.unregister(&canonical, id).await;
        }
    }

    fn new_assembler(&self) -> UtteranceAssembler<WebrtcVad> {
        let cfg = &self.config.session;
        UtteranceAssembler::new(
            VadPolicy {
                silence_ms: cfg.silence_ms,
                min_utterance_ms: cfg.min_utterance_ms,
                max_utterance_ms: cfg.max_utterance_ms,
                pre_speech_ms: cfg.pre_speech_ms,
            },
            WebrtcVad::new(),
        )
    }

    async fn send_state(&self, state: SessionState) {
        let _ = self.out.send(ServerMsg::State { state }).await;
    }
}

/// Outcome of feeding one audio chunk: all utterances finalized within it
/// (can be more than one for chunks longer than an utterance's max length,
/// and zero when no endpoint fired).
struct FeedResult {
    utterances: Vec<Utterance>,
}

fn samples_from_le_bytes(bytes: &[u8]) -> Option<Vec<i16>> {
    if !bytes.len().is_multiple_of(2) {
        return None;
    }
    Some(
        bytes
            .chunks_exact(2)
            .map(|p| i16::from_le_bytes([p[0], p[1]]))
            .collect(),
    )
}

fn next_session_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    format!("conn-{}", NEXT.fetch_add(1, Ordering::Relaxed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::SessionStore;
    use harness_core::config::Config;
    use harness_core::types::{ProviderDescriptor, ToolDescriptor};

    fn calendar_announce() -> Vec<ProviderDescriptor> {
        vec![ProviderDescriptor {
            id: "calendar".into(),
            tools: vec![ToolDescriptor {
                name: "events".into(),
                description: "List events.".into(),
                parameters: serde_json::json!({"type": "object", "properties": {}}),
            }],
        }]
    }

    /// A bare `ConnState` with a stub deps factory (never invoked by the
    /// handlers under test) and a fresh session store.
    fn conn() -> (ConnState, mpsc::Receiver<ServerMsg>) {
        let (out_tx, out_rx) = mpsc::channel(64);
        let ws = WsState {
            config: Config::default(),
            deps_factory: Arc::new(|| Deps {
                config: Config::default(),
                llm: Arc::new(DeadLlm),
                tts: Arc::new(DeadTts),
                stt: Arc::new(DeadStt),
                plugins: Arc::new(harness_plugins::PluginRegistry::new()),
                store: None,
                router: None,
                self_session_id: None,
            }),
            sessions: Arc::new(SessionStore::new()),
            stt_realtime: None,
            router: Arc::new(FederationRouter::new()),
            identities: Arc::new(tokio::sync::RwLock::new(
                crate::identity::IdentityRegistry::default(),
            )),
        };
        (ConnState::new(&ws, out_tx), out_rx)
    }

    // -------------------------------------------------- mock providers
    // The handlers under test never call these; they only satisfy `Deps`.

    struct DeadLlm;
    #[async_trait::async_trait]
    impl harness_providers::llm::LlmProvider for DeadLlm {
        async fn stream_chat(
            &self,
            _req: harness_providers::llm::ChatRequest,
        ) -> Result<harness_providers::llm::ChatStream, harness_core::error::HarnessError> {
            Err(harness_core::error::HarnessError::protocol("unused"))
        }
    }

    struct DeadTts;
    #[async_trait::async_trait]
    impl harness_providers::tts::TtsProvider for DeadTts {
        async fn synthesize(
            &self,
            _text: &str,
        ) -> Result<Vec<u8>, harness_core::error::HarnessError> {
            Err(harness_core::error::HarnessError::protocol("unused"))
        }
    }

    struct DeadStt;
    #[async_trait::async_trait]
    impl harness_providers::stt::SttProvider for DeadStt {
        async fn transcribe(
            &self,
            _pcm16k: &[i16],
        ) -> Result<String, harness_core::error::HarnessError> {
            Err(harness_core::error::HarnessError::protocol("unused"))
        }
    }

    #[tokio::test]
    async fn context_announce_stores_catalog_on_session() {
        let (mut conn, _rx) = conn();
        conn.on_text(r#"{"type":"session.start","device_id":"dev-a"}"#)
            .await;
        let providers = calendar_announce();
        let msg = serde_json::to_string(&ClientMsg::ContextAnnounce {
            providers,
            identity_keys: Vec::new(),
        })
        .unwrap();
        conn.on_text(&msg).await;

        let session = conn.sessions.get("dev-a").await;
        let session = session.read().await;
        assert!(
            !session.context.is_empty(),
            "announce must store the catalog on the session"
        );
        assert_eq!(session.context.providers.len(), 1);
        assert_eq!(
            session
                .context
                .resolve("personal.calendar.events")
                .expect("announced tool is routable")
                .name,
            "events"
        );
    }

    #[tokio::test]
    async fn context_announce_without_session_is_ignored() {
        let (mut conn, mut rx) = conn();
        // No session.start: the announce has nowhere to land.
        let msg = serde_json::to_string(&ClientMsg::ContextAnnounce {
            providers: calendar_announce(),
            identity_keys: Vec::new(),
        })
        .unwrap();
        let end = conn.on_text(&msg).await;
        assert!(!end, "announce before session.start must not close");
        // No error frame either: tolerated + logged, connection usable.
        assert!(
            rx.try_recv().is_err(),
            "no outbound frame for a session-less announce"
        );
    }

    #[tokio::test]
    async fn session_start_resets_stale_catalog() {
        let (mut conn, _rx) = conn();
        conn.on_text(r#"{"type":"session.start","device_id":"dev-a"}"#)
            .await;
        let msg = serde_json::to_string(&ClientMsg::ContextAnnounce {
            providers: calendar_announce(),
            identity_keys: Vec::new(),
        })
        .unwrap();
        conn.on_text(&msg).await;

        // A reconnect with the same device id starts fresh: any catalog left
        // by the previous connection is stale.
        conn.on_text(r#"{"type":"session.start","device_id":"dev-a"}"#)
            .await;
        let session = conn.sessions.get("dev-a").await;
        let session = session.read().await;
        assert!(
            session.context.is_empty(),
            "fresh session.start clears a stale announce"
        );
    }

    #[tokio::test]
    async fn tool_result_reaches_the_turn_inbox() {
        let (mut conn, _rx) = conn();
        conn.on_text(r#"{"type":"session.start","device_id":"dev-a"}"#)
            .await;
        let (tx, mut inbx) = mpsc::channel(4);
        conn.client_results = Some(tx);

        let msg = serde_json::to_string(&ClientMsg::ToolResult {
            call_id: 7,
            ok: true,
            text: "Dentist 9:30".into(),
        })
        .unwrap();
        conn.on_text(&msg).await;

        let got = tokio::time::timeout(std::time::Duration::from_secs(1), inbx.recv())
            .await
            .expect("result forwarded within bound");
        assert_eq!(got, Some((7, true, "Dentist 9:30".to_string())));
    }

    #[tokio::test]
    async fn tool_result_without_turn_is_dropped() {
        let (mut conn, mut rx) = conn();
        conn.on_text(r#"{"type":"session.start","device_id":"dev-a"}"#)
            .await;
        // The session.start ack (State(listening)) is still queued — it is
        // not part of this test's subject. Drain it first so any frame the
        // tool.result handler (wrongly) emits is deterministic.
        assert_eq!(
            rx.try_recv().ok(),
            Some(ServerMsg::State {
                state: SessionState::Listening
            })
        );
        // No in-flight turn: no `client_results` sender.
        let msg = serde_json::to_string(&ClientMsg::ToolResult {
            call_id: 3,
            ok: true,
            text: "late".into(),
        })
        .unwrap();
        let end = conn.on_text(&msg).await;
        assert!(!end, "a late tool.result must not close the connection");
        // Allow a moment for any (wrong) outbound frame, then assert none.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            rx.try_recv().is_err(),
            "dropped result sends no outbound frame"
        );
    }

    #[tokio::test]
    async fn full_result_inbox_drops_the_newest_result() {
        let (mut conn, _rx) = conn();
        conn.on_text(r#"{"type":"session.start","device_id":"dev-a"}"#)
            .await;
        let (tx, mut inbx) = mpsc::channel(4);
        conn.client_results = Some(tx);
        // Fill the inbox to capacity (4).
        for i in 0..4 {
            let msg = serde_json::to_string(&ClientMsg::ToolResult {
                call_id: i,
                ok: true,
                text: "fill".into(),
            })
            .unwrap();
            conn.on_text(&msg).await;
        }
        // Fifth result: capacity full → warn + drop, connection stays open.
        let msg = serde_json::to_string(&ClientMsg::ToolResult {
            call_id: 99,
            ok: true,
            text: "overflow".into(),
        })
        .unwrap();
        let end = conn.on_text(&msg).await;
        assert!(!end, "a full inbox must not close the connection");
        // Drain: exactly the first 4 results, no fifth.
        let mut ids = Vec::new();
        while let Ok(Some((id, _, _))) =
            tokio::time::timeout(std::time::Duration::from_millis(100), inbx.recv()).await
        {
            ids.push(id);
        }
        assert_eq!(ids, vec![0, 1, 2, 3], "overflow result is dropped");
    }

    #[tokio::test]
    async fn tool_result_for_a_routed_sibling_call_reaches_the_reply() {
        // A sibling turn routed a call to THIS connection: the orchestrator
        // armed a pending reply under this session. The client's tool.result
        // must flow into that reply channel (not the, here absent, own inbox).
        let (mut conn, mut rx) = conn();
        conn.on_text(r#"{"type":"session.start","device_id":"dev-a"}"#)
            .await;
        let (reply_tx, mut reply_rx) = mpsc::channel(4);
        conn.router.arm_pending("dev-a", 42, reply_tx).await;
        let msg = serde_json::to_string(&ClientMsg::ToolResult {
            call_id: 42,
            ok: true,
            text: "Mom: +1 555 0100".into(),
        })
        .unwrap();
        let end = conn.on_text(&msg).await;
        assert!(!end, "routed result must not close the connection");
        let got = tokio::time::timeout(std::time::Duration::from_secs(1), reply_rx.recv())
            .await
            .expect("routed reply forwarded within bound");
        assert_eq!(got, Some((42, true, "Mom: +1 555 0100".to_string())));
        // No stray outbound frames from the forwarding.
        let _ = rx.try_recv();
    }

    #[tokio::test]
    async fn session_stop_deactivates_and_unregisters() {
        let (mut conn, _rx) = conn();
        conn.on_text(r#"{"type":"session.start","device_id":"dev-a"}"#)
            .await;
        {
            let session = conn.sessions.get("dev-a").await;
            let mut s = session.write().await;
            s.active = true;
            s.canonical_user = Some("user-1".into());
        }
        conn.router
            .register("user-1", "dev-a", conn.out.clone())
            .await;
        let stop = conn.on_text(r#"{"type":"session.stop"}"#).await;
        assert!(stop, "session.stop ends the connection");
        let session = conn.sessions.get("dev-a").await;
        let s = session.read().await;
        assert!(!s.active, "session.stop marks the session inactive");
        assert!(
            conn.router.routes_for("user-1", "other").await.is_empty(),
            "the route is unregistered"
        );
    }

    #[tokio::test]
    async fn session_stop_clears_the_inbox() {
        let (mut conn, _rx) = conn();
        conn.on_text(r#"{"type":"session.start","device_id":"dev-a"}"#)
            .await;
        let (tx, _inbx) = mpsc::channel(4);
        conn.client_results = Some(tx);
        let stop = conn.on_text(r#"{"type":"session.stop"}"#).await;
        assert!(stop, "session.stop ends the connection");
        assert!(
            conn.client_results.is_none(),
            "session.stop must clear the per-turn inbox"
        );
    }
}
