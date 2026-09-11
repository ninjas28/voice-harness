//! `time` plugin: current date/time per timezone — no network, no chrono.

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::{Plugin, PluginManifest};

/// Reports the current datetime (ISO-8601) for a requested IANA timezone.
pub struct TimePlugin;

/// Days from 1970-01-01 to the civil date containing `days` (Howard
/// Hinnant's `civil_from_days` algorithm) → (year, month, day).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Format a fixed-offset datetime from a Unix timestamp.
fn format_offset(epoch_secs: i64, offset_secs: i64, tz_label: &str) -> Value {
    let local = epoch_secs + offset_secs;
    let days = local.div_euclid(86_400);
    let secs_of_day = local.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let (h, m, s) = (
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60,
    );
    let offset_str = if offset_secs == 0 {
        "Z".to_string()
    } else {
        let sign = if offset_secs < 0 { '-' } else { '+' };
        let abs = offset_secs.abs();
        format!("{sign}{:02}:{:02}", abs / 3600, (abs % 3600) / 60)
    };
    json!({
        "datetime": format!("{year:04}-{month:02}-{day:02}T{h:02}:{m:02}:{s:02}{offset_str}"),
        "date": format!("{year:04}-{month:02}-{day:02}"),
        "time": format!("{h:02}:{m:02}:{s:02}"),
        "timezone": tz_label,
    })
}

/// Resolve a timezone argument to a fixed UTC offset (seconds).
/// Supports "UTC", fixed offsets like "+05:30"/"-08:00", and a small set of
/// common US/EU zone names. Anything else → local system offset.
fn tz_offset_secs(tz: &str) -> Option<i64> {
    let tz = tz.trim();
    if tz.is_empty() || tz.eq_ignore_ascii_case("local") {
        return None; // caller falls back to the system offset
    }
    if tz.eq_ignore_ascii_case("utc") || tz.eq_ignore_ascii_case("z") {
        return Some(0);
    }
    // Fixed offset: +HH:MM / -HH:MM (also tolerate +HHMM / +HH).
    let bytes = tz.as_bytes();
    if (bytes[0] == b'+' || bytes[0] == b'-') && tz.len() >= 3 {
        let sign = if bytes[0] == b'-' { -1 } else { 1 };
        let digits: String = tz[1..].chars().filter(|c| c.is_ascii_digit()).collect();
        if digits.len() >= 2 {
            let h: i64 = digits[..2].parse().ok()?;
            let m: i64 = if digits.len() >= 4 {
                digits[2..4].parse().ok()?
            } else {
                0
            };
            if h <= 23 && m <= 59 {
                return Some(sign * (h * 3600 + m * 60));
            }
        }
        return None;
    }
    match tz.to_ascii_lowercase().as_str() {
        "america/los_angeles" | "us/pacific" | "pacific" => Some(-8 * 3600),
        "america/denver" | "us/mountain" | "mountain" => Some(-7 * 3600),
        "america/chicago" | "us/central" | "central" => Some(-6 * 3600),
        "america/new_york" | "us/eastern" | "eastern" => Some(-5 * 3600),
        "europe/london" => Some(0),
        "europe/paris" | "europe/berlin" => Some(3600),
        "asia/tokyo" => Some(9 * 3600),
        "asia/shanghai" => Some(8 * 3600),
        "asia/kolkata" => Some(5 * 3600 + 1800),
        "australia/sydney" => Some(10 * 3600),
        _ => None,
    }
}

/// System-local UTC offset (std has no zoneinfo; try `date +%z` via env
/// fallback) — parses `TZ` if set, else 0.
fn local_offset_secs() -> i64 {
    // std offers no timezone database. Use the `TZ` env var when it carries
    // an explicit offset (libc form like "PST+8"), else assume UTC and let
    // the LLM/user pass an explicit tz for correctness.
    match std::env::var("TZ") {
        Ok(tz) => {
            // libc TZ form: "PST+8" / "IST-5:30" — offset is inverted.
            if let Some(pos) = tz.find(['+', '-']) {
                let raw = tz[pos..].to_string();
                if let Some(off) = tz_offset_secs(&raw) {
                    return -off;
                }
            }
            0
        }
        Err(_) => 0,
    }
}

impl TimePlugin {
    fn now_value(&self, tz_arg: Option<&str>) -> Value {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let epoch = now.as_secs() as i64;
        match tz_arg.map(str::to_string).map(|t| tz_offset_secs(&t)) {
            Some(Some(off)) => format_offset(epoch, off, tz_arg.unwrap_or("UTC")),
            _ => format_offset(epoch, local_offset_secs(), tz_arg.unwrap_or("local")),
        }
    }
}

#[async_trait]
impl Plugin for TimePlugin {
    fn manifest(&self) -> &PluginManifest {
        static MANIFEST: std::sync::OnceLock<PluginManifest> = std::sync::OnceLock::new();
        MANIFEST.get_or_init(|| PluginManifest {
            name: "time",
            version: "0.1.0",
            description: "Current date and time, optionally per timezone",
        })
    }

    fn tool_specs(&self) -> Vec<Value> {
        vec![json!({
            "type": "function",
            "function": {
                "name": "time.get_time",
                "description": "Get the current date and time. Pass an IANA timezone name \
                               (e.g. \"America/Los_Angeles\") or a fixed offset (\"+05:30\"); \
                               omit for the server's local time.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "tz": { "type": "string", "description": "IANA timezone or fixed offset" }
                    },
                    "required": []
                }
            }
        })]
    }

    async fn call(&self, name: &str, arguments: Value) -> Result<Value, String> {
        if name != "get_time" {
            return Err(format!("time plugin has no tool '{name}'"));
        }
        let tz = arguments
            .get("tz")
            .and_then(|v| v.as_str())
            .or_else(|| arguments.get("timezone").and_then(|v| v.as_str()));
        Ok(self.now_value(tz))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_epoch_reference() {
        // Verified via `date -u -r <days*86400>`.
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
    }

    #[test]
    fn offset_formatting() {
        let v = format_offset(0, 0, "UTC");
        assert_eq!(v["datetime"], "1970-01-01T00:00:00Z");
        let v = format_offset(951_782_400, 0, "UTC");
        assert_eq!(v["datetime"], "2000-02-29T00:00:00Z");
        let v = format_offset(951_782_400, 5 * 3600 + 1800, "+05:30");
        assert_eq!(v["datetime"], "2000-02-29T05:30:00+05:30");
        let v = format_offset(951_782_400, -8 * 3600, "PST");
        assert_eq!(v["date"], "2000-02-28");
        assert_eq!(v["time"], "16:00:00");
    }

    #[test]
    fn tz_parsing() {
        assert_eq!(tz_offset_secs("UTC"), Some(0));
        assert_eq!(tz_offset_secs("+05:30"), Some(5 * 3600 + 1800));
        assert_eq!(tz_offset_secs("-08:00"), Some(-8 * 3600));
        assert_eq!(tz_offset_secs("America/Los_Angeles"), Some(-8 * 3600));
        assert_eq!(tz_offset_secs("Mars/Olympus"), None);
        assert_eq!(tz_offset_secs(""), None);
    }

    #[tokio::test]
    async fn call_returns_datetime_shape() {
        let p = TimePlugin;
        let out = p
            .call("get_time", json!({ "tz": "UTC" }))
            .await
            .expect("ok");
        assert!(out["datetime"].as_str().expect("str").contains('T'));
        assert_eq!(out["timezone"], "UTC");

        let err = p.call("nope", json!({})).await.expect_err("unknown tool");
        assert!(err.contains("nope"));
    }
}
