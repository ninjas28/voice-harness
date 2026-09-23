//! Turn orchestration: prompt build → streaming LLM → sentence chunks → TTS →
//! ordered `AudioChunk`s, with a bounded plugin tool loop. Also the audio path
//! (`run_audio_utterance`): STT first, then the same pipeline.
//!
//! Federation: when the turn's session has a canonical user (resolved from
//! announced identity keys) and `[personal_context.federation]` is on, the
//! tool list additionally carries the personal catalogs of the user's other
//! ACTIVE sessions (deduped by fq name, own definitions first), and
//! `personal.*` calls route to a sibling device — either because the tool
//! lives there, or as a bounded fallback when the own client cannot serve a
//! merged call.

use std::collections::HashSet;
use std::sync::Arc;

use base64::Engine as _;
use harness_core::chunker::{strip_for_speech, TextChunker};
use harness_core::config::Config;
use harness_core::error::HarnessError;
use harness_core::types::{ServerMsg, ThinkingDetail};
use harness_plugins::PluginRegistry;
use harness_providers::llm::{ChatMessage, ChatRequest, LlmEvent, LlmProvider};
use harness_providers::stt::SttProvider;
use harness_providers::tts::TtsProvider;
use tokio::sync::mpsc;
use tokio_stream::StreamExt;

use crate::state::{FederationRouter, Session, SessionStore};

/// Maximum LLM round trips in one turn (initial + up to 7 tool rounds). The
/// headroom lets multi-step agents (e.g. Home Assistant discovery via
/// ha_search) recover from bad calls and still answer.
const MAX_TOOL_ROUNDS: usize = 8;

/// Upper bound on federated routing attempts per client tool call (own client
/// first, then up to this many sibling sessions in ascending session-id
/// order). Every attempt uses the configured per-call timeout.
const MAX_FEDERATION_ATTEMPTS: usize = 3;

/// Sibling-routed calls get ids from the top of the u64 range so they can
/// never collide with the own client's 1, 2, 3… ids on the same inbox.
const SIBLING_CALL_ID_BASE: u64 = 1 << 62;

/// Per-turn inbox of client tool results, handed over by the WS layer.
/// `None` on surfaces without a client (HTTP `POST /v1/turn`).
pub type ToolInbox = mpsc::Receiver<(u64, bool, String)>;

/// Namespaced prefix marking a tool call as client-executed. No builtin
/// plugin uses this namespace, so routing is collision-free.
const PERSONAL_PREFIX: &str = "personal.";

/// One static hint line appended to the system prompt when the session has an
/// announced client catalog, so the model knows what `personal.*` is for.
const PROMPT_HINT: &str = "Personal-context tools (personal.*) return a plain-text digest of the user's calendar, reminders, contacts, or photos from their device. Call them when asked about their schedule, people, or photos.";

