//! `weather` plugin: current conditions + forecast for any location via
//! Open-Meteo (keyless). Geocodes a place name, then fetches the forecast.

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::{Plugin, PluginManifest};

/// Official forecast endpoint (overridable via `[plugins.weather].api_base`).
const DEFAULT_FORECAST_URL: &str = "https://api.open-meteo.com/v1/forecast";
/// Official geocoding endpoint (overridable via `[plugins.weather].geocoding_base`).
const DEFAULT_GEOCODE_URL: &str = "https://geocoding-api.open-meteo.com/v1/search";
/// Request timeout per upstream call (plan).
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
/// Response body cap: 64 KiB (plan).
const MAX_BODY_BYTES: usize = 64 * 1024;

/// Allowlist-free weather lookup. The upstream host is fixed (or configured);
/// only the LLM-supplied location name travels as a query parameter.
pub struct WeatherPlugin {
    api_base: String,
    geocoding_base: String,
    /// Location used when the LLM omits one.
    default_location: String,
    /// Unit system when the LLM omits `units` (None = metric).
    default_units: Option<Units>,
}

impl WeatherPlugin {
    pub fn new(default_location: String) -> Self {
        Self {
            api_base: String::new(),
            geocoding_base: String::new(),
            default_location,
            default_units: None,
        }
    }

    /// Endpoint overrides for tests and self-hosted instances.
    /// Order: (forecast api_base, geocoding_base). Empty = official URL.
    pub fn with_endpoints(mut self, api_base: String, geocoding_base: String) -> Self {
        self.api_base = api_base;
        self.geocoding_base = geocoding_base;
        self
    }

    /// Default unit system from `[plugins.weather].units`
    /// ("" or anything unrecognized = metric).
    pub fn with_default_units(mut self, units: &str) -> Self {
        self.default_units = Some(units_from(units));
        self
    }

    /// Endpoint override: forecast URL. Empty = official URL.
    fn forecast_url(&self) -> String {
        if self.api_base.trim().is_empty() {
            DEFAULT_FORECAST_URL.to_string()
        } else {
            self.api_base.trim().to_string()
        }
    }

    fn geocode_url(&self) -> String {
        if self.geocoding_base.trim().is_empty() {
            DEFAULT_GEOCODE_URL.to_string()
        } else {
            self.geocoding_base.trim().to_string()
        }
    }

    /// Resolve a location string against the Geocoding API, returning the
    /// first match's coordinates and labels.
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

    /// Request current conditions + daily forecast for geocoded coordinates.
    async fn fetch_forecast(
        &self,
        geo: &GeoResult,
        days: u32,
        units: Units,
    ) -> Result<Value, String> {
        // Open-Meteo rejects `millimeter` with a 400 ("Cannot initialize
        // PrecipitationUnit from invalid String value"); `mm` is accepted.
        let (temp_unit, wind_unit, precip_unit) = match units {
            Units::Metric => ("celsius", "kmh", "mm"),
            Units::Imperial => ("fahrenheit", "mph", "inch"),
        };
        let client = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .build()
            .map_err(|e| format!("http client build failed: {e}"))?;
        let resp = client
            .get(self.forecast_url())
            .query(&[
                ("latitude", format!("{}", geo.latitude)),
                ("longitude", format!("{}", geo.longitude)),
                (
                    "current",
                    "temperature_2m,apparent_temperature,relative_humidity_2m,is_day,weather_code,wind_speed_10m"
                        .to_string(),
                ),
                (
                    "daily",
                    "weather_code,temperature_2m_max,temperature_2m_min,precipitation_probability_max"
                        .to_string(),
                ),
                ("timezone", "auto".to_string()),
                ("forecast_days", days.to_string()),
                ("temperature_unit", temp_unit.to_string()),
                ("wind_speed_unit", wind_unit.to_string()),
                ("precipitation_unit", precip_unit.to_string()),
            ])
            .send()
            .await
            .map_err(|e| format!("forecast request failed: {e}"))?;
        let status = resp.status();
        let body = resp
            .bytes()
            .await
            .map_err(|e| format!("forecast body read failed: {e}"))?;
        if body.len() > MAX_BODY_BYTES {
            return Err(format!("forecast response exceeds {MAX_BODY_BYTES} bytes"));
        }
        let body: Value = serde_json::from_slice(&body)
            .map_err(|e| format!("forecast response not JSON: {e}"))?;
        if body.get("error").and_then(Value::as_bool) == Some(true) {
            let reason = body
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("unknown reason");
            return Err(format!("forecast failed (HTTP {status}): {reason}"));
        }
        if !status.is_success() {
            return Err(format!("forecast failed (HTTP {status})"));
        }
        Ok(shape_forecast(geo, &body, units))
    }
}

