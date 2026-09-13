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

/// Unit system for the forecast request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // consumed by the forecast fetch (later task)
enum Units {
    Metric,
    Imperial,
}

#[allow(dead_code)] // consumed by `call` argument parsing (later task)
fn units_from(arg: &str) -> Units {
    if arg.eq_ignore_ascii_case("imperial") {
        Units::Imperial
    } else {
        Units::Metric
    }
}

#[allow(dead_code)] // consumed by `call` argument parsing (later task)
fn units_from_none(arg: Option<&str>) -> Units {
    arg.map(units_from).unwrap_or(Units::Metric)
}

/// Forecast days including today, clamped to the API's 1..=7 range.
#[allow(dead_code)] // consumed by `call` argument parsing (later task)
fn clamp_days(arg: Option<u64>) -> u32 {
    arg.unwrap_or(1).clamp(1, 7) as u32
}

/// WMO weather interpretation code → short speakable description
/// (per Open-Meteo docs, "Weather variable documentation").
#[allow(dead_code)] // consumed by forecast shaping (later task)
fn wmo_description(code: i64) -> &'static str {
    match code {
        0 => "Clear sky",
        1 => "Mainly clear",
        2 => "Partly cloudy",
        3 => "Overcast",
        45 | 48 => "Fog",
        51 => "Light drizzle",
        53 => "Drizzle",
        55 => "Heavy drizzle",
        56 | 57 => "Freezing drizzle",
        61 => "Light rain",
        63 => "Rain",
        65 => "Heavy rain",
        66 | 67 => "Freezing rain",
        71 => "Light snow",
        73 => "Snow",
        75 => "Heavy snow",
        77 => "Snow grains",
        80 => "Light rain showers",
        81 => "Rain showers",
        82 => "Violent rain showers",
        85 => "Snow showers",
        86 => "Heavy snow showers",
        95 => "Thunderstorm",
        96 | 99 => "Thunderstorm with hail",
        _ => "Unknown",
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wmo_codes_map_to_speakable_conditions() {
        assert_eq!(wmo_description(0), "Clear sky");
        assert_eq!(wmo_description(2), "Partly cloudy");
        assert_eq!(wmo_description(45), "Fog");
        assert_eq!(wmo_description(63), "Rain");
        assert_eq!(wmo_description(66), "Freezing rain");
        assert_eq!(wmo_description(75), "Heavy snow");
        assert_eq!(wmo_description(95), "Thunderstorm");
        assert_eq!(wmo_description(99), "Thunderstorm with hail");
        assert_eq!(wmo_description(42), "Unknown");
        assert_eq!(wmo_description(-1), "Unknown");
    }

    #[test]
    fn units_and_days_parse_and_clamp() {
        assert_eq!(units_from("imperial"), Units::Imperial);
        assert_eq!(units_from("metric"), Units::Metric);
        assert_eq!(units_from("bogus"), Units::Metric);
        assert_eq!(units_from_none(None), Units::Metric);
        assert_eq!(clamp_days(None), 1);
        assert_eq!(clamp_days(Some(3)), 3);
        assert_eq!(clamp_days(Some(0)), 1);
        assert_eq!(clamp_days(Some(99)), 7);
    }
}
