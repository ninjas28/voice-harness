# AGENTS.md

Guidance for AI coding agents working in this repository. Read this before
making changes; it encodes conventions and hard-won lessons from this codebase.

## Project

**voice-harness** is a Rust daemon bridging audio clients (ESP32-class smart
speakers, the bundled macOS menu-bar client) to self-hosted voice services:

- **STT/TTS**: any OpenAI-compatible voice server (`/v1/audio/*` routes)
- **LLM**: any OpenAI-compatible chat endpoint (e.g. Open WebUI) — note the
  client configures `base_url + chat_path`; don't double-prefix `/api`

The harness packages a transcribed turn with a system prompt and MCP-style
plugins, calls the LLM, streams the response back as text deltas and
sentence-chunked TTS audio.

## Layout

```
crates/
  harness-core/      config, wire types, VAD, utterance assembly, chunker
  harness-providers/ STT/TTS/LLM HTTP clients (wiremock-tested, never live)
  harness-plugins/   tool-call loop + built-in plugins (time, weather, web_search) + MCP client/plugin (Streamable HTTP, bearer/OAuth)
  harness-server/    axum HTTP (`POST /v1/turn`) + WS (`/v1/realtime`)
examples/loopback.rs WAV file → full turn over WS (manual E2E check)
clients/macos/voice-harness-client/  SwiftPM macOS menu-bar client
clients/ios/voice-harness-client/    iOS app (XcodeGen project; shares voicekit via path dep)
config/voice-harness.toml             server config (API keys — never commit)
.hermes/plans/                        task plans (markdown)
```

## Wire protocol (dotted tags)

Client → server: `session.start` (sample_rate, default 16000), `audio.data`
(base64 PCM16 16 kHz mono), `speech.end`, `session.stop`.
Server → client: `state` (listening|speech|thinking|speaking), `transcript`,
`response.text.delta`, `audio.chunk` (base64 PCM16 16 kHz mono, seq from 0),
`turn.completed`, `error`.

## Non-negotiable workflow

- **Strict TDD**: write the failing test first, watch it fail (RED), then make
  it pass (GREEN). Commit per task. Small, focused commits
  (`fix:`, `feat:`, `test:`, `docs:`, `refactor:`).
- **Gates before every commit** (all must pass):
  ```
  cargo fmt --all
  cargo clippy --workspace --all-targets -- -D warnings
  cargo test --workspace
  ```
  Swift (in `clients/macos/voice-harness-client/`):
  ```
  swift build && swift test
  ```
- **Never probe real servers from tests or agents.** Providers are tested
  against `wiremock` only. Real endpoints are verified manually, deliberately.
- **Secrets**: `config/voice-harness.toml` holds API keys. Never echo, log, or
  commit them. `config/voice-harness.toml.example` is the committed template.
- **Bounded everything**: every async stream/receive/collect in tests must be
  wrapped in `tokio::time::timeout` (10 s in Rust, ≤2 s waits in Swift player
  tests). No unbounded streams, no retry loops without a cap. An unbounded
  collect in this repo once OOM'd an entire machine.
- **Wiremock gotchas**: a responder closure receives `&Request` (not `Request`
  — closure type mismatches are cryptic E0631s) and there is no built-in
  request counter. When a mock provider trait already wraps the upstream URL
  (e.g. `MockLlm { url, requests }`), count there — incrementing the same
  `Arc<AtomicUsize>` from both the wiremock closure AND the trait mock
  double-counts every call.
- **Tokio time races with default config**: a test that sleeps past a config
  default (`sentence_end_wait_ms = 2s`, future `silence_ms`-adjacent timers)
  hits the production timer. Tests must set wait knobs explicitly — long
  (60s) to prove a hold, short (500ms) to prove a flush.
- Commits are local-only (no push) unless explicitly asked.

## Hard-won domain lessons

- **WebRTC VAD accepts only 10/20/30 ms frames** (≤480 samples @ 16 kHz).
  Anything else classifies as silence. The server must split arbitrary client
  chunks into ≤480-sample sub-frames (`ConnState::VAD_FRAME_SAMPLES`) before
  feeding the VAD/assembler — do not "fix" this on the client side; ESP32
  clients will also send odd framings.
- **One `AVAudioEngine` per process** on macOS, especially with Bluetooth
  headsets: two engines fight over the route (A2DP vs HFP) and the playback
  engine's buffers "complete" without ever sounding. Share the engine between
  `AudioPlayer` and `MicCapture`.
- **Install input taps before `engine.start()`** — a tap added to a running
  engine's input node silently never fires.
- **`AVAudioUnitTimePitch` (and effect units generally) reject Int16
  connections** (error -10868). Convert to canonical Float32 buffers; keep the
  wire protocol PCM16.
- **The server cannot know when the client finishes playing audio.** While
  client-side playback is pending, the client owns the phase and must absorb
  server `state` messages — including `speech` (the mic hears the client's own
  TTS; there is no AEC). The drain watcher completes the transition.
- **URLSessionWebSocketTask needs `task.resume()`** after creation, or
  `send()` suspends forever.
- **`MenuBarExtra(.window)` freezes its window at first-measured height**;
  `.fixedSize(horizontal: false, vertical: true)` lets the panel grow with
  content.
- **LSEnvironment / `open --env` don't reach unsigned GUI bundles**; use
  `launchctl setenv VAR value` when a launch-time env var is genuinely needed.
- **Pass the config path bare** (`harness-server config/voice-harness.toml`);
  there is no `--config` flag.

## MCP servers (Streamable HTTP)

- Tools from `[plugins.mcp]` servers surface as `mcp.<server>.<tool>`; the single
  `mcp` plugin aggregates all servers and warms at startup (`PluginRegistry::warm_all`).
- A dead/unauthorized MCP server never blocks boot — it is skipped with a warning and
  contributes no tools.
- Auth: `auth = "bearer"` + `api_key`, or `auth = "oauth"` (OAuth 2.1, PKCE, dynamic
  client registration, loopback redirect). Authorize once with
  `harness-server auth <name>`; tokens live in `config/mcp-tokens.json` (chmod 600,
  never commit). Refresh is automatic; a hard 401 tells the LLM to surface the error.
- The MCP client is hand-rolled (`crates/harness-plugins/src/mcp/`), wiremock-tested,
  and timeout-bounded everywhere — no MCP SDK dependency.
- Session handling: `Mcp-Session-Id` is tracked per server; a 404 re-initializes once
  and retries. Only `initialize` / `tools/list` / `tools/call` are spoken.

## Client deployment (macOS)

```
cd clients/macos/voice-harness-client
swift build
pkill -f VoiceHarnessClient.app
cp .build/debug/VoiceHarnessClient /Applications/VoiceHarnessClient.app/Contents/MacOS/VoiceHarnessClient
open /Applications/VoiceHarnessClient.app
```

The app is launchd-owned via `open`; bare background launches get reaped.
Settings persist in the app's `UserDefaults` domain (`server_url`, `tts_rate`).

## Style

- Rust: workspace crates, `anyhow`-free typed errors (`thiserror`), no `unwrap`
  in server paths, `tracing` for logs.
- Swift: zero-dependency SwiftPM, Swift 6 strict concurrency, actors for
  transport, `@MainActor` for UI state, injectable `Transport` protocol for
  tests.
- Keep plugin tools async and cancellable; the orchestrator aborts turns.
