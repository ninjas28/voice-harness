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
    /// Browser origins allowed to open the `/v1/realtime` WebSocket. Native
    /// clients send no `Origin` header and are always allowed; a browser
    /// request's Origin must exactly match one of these entries. Empty = all
    /// Origin-bearing (browser) requests are rejected (cross-site WebSocket
    /// hijacking guard); native clients are unaffected.
    #[serde(default)]
    pub allowed_origins: Vec<String>,
}

fn default_bind() -> String {
    "127.0.0.1:8090".to_string()
}

/// `[stt]` batch STT client config (multipart WAV upload → transcript).
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
    /// Streaming mode over the ASR server's realtime transcription WebSocket.
    /// Disabled by default: batch stays the fallback and the loopback path.
    #[serde(default)]
    pub realtime: SttRealtimeConfig,
}

/// `[stt.realtime]`: streaming STT over the ASR server's realtime
/// transcription WebSocket (nemo-speech.cpp). Enabled = the harness forwards
/// client audio frames to the server's own VAD/endpointing and receives
/// partial + final transcripts. Uses `[stt]`'s base_url/api_key (same server).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SttRealtimeConfig {
    /// Off by default; batch transcription remains the fallback.
    #[serde(default)]
    pub enabled: bool,
    /// Realtime WS endpoint path on the same base_url as `[stt]`.
    #[serde(default = "default_stt_realtime_path")]
    pub path: String,
    /// End-of-utterance silence threshold the upstream endpointing uses.
    #[serde(default = "default_stt_realtime_endpointing_ms")]
    pub endpointing_ms: u64,
    /// BCP-47 language hint, e.g. "en-US". Empty = model default.
    #[serde(default)]
    pub language: String,
}

fn default_stt_realtime_path() -> String {
    "/v1/audio/transcriptions/realtime".to_string()
}
fn default_stt_realtime_endpointing_ms() -> u64 {
    700
}

fn default_stt_base_url() -> String {
    "http://127.0.0.1:8000".to_string()
}
fn default_stt_chat_path() -> String {
    "/v1/audio/transcriptions".to_string()
}
fn default_stt_model() -> String {
    "asr-model".to_string()
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
    "http://127.0.0.1:8000".to_string()
}
fn default_tts_speech_path() -> String {
    "/v1/audio/speech".to_string()
}
fn default_tts_model() -> String {
    "tts-model".to_string()
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
    /// Optional reasoning effort sent as `reasoning_effort` on every chat
    /// request ("minimal", "low", "medium", "high"). Empty = not sent.
    #[serde(default)]
    pub reasoning_effort: String,
}

