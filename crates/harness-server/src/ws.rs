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
use axum::response::Response;
use base64::Engine as _;
use futures::{SinkExt, StreamExt};
use harness_core::types::{ClientMsg, ServerMsg, SessionState};
use harness_core::vad::{Utterance, UtteranceAssembler, VadPolicy, WebrtcVad};
use tokio::sync::mpsc;

use crate::http::RouterDeps;
use crate::orchestrator::{run_audio_utterance, Deps};
use crate::state::SessionStore;

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
}

impl WsState {
    /// Derive from the HTTP router state (same deps, one wiring path).
    pub fn from_router_deps(deps: &RouterDeps) -> Self {
        Self {
            config: deps.config.clone(),
            sessions: deps.sessions.clone(),
            deps_factory: Arc::new({
                let deps = deps.clone();
                move || Deps {
                    config: deps.config.clone(),
                    llm: deps.llm.clone(),
                    tts: deps.tts.clone(),
                    stt: deps.stt.clone(),
                    plugins: deps.plugins.clone(),
                }
            }),
        }
    }
}

/// `GET /v1/realtime` handler: upgrade, then serve the connection loop.
pub async fn realtime_handler(State(ws): State<WsState>, upgrade: WebSocketUpgrade) -> Response {
    upgrade.on_upgrade(move |socket| handle_socket(socket, ws))
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
    // timeout, socket error, or a clean `session.stop`.
    loop {
        let incoming = tokio::time::timeout(idle, ws_rx.next()).await;
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

    conn.abort_turn();
    drop(conn);
    drop(out_tx); // writer drains whatever is queued, then exits
    let _ = writer.await;
}

/// Everything one connection owns.
struct ConnState {
    config: harness_core::config::Config,
    deps_factory: DepsFactory,
    sessions: Arc<SessionStore>,
    /// Outbound event channel feeding the single writer task.
    out: mpsc::Sender<ServerMsg>,
    /// Server-side VAD + utterance segmentation (one per connection).
    assembler: Option<UtteranceAssembler<WebrtcVad>>,
    /// Session id bound by `session.start` (device_id or generated).
    session_id: Option<String>,
    /// In-flight turn task; aborted by session.stop / new session.start.
    turn: Option<tokio::task::JoinHandle<()>>,
    /// Debug accounting (VH_DEBUG_WIRE=1): received-frame count and max |sample|.
    debug_wire: bool,
    wire_frames: u64,
    wire_peak: u16,
}

impl ConnState {
    /// VAD sub-frame size: 30 ms @ 16 kHz — the largest frame WebRTC VAD
    /// accepts. Client chunks are split into pieces of at most this size.
    const VAD_FRAME_SAMPLES: usize = 480;
    fn new(session: &WsState, out: mpsc::Sender<ServerMsg>) -> Self {
        Self {
            config: session.config.clone(),
            deps_factory: session.deps_factory.clone(),
            sessions: session.sessions.clone(),
            out,
            assembler: None,
            session_id: None,
            turn: None,
            debug_wire: std::env::var("VH_DEBUG_WIRE").as_deref() == Ok("1"),
            wire_frames: 0,
            wire_peak: 0,
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
            ClientMsg::SessionStart { device_id, .. } => {
                // A fresh session aborts any in-flight turn (interruption
                // story) and starts a new VAD stream.
                self.abort_turn();
                self.assembler = Some(self.new_assembler());
                self.session_id = Some(device_id.unwrap_or_else(next_session_id));
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
                let decoded = base64::engine::general_purpose::STANDARD
                    .decode(&pcm)
                    .ok()
                    .and_then(|bytes| samples_from_le_bytes(&bytes));
                let Some(samples) = decoded else {
                    let _ = self
                        .out
                        .send(ServerMsg::Error {
                            code: "protocol".into(),
                            message: "audio.data payload is not base64 PCM16".into(),
                        })
                        .await;
                    return false;
                };
                // Debug accounting of what actually arrives over the wire
                // (VH_DEBUG_WIRE=1): count frames and track the max |sample|.
                if self.debug_wire {
                    let frame_peak = samples.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
                    self.wire_frames += 1;
                    if frame_peak > self.wire_peak {
                        self.wire_peak = frame_peak;
                    }
                    if self.wire_frames.is_multiple_of(20) {
                        tracing::info!(
                            "wire: frames={} peak={} samples={}",
                            self.wire_frames,
                            self.wire_peak,
                            samples.len()
                        );
                    }
                }
                // Borrow dance: feed_audio needs &mut self and &mut assembler.
                let mut assembler = self.assembler.take().unwrap();
                let result = self.feed_audio(&mut assembler, &samples).await;
                self.assembler = Some(assembler);
                for utt in result.utterances {
                    self.spawn_turn(utt);
                }
            }
            ClientMsg::SpeechEnd => {
                if let Some(assembler) = self.assembler.as_mut() {
                    if let Some(utt) = assembler.force_end() {
                        self.send_state(SessionState::Listening).await;
                        self.spawn_turn(utt);
                    }
                }
            }
            ClientMsg::SessionStop => {
                self.abort_turn();
                self.assembler = None;
                self.session_id = None;
                return true; // clean disconnect
            }
        }
        false
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

    /// Start the turn pipeline on an abortable task. The task owns the
    /// orchestrator deps and the session lock; events flow out via `out`.
    fn spawn_turn(&mut self, utt: Utterance) {
        let deps = (self.deps_factory)();
        let out = self.out.clone();
        let session_id = self.session_id.clone().unwrap_or_else(next_session_id);
        let store = self.sessions.clone();
        let turn = tokio::spawn(async move {
            let session = store.get(session_id).await;
            let mut session = session.write().await;
            // `Thinking` goes out before the LLM round starts.
            let _ = out
                .send(ServerMsg::State {
                    state: SessionState::Thinking,
                })
                .await;
            if let Err(e) = run_audio_utterance(&deps, &mut session, &utt.pcm, out.clone()).await {
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
        });
        self.turn = Some(turn);
    }

    /// Abort the in-flight turn task, if any.
    fn abort_turn(&mut self) {
        if let Some(turn) = self.turn.take() {
            turn.abort();
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
