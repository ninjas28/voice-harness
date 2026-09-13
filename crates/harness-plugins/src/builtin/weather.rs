//! `weather` plugin: current conditions + forecast for any location via
//! Open-Meteo (keyless). Geocodes a place name, then fetches the forecast.

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::{Plugin, PluginManifest};

/// Official forecast endpoint (overridable via `[plugins.weather].api_base`).
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

    /// Resolve a location string against the Geocoding API, returning the
    /// first match's coordinates and labels.
    #[allow(dead_code)] // consumed by `call` orchestration (Task 6)
    async fn geocode(&self, location: &str) -> Result<GeoResult, String> {
        let client = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .build()
            .map_err(|e| format!("http client build failed: {e}"))?;
        let resp = client
            .get(self.geocode_url())
            .query(&[
                ("name", location),
                ("count", "5"),
                ("language", "en"),
                ("format", "json"),
            ])
            .send()
            .await
            .map_err(|e| format!("geocoding request failed: {e}"))?;
        let status = resp.status();
        let body = resp
            .bytes()
            .await
            .map_err(|e| format!("geocoding body read failed: {e}"))?;
        if body.len() > MAX_BODY_BYTES {
            return Err(format!("geocoding response exceeds {MAX_BODY_BYTES} bytes"));
        }
        let body: Value = serde_json::from_slice(&body)
            .map_err(|e| format!("geocoding response not JSON: {e}"))?;
        if !status.is_success() {
            let reason = body
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("unknown reason");
            return Err(format!("geocoding failed (HTTP {status}): {reason}"));
        }
        if body.get("error").and_then(Value::as_bool) == Some(true) {
            let reason = body
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("unknown reason");
            return Err(format!("geocoding failed: {reason}"));
        }
        let first = body
            .get("results")
            .and_then(Value::as_array)
            .and_then(|r| r.first());
        let Some(first) = first else {
            return Err(format!("no location found for '{location}'"));
        };
        Ok(GeoResult {
            name: first
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or(location)
                .to_string(),
            admin1: first
                .get("admin1")
                .and_then(Value::as_str)
                .map(str::to_string),
            country: first
                .get("country")
                .and_then(Value::as_str)
                .map(str::to_string),
            timezone: first
                .get("timezone")
                .and_then(Value::as_str)
                .map(str::to_string),
            latitude: first
                .get("latitude")
                .and_then(Value::as_f64)
                .ok_or("geocoding result missing latitude")?,
            longitude: first
                .get("longitude")
                .and_then(Value::as_f64)
                .ok_or("geocoding result missing longitude")?,
        })
    }
}

/// One geocoded place (first Open-Meteo search result).
#[derive(Debug)]
#[allow(dead_code)] // consumed by the forecast fetch (Task 5)
struct GeoResult {
    name: String,
    admin1: Option<String>,
    country: Option<String>,
    timezone: Option<String>,
    latitude: f64,
    longitude: f64,
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

    const GEOCODE_HIT: &str = r#"{
        "results": [{"id": 2950159, "name": "Paris", "latitude": 48.85341,
                     "longitude": 2.3488, "country": "France",
                     "admin1": "Île-de-France", "timezone": "Europe/Paris"}],
        "generationtime_ms": 1.2
    }"#;

    #[tokio::test]
    async fn geocode_resolves_first_result() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::query_param("name", "Paris"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_string(GEOCODE_HIT))
            .mount(&server)
            .await;

        let p = WeatherPlugin::new(String::new()).with_endpoints(String::new(), server.uri());
        let geo = p.geocode("Paris").await.expect("resolved");
        assert_eq!(geo.name, "Paris");
        assert_eq!(geo.country.as_deref(), Some("France"));
        assert_eq!(geo.admin1.as_deref(), Some("Île-de-France"));
        assert_eq!(geo.timezone.as_deref(), Some("Europe/Paris"));
        assert!((geo.latitude - 48.85341).abs() < 1e-9);
        assert!((geo.longitude - 2.3488).abs() < 1e-9);
    }

    #[tokio::test]
    async fn geocode_no_results_is_a_recoverable_error() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_string(r#"{"generationtime_ms": 0.5}"#),
            )
            .mount(&server)
            .await;

        let p = WeatherPlugin::new(String::new()).with_endpoints(String::new(), server.uri());
        let err = p.geocode("Nowhereville").await.expect_err("must fail");
        assert!(
            err.contains("no location found for 'Nowhereville'"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn geocode_upstream_error_surfaces_the_reason() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(
                wiremock::ResponseTemplate::new(400)
                    .set_body_string(r#"{"error": true, "reason": "Invalid name"}"#),
            )
            .mount(&server)
            .await;

        let p = WeatherPlugin::new(String::new()).with_endpoints(String::new(), server.uri());
        let err = p.geocode("X").await.expect_err("must fail");
        assert!(err.contains("400") && err.contains("Invalid name"), "{err}");
    }
}
