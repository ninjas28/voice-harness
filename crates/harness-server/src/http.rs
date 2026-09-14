//! HTTP surface: `POST /v1/turn` (JSON text or raw WAV ingest), `GET /healthz`,
//! and bearer/X-API-Key auth middleware.
//!
//! A turn runs synchronously over a bounded mpsc: the orchestrator's
//! `ServerMsg` stream is collected into a single response object
//! `{transcript, response_text, audio_wav_base64, audio_seq}` with all audio
//! chunks assembled into one WAV (16 kHz mono PCM16).

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine as _;
use harness_core::config::Config;
use harness_core::error::HarnessError;
use harness_core::types::ServerMsg;
use harness_providers::stt::wav_from_pcm;
use serde::Deserialize;
use tokio::sync::mpsc;

use crate::orchestrator::{run_audio_utterance, run_text_turn, Deps};
use crate::state::{Session, SessionStore};

/// Event channel bound per turn. The turn completes even if the collector
/// walked away (sends just fail); the bound only caps peak memory.
const EVENT_CHANNEL_BOUND: usize = 1024;

/// Per-event cap on how long we wait for the orchestrator to produce.
const EVENT_RECV_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Everything the router needs: orchestrator deps plus a shared session store.
#[derive(Clone)]
pub struct RouterDeps {
    pub config: Config,
    pub llm: Arc<dyn harness_providers::llm::LlmProvider>,
    pub tts: Arc<dyn harness_providers::tts::TtsProvider>,
    pub stt: Arc<dyn harness_providers::stt::SttProvider>,
    /// Streaming STT client for the ASR server's realtime transcription WS.
    /// `None` = batch mode (harness-side VAD + WAV upload) — the default and
    /// the loopback example path.
    pub stt_realtime: Option<Arc<harness_providers::stt_realtime::RealtimeSttClient>>,
    pub plugins: Arc<harness_plugins::PluginRegistry>,
    /// Conversation memory keyed by session id; HTTP turns with a `device_id`
    /// reuse that session so context survives across requests.
    pub sessions: Arc<SessionStore>,
}

impl RouterDeps {
    fn orchestrator_deps(&self) -> Deps {
        Deps {
            config: self.config.clone(),
            llm: self.llm.clone(),
            tts: self.tts.clone(),
            stt: self.stt.clone(),
            plugins: self.plugins.clone(),
        }
    }
}

/// POST /v1/turn with `Content-Type: application/json`.
#[derive(Deserialize)]
struct TextTurnRequest {
    text: String,
    #[serde(default)]
    device_id: Option<String>,
}

/// POST /v1/turn response.
#[derive(serde::Serialize)]
struct TurnResponse {
    transcript: String,
    response_text: String,
    audio_wav_base64: String,
    audio_seq: Vec<u32>,
}

/// Collected per-turn outcome from the orchestrator event stream.
#[derive(Default)]
struct CollectedTurn {
    transcript: String,
    response_text: String,
    audio_pcm: Vec<i16>,
    audio_seq: Vec<u32>,
}

/// Drain the orchestrator event channel (bounded), folding it into a
/// `CollectedTurn`. The channel closes when the turn drops its sender clone;
/// every receive is timeout-wrapped so a stuck producer can't hang the API.
async fn collect_turn(mut rx: mpsc::Receiver<ServerMsg>) -> CollectedTurn {
    let mut out = CollectedTurn::default();
    while let Ok(Some(msg)) = tokio::time::timeout(EVENT_RECV_TIMEOUT, rx.recv()).await {
        match msg {
            ServerMsg::Transcript { text } => out.transcript = text,
            ServerMsg::ResponseText { text } => out.response_text = text,
            ServerMsg::AudioChunk { pcm, seq } => {
                if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(&pcm) {
                    for pair in bytes.chunks_exact(2) {
                        out.audio_pcm.push(i16::from_le_bytes([pair[0], pair[1]]));
                    }
                }
                out.audio_seq.push(seq);
            }
            _ => {}
        }
    }
    out
}