/// Classify outcome for a `personal.*` call: `Execute` = forward to a client
/// (own or a federated sibling); `Unavailable(why)` = answer with an error
/// digest the LLM can recover from.
enum ClientRoute {
    Execute,
    Unavailable(&'static str),
}

/// Classify a `personal.*` tool call: `Some(Execute)` = forward to a client
/// (own or a federated sibling); `Some(Unavailable(why))` = answer with an
/// error digest the LLM can recover from; `None` = not a client call (route
/// to server plugins as usual).
fn classify_client_call(deps: &Deps, session: &Session, name: &str) -> Option<ClientRoute> {
    if !name.starts_with(PERSONAL_PREFIX) {
        return None;
    }
    if !deps.config.personal_context.enabled {
        return Some(ClientRoute::Unavailable(
            "personal context is disabled on the server",
        ));
    }
    // Own catalog resolves it (announced or merged sibling-owned — the LLM
    // saw it in the merged spec list, which build_request derived from the
    // own catalog plus siblings). Without federation, an unresolvable name
    // is unknown; with federation, dispatch decides (own client or routed).
    if session.context.resolve(name).is_none() && !federation_ready(deps, session) {
        return Some(ClientRoute::Unavailable("unknown personal-context tool"));
    }
    Some(ClientRoute::Execute)
}

/// Forward one tool call to the announcing client and wait, bounded, for its
/// digest. Results for other call ids (stale/late) are skipped; a closed
/// inbox means the client connection ended. Digests are clamped to
/// `max_bytes` before they enter history.
async fn client_tool_dispatch(
    events: &mpsc::Sender<ServerMsg>,
    inbox: &mut ToolInbox,
    call_id: u64,
    name: &str,
    arguments: &str,
    timeout: std::time::Duration,
    max_bytes: usize,
) -> Result<String, String> {
    events
        .send(ServerMsg::ToolCall {
            call_id,
            name: name.into(),
            arguments: arguments.into(),
        })
        .await
        .map_err(|_| "client connection closed".to_string())?;
    let res = tokio::time::timeout(timeout, async {
        loop {
            match inbox.recv().await {
                Some((id, ok, text)) if id == call_id => return Some((ok, text)),
                Some(_) => continue, // result for another call: skip
                None => return None, // inbox closed: client connection gone
            }
        }
    })
    .await;
    match res {
        Ok(Some((true, text))) => Ok(clamp_digest(text, max_bytes)),
        Ok(Some((false, text))) => Err(clamp_digest(text, max_bytes)),
        Ok(None) => Err("client connection closed".into()),
        Err(_) => Err(format!("client did not answer '{name}' within the timeout")),
    }
}

/// Clamp a digest to `max_bytes` with a spoken-style truncation marker. Never
/// panics: the cut point walks back to a UTF-8 char boundary first.
fn clamp_digest(mut text: String, max_bytes: usize) -> String {
    if text.len() > max_bytes {
        let mut end = max_bytes;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push_str(" …(truncated)");
    }
    text
}

/// Injected provider set driving a turn. Trait objects throughout so tests can
/// drive mocks (and `main.rs` wires the real clients). The federation fields
/// are `None` in bare test setups and on surfaces without a session store —
/// there the behavior is exactly v1 (own catalog only, own client only).
pub struct Deps {
    pub config: Config,
    pub llm: Arc<dyn LlmProvider>,
    pub tts: Arc<dyn TtsProvider>,
    pub stt: Arc<dyn SttProvider>,
    pub plugins: Arc<PluginRegistry>,
    /// Session registry for cross-session federation (spec merge + routing).
    pub store: Option<Arc<SessionStore>>,
    /// Cross-session `personal.*` routes, maintained by ws.rs per connection.
    pub router: Option<Arc<FederationRouter>>,
    /// The session this turn belongs to (sibling queries exclude it).
    pub self_session_id: Option<String>,
}

/// True when cross-session federation can act for this turn: enabled in
/// config, a session store + router wired, and the session has a canonical
/// user (anonymous sessions never merge).
fn federation_ready(deps: &Deps, session: &Session) -> bool {
    deps.store.is_some()
        && deps.router.is_some()
        && deps.config.personal_context.federation.enabled
        && session.canonical_user.is_some()
}

/// True when a sibling session of the same canonical user (with a registered
/// route — only connected siblings can serve anything) owns this tool name.
async fn sibling_owns(deps: &Deps, session: &Session, name: &str) -> bool {
    if !federation_ready(deps, session) {
        return false;
    }
    let store = deps.store.as_ref().expect("checked by federation_ready");
    let router = deps.router.as_ref().expect("checked by federation_ready");
    let self_id = deps.self_session_id.as_deref().unwrap_or_default();
    let canonical = session.canonical_user.as_deref().expect("checked above");
    for (sib_id, _) in router.routes_for(canonical, self_id).await {
        let sib = store.get(sib_id).await;
        if sib.read().await.context.resolve(name).is_some() {
            return true;
        }
    }
    false
}

/// Sibling sessions' tool specs for the turn: active sessions of the same
/// canonical user, ascending by session id, deduped against the own fq names
/// (own definitions always win). Empty when federation is off/unwired.
async fn merged_client_specs(
    deps: &Deps,
    session: &Session,
    own_specs: &[serde_json::Value],
) -> Vec<serde_json::Value> {
    if !federation_ready(deps, session) {
        return Vec::new();
    }
    let store = deps.store.as_ref().expect("checked by federation_ready");
    let self_id = deps.self_session_id.as_deref().unwrap_or_default();
    let canonical = session.canonical_user.as_deref().expect("checked above");
    let mut seen: HashSet<String> = own_specs
        .iter()
        .map(|s| {
            s["function"]["name"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        })
        .collect();
    let mut out = Vec::new();
    for spec in store.federated_specs(self_id, canonical).await {
        let name = spec["function"]["name"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        if seen.insert(name) {
            out.push(spec);
        }
    }
    out
}

/// Everything one federated routing attempt needs, bundled so the dispatch
/// helpers stay within clippy's argument limit.
struct RoutedCall<'a> {
    router: &'a FederationRouter,
    sibling_id: &'a str,
    sib_tx: &'a mpsc::Sender<ServerMsg>,
    call_id: u64,
    name: &'a str,
    arguments: &'a str,
    timeout: std::time::Duration,
    max_bytes: usize,
}

/// Route one call to a sibling session's connection and wait, bounded, for
/// its client's digest. The sibling connection learns about the pending
/// reply via [`FederationRouter::take_pending`]: this arms the registry, so
/// when the sibling's client answers `tool.result`, the sibling's ws layer
/// forwards `(call_id, ok, text)` into the reply channel.
async fn routed_dispatch(call: RoutedCall<'_>) -> Result<String, String> {
    let RoutedCall {
        router,
        sibling_id,
        sib_tx,
        call_id,
        name,
        arguments,
        timeout,
        max_bytes,
    } = call;
    let (reply_tx, mut reply_rx) = mpsc::channel::<(u64, bool, String)>(4);
    router.arm_pending(sibling_id, call_id, reply_tx).await;
    sib_tx
        .send(ServerMsg::ToolCall {
            call_id,
            name: name.into(),
            arguments: arguments.into(),
        })
        .await
        .map_err(|_| format!("sibling '{sibling_id}' connection closed"))?;
    match tokio::time::timeout(timeout, reply_rx.recv()).await {
        Ok(Some((_, true, text))) => Ok(clamp_digest(text, max_bytes)),
        Ok(Some((_, false, text))) => Err(clamp_digest(text, max_bytes)),
        Ok(None) => Err(format!("sibling '{sibling_id}' connection closed")),
        Err(_) => Err(format!(
            "sibling '{sibling_id}' did not answer '{name}' within the timeout"
        )),
    }
}

/// The call itself (name + allocated ids), bundled so the dispatch helper
/// stays within clippy's argument limit.
struct PersonalCall<'a> {
    own_call_id: u64,
    sibling_call_id: u64,
    name: &'a str,
    arguments: &'a str,
}

/// Serve one `personal.*` tool call from the best client available: the
/// turn's own client first (unless the tool lives ONLY on a sibling — the own
/// client would just let it time out), then the user's other active sessions
/// in ascending session-id order as the bounded fallback. Every attempt uses
/// the configured per-call timeout; sibling attempts are capped at
/// [`MAX_FEDERATION_ATTEMPTS`].
async fn dispatch_personal_call(
    deps: &Deps,
    session: &Session,
    events: &mpsc::Sender<ServerMsg>,
    inbox: &mut Option<&mut ToolInbox>,
    call: PersonalCall<'_>,
) -> Result<String, String> {
    let PersonalCall {
        own_call_id,
        sibling_call_id,
        name,
        arguments,
    } = call;
    let timeout = std::time::Duration::from_secs(deps.config.personal_context.call_timeout_secs);
    let max_bytes = deps.config.personal_context.max_result_bytes;

    // Own client first — except when the own catalog lacks the tool and a
    // sibling owns it: forwarding to a client that cannot serve the call
    // would burn a full timeout for nothing.
    let own_has_tool = session.context.resolve(name).is_some();
    let sib_has_tool = sibling_owns(deps, session, name).await;
    let mut own_result: Option<Result<String, String>> = None;
    if own_has_tool || !sib_has_tool {
        if let Some(rx) = inbox.as_mut() {
            own_result = Some(
                client_tool_dispatch(events, rx, own_call_id, name, arguments, timeout, max_bytes)
                    .await,
            );
            if own_result.as_ref().expect("just set").is_ok() {
                return own_result.expect("just set");
            }
            if deps.router.is_none() {
                // v1 behavior: no federation, report the own-client error.
                return own_result.expect("just set");
            }
        }
    }

    // Federated fallback: siblings of the same canonical user, ascending
    // session id ("first" policy), attempts capped.
    let Some(router) = deps.router.as_ref() else {
        return own_result
            .unwrap_or_else(|| Err("personal context requires a connected client".into()));
    };
    let self_id = deps.self_session_id.as_deref().unwrap_or_default();
    let canonical = session.canonical_user.as_deref().unwrap_or_default();
    for (attempts, (sib_id, sib_tx)) in router
        .routes_for(canonical, self_id)
        .await
        .into_iter()
        .enumerate()
    {
        if attempts >= MAX_FEDERATION_ATTEMPTS {
            break;
        }
        let result = routed_dispatch(RoutedCall {
            router,
            sibling_id: &sib_id,
            sib_tx: &sib_tx,
            call_id: sibling_call_id,
            name,
            arguments,
            timeout,
            max_bytes,
        })
        .await;
        if result.is_ok() {
            return result;
        }
    }
    own_result.unwrap_or_else(|| Err(format!("no connected device can serve '{name}'")))
}

/// Days since epoch → (year, month, day) — Howard Hinnant's `civil_from_days`.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Current UTC time formatted `YYYY-MM-DDTHH:MM:SSZ` (chrono-free, like the
/// time plugin).
fn utc_now_string() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let (year, month, day) = civil_from_days(now.div_euclid(86_400) as i64);
    let secs = now.rem_euclid(86_400);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

/// Build the outgoing chat request: system prompt (with current date/time,
/// plus the personal-context hint when client tools are available) prepended
/// to `messages` (history + new user message), plus tool specs (server
/// plugins + announced client tools, own catalog first then federated
/// siblings deduped by fq name).
async fn build_request(deps: &Deps, session: &Session, messages: &[ChatMessage]) -> ChatRequest {
    let mut system = format!(
        "{}\nCurrent date/time: {}",
        deps.config.prompts.system,
        utc_now_string()
    );
    let mut specs = deps.plugins.tool_specs();
    let own_specs = session.context.openai_tool_specs();
    let federated = merged_client_specs(deps, session, &own_specs).await;
    if !own_specs.is_empty() || !federated.is_empty() {
        system.push('\n');
        system.push_str(PROMPT_HINT);
        specs.extend(own_specs);
        specs.extend(federated);
    }
    let mut full = vec![ChatMessage::text("system", system)];
    full.extend(messages.iter().cloned());
    ChatRequest {
        model: deps.config.llm.model.clone(),
        messages: full,
        tools: Some(specs),
        reasoning_effort: (!deps.config.llm.reasoning_effort.trim().is_empty())
            .then(|| deps.config.llm.reasoning_effort.clone()),
    }
}

/// Parse tool-call arguments; malformed JSON becomes `{"_raw": args}` so the
/// error flows back to the LLM as a tool result instead of aborting the turn.
fn parse_tool_args(args: &str) -> serde_json::Value {
    serde_json::from_str(args).unwrap_or_else(|_| serde_json::json!({ "_raw": args }))
}

/// Append the assistant tool-call round + tool result messages in OpenAI wire
/// shape, so the next LLM round sees what its calls returned.
fn append_tool_round(
    messages: &mut Vec<ChatMessage>,
    calls: &[(String, String, String)],
    results: &[(String, String)],
) {
    messages.push(ChatMessage {
        role: "assistant".into(),
        content: None,
        tool_calls: Some(
            calls
                .iter()
                .map(|(id, name, args)| {
                    serde_json::json!({
                        "id": id,
                        "type": "function",
                        "function": { "name": name, "arguments": args }
                    })
                })
                .collect(),
        ),
        tool_call_id: None,
        name: None,
    });
    for (id, result_json) in results {
        messages.push(ChatMessage {
            role: "tool".into(),
            content: Some(result_json.clone()),
            tool_call_id: Some(id.clone()),
            tool_calls: None,
            name: None,
        });
    }
}

/// One streamed LLM round: forward `ResponseTextDelta` per delta, feed the
/// sentence chunker, and synthesize+emit an `AudioChunk` (sequence number
/// `seq` → incremented) per emitted chunk. Returns the completed tool calls
/// and the full response text.
async fn run_llm_round(
    deps: &Deps,
    req: ChatRequest,
    events: &mpsc::Sender<ServerMsg>,
    seq: &mut u32,
) -> Result<(Vec<(String, String, String)>, String), HarnessError> {
    let mut chunker = TextChunker::new(deps.config.session.chunk_max_chars);
    let mut stream = deps.llm.stream_chat(req).await?;
    let mut tool_calls = Vec::new();
    let mut full_text = String::new();

    while let Some(ev) = stream.next().await {
        match ev? {
            LlmEvent::Delta(delta) => {
                full_text.push_str(&delta);
                let _ = events
                    .send(ServerMsg::ResponseTextDelta {
                        text: delta.clone(),
                    })
                    .await;
                for chunk in chunker.push(&delta) {
                    synthesize_chunk(deps, &chunk, events, seq).await?;
                }
            }
            LlmEvent::ToolCall {
                id,
                name,
                arguments,
            } => {
                tool_calls.push((id, name, arguments));
            }
            LlmEvent::Done { .. } => {}
        }
    }

    // Flush any remainder that never hit a sentence boundary.
    for chunk in chunker.finish() {
        synthesize_chunk(deps, &chunk, events, seq).await?;
    }
    Ok((tool_calls, full_text))
}

/// Strip markdown from a sentence chunk, synthesize it via TTS, and emit the
/// base64-encoded 16 kHz PCM16 audio as `AudioChunk { seq }`.
async fn synthesize_chunk(
    deps: &Deps,
    chunk: &str,
    events: &mpsc::Sender<ServerMsg>,
    seq: &mut u32,
) -> Result<(), HarnessError> {
    let speakable = strip_for_speech(chunk);
    if speakable.trim().is_empty() {
        return Ok(());
    }
    let pcm_bytes = deps.tts.synthesize(&speakable).await?;
    let _ = events
        .send(ServerMsg::AudioChunk {
            pcm: base64::engine::general_purpose::STANDARD.encode(&pcm_bytes),
            seq: *seq,
        })
        .await;
    *seq += 1;
    Ok(())
}

/// History plus any accumulated tool-round messages, for the next LLM round.
fn messages_for_round(history: &[ChatMessage], extra: &[ChatMessage]) -> Vec<ChatMessage> {
    let mut messages = Vec::with_capacity(history.len() + extra.len() + 1);
    messages.extend(history.iter().cloned());
    messages.extend(extra.iter().cloned());
    messages
}

/// Core turn execution shared by the text and audio paths: prompt → streamed
/// LLM rounds (sentence-chunked TTS) → `ResponseText`, then history update.
/// Does NOT emit `TurnCompleted` — each public entry point sends it exactly
/// once, after the pipeline finishes (or skips the pipeline entirely).
async fn execute_turn(
    deps: &Deps,
    session: &mut Session,
    user_text: &str,
    events: mpsc::Sender<ServerMsg>,
    mut inbox: Option<&mut ToolInbox>,
) -> Result<(), HarnessError> {
    session
        .history
        .push_back(ChatMessage::text("user", user_text));

    let history: Vec<ChatMessage> = session.history.iter().cloned().collect();
    let mut extra: Vec<ChatMessage> = Vec::new();
    let mut final_text = String::new();

    for round in 0..MAX_TOOL_ROUNDS {
        let req = build_request(deps, session, &messages_for_round(&history, &extra)).await;
        let mut seq = 0u32;
        let (tool_calls, text) = run_llm_round(deps, req, &events, &mut seq).await?;
        final_text = text;

        if tool_calls.is_empty() || round == MAX_TOOL_ROUNDS - 1 {
            break;
        }

        // Dispatch every tool call; failures become error payloads the LLM can
        // read and recover from, never an abort. Each dispatch is bracketed by
        // `state.thinking` frames so clients can show tool activity.
        // `personal.*` calls bypass the plugin registry: they execute on the
        // announcing client (own, or a federated sibling as fallback).
        let mut next_client_call_id = 1u64;
        let mut next_sibling_call_id = SIBLING_CALL_ID_BASE;
        let mut results: Vec<(String, String)> = Vec::new();
        for (id, name, arguments) in &tool_calls {
            let _ = events
                .send(ServerMsg::StateThinking {
                    detail: ThinkingDetail::CallingTools,
                })
                .await;
            let outcome = match classify_client_call(deps, session, name) {
                None => deps
                    .plugins
                    .dispatch(name, parse_tool_args(arguments))
                    .await
                    .map(|v| {
                        serde_json::to_string(&v)
                            .unwrap_or_else(|_| "{\"error\":\"unserializable tool result\"}".into())
                    }),
                Some(ClientRoute::Unavailable(why)) => Err(why.to_string()),
                Some(ClientRoute::Execute) => {
                    let own_call_id = next_client_call_id;
                    next_client_call_id += 1;
                    let sibling_call_id = next_sibling_call_id;
                    next_sibling_call_id += 1;
                    dispatch_personal_call(
                        deps,
                        session,
                        &events,
                        &mut inbox,
                        PersonalCall {
                            own_call_id,
                            sibling_call_id,
                            name,
                            arguments,
                        },
                    )
                    .await
                }
            };
            let _ = events
                .send(ServerMsg::StateThinking {
                    detail: ThinkingDetail::Thinking,
                })
                .await;
            let payload = match outcome {
                Ok(text) => text, // already the string content the LLM reads
                Err(err) => serde_json::json!({ "error": err }).to_string(),
            };
            results.push((id.clone(), payload));
        }
        append_tool_round(&mut extra, &tool_calls, &results);
    }

    // glm53 occasionally finishes a round with no text and no tool calls
    // (nondeterministic empty replies). Surface that as an upstream error so
    // clients don't read the silence as a normal turn end.
    if final_text.trim().is_empty() {
        let _ = events
            .send(ServerMsg::Error {
                code: "upstream".into(),
                message: "model returned an empty answer".into(),
            })
            .await;
    }

    let _ = events
        .send(ServerMsg::ResponseText {
            text: final_text.clone(),
        })
        .await;

    session
        .history
        .push_back(ChatMessage::text("assistant", &final_text));
    crate::state::trim_history(&mut session.history, deps.config.session.max_history_turns);
    Ok(())
}

/// Run a full text turn: prompt → (streamed LLM → TTS chunks, bounded tool
/// rounds) → `ResponseText` + `TurnCompleted`. `inbox` is the per-turn client
/// tool-result channel (WS); `None` on clientless surfaces (HTTP turn).
pub async fn run_text_turn(
    deps: &Deps,
    session: &mut Session,
    user_text: &str,
    events: mpsc::Sender<ServerMsg>,
    inbox: Option<&mut ToolInbox>,
) -> Result<(), HarnessError> {
    execute_turn(deps, session, user_text, events.clone(), inbox).await?;
    let _ = events.send(ServerMsg::TurnCompleted).await;
    Ok(())
}

/// Audio ingest path: transcribe the utterance, emit `Transcript`, then run
/// the same turn pipeline as [`run_text_turn`]. An empty transcript (silent
/// audio / STT found nothing) skips the LLM entirely: `Transcript{text:""}` +
/// `TurnCompleted`, nothing enters history.
pub async fn run_audio_utterance(
    deps: &Deps,
    session: &mut Session,
    pcm16k: &[i16],
    events: mpsc::Sender<ServerMsg>,
    inbox: Option<&mut ToolInbox>,
) -> Result<(), HarnessError> {
    let text = deps.stt.transcribe(pcm16k).await?;
    let _ = events
        .send(ServerMsg::Transcript { text: text.clone() })
        .await;

    if text.trim().is_empty() {
        let _ = events.send(ServerMsg::TurnCompleted).await;
        return Ok(());
    }

    execute_turn(deps, session, &text, events.clone(), inbox).await?;
    let _ = events.send(ServerMsg::TurnCompleted).await;
    Ok(())
}
