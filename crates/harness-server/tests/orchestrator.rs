//! Orchestrator tests (mock LLM/TTS behind injected trait objects).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use harness_core::config::Config;
use harness_core::types::ServerMsg;
use harness_plugins::{Plugin, PluginManifest, PluginRegistry};
use harness_providers::llm::{ChatRequest, ChatStream, LlmEvent, LlmProvider};
use harness_providers::stt::SttProvider;
use harness_providers::tts::TtsProvider;
use harness_server::orchestrator::{run_audio_utterance, run_text_turn, Deps};
use harness_server::state::{Session, SessionStore};
use tokio::sync::mpsc;
use tokio::time::{timeout, Duration};

const TEST_TIMEOUT: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------- mock LLM

/// Scripted LLM: each `stream_chat` call returns the next script's events.
/// (Interior-mutable so `&self` can advance the script; when only one script
/// is given it is reused for every call.)
struct MockLlm {
    script: Mutex<Vec<Vec<LlmEvent>>>,
    requests: Mutex<Vec<ChatRequest>>,
}

impl MockLlm {
    fn new(script: Vec<Vec<LlmEvent>>) -> Self {
        Self {
            script: Mutex::new(script),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn requests(&self) -> Vec<ChatRequest> {
        self.requests.lock().unwrap().clone()
    }
}

#[async_trait]
impl LlmProvider for MockLlm {
    async fn stream_chat(
        &self,
        req: ChatRequest,
    ) -> Result<ChatStream, harness_core::error::HarnessError> {
        self.requests.lock().unwrap().push(req);
        let events = {
            let mut script = self.script.lock().unwrap();
            if script.len() == 1 {
                script[0].clone()
            } else {
                script.remove(0)
            }
        };
        Ok(Box::pin(futures::stream::iter(events.into_iter().map(Ok))))
    }
}

// ---------------------------------------------------------------- mock TTS

/// Records the text it was asked to speak; returns a few sine samples.
struct MockTts {
    calls: Mutex<Vec<String>>,
}

impl MockTts {
    fn new() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl TtsProvider for MockTts {
    async fn synthesize(&self, text: &str) -> Result<Vec<u8>, harness_core::error::HarnessError> {
        self.calls.lock().unwrap().push(text.to_string());
        Ok(vec![0u8; 32])
    }
}

// ---------------------------------------------------------------- mock STT

struct MockStt {
    text: String,
    calls: AtomicUsize,
}

#[async_trait]
impl SttProvider for MockStt {
    async fn transcribe(
        &self,
        _pcm16k: &[i16],
    ) -> Result<String, harness_core::error::HarnessError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.text.clone())
    }
}

// ---------------------------------------------------------------- plugin

/// Records dispatches and returns a fixed payload.
struct EchoPlugin {
    calls: Mutex<Vec<(String, serde_json::Value)>>,
}

#[async_trait]
impl Plugin for EchoPlugin {
    fn manifest(&self) -> &PluginManifest {
        static M: std::sync::OnceLock<PluginManifest> = std::sync::OnceLock::new();
        M.get_or_init(|| PluginManifest {
            name: "time",
            version: "0.1.0",
            description: "test",
        })
    }

    fn tool_specs(&self) -> Vec<serde_json::Value> {
        vec![serde_json::json!({
            "type": "function",
            "function": {
                "name": "time.get_time",
                "description": "test",
                "parameters": { "type": "object", "properties": {} }
            }
        })]
    }