/// Await a turn future while its events are collected on a spawned task.
async fn await_turn_collect(
    turn: impl std::future::Future<Output = Result<(), HarnessError>>,
    rx: mpsc::Receiver<ServerMsg>,
) -> Result<CollectedTurn, HarnessError> {
    let collector = tokio::spawn(collect_turn(rx));
    turn.await?;
    Ok(collector.await.expect("collector task never panics"))
}

/// Fold a collected turn into the HTTP response. `transcript_override` covers
/// the text path, which never emits a `Transcript` event (that belongs to the
/// audio path) — the transcript there is simply the submitted text.
fn format_turn(collected: CollectedTurn, transcript_override: Option<String>) -> TurnResponse {
    TurnResponse {
        transcript: transcript_override.unwrap_or(collected.transcript),
        response_text: collected.response_text,
        audio_wav_base64: if collected.audio_pcm.is_empty() {
            String::new()
        } else {
            base64::engine::general_purpose::STANDARD
                .encode(wav_from_pcm(&collected.audio_pcm, 16_000))
        },
        audio_seq: collected.audio_seq,
    }
}

/// Parse a raw WAV body into 16 kHz mono PCM16 samples (audio contract: the
/// client sends 16 kHz; anything else is a 400 rather than misread audio).
fn parse_wav_body(body: &[u8]) -> Result<Vec<i16>, ApiError> {
    let (samples, rate) = harness_providers::tts::wav_to_pcm16(body).map_err(|e| {
        ApiError(
            StatusCode::BAD_REQUEST,
            format!("unparseable wav body: {e}"),
        )
    })?;
    if rate != 16_000 {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            format!("expected 16 kHz wav, got {rate} Hz"),
        ));
    }
    Ok(samples)
}

/// Handler for `POST /v1/turn`: dispatch on content type, run the turn,
/// collect the event stream into the response object.
async fn turn_handler(
    State(deps): State<RouterDeps>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<TurnResponse>, ApiError> {
    let ct = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let orchestrator = deps.orchestrator_deps();

    if ct.starts_with("application/json") {
        let req: TextTurnRequest = serde_json::from_slice(&body)
            .map_err(|e| ApiError(StatusCode::BAD_REQUEST, format!("invalid json body: {e}")))?;
        let session = match req.device_id.as_deref() {
            Some(id) if !id.is_empty() => deps.sessions.get(id).await,
            _ => Arc::new(tokio::sync::RwLock::new(Session::default())),
        };
        let mut session = session.write().await;
        let (tx, rx) = mpsc::channel(EVENT_CHANNEL_BOUND);
        let text = req.text;
        let collected =
            await_turn_collect(run_text_turn(&orchestrator, &mut session, &text, tx), rx).await?;
        Ok(Json(format_turn(collected, Some(text))))
    } else if ct.starts_with("audio/wav") || ct.starts_with("audio/x-wav") {
        let pcm = parse_wav_body(&body)?;
        let mut session = Session::default();
        let (tx, rx) = mpsc::channel(EVENT_CHANNEL_BOUND);
        let collected = await_turn_collect(
            run_audio_utterance(&orchestrator, &mut session, &pcm, tx),
            rx,
        )
        .await?;
        Ok(Json(format_turn(collected, None)))
    } else {
        Err(ApiError(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            format!("unsupported content type: {ct}"),
        ))
    }
}

/// Uniform API error → status + plain-text body.
struct ApiError(StatusCode, String);

impl From<HarnessError> for ApiError {
    fn from(e: HarnessError) -> Self {
        // Upstream failures surface as 502 (the harness itself is healthy).
        ApiError(StatusCode::BAD_GATEWAY, format!("turn failed: {e}"))
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, self.1).into_response()
    }
}