/// Shape the Open-Meteo forecast response into the compact LLM-facing JSON.
fn shape_forecast(geo: &GeoResult, body: &Value, units: Units) -> Value {
    let (temp_name, wind_name, precip_name) = match units {
        Units::Metric => ("degrees Celsius", "kilometers per hour", "millimeters"),
        Units::Imperial => ("degrees Fahrenheit", "miles per hour", "inches"),
    };
    let cur = body.get("current").cloned().unwrap_or(Value::Null);
    let daily = body.get("daily").cloned().unwrap_or(Value::Null);

    let arr = |obj: &Value, key: &str| -> Vec<Value> {
        obj.get(key)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    };
    let times = arr(&daily, "time");
    let codes = arr(&daily, "weather_code");
    let highs = arr(&daily, "temperature_2m_max");
    let lows = arr(&daily, "temperature_2m_min");
    let probs = arr(&daily, "precipitation_probability_max");

    let days: Vec<Value> = times
        .iter()
        .enumerate()
        .map(|(i, t)| {
            json!({
                "date": t,
                "high": highs.get(i),
                "low": lows.get(i),
                "condition": codes.get(i).and_then(Value::as_i64).map(wmo_description).unwrap_or("Unknown"),
                "precip_probability_pct": probs.get(i),
            })
        })
        .collect();

    json!({
        "location": {
            "name": geo.name,
            "admin1": geo.admin1,
            "country": geo.country,
            "timezone": geo.timezone,
            "latitude": geo.latitude,
            "longitude": geo.longitude,
        },
        "units": {
            "temperature": temp_name,
            "wind": wind_name,
            "precipitation": precip_name,
        },
        "current": {
            "time": cur.get("time"),
            "temperature": cur.get("temperature_2m"),
            "apparent": cur.get("apparent_temperature"),
            "condition": cur.get("weather_code").and_then(Value::as_i64).map(wmo_description).unwrap_or("Unknown"),
            "wind": cur.get("wind_speed_10m"),
            "humidity_pct": cur.get("relative_humidity_2m"),
            "is_day": cur.get("is_day").and_then(Value::as_i64).map(|v| v != 0),
        },
        "today": days.first().cloned().unwrap_or(Value::Null),
        "days": days,
    })
}

/// One geocoded place (first Open-Meteo search result).
#[derive(Debug)]
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
enum Units {
    Metric,
    Imperial,
}

fn units_from(arg: &str) -> Units {
    if arg.eq_ignore_ascii_case("imperial") {
        Units::Imperial
    } else {
        Units::Metric
    }
}

/// Units for a call: an explicit `units` argument always wins; otherwise the
/// configured default; otherwise metric.
fn units_for(arg: Option<&str>, configured: Option<Units>) -> Units {
    match arg {
        Some(a) => units_from(a),
        None => configured.unwrap_or(Units::Metric),
    }
}

/// Forecast days including today, clamped to the API's 1..=7 range.
fn clamp_days(arg: Option<u64>) -> u32 {
    arg.unwrap_or(1).clamp(1, 7) as u32
}