    async fn call(
        &self,
        name: &str,
        arguments: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        self.calls
            .lock()
            .unwrap()
            .push((name.to_string(), arguments));
        Ok(serde_json::json!({ "datetime": "2026-09-11T12:00:00Z" }))
    }
}

// ---------------------------------------------------------------- helpers

fn two_sentence_script() -> Vec<Vec<LlmEvent>> {
    // Each sentence clears the chunker's 30-char minimum on its own.
    vec![vec![
        LlmEvent::Delta("The quick brown fox jumps over the lazy dog. ".into()),
        LlmEvent::Delta("Then it wandered off to nap under the old oak tree.".into()),
        LlmEvent::Done {
            finish_reason: Some("stop".into()),
        },
    ]]
}

fn deps_with(
    llm: Arc<dyn LlmProvider>,
    tts: Arc<dyn TtsProvider>,
    plugins: PluginRegistry,
) -> Deps {
    Deps {
        config: Config::default(),
        llm,
        tts,
        stt: Arc::new(MockStt {
            text: String::new(),
            calls: AtomicUsize::new(0),
        }),
        plugins: Arc::new(plugins),
    }
}

/// Drain the event channel until it closes (timeout guard per receive), so
/// tests never hang and don't have to guess the event count.
async fn collect_events_all(mut rx: mpsc::Receiver<ServerMsg>) -> Vec<ServerMsg> {
    let mut out = Vec::new();
    while let Ok(Some(msg)) = timeout(TEST_TIMEOUT, rx.recv()).await {
        out.push(msg);
    }
    out
}

fn short_names(events: &[ServerMsg]) -> Vec<&'static str> {
    events
        .iter()
        .map(|m| match m {
            ServerMsg::State { .. } => "state",
            ServerMsg::StateThinking { .. } => "state_thinking",
            ServerMsg::Transcript { .. } => "transcript",
            ServerMsg::ResponseTextDelta { .. } => "delta",
            ServerMsg::ResponseText { .. } => "response_text",
            ServerMsg::AudioChunk { .. } => "audio",
            ServerMsg::TurnCompleted => "turn_completed",
            ServerMsg::Error { .. } => "error",
        })
        .collect()
}

// ---------------------------------------------------------------- tests

#[tokio::test]
async fn text_turn_streams_deltas_and_chunked_audio() {
    let tts = Arc::new(MockTts::new());
    let llm = Arc::new(MockLlm::new(two_sentence_script()));
    let deps = deps_with(llm, tts.clone(), PluginRegistry::new());

    let (tx, rx) = mpsc::channel(64);
    let mut session = Session::default();
    run_text_turn(&deps, &mut session, "hi", tx)
        .await
        .expect("turn completes");

    // Delta Audio Delta Audio ResponseText TurnCompleted — each sentence
    // chunk is TTS'd and emitted before the next delta streams through.
    let events = collect_events_all(rx).await;
    assert_eq!(
        short_names(&events),
        vec![
            "delta",
            "audio",
            "delta",
            "audio",
            "response_text",
            "turn_completed"
        ],
        "event order: {events:?}"
    );

    // Full text assembled from both deltas.
    match &events[4] {
        ServerMsg::ResponseText { text } => assert_eq!(
            text,
            "The quick brown fox jumps over the lazy dog. Then it wandered off to nap under the old oak tree."
        ),
        other => panic!("expected ResponseText, got {other:?}"),
    }

    // TTS was called once per sentence chunk, in order.
    let tts_calls = tts.calls.lock().unwrap().clone();
    assert_eq!(
        tts_calls,
        vec![
            "The quick brown fox jumps over the lazy dog.",
            "Then it wandered off to nap under the old oak tree.",
        ]
    );

    // Audio chunks carry sequential seq numbers and base64 PCM.
    // Order is delta/audio/delta/audio, so chunks sit at indices 1 and 3.
    match (&events[1], &events[3]) {
        (ServerMsg::AudioChunk { seq: s0, .. }, ServerMsg::AudioChunk { seq: s1, .. }) => {
            assert_eq!((*s0, *s1), (0, 1));
        }
        other => panic!("expected audio chunks, got {other:?}"),
    }
}

