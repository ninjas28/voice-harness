//! Turn orchestration: prompt build → streaming LLM → sentence chunks → TTS →
//! ordered `AudioChunk`s, with a bounded plugin tool loop. Also the audio path
//! (`run_audio_utterance`): STT first, then the same pipeline.

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

/// Maximum LLM round trips in one turn (initial + up to 7 tool rounds). The
/// headroom lets multi-step agents (e.g. Home Assistant discovery via
/// ha_search) recover from bad calls and still answer.
const MAX_TOOL_ROUNDS: usize = 8;

/// Injected provider set driving a turn. Trait objects throughout so tests can
/// drive mocks (and `main.rs` wires the real clients).
pub struct Deps {
    pub config: Config,
    pub llm: Arc<dyn LlmProvider>,
    pub tts: Arc<dyn TtsProvider>,
    pub stt: Arc<dyn SttProvider>,
    pub plugins: Arc<PluginRegistry>,
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

/// Build the outgoing chat request: system prompt (with current date/time)
/// prepended to `messages` (history + new user message), plus tool specs.
fn build_request(deps: &Deps, messages: &[ChatMessage]) -> ChatRequest {
    let system = format!(
        "{}\nCurrent date/time: {}",
        deps.config.prompts.system,
        utc_now_string()
    );
    let mut full = vec![ChatMessage::text("system", system)];
    full.extend(messages.iter().cloned());
    ChatRequest {
        model: deps.config.llm.model.clone(),
        messages: full,
        tools: Some(deps.plugins.tool_specs()),
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
    session: &mut crate::state::Session,
    user_text: &str,
    events: mpsc::Sender<ServerMsg>,
) -> Result<(), HarnessError> {
    session
        .history
        .push_back(ChatMessage::text("user", user_text));

    let history: Vec<ChatMessage> = session.history.iter().cloned().collect();
    let mut extra: Vec<ChatMessage> = Vec::new();
    let mut final_text = String::new();

    for round in 0..MAX_TOOL_ROUNDS {
        let req = build_request(deps, &messages_for_round(&history, &extra));
        let mut seq = 0u32;
        let (tool_calls, text) = run_llm_round(deps, req, &events, &mut seq).await?;
        final_text = text;

        if tool_calls.is_empty() || round == MAX_TOOL_ROUNDS - 1 {
            break;
        }

        // Dispatch every tool call; failures become error payloads the LLM can
        // read and recover from, never an abort. Each dispatch is bracketed by
        // `state.thinking` frames so clients can show tool activity.
        let mut results: Vec<(String, String)> = Vec::new();
        for (id, name, arguments) in &tool_calls {
            let _ = events
                .send(ServerMsg::StateThinking {
                    detail: ThinkingDetail::CallingTools,
                })
                .await;
            let outcome = deps
                .plugins
                .dispatch(name, parse_tool_args(arguments))
                .await;
            let _ = events
                .send(ServerMsg::StateThinking {
                    detail: ThinkingDetail::Thinking,
                })
                .await;
            let payload = match outcome {
                Ok(value) => serde_json::to_string(&value)
                    .unwrap_or_else(|_| "{\"error\":\"unserializable tool result\"}".into()),
                Err(err) => serde_json::json!({ "error": err }).to_string(),
            };
            results.push((id.clone(), payload));
        }
        append_tool_round(&mut extra, &tool_calls, &results);
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
/// rounds) → `ResponseText` + `TurnCompleted`.
pub async fn run_text_turn(
    deps: &Deps,
    session: &mut crate::state::Session,
    user_text: &str,
    events: mpsc::Sender<ServerMsg>,
) -> Result<(), HarnessError> {
    execute_turn(deps, session, user_text, events.clone()).await?;
    let _ = events.send(ServerMsg::TurnCompleted).await;
    Ok(())
}

/// Audio ingest path: transcribe the utterance, emit `Transcript`, then run
/// the same turn pipeline as [`run_text_turn`]. An empty transcript (silent
/// audio / STT found nothing) skips the LLM entirely: `Transcript{text:""}` +
/// `TurnCompleted`, nothing enters history.
pub async fn run_audio_utterance(
    deps: &Deps,
    session: &mut crate::state::Session,
    pcm16k: &[i16],
    events: mpsc::Sender<ServerMsg>,
) -> Result<(), HarnessError> {
    let text = deps.stt.transcribe(pcm16k).await?;
    let _ = events
        .send(ServerMsg::Transcript { text: text.clone() })
        .await;

    if text.trim().is_empty() {
        let _ = events.send(ServerMsg::TurnCompleted).await;
        return Ok(());
    }

    execute_turn(deps, session, &text, events.clone()).await?;
    let _ = events.send(ServerMsg::TurnCompleted).await;
    Ok(())
}