fn default_llm_base_url() -> String {
    "http://127.0.0.1:3000/api".to_string()
}
fn default_llm_chat_path() -> String {
    "/v1/chat/completions".to_string()
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
    /// Require a transcribed utterance to end with sentence-terminal
    /// punctuation before dispatching it to the LLM. Utterances that end
    /// mid-sentence (a VAD pause around an "uh") are held and concatenated
    /// with the following utterance's transcript.
    #[serde(default = "default_require_sentence_end")]
    pub require_sentence_end: bool,
    /// How long a held (sentence-incomplete) transcript may sit without new
    /// speech before it dispatches anyway, so a complete command can never
    /// hang forever. The deadline is deferred whenever the VAD reopens.
    #[serde(default = "default_sentence_end_wait_ms")]
    pub sentence_end_wait_ms: u64,
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
fn default_require_sentence_end() -> bool {
    true
}
fn default_sentence_end_wait_ms() -> u64 {
    2_000
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PromptsConfig {
    #[serde(default = "default_system_prompt")]
    pub system: String,
    /// Optional path to a markdown file whose content replaces `system`.
    /// Resolved relative to the config file's directory; empty = not used.
    /// Loaded once at config load; a missing file is a hard error.
    #[serde(default)]
    pub system_file: String,
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
    /// Web search/fetch via self-hosted Firecrawl.
    #[serde(default)]
    pub web_search: WebSearchConfig,
    /// Built-in weather plugin settings (Open-Meteo).
    #[serde(default)]
    pub weather: WeatherConfig,
    /// MCP servers exposed as tools (Streamable HTTP).
    #[serde(default)]
    pub mcp: McpConfig,
}

/// `[plugins.web_search]`: web search + page fetch via a self-hosted
/// Firecrawl instance (`POST /v2/search`, `POST /v2/scrape`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WebSearchConfig {
    /// Base URL of the Firecrawl server, e.g. `http://192.168.10.94:3002`.
    /// Empty = the plugin contributes no tools even if listed in `enabled`.
    #[serde(default)]
    pub base_url: String,
    /// Optional bearer token (cloud Firecrawl). Empty = no auth header
    /// (self-hosted default).
    #[serde(default)]
    pub api_key: String,
}

/// `[plugins.weather]`: built-in weather via Open-Meteo (keyless).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WeatherConfig {
    /// Location used when the LLM calls `weather.get_forecast` without one
    /// ("what's the weather?"). Empty = the tool errors and the LLM asks.
    #[serde(default)]
    pub default_location: String,
    /// Full forecast endpoint URL override (self-hosted Open-Meteo).
    /// Empty = official `https://api.open-meteo.com/v1/forecast`.
    #[serde(default)]
    pub api_base: String,
    /// Full geocoding endpoint URL override. Empty = official
    /// `https://geocoding-api.open-meteo.com/v1/search`.
    #[serde(default)]
    pub geocoding_base: String,
    /// Default unit system when the LLM omits `units`: empty = metric,
    /// `imperial` = Fahrenheit/mph/inches. Explicit tool arguments override.
    #[serde(default)]
    pub units: String,
}

/// MCP (Model Context Protocol) servers exposed as tools over Streamable HTTP.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpConfig {
    /// Where OAuth tokens for MCP servers are persisted. Never commit this file.
    #[serde(default = "default_mcp_token_store")]
    pub token_store: String,
    #[serde(default)]
    pub servers: Vec<McpServerConfig>,
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            token_store: default_mcp_token_store(),
            servers: Vec::new(),
        }
    }
}

fn default_mcp_token_store() -> String {
    "config/mcp-tokens.json".to_string()
}

/// One MCP server. Its tools surface to the LLM as `mcp.<name>.<tool>`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct McpServerConfig {
    /// `[A-Za-z0-9_-]+` — must not contain `.` (the registry tool separator).
    pub name: String,
    /// Streamable HTTP endpoint, e.g. `https://mcp.example.com/mcp`.
    pub url: String,
    /// `"bearer"` (uses `api_key`) or `"oauth"`.
    #[serde(default)]
    pub auth: String,
    /// Bearer token when `auth = "bearer"`.
    #[serde(default)]
    pub api_key: String,
    /// OAuth scopes to request when `auth = "oauth"`.
    #[serde(default)]
    pub scopes: Vec<String>,
    /// Static OAuth client_id — only needed if the server lacks dynamic
    /// client registration.
    #[serde(default)]
    pub client_id: String,
    /// When non-empty, only these upstream tool names surface to the LLM and
    /// are dispatchable; everything else the server advertises is pruned.
    /// Empty = every advertised tool passes through.
    #[serde(default)]
    pub tool_allowlist: Vec<String>,
}