#[tokio::test]
async fn text_turn_prompt_includes_system_and_datetime() {
    let llm = Arc::new(MockLlm::new(two_sentence_script()));
    let deps = deps_with(llm.clone(), Arc::new(MockTts::new()), PluginRegistry::new());

    let (tx, mut rx) = mpsc::channel(64);
    let mut session = Session::default();
    run_text_turn(&deps, &mut session, "hi", tx).await.unwrap();
    let _ = rx.recv().await; // drain without asserting

    let reqs = llm.requests();
    assert_eq!(reqs.len(), 1);
    let msgs = &reqs[0].messages;
    assert_eq!(msgs[0].role, "system");
    let sys = msgs[0].content.as_deref().unwrap();
    assert!(
        sys.starts_with(&deps.config.prompts.system),
        "system prompt first: {sys}"
    );
    assert!(
        sys.contains("\nCurrent date/time: "),
        "datetime appended: {sys}"
    );
    assert_eq!(msgs[1].role, "user");
    assert_eq!(msgs[1].content.as_deref(), Some("hi"));
    assert_eq!(reqs[0].model, deps.config.llm.model);
}

#[tokio::test]
async fn text_turn_appends_and_trims_history() {
    let llm = Arc::new(MockLlm::new(two_sentence_script()));
    let deps = deps_with(llm.clone(), Arc::new(MockTts::new()), PluginRegistry::new());

    let mut session = Session::default();
    for i in 0..10 {
        let (tx, _rx) = mpsc::channel(64);
        run_text_turn(&deps, &mut session, &format!("msg {i}"), tx)
            .await
            .unwrap();
    }

    let users = session.history.iter().filter(|m| m.role == "user").count();
    assert_eq!(
        users, deps.config.session.max_history_turns,
        "history trimmed to max turns"
    );
    // Most recent user message and assistant reply survive.
    assert_eq!(
        session.history.back().map(|m| m.role.as_str()),
        Some("assistant")
    );
    assert!(session
        .history
        .iter()
        .any(|m| m.role == "user" && m.content.as_deref() == Some("msg 9")));
}

#[tokio::test]
async fn tool_call_round_dispatches_and_feeds_result_to_llm() {
    let llm = Arc::new(MockLlm::new(vec![
        vec![
            LlmEvent::ToolCall {
                id: "call_1".into(),
                name: "time.get_time".into(),
                arguments: "{}".into(),
            },
            LlmEvent::Done {
                finish_reason: Some("tool_calls".into()),
            },
        ],
        vec![
            LlmEvent::Delta("It is noon right now.".into()),
            LlmEvent::Done {
                finish_reason: Some("stop".into()),
            },
        ],
    ]));
    let plugin = Arc::new(EchoPlugin {
        calls: Mutex::new(Vec::new()),
    });
    let mut registry = PluginRegistry::new();
    registry.register(Box::new(EchoPluginShared(plugin.clone())));
    let deps = deps_with(llm.clone(), Arc::new(MockTts::new()), registry);

    let (tx, rx) = mpsc::channel(64);
    let mut session = Session::default();
    run_text_turn(&deps, &mut session, "what time is it", tx)
        .await
        .unwrap();
    let _ = collect_events_all(rx).await; // drain: delta, audio, response_text, turn_completed

    // Plugin was dispatched with the parsed arguments.
    let calls = plugin.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "get_time");

    // Second LLM request contains the assistant tool-call round + tool result.
    let reqs = llm.requests();
    assert_eq!(reqs.len(), 2, "tool call forces a second LLM round");
    let msgs = &reqs[1].messages;
    let assistant_idx = msgs
        .iter()
        .position(|m| m.role == "assistant" && m.tool_calls.is_some())
        .expect("assistant tool-call message present");
    let tool_msg = &msgs[assistant_idx + 1];
    assert_eq!(tool_msg.role, "tool");
    assert_eq!(tool_msg.tool_call_id.as_deref(), Some("call_1"));
    assert!(
        tool_msg
            .content
            .as_deref()
            .unwrap_or_default()
            .contains("2026-09-11"),
        "tool result JSON fed to LLM: {:?}",
        tool_msg.content
    );
}

/// Shared wrapper so one Arc'd plugin can be both registered and asserted on
/// (the registry owns plugins; the test needs a second handle).
struct EchoPluginShared(Arc<EchoPlugin>);