/// WMO weather interpretation code → short speakable description
/// (per Open-Meteo docs, "Weather variable documentation").
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
                        "units": { "type": "string", "enum": ["metric", "imperial"], "description": "Unit system; omit for the configured default" }
                    },
                    "required": []
                }
            }
        })]
    }

    async fn call(&self, name: &str, arguments: Value) -> Result<Value, String> {
        if name != "get_forecast" {
            return Err(format!("weather plugin has no tool '{name}'"));
        }
        let location = arguments
            .get("location")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| {
                let default = self.default_location.trim();
                if default.is_empty() {
                    None
                } else {
                    Some(default.to_string())
                }
            });
        let Some(location) = location else {
            return Err("no location given and no default location is configured; \
                 ask the user which place they want weather for"
                .to_string());
        };
        let days = clamp_days(arguments.get("days").and_then(Value::as_u64));
        let units = units_for(
            arguments.get("units").and_then(Value::as_str),
            self.default_units,
        );

        let geo = self.geocode(&location).await?;
        self.fetch_forecast(&geo, days, units).await
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
        assert_eq!(clamp_days(None), 1);
        assert_eq!(clamp_days(Some(3)), 3);
        assert_eq!(clamp_days(Some(0)), 1);
        assert_eq!(clamp_days(Some(99)), 7);
    }

    #[test]
    fn units_resolution_argument_wins_over_configured_default() {
        // No argument, no configured default → metric.
        assert_eq!(units_for(None, None), Units::Metric);
        // No argument, configured default → the default.
        assert_eq!(units_for(None, Some(Units::Imperial)), Units::Imperial);
        // Explicit argument wins over the configured default.
        assert_eq!(
            units_for(Some("metric"), Some(Units::Imperial)),
            Units::Metric
        );
        assert_eq!(
            units_for(Some("imperial"), Some(Units::Metric)),
            Units::Imperial
        );
        // Unrecognized argument falls back to metric, even with a configured
        // imperial default (same tolerance as `units_from`).
        assert_eq!(
            units_for(Some("bogus"), Some(Units::Imperial)),
            Units::Metric
        );
    }

    #[tokio::test]
    async fn call_uses_configured_default_units_when_argument_omitted() {
        let (geo, api) = setup_mock_pair().await;
        let p = WeatherPlugin::new(String::new())
            .with_endpoints(api.uri(), geo.uri())
            .with_default_units("imperial");
        let out = p
            .call("get_forecast", json!({ "location": "Paris" }))
            .await
            .expect("ok");
        assert_eq!(out["units"]["temperature"], "degrees Fahrenheit");
        let requests = api.received_requests().await.expect("requests");
        let url = requests[0].url.to_string();
        assert!(url.contains("temperature_unit=fahrenheit"), "{url}");
    }

    #[tokio::test]
    async fn call_argument_units_override_configured_default() {
        let (geo, api) = setup_mock_pair().await;
        let p = WeatherPlugin::new(String::new())
            .with_endpoints(api.uri(), geo.uri())
            .with_default_units("imperial");
        let out = p
            .call(
                "get_forecast",
                json!({ "location": "Paris", "units": "metric" }),
            )
            .await
            .expect("ok");
        assert_eq!(out["units"]["temperature"], "degrees Celsius");
    }

    #[tokio::test]
    async fn call_empty_configured_units_defaults_to_metric() {
        let (geo, api) = setup_mock_pair().await;
        let p = WeatherPlugin::new(String::new())
            .with_endpoints(api.uri(), geo.uri())
            .with_default_units("");
        let out = p
            .call("get_forecast", json!({ "location": "Paris" }))
            .await
            .expect("ok");
        assert_eq!(out["units"]["temperature"], "degrees Celsius");
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

    const FORECAST_JSON: &str = r#"{
        "timezone": "Europe/Paris",
        "current": {"time": "2026-09-12T14:30", "temperature_2m": 22.4,
                    "apparent_temperature": 23.1, "relative_humidity_2m": 58,
                    "is_day": 1, "weather_code": 2, "wind_speed_10m": 11.2},
        "daily": {"time": ["2026-09-12", "2026-09-13"], "weather_code": [2, 3],
                  "temperature_2m_max": [24.1, 25.0],
                  "temperature_2m_min": [15.3, 16.0],
                  "precipitation_probability_max": [10, 40]}
    }"#;

    fn geo_paris() -> GeoResult {
        GeoResult {
            name: "Paris".to_string(),
            admin1: Some("Île-de-France".to_string()),
            country: Some("France".to_string()),
            timezone: Some("Europe/Paris".to_string()),
            latitude: 48.85341,
            longitude: 2.3488,
        }
    }

    #[tokio::test]
    async fn forecast_fetches_and_shapes_compact_json() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::query_param("latitude", "48.85341"))
            .and(wiremock::matchers::query_param("longitude", "2.3488"))
            .and(wiremock::matchers::query_param("forecast_days", "2"))
            .and(wiremock::matchers::query_param("timezone", "auto"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_string(FORECAST_JSON))
            .mount(&server)
            .await;

        let p = WeatherPlugin::new(String::new()).with_endpoints(server.uri(), String::new());
        let out = p
            .fetch_forecast(&geo_paris(), 2, Units::Metric)
            .await
            .expect("shaped");
        assert_eq!(out["location"]["name"], "Paris");
        assert_eq!(out["location"]["country"], "France");
        assert_eq!(out["location"]["timezone"], "Europe/Paris");
        assert_eq!(out["units"]["temperature"], "degrees Celsius");
        assert_eq!(out["current"]["temperature"], 22.4);
        assert_eq!(out["current"]["condition"], "Partly cloudy");
        assert_eq!(out["current"]["is_day"], true);
        assert_eq!(out["today"]["high"], 24.1);
        assert_eq!(out["today"]["precip_probability_pct"], 10);
        assert_eq!(out["days"].as_array().map(Vec::len), Some(2));
        assert_eq!(out["days"][1]["condition"], "Overcast");
    }

    /// Regression: Open-Meteo rejects `precipitation_unit=millimeter` with a
    /// 400 ("Cannot initialize PrecipitationUnit from invalid String value"),
    /// so the metric request must send the accepted `mm` spelling.
    #[tokio::test]
    async fn forecast_metric_precipitation_unit_is_mm() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_string(FORECAST_JSON))
            .mount(&server)
            .await;

        let p = WeatherPlugin::new(String::new()).with_endpoints(server.uri(), String::new());
        p.fetch_forecast(&geo_paris(), 1, Units::Metric)
            .await
            .expect("shaped");
        let requests = server.received_requests().await.expect("requests");
        let url = requests[0].url.to_string();
        assert!(
            url.contains("precipitation_unit=mm"),
            "metric forecast must send precipitation_unit=mm: {url}"
        );
        assert!(
            !url.contains("precipitation_unit=millimeter"),
            "upstream rejects 'millimeter': {url}"
        );
        // The LLM-facing label still speaks in millimeters.
        assert!(url.contains("temperature_unit=celsius"), "{url}");
    }

    #[tokio::test]
    async fn forecast_imperial_units_pass_through() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_string(FORECAST_JSON))
            .mount(&server)
            .await;

        let p = WeatherPlugin::new(String::new()).with_endpoints(server.uri(), String::new());
        let out = p
            .fetch_forecast(&geo_paris(), 1, Units::Imperial)
            .await
            .expect("shaped");
        assert_eq!(out["units"]["temperature"], "degrees Fahrenheit");
        let requests = server.received_requests().await.expect("requests");
        let url = requests[0].url.to_string();
        assert!(url.contains("temperature_unit=fahrenheit"), "{url}");
        assert!(url.contains("wind_speed_unit=mph"), "{url}");
        assert!(url.contains("precipitation_unit=inch"), "{url}");
    }

    #[tokio::test]
    async fn forecast_upstream_error_surfaces_the_reason() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(wiremock::ResponseTemplate::new(400).set_body_string(
                r#"{"error": true, "reason": "Latitude must be between -90 and 90"}"#,
            ))
            .mount(&server)
            .await;

        let p = WeatherPlugin::new(String::new()).with_endpoints(server.uri(), String::new());
        let err = p
            .fetch_forecast(&geo_paris(), 1, Units::Metric)
            .await
            .expect_err("must fail");
        assert!(err.contains("Latitude must be"), "{err}");
    }

    /// Both wiremock servers: geocode + forecast.
    async fn setup_mock_pair() -> (wiremock::MockServer, wiremock::MockServer) {
        let geo = wiremock::MockServer::start().await;
        let api = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_string(GEOCODE_HIT))
            .mount(&geo)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_string(FORECAST_JSON))
            .mount(&api)
            .await;
        (geo, api)
    }

    #[tokio::test]
    async fn call_end_to_end_with_location() {
        let (geo, api) = setup_mock_pair().await;
        let p = WeatherPlugin::new(String::new()).with_endpoints(api.uri(), geo.uri());
        let out = p
            .call("get_forecast", json!({ "location": "Paris", "days": 2 }))
            .await
            .expect("ok");
        assert_eq!(out["location"]["name"], "Paris");
        assert_eq!(out["current"]["temperature"], 22.4);
    }

    #[tokio::test]
    async fn call_without_location_uses_default_location() {
        let (geo, api) = setup_mock_pair().await;
        let p = WeatherPlugin::new("Paris".to_string()).with_endpoints(api.uri(), geo.uri());
        let out = p.call("get_forecast", json!({})).await.expect("ok");
        assert_eq!(out["location"]["name"], "Paris");
        // The geocoder must have received the default location.
        let requests = geo.received_requests().await.expect("requests");
        let url = requests[0].url.to_string();
        assert!(url.contains("name=Paris"), "{url}");
    }

    #[tokio::test]
    async fn call_without_location_or_default_is_a_recoverable_error() {
        let (geo, api) = setup_mock_pair().await;
        let p = WeatherPlugin::new(String::new()).with_endpoints(api.uri(), geo.uri());
        let err = p
            .call("get_forecast", json!({}))
            .await
            .expect_err("must fail");
        assert!(err.contains("no location") && err.contains("ask"), "{err}");
        // Neither upstream may have been contacted.
        assert!(geo.received_requests().await.expect("geo").is_empty());
        assert!(api.received_requests().await.expect("api").is_empty());
    }

    #[tokio::test]
    async fn call_days_out_of_range_is_clamped() {
        let (geo, api) = setup_mock_pair().await;
        let p = WeatherPlugin::new(String::new()).with_endpoints(api.uri(), geo.uri());
        p.call("get_forecast", json!({ "location": "Paris", "days": 99 }))
            .await
            .expect("ok");
        let requests = api.received_requests().await.expect("requests");
        let url = requests[0].url.to_string();
        assert!(url.contains("forecast_days=7"), "{url}");
    }
}
