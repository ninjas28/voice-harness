//! Server binary: load config, build real provider clients, serve the router.
//! Subcommand: `harness-server auth <mcp-server-name> [config-path]` runs the
//! OAuth 2.1 browser flow for one MCP server and stores its tokens.

use std::sync::Arc;

use harness_core::config::Config;
use harness_plugins::registry_from_config;
use harness_providers::llm::OpenAiLlmClient;
use harness_providers::stt::OpenAiSttClient;
use harness_providers::stt_realtime::RealtimeSttClient;
use harness_providers::tts::OpenAiTtsClient;
use harness_server::http::{build_router, RouterDeps};
use harness_server::state::SessionStore;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,tower_http=info".into()),
        )
        .init();

    let mut args = std::env::args().skip(1);
    let first = args.next();

    if first.as_deref() == Some("auth") {
        let Some(server_name) = args.next() else {
            eprintln!("usage: harness-server auth <mcp-server-name> [config-path]");
            std::process::exit(2);
        };
        let config_path = args.next();
        run_auth_command(&server_name, config_path.as_deref()).await;
        return;
    }

    let config_path = first.map(std::path::PathBuf::from).or_else(|| {
        std::path::PathBuf::from("config/voice-harness.toml")
            .exists()
            .then_some(std::path::PathBuf::from("config/voice-harness.toml"))
    });

    let config = Config::load(config_path.as_deref()).unwrap_or_else(|e| {
        eprintln!("failed to load config: {e}");
        std::process::exit(1);
    });
    if let Err(missing) = config.validate() {
        eprintln!(
            "config missing required keys: {} (set them in the file or via VH_<SECTION>__<KEY> env vars)",
            missing.join(", ")
        );
        std::process::exit(1);
    }

    // Streaming STT is opt-in ([stt.realtime].enabled); batch stays the
    // fallback and the loopback example path.
    let stt_realtime = config.stt.realtime.enabled.then(|| {
        Arc::new(RealtimeSttClient::new(
            &config.stt.base_url,
            &config.stt.realtime.path,
            &config.stt.api_key,
            config.stt.realtime.endpointing_ms,
            &config.stt.realtime.language,
        ))
    });
    let deps = RouterDeps {
        sessions: Arc::new(SessionStore::new()),
        llm: Arc::new(OpenAiLlmClient::new(
            &config.llm.base_url,
            &config.llm.chat_path,
            &config.llm.api_key,
        )),
        tts: Arc::new(OpenAiTtsClient::from_config(&config.tts)),
        stt: Arc::new(OpenAiSttClient::from_config(&config.stt)),
        stt_realtime,
        plugins: Arc::new(registry_from_config(&config.plugins)),
        config: config.clone(),
    };

    // Async one-time plugin setup: MCP servers fetch their tool lists here.
    deps.plugins.warm_all().await;

    let app = build_router(deps);
    let listener = tokio::net::TcpListener::bind(&config.server.bind)
        .await
        .unwrap_or_else(|e| {
            eprintln!("failed to bind {}: {e}", config.server.bind);
            std::process::exit(1);
        });
    // Exposed-but-unauthenticated is the classic footgun: a LAN/WAN bind with
    // an empty api_keys list accepts audio + LLM turns from anyone who can
    // reach the port. Say so loudly at startup instead of failing silently.
    if config.server.api_keys.is_empty() && !is_loopback_bind(&config.server.bind) {
        tracing::warn!(
            "server is bound to {} with NO api_keys — the daemon is exposed unauthenticated \
             (anyone on the network can stream audio and use your LLM/TTS); set [server].api_keys \
             or bind to a loopback address",
            config.server.bind
        );
    }
    if config.prompts.system_file.trim().is_empty() {
        tracing::info!(
            "system prompt: inline ([prompts.system], {} chars)",
            config.prompts.system.len()
        );
    } else {
        tracing::info!(
            "system prompt: loaded from {} ({} chars)",
            config.prompts.system_file,
            config.prompts.system.len()
        );
    }
    tracing::info!(
        "stt: {}",
        if config.stt.realtime.enabled {
            format!("realtime ({})", config.stt.realtime.path)
        } else {
            "batch".to_string()
        }
    );
    tracing::info!("voice-harness listening on http://{}", config.server.bind);
    axum::serve(listener, app)
        .await
        .expect("server runs until stopped");
}

/// True when `bind` (`host:port`, IPv6 in brackets) targets a loopback
/// address (127.x.x.x, ::1, or the `localhost` name). Used to decide whether
/// an empty `server.api_keys` config is a "localhost trust" setup or an
/// exposed unauthenticated daemon.
fn is_loopback_bind(bind: &str) -> bool {
    let Some((host, _port)) = bind.rsplit_once(':') else {
        return false;
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if host == "localhost" {
        return true;
    }
    if host == "::1" {
        return true;
    }
    host.parse::<std::net::Ipv4Addr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or(false)
}

/// `auth` subcommand: run the OAuth flow for one configured MCP server.
/// With no explicit path, auto-detects `config/voice-harness.toml` exactly
/// like the server path does (falling back to defaults was a bug: it silently
/// produced an empty server list).
async fn run_auth_command(server_name: &str, config_path: Option<&str>) {
    let config_path = config_path.map(std::path::PathBuf::from).or_else(|| {
        std::path::PathBuf::from("config/voice-harness.toml")
            .exists()
            .then_some(std::path::PathBuf::from("config/voice-harness.toml"))
    });
    let config = Config::load(config_path.as_deref()).unwrap_or_else(|e| {
        eprintln!("failed to load config: {e}");
        std::process::exit(1);
    });
    let Some(server) = config
        .plugins
        .mcp
        .servers
        .iter()
        .find(|s| s.name == server_name)
    else {
        eprintln!(
            "no mcp server named '{server_name}' in config (configured: {:?})",
            config
                .plugins
                .mcp
                .servers
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>()
        );
        std::process::exit(1);
    };
    if server.auth != "oauth" {
        eprintln!(
            "mcp server '{server_name}' uses '{}' auth — no interactive authorization needed",
            server.auth
        );
        std::process::exit(1);
    }
    match harness_plugins::mcp::oauth::run_authorization_flow(
        server,
        &config.plugins.mcp.token_store,
    )
    .await
    {
        Ok(()) => println!("stored OAuth token for mcp server '{server_name}'"),
        Err(e) => {
            eprintln!("authorization failed: {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::is_loopback_bind;

    #[test]
    fn loopback_binds_are_detected() {
        assert!(is_loopback_bind("127.0.0.1:8090"));
        assert!(is_loopback_bind("[::1]:8090"));
        assert!(is_loopback_bind("localhost:8090"));
        assert!(is_loopback_bind("127.0.0.1:0"));
    }

    #[test]
    fn non_loopback_binds_are_detected() {
        assert!(!is_loopback_bind("0.0.0.0:8090"));
        assert!(!is_loopback_bind("192.168.10.94:8090"));
        assert!(!is_loopback_bind("[::]:8090"));
    }
}