#[async_trait]
impl Plugin for EchoPluginShared {
    fn manifest(&self) -> &PluginManifest {
        self.0.manifest()
    }
    fn tool_specs(&self) -> Vec<serde_json::Value> {
        self.0.tool_specs()
    }
    async fn call(
        &self,
        name: &str,
        arguments: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        self.0.call(name, arguments).await
    }
}

#[tokio::test]
async fn tool_loop_is_capped_at_seven_rounds() {
    // LLM always demands a tool call: the loop must stop after the cap.
    let llm = Arc::new(MockLlm::new(vec![vec![
        LlmEvent::ToolCall {
            id: "call_x".into(),
            name: "time.get_time".into(),
            arguments: "{}".into(),
        },
        LlmEvent::Done {
            finish_reason: Some("tool_calls".into()),
        },
    ]]));
    let mut registry = PluginRegistry::new();
    registry.register(Box::new(EchoPlugin {
        calls: Mutex::new(Vec::new()),
    }));
    let deps = deps_with(llm.clone(), Arc::new(MockTts::new()), registry);

    let (tx, _rx) = mpsc::channel(64);
    let mut session = Session::default();
    run_text_turn(&deps, &mut session, "loop forever", tx)
        .await
        .unwrap();

    assert_eq!(llm.requests().len(), 8, "initial + 7 tool rounds max");
}

#[tokio::test]
async fn unknown_tool_returns_error_payload_to_llm() {
    let llm = Arc::new(MockLlm::new(vec![
        vec![
            LlmEvent::ToolCall {
                id: "call_9".into(),
                name: "nope.nada".into(),
                arguments: "{}".into(),
            },
            LlmEvent::Done {
                finish_reason: Some("tool_calls".into()),
            },
        ],
        vec![
            LlmEvent::Delta("Sorry, I could not look that up.".into()),
            LlmEvent::Done {
                finish_reason: Some("stop".into()),
            },
        ],
    ]));
    let deps = deps_with(llm.clone(), Arc::new(MockTts::new()), PluginRegistry::new());

    let (tx, _rx) = mpsc::channel(64);
    let mut session = Session::default();
    run_text_turn(&deps, &mut session, "do the thing", tx)
        .await
        .unwrap();

    let reqs = llm.requests();
    assert_eq!(reqs.len(), 2, "recovers via a second round");
    let tool_msg = reqs[1]
        .messages
        .iter()
        .find(|m| m.role == "tool")
        .expect("tool message present");
    assert!(
        tool_msg
            .content
            .as_deref()
            .unwrap_or_default()
            .contains("unknown tool"),
        "error flows back as a tool result: {:?}",
        tool_msg.content
    );
}

#[tokio::test]
async fn session_store_isolates_sessions() {
    let store = SessionStore::new();
    let a = store.get("dev-a").await;
    let b = store.get("dev-b").await;
    a.write().await.device_id = Some("dev-a".into());
    assert_eq!(b.read().await.device_id, None, "sessions are independent");
    assert!(
        Arc::ptr_eq(&store.get("dev-a").await, &a),
        "same id → same session"
    );
}

// ------------------------------------------------------- audio utterance path

/// STT mock whose transcript is set after construction (shared handle).
struct SettableStt {
    text: std::sync::Mutex<String>,
    calls: AtomicUsize,
}

#[async_trait]
impl SttProvider for SettableStt {
    async fn transcribe(
        &self,
        _pcm16k: &[i16],
    ) -> Result<String, harness_core::error::HarnessError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.text.lock().unwrap().clone())
    }
}

