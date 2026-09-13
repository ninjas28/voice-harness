//! `weather` plugin: current conditions + forecast for any location via
//! Open-Meteo (keyless). Geocodes a place name, then fetches the forecast.

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::{Plugin, PluginManifest};

/// Official forecast endpoint (overridable via `[plugins.weather].api_base`).
#[allow(dead_code)] // consumed by the forecast fetch (later task)
const DEFAULT_FORECAST_URL: &str = "https://api.open-meteo.com/v1/forecast";
/// Official geocoding endpoint (overridable via `[plugins.weather].geocoding_base`).
#[allow(dead_code)] // consumed by the geocoding request (later task)
const DEFAULT_GEOCODE_URL: &str = "https://geocoding-api.open-meteo.com/v1/search";
/// Request timeout per upstream call (plan).
#[allow(dead_code)] // consumed by the upstream fetches (later tasks)
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
/// Response body cap: 64 KiB (plan).
#[allow(dead_code)] // consumed by the upstream fetches (later tasks)
const MAX_BODY_BYTES: usize = 64 * 1024;

/// Allowlist-free weather lookup. The upstream host is fixed (or configured);
/// only the LLM-supplied location name travels as a query parameter.
pub struct WeatherPlugin {
    api_base: String,
    geocoding_base: String,
    /// Location used when the LLM omits one (read by `call`, later task).
    #[allow(dead_code)]
    default_location: String,
}

impl WeatherPlugin {
    pub fn new(default_location: String) -> Self {
        Self {
            api_base: String::new(),
            geocoding_base: String::new(),
            default_location,
        }
    }

    /// Endpoint overrides for tests and self-hosted instances.
    /// Order: (forecast api_base, geocoding_base). Empty = official URL.
    pub fn with_endpoints(mut self, api_base: String, geocoding_base: String) -> Self {
        self.api_base = api_base;
        self.geocoding_base = geocoding_base;
        self
    }

    #[allow(dead_code)] // consumed by the upstream fetches (later tasks)
    fn forecast_url(&self) -> String {
        if self.api_base.trim().is_empty() {
            DEFAULT_FORECAST_URL.to_string()
        } else {
            self.api_base.trim().to_string()
        }
    }

    #[allow(dead_code)] // consumed by the upstream fetches (later tasks)
    fn geocode_url(&self) -> String {
        if self.geocoding_base.trim().is_empty() {
            DEFAULT_GEOCODE_URL.to_string()
        } else {
            self.geocoding_base.trim().to_string()
        }
    }
}

#[async_trait]
impl Plugin for WeatherPlugin {
    fn manifest(&self) -> &PluginManifest {
        static MANIFEST: std::sync::OnceLock<PluginManifest> = std::sync::OnceLock::new();
        MANIFEST.get_or_init(|| PluginManifest {
            name: "weather",
            version: "0.1.0",
            description: "Current weather and forecast for a named location (Open-Meteo)",
        })
    }

    fn tool_specs(&self) -> Vec<Value> {
        vec![json!({
            "type": "function",
            "function": {
                "name": "weather.get_forecast",
                "description": "Current conditions and a daily forecast for a location. \
                               Pass a place name such as \"Paris\" or \"Austin, Texas\"; \
                               omit `location` to use the configured default home \
                               location. Optionally set `days` (1-7) and `units` \
                               (\"metric\" or \"imperial\").",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "location": { "type": "string", "description": "Place name, e.g. \"Paris\" or \"Austin, Texas\"" },
                        "days": { "type": "integer", "description": "Forecast days including today (1-7, default 1)" },
                        "units": { "type": "string", "enum": ["metric", "imperial"], "description": "Unit system (default metric)" }
                    },
                    "required": []
                }
            }
        })]
    }

    async fn call(&self, name: &str, _arguments: Value) -> Result<Value, String> {
        if name != "get_forecast" {
            return Err(format!("weather plugin has no tool '{name}'"));
        }
        Err("weather lookup not implemented yet".to_string())
    }
}
