//! Built-in plugins: `time` (no network), `http_fetch` (allowlisted GET),
//! `weather` (Open-Meteo lookup), and `web_search` (Firecrawl search + fetch).

pub mod http_fetch;
pub mod time;
pub mod weather;
pub mod web_search;
