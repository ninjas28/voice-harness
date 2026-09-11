# voice-harness

Rust voice-assistant bridge: sits between clients (ESP32-class smart speakers, desktop apps)
and the existing voice servers (Nemotron ASR + magpie TTS on voicebox, Open WebUI LLM on ai).
Accepts streamed PCM16 mic audio or pre-transcribed text, runs an agentic pipeline
(system prompt + plugin tools -> LLM), returns transcript + sentence-chunked TTS audio.

## Layout

- `crates/harness-core` — config, wire protocol types, VAD/utterance FSM, chunker
- `crates/harness-providers` — STT / TTS / LLM upstream clients
- `crates/harness-plugins` — plugin trait, registry, built-in plugins
- `crates/harness-server` — axum app: `POST /v1/turn`, `WS /v1/realtime`
- `config/voice-harness.toml.example` — config template (real config is gitignored)

## Status

Phase A: workspace scaffold, config loading, wire protocol types, VAD/utterance FSM.
