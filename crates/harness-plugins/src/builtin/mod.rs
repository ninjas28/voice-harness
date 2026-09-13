//! Built-in plugins: `time` (no network), `http_fetch` (allowlisted GET), and
//! `weather` (Open-Meteo lookup).

pub mod http_fetch;
pub mod time;
pub mod weather;