fn default_enabled_plugins() -> Vec<String> {
    vec!["time".to_string(), "weather".to_string()]
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: ServerConfig {
                bind: default_bind(),
                api_keys: Vec::new(),
                allowed_origins: Vec::new(),
            },
            stt: SttConfig {
                base_url: default_stt_base_url(),
                api_key: String::new(),
                chat_path: default_stt_chat_path(),
                model: default_stt_model(),
                realtime: SttRealtimeConfig {
                    enabled: false,
                    path: default_stt_realtime_path(),
                    endpointing_ms: default_stt_realtime_endpointing_ms(),
                    language: String::new(),
                },
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
                reasoning_effort: String::new(),
            },
            session: SessionConfig {
                silence_ms: default_silence_ms(),
                min_utterance_ms: default_min_utterance_ms(),
                max_utterance_ms: default_max_utterance_ms(),
                pre_speech_ms: default_pre_speech_ms(),
                chunk_max_chars: default_chunk_max_chars(),
                max_history_turns: default_max_history_turns(),
                idle_timeout_secs: default_idle_timeout_secs(),
                require_sentence_end: default_require_sentence_end(),
                sentence_end_wait_ms: default_sentence_end_wait_ms(),
            },
            prompts: PromptsConfig {
                system: default_system_prompt(),
                system_file: String::new(),
            },
            plugins: PluginsConfig {
                enabled: default_enabled_plugins(),
                web_search: Default::default(),
                weather: Default::default(),
                mcp: Default::default(),
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
            // Force TOML: the example file ends in `.toml.example`, whose real
            // extension would not infer a format (and `required(false)` would
            // silently skip the file).
            builder = builder.add_source(
                config::File::from(p)
                    .required(false)
                    .format(config::FileFormat::Toml),
            );
        }

        builder = builder.add_source(
            config::Environment::with_prefix("VH")
                .prefix_separator("_")
                .separator("__")
                .try_parsing(true),
        );

        let cfg: Config = builder.build()?.try_deserialize()?;

        // `prompts.system_file`: optional markdown file whose content replaces
        // the inline `prompts.system`. Resolved relative to the config file's
        // directory (falling back to cwd when no file was loaded); loaded once
        // at startup. A configured-but-missing file is a hard error so a typo
        // can never silently drop the real prompt.
        if !cfg.prompts.system_file.trim().is_empty() {
            let base = path.and_then(Path::parent).unwrap_or(Path::new("."));
            let prompt_path = base.join(&cfg.prompts.system_file);
            let content = std::fs::read_to_string(&prompt_path).map_err(|e| {
                format!(
                    "prompts.system_file: cannot read '{}': {e}",
                    prompt_path.display()
                )
            })?;
            let mut cfg = cfg;
            cfg.prompts.system = content.trim_end().to_string();
            return Ok(cfg);
        }

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_config_defaults_and_parses_from_toml() {
        // Defaults: no servers, default token store path.
        let defaults = Config::load(None).expect("defaults load");
        assert!(defaults.plugins.mcp.servers.is_empty());
        assert_eq!(defaults.plugins.mcp.token_store, "config/mcp-tokens.json");

        let path = std::env::temp_dir().join(format!("vh-mcp-cfg-{}.toml", std::process::id()));
        std::fs::write(
            &path,
            r#"
[plugins]
enabled = ["time", "mcp"]

[plugins.mcp]
token_store = "/tmp/vh-tokens.json"

[[plugins.mcp.servers]]
name = "weather"
url = "https://mcp.example.com/mcp"
auth = "bearer"
api_key = "sk-test"

[[plugins.mcp.servers]]
name = "home"
url = "https://mcp.example.org/mcp"
auth = "oauth"
scopes = ["home.read"]
"#,
        )
        .expect("write temp config");
        let cfg = Config::load(Some(&path)).expect("parses");
        assert_eq!(
            cfg.plugins.enabled,
            vec!["time".to_string(), "mcp".to_string()]
        );
        assert_eq!(cfg.plugins.mcp.servers.len(), 2);
        let weather = &cfg.plugins.mcp.servers[0];
        assert_eq!(weather.name, "weather");
        assert_eq!(weather.auth, "bearer");
        assert_eq!(weather.api_key, "sk-test");
        let home = &cfg.plugins.mcp.servers[1];
        assert_eq!(home.auth, "oauth");
        assert_eq!(home.scopes, vec!["home.read".to_string()]);
    }

    #[test]
    fn weather_config_defaults_and_parses_from_toml() {
        let defaults = Config::load(None).expect("defaults load");
        assert!(defaults.plugins.weather.default_location.is_empty());
        assert!(defaults.plugins.weather.api_base.is_empty());
        assert!(defaults.plugins.weather.geocoding_base.is_empty());
        assert!(
            defaults.plugins.weather.units.is_empty(),
            "default units empty = metric"
        );
        assert!(
            defaults.plugins.enabled.contains(&"weather".to_string()),
            "weather ships enabled by default: {:?}",
            defaults.plugins.enabled
        );

        let path = std::env::temp_dir().join(format!("vh-weather-cfg-{}.toml", std::process::id()));
        std::fs::write(
            &path,
            r#"
[plugins]
enabled = ["time", "weather"]

[plugins.weather]
default_location = "Portland, Oregon"
api_base = "http://127.0.0.1:8080/v1/forecast"
geocoding_base = "http://127.0.0.1:8080/v1/search"
units = "imperial"
"#,
        )
        .expect("write temp config");
        let cfg = Config::load(Some(&path)).expect("parses");
        assert_eq!(cfg.plugins.weather.default_location, "Portland, Oregon");
        assert_eq!(
            cfg.plugins.weather.api_base,
            "http://127.0.0.1:8080/v1/forecast"
        );
        assert_eq!(
            cfg.plugins.weather.geocoding_base,
            "http://127.0.0.1:8080/v1/search"
        );
        assert_eq!(cfg.plugins.weather.units, "imperial");
    }

    #[test]
    fn web_search_config_defaults_and_parses_from_toml() {
        let defaults = Config::load(None).expect("defaults load");
        assert!(defaults.plugins.web_search.base_url.is_empty());
        assert!(defaults.plugins.web_search.api_key.is_empty());

        let path =
            std::env::temp_dir().join(format!("vh-websearch-cfg-{}.toml", std::process::id()));
        std::fs::write(
            &path,
            r#"
[plugins]
enabled = ["time", "web_search"]

[plugins.web_search]
base_url = "http://192.168.10.94:3002"
api_key = "fc-selfhosted-no-key"
"#,
        )
        .expect("write temp config");
        let cfg = Config::load(Some(&path)).expect("parses");
        assert_eq!(cfg.plugins.web_search.base_url, "http://192.168.10.94:3002");
        assert_eq!(cfg.plugins.web_search.api_key, "fc-selfhosted-no-key");
    }

    #[test]
    fn defaults_enable_only_time_and_weather() {
        let defaults = Config::load(None).expect("defaults load");
        // Exact list: removal-only default `[time, weather]`; web_search stays
        // opt-in (it needs [plugins.web_search].base_url to contribute tools).
        assert_eq!(
            defaults.plugins.enabled,
            vec!["time".to_string(), "weather".to_string()],
            "defaults must enable only time + weather: {:?}",
            defaults.plugins.enabled
        );
    }

    /// The checked-in example config must always parse against the real
    /// `Config` struct — guards the template against field drift.
    #[test]
    fn example_config_file_parses() {
        let path = std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../config/voice-harness.toml.example"
        ));
        let cfg = Config::load(Some(path)).expect("example config parses");
        eprintln!(
            "DEBUG bind={} stt_key={:?} tts_key={:?} llm_key={:?} raw_rate={}",
            cfg.server.bind,
            cfg.stt.api_key,
            cfg.tts.api_key,
            cfg.llm.api_key,
            cfg.tts.raw_sample_rate
        );
        assert_eq!(cfg.server.bind, "127.0.0.1:8090");
        assert_eq!(cfg.tts.raw_sample_rate, 16_000);
        assert!(cfg.validate().is_ok(), "example has SET-ME keys filled");
    }

    #[test]
    fn system_file_defaults_to_empty() {
        let cfg = Config::load(None).expect("defaults load");
        assert_eq!(
            cfg.prompts.system_file, "",
            "default keeps the inline [prompts.system] behavior"
        );
    }

    #[test]
    fn sentence_gate_defaults_on_with_two_second_wait() {
        let cfg = Config::load(None).expect("defaults load");
        assert!(cfg.session.require_sentence_end, "gate defaults on");
        assert_eq!(cfg.session.sentence_end_wait_ms, 2_000);
    }

    #[test]
    fn sentence_gate_parses_from_toml_and_env() {
        let dir = std::env::temp_dir().join(format!("vh-sentgate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let cfg_path = dir.join("config.toml");
        std::fs::write(
            &cfg_path,
            r#"
[session]
require_sentence_end = false
sentence_end_wait_ms = 1500
"#,
        )
        .expect("write config");
        let cfg = Config::load(Some(&cfg_path)).expect("parses");
        assert!(!cfg.session.require_sentence_end);
        assert_eq!(cfg.session.sentence_end_wait_ms, 1_500);

        // Env override wins over the file.
        // SAFETY: tests run in one process; set_var here races nothing since
        // each load consumes its own env vars and keys are test-unique.
        std::env::set_var("VH_SESSION__REQUIRE_SENTENCE_END", "true");
        let cfg = Config::load(Some(&cfg_path)).expect("parses");
        assert!(cfg.session.require_sentence_end, "env overrides file");
    }

    #[test]
    fn system_file_overrides_inline_system_from_toml() {
        // Temp prompt + config in a dedicated dir (prompt path is resolved
        // relative to the config file's directory).
        let dir = std::env::temp_dir().join(format!("vh-prompt-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let prompt_path = dir.join("prompt.md");
        std::fs::write(&prompt_path, "SPEAK PLAIN.\nBe brief always.").expect("write prompt file");
        let cfg_path = dir.join("config.toml");
        std::fs::write(
            &cfg_path,
            r#"
[prompts]
system = "inline fallback text"
system_file = "prompt.md"
"#,
        )
        .expect("write config");

        let cfg = Config::load(Some(&cfg_path)).expect("parses");
        assert_eq!(
            cfg.prompts.system, "SPEAK PLAIN.\nBe brief always.",
            "system_file content wins over inline system; trailing newline trimmed"
        );
    }

    #[test]
    fn allowed_origins_defaults_empty_and_parses_from_toml() {
        let defaults = Config::load(None).expect("defaults load");
        assert!(
            defaults.server.allowed_origins.is_empty(),
            "empty allowlist by default: all browser origins rejected"
        );

        let path = std::env::temp_dir().join(format!("vh-origins-cfg-{}.toml", std::process::id()));
        std::fs::write(
            &path,
            r#"
[server]
bind = "127.0.0.1:8090"
allowed_origins = ["https://home.example.com"]
"#,
        )
        .expect("write temp config");
        let cfg = Config::load(Some(&path)).expect("parses");
        assert_eq!(
            cfg.server.allowed_origins,
            vec!["https://home.example.com".to_string()]
        );
    }

    #[test]
    fn stt_realtime_defaults_disabled() {
        let de: SttRealtimeConfig = serde_json::from_str("{}").expect("empty map parses");
        assert!(!de.enabled);
        assert_eq!(de.path, "/v1/audio/transcriptions/realtime");
        assert_eq!(de.endpointing_ms, 700);
        assert_eq!(de.language, "");
    }

    #[test]
    fn stt_config_embeds_realtime_defaults() {
        let de: SttConfig = serde_json::from_str("{}").expect("stt parses");
        assert!(!de.realtime.enabled);
    }

    /// The JSON base layer in `Config::load` is serialized from `Config::default()`;
    /// if the manual `Default` impl dropped or zeroed the realtime section, a
    /// partial config file would silently lose the documented path/endpointing.
    #[test]
    fn config_load_defaults_carry_realtime_section() {
        let cfg = Config::load(None).expect("defaults load");
        assert!(!cfg.stt.realtime.enabled);
        assert_eq!(cfg.stt.realtime.path, "/v1/audio/transcriptions/realtime");
        assert_eq!(cfg.stt.realtime.endpointing_ms, 700);
        assert_eq!(cfg.stt.realtime.language, "");
    }

    #[test]
    fn system_file_missing_reports_the_path() {
        let dir = std::env::temp_dir().join(format!("vh-prompt-missing-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let cfg_path = dir.join("config.toml");
        std::fs::write(
            &cfg_path,
            r#"
[prompts]
system_file = "does-not-exist.md"
"#,
        )
        .expect("write config");

        let err = Config::load(Some(&cfg_path)).expect_err("missing prompt file must fail");
        let msg = err.to_string();
        assert!(
            msg.contains("prompts.system_file") && msg.contains("does-not-exist.md"),
            "error must name the config key and the path: {msg}"
        );
    }
}