#[tokio::test]
async fn audio_utterance_transcribes_then_runs_pipeline() {
    let stt = Arc::new(SettableStt {
        text: std::sync::Mutex::new("what time is it".into()),
        calls: AtomicUsize::new(0),
    });
    let tts = Arc::new(MockTts::new());
    let llm = Arc::new(MockLlm::new(two_sentence_script()));
    let deps = Deps {
        config: Config::default(),
        llm,
        tts,
        stt: stt.clone(),
        plugins: Arc::new(PluginRegistry::new()),
    };

    let (tx, rx) = mpsc::channel(64);
    let mut session = Session::default();
    run_audio_utterance(&deps, &mut session, &[1i16; 480], tx)
        .await
        .expect("audio turn completes");

    let events = collect_events_all(rx).await;
    // Transcript precedes the deltas.
    assert_eq!(
        short_names(&events),
        vec![
            "transcript",
            "delta",
            "audio",
            "delta",
            "audio",
            "response_text",
            "turn_completed"
        ],
        "audio path event order: {events:?}"
    );
    match &events[0] {
        ServerMsg::Transcript { text } => assert_eq!(text, "what time is it"),
        other => panic!("expected Transcript first, got {other:?}"),
    }
    assert_eq!(stt.calls.load(Ordering::SeqCst), 1, "STT called once");
}

#[tokio::test]
async fn audio_utterance_empty_transcript_skips_llm() {
    let stt = Arc::new(SettableStt {
        text: std::sync::Mutex::new(String::new()),
        calls: AtomicUsize::new(0),
    });
    let llm = Arc::new(MockLlm::new(two_sentence_script()));
    let deps = Deps {
        config: Config::default(),
        llm: llm.clone(),
        tts: Arc::new(MockTts::new()),
        stt: stt.clone(),
        plugins: Arc::new(PluginRegistry::new()),
    };

    let (tx, rx) = mpsc::channel(64);
    let mut session = Session::default();
    run_audio_utterance(&deps, &mut session, &[1i16; 480], tx)
        .await
        .expect("empty-audio turn completes");

    let events = collect_events_all(rx).await;
    assert_eq!(
        short_names(&events),
        vec!["transcript", "turn_completed"],
        "empty transcript: no LLM round: {events:?}"
    );
    match &events[0] {
        ServerMsg::Transcript { text } => assert_eq!(text, ""),
        other => panic!("expected Transcript, got {other:?}"),
    }
    assert_eq!(
        llm.requests().len(),
        0,
        "LLM must not be called on empty transcript"
    );
    // Nothing entered history either.
    assert!(session.history.is_empty());
}

#[tokio::test]
async fn llm_reasoning_effort_flows_from_config_into_every_request() {
    let llm = Arc::new(MockLlm::new(two_sentence_script()));
    let mut config = Config::default();
    config.llm.reasoning_effort = "high".to_string();
    let deps = Deps {
        config,
        llm: llm.clone(),
        tts: Arc::new(MockTts::new()),
        stt: Arc::new(MockStt {
            text: String::new(),
            calls: AtomicUsize::new(0),
        }),
        plugins: Arc::new(PluginRegistry::new()),
    };

    let (tx, rx) = mpsc::channel(64);
    let mut session = Session::default();
    run_text_turn(&deps, &mut session, "hi", tx)
        .await
        .expect("turn completes");
    let _ = collect_events_all(rx).await;

    let requests = llm.requests();
    assert_eq!(requests.len(), 1, "one LLM round expected");
    assert_eq!(
        requests[0].reasoning_effort.as_deref(),
        Some("high"),
        "config.llm.reasoning_effort must reach the LLM request"
    );

    // Unset default → the request carries None.
    let llm2 = Arc::new(MockLlm::new(two_sentence_script()));
    let deps2 = deps_with(
        llm2.clone(),
        Arc::new(MockTts::new()),
        PluginRegistry::new(),
    );
    let (tx2, rx2) = mpsc::channel(64);
    let mut session2 = Session::default();
    run_text_turn(&deps2, &mut session2, "hi", tx2)
        .await
        .expect("turn completes");
    let _ = collect_events_all(rx2).await;
    assert_eq!(
        llm2.requests()[0].reasoning_effort,
        None,
        "default config must not set reasoning_effort"
    );
}