/// Auth middleware: when `server.api_keys` is non-empty, require
/// `Authorization: Bearer <key>` or `X-API-Key: <key>`; otherwise pass
/// (localhost/LAN trust model). Auth is header-only: query-string tokens
/// leak into proxies/access logs. Applies to every route.
async fn require_api_key(
    State(config): State<Arc<Config>>,
    headers: HeaderMap,
    req: axum::extract::Request,
    next: Next,
) -> Result<Response, StatusCode> {
    if config.server.api_keys.is_empty() {
        return Ok(next.run(req).await);
    }
    let provided = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_owned)
        .or_else(|| {
            headers
                .get("x-api-key")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        });
    match provided {
        Some(key) if config.server.api_keys.contains(&key) => Ok(next.run(req).await),
        _ => Err(StatusCode::UNAUTHORIZED),
    }
}

/// The request URI as logged by [`RedactedMakeSpan`]: PATH ONLY. Query strings
/// carry `?token=<api key>` on WebSocket handshakes (browsers cannot set
/// headers there) and must never reach logs, so everything after `?` is
/// dropped; an empty request target normalizes to `/`.
fn redacted_uri(uri: &axum::http::Uri) -> String {
    let path = uri.path();
    if path.is_empty() {
        "/".to_string()
    } else {
        path.to_string()
    }
}

/// [`tower_http::trace::MakeSpan`] that records method, URI PATH, and version —
/// never the full URI. Query strings are dropped as defense-in-depth (they
/// historically carried the `token=` API key and may carry sensitive values
/// again); an empty request target normalizes to `/`.
#[derive(Debug, Clone, Default)]
struct RedactedMakeSpan;

impl<B> tower_http::trace::MakeSpan<B> for RedactedMakeSpan {
    fn make_span(&mut self, request: &axum::http::Request<B>) -> tracing::Span {
        tracing::span!(
            tracing::Level::DEBUG,
            "request",
            method = %request.method(),
            uri = %redacted_uri(request.uri()),
            version = ?request.version(),
        )
    }
}

/// Build the application router (also the unit under test). Serves
/// `POST /v1/turn`, `GET /v1/realtime` (WS upgrade), and `GET /healthz`,
/// all behind the auth middleware.
pub fn build_router(deps: RouterDeps) -> Router {
    let auth_config = Arc::new(deps.config.clone());
    let ws_state = crate::ws::WsState::from_router_deps(&deps);
    // The two routes use different state types (RouterDeps vs WsState), so
    // each sub-router is built with its own state and merged; the auth + trace
    // layers wrap the merged result and apply to both. The trace span records
    // the request URI PATH ONLY — query strings are dropped as
    // defense-in-depth (they historically carried the `token=` API key and
    // must never reach logs).
    let api = Router::new()
        .route("/v1/turn", post(turn_handler))
        .route("/healthz", get(|| async { "ok" }))
        .with_state(deps);
    let ws = Router::new()
        .route("/v1/realtime", get(crate::ws::realtime_handler))
        .with_state(ws_state);
    api.merge(ws)
        .layer(middleware::from_fn_with_state(auth_config, require_api_key))
        .layer(tower_http::trace::TraceLayer::new_for_http().make_span_with(RedactedMakeSpan))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacted_uri_drops_query_string() {
        let uri: axum::http::Uri = "/v1/realtime?token=super-secret".parse().unwrap();
        assert_eq!(redacted_uri(&uri), "/v1/realtime");
    }

    #[test]
    fn redacted_uri_path_only_unchanged() {
        let uri: axum::http::Uri = "/v1/turn".parse().unwrap();
        assert_eq!(redacted_uri(&uri), "/v1/turn");
    }

    #[test]
    fn redacted_uri_empty_path_and_empty_query() {
        // Default (empty) URI: path() normalizes to "/".
        assert_eq!(redacted_uri(&axum::http::Uri::default()), "/");
        // Bare authority-less target with an empty path + a query.
        let uri: axum::http::Uri = "/?x=1".parse().unwrap();
        assert_eq!(redacted_uri(&uri), "/");
    }
}
