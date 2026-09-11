//! Runtime configuration: defaults < optional TOML file < `VH_<SECTION>__<KEY>` env vars.

use serde::{Deserialize, Serialize};
use std::path::Path;
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub stt: SttConfig,
    #[serde(default)]
    pub tts: TtsConfig,
    #[serde(default)]
    pub llm: LlmConfig,
    #[serde(default)]
    pub session: SessionConfig,
    #[serde(default)]
    pub prompts: PromptsConfig,
    #[serde(default)]
    pub plugins: PluginsConfig,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_bind")]
    pub bind: String,
    #[serde(default)]
    pub api_keys: Vec<String>,
}

fn default_bind() -> String {
    "127.0.0.1:8090".to_string()
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SttConfig {
    #[serde(default = "default_stt_base_url")]
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
    #[serde(default = "default_stt_chat_path")]
    pub chat_path: String,
    #[serde(default = "default_stt_model")]
    pub model: String,
}

fn default_stt_base_url() -> String {
    "https://voicebox.zippystation.com".to_string()
}
fn default_stt_chat_path() -> String {
    "/v1/audio/transcriptions".to_string()
}
fn default_stt_model() -> String {
    "nemotron-asr".to_string()
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TtsConfig {
    #[serde(default = "default_tts_base_url")]
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
    #[serde(default = "default_tts_speech_path")]
    pub speech_path: String,
    #[serde(default = "default_tts_model")]
    pub model: String,
    #[serde(default = "default_tts_voice")]
    pub voice: String,
    #[serde(default = "default_tts_response_format")]
    pub response_format: String,
    /// Sample rate of the upstream's *raw* (non-WAV) PCM output; 16000 means
    /// raw bodies pass through unchanged. Ignored for WAV responses (the
    /// header carries the rate).
    #[serde(default = "default_tts_raw_sample_rate")]
    pub raw_sample_rate: u32,
}

fn default_tts_raw_sample_rate() -> u32 {
    16_000
}

fn default_tts_base_url() -> String {
    "https://voicebox.zippystation.com".to_string()
}
fn default_tts_speech_path() -> String {
    "/v1/audio/speech".to_string()
}
fn default_tts_model() -> String {
    "magpie-tts".to_string()
}
fn default_tts_voice() -> String {
    "default".to_string()
}
fn default_tts_response_format() -> String {
    "wav".to_string()
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LlmConfig {
    #[serde(default = "default_llm_base_url")]
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
    #[serde(default = "default_llm_chat_path")]
    pub chat_path: String,
    #[serde(default = "default_llm_model")]
    pub model: String,
}

fn default_llm_base_url() -> String {
    "https://ai.zippystation.com/api".to_string()
}
fn default_llm_chat_path() -> String {
    "/api/chat/completions".to_string()
}
fn default_llm_model() -> String {
    "default".to_string()
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionConfig {
    #[serde(default = "default_silence_ms")]
    pub silence_ms: u64,
    #[serde(default = "default_min_utterance_ms")]
    pub min_utterance_ms: u64,
    #[serde(default = "default_max_utterance_ms")]
    pub max_utterance_ms: u64,
    #[serde(default = "default_pre_speech_ms")]
    pub pre_speech_ms: u64,
    #[serde(default = "default_chunk_max_chars")]
    pub chunk_max_chars: usize,
    #[serde(default = "default_max_history_turns")]
    pub max_history_turns: usize,
    #[serde(default = "default_idle_timeout_secs")]
    pub idle_timeout_secs: u64,
}

fn default_silence_ms() -> u64 {
    700
}
fn default_min_utterance_ms() -> u64 {
    300
}
fn default_max_utterance_ms() -> u64 {
    30_000
}
fn default_pre_speech_ms() -> u64 {
    500
}
fn default_chunk_max_chars() -> usize {
    200
}
fn default_max_history_turns() -> usize {
    8
}
fn default_idle_timeout_secs() -> u64 {
    600
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PromptsConfig {
    #[serde(default = "default_system_prompt")]
    pub system: String,
}

fn default_system_prompt() -> String {
    "You are a helpful home voice assistant. Answer concisely — 1 to 3 sentences unless \
     the user asks for detail. Speak plainly; never output markdown, code fences, or lists. \
     You can call tools when useful."
        .to_string()
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PluginsConfig {
    #[serde(default = "default_enabled_plugins")]
    pub enabled: Vec<String>,
    /// Host allowlist for the `http_fetch` built-in (deny by default).
    #[serde(default)]
    pub http_fetch: HttpFetchConfig,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HttpFetchConfig {
    /// Hosts `http_fetch` may GET. Empty = deny everything.
    #[serde(default)]
    pub allowed_hosts: Vec<String>,
}

fn default_enabled_plugins() -> Vec<String> {
    vec!["time".to_string(), "http_fetch".to_string()]
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: ServerConfig {
                bind: default_bind(),
                api_keys: Vec::new(),
            },
            stt: SttConfig {
                base_url: default_stt_base_url(),
                api_key: String::new(),
                chat_path: default_stt_chat_path(),
                model: default_stt_model(),
            },
            tts: TtsConfig {
                base_url: default_tts_base_url(),
                api_key: String::new(),
                speech_path: default_tts_speech_path(),
                model: default_tts_model(),
                voice: default_tts_voice(),
                response_format: default_tts_response_format(),
                raw_sample_rate: default_tts_raw_sample_rate(),
            },
            llm: LlmConfig {
                base_url: default_llm_base_url(),
                api_key: String::new(),
                chat_path: default_llm_chat_path(),
                model: default_llm_model(),
            },
            session: SessionConfig {
                silence_ms: default_silence_ms(),
                min_utterance_ms: default_min_utterance_ms(),
                max_utterance_ms: default_max_utterance_ms(),
                pre_speech_ms: default_pre_speech_ms(),
                chunk_max_chars: default_chunk_max_chars(),
                max_history_turns: default_max_history_turns(),
                idle_timeout_secs: default_idle_timeout_secs(),
            },
            prompts: PromptsConfig {
                system: default_system_prompt(),
            },
            plugins: PluginsConfig {
                enabled: default_enabled_plugins(),
                http_fetch: Default::default(),
            },
        }
    }
}

impl Config {
    /// Load configuration with layering: built-in defaults < optional file < `VH_` env.
    ///
    /// Env vars are uppercase `VH_<SECTION>__<KEY>`, e.g. `VH_SESSION__SILENCE_MS=250`
    /// or `VH_STT__API_KEY=...`. Each section is nested (serde `#[serde(default)]` +
    /// TOML tables), so partial files/env keep untouched defaults.
    pub fn load(path: Option<&Path>) -> Result<Self, Box<dyn std::error::Error>> {
        // Base the merge on the full defaults: our serde model already implements
        // Default, so serialize it straight to JSON, then let file/env layers
        // override individual leaves.
        let json = serde_json::to_string(&Config::default())?;

        let mut builder = config::Config::builder()
            .add_source(config::File::from_str(&json, config::FileFormat::Json));

        if let Some(p) = path {
            builder = builder.add_source(config::File::from(p).required(false));
        }

        builder = builder.add_source(
            config::Environment::with_prefix("VH")
                .prefix_separator("_")
                .separator("__")
                .try_parsing(true),
        );

        let cfg: Config = builder.build()?.try_deserialize()?;
        Ok(cfg)
    }

    /// Check required secrets are present. Returns every missing key, not just the first.
    pub fn validate(&self) -> Result<(), Vec<String>> {
        let mut missing = Vec::new();
        if self.stt.api_key.trim().is_empty() {
            missing.push("stt.api_key".to_string());
        }
        if self.tts.api_key.trim().is_empty() {
            missing.push("tts.api_key".to_string());
        }
        if self.llm.api_key.trim().is_empty() {
            missing.push("llm.api_key".to_string());
        }
        if missing.is_empty() {
            Ok(())
        } else {
            Err(missing)
        }
    }
}
