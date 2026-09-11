//! STT provider: OpenAI-compatible multipart transcription client.
//!
//! Wraps 16 kHz mono PCM16 into a canonical 44-byte-header WAV in memory,
//! uploads it as a multipart `file=audio.wav` part plus a `model` field to
//! `{base_url}{chat_path}` with Bearer auth, and parses the JSON response's
//! `text` field (plain OpenAI shape and verbose-JSON shape both carry it).
//! Empty/silent audio short-circuits to `""` without a network call.

use async_trait::async_trait;

use harness_core::config::SttConfig;
use harness_core::error::HarnessError;

/// Speech-to-text provider over 16 kHz mono PCM16.
#[async_trait]
pub trait SttProvider: Send + Sync {
    async fn transcribe(&self, pcm16k: &[i16]) -> Result<String, HarnessError>;
}

/// OpenAI-compatible STT client.
pub struct OpenAiSttClient {
    http: reqwest::Client,
    base_url: String,
    chat_path: String,
    api_key: String,
    model: String,
}

/// Wrap PCM16 mono samples into a minimal in-memory WAV
/// (44-byte canonical header + little-endian data).
pub fn wav_from_pcm(pcm: &[i16], sample_rate: u32) -> Vec<u8> {
    let mut w = Vec::with_capacity(44 + pcm.len() * 2);
    let data_len = pcm.len() * 2;
    let bits = 16u16;
    let channels = 1u16;
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&((36 + data_len) as u32).to_le_bytes());
    w.extend_from_slice(b"WAVE");
    w.extend_from_slice(b"fmt ");
    w.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
    w.extend_from_slice(&1u16.to_le_bytes()); // PCM
    w.extend_from_slice(&channels.to_le_bytes());
    w.extend_from_slice(&sample_rate.to_le_bytes());
    let byte_rate = sample_rate * channels as u32 * bits as u32 / 8;
    w.extend_from_slice(&byte_rate.to_le_bytes());
    w.extend_from_slice(&(channels * bits / 8).to_le_bytes()); // block align
    w.extend_from_slice(&bits.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&(data_len as u32).to_le_bytes());
    for s in pcm {
        w.extend_from_slice(&s.to_le_bytes());
    }
    w
}

/// Silence gate: below this peak amplitude the audio is treated as empty.
const SILENCE_EPSILON: i16 = 10;

/// Parse a transcription response into its text field.
/// Both `{"text": "..."}` and the verbose JSON shape carry `text` at top
/// level, so a single field read covers both.
fn parse_transcript(body: &str) -> Result<String, HarnessError> {
    let v: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| HarnessError::protocol(format!("bad stt response: {e}")))?;
    let text = v
        .get("text")
        .and_then(|t| t.as_str())
        .ok_or_else(|| HarnessError::protocol("stt response missing text field"))?;
    Ok(text.to_string())
}

impl OpenAiSttClient {
    pub fn new(
        base_url: impl Into<String>,
        chat_path: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            http: reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(10))
                .build()
                .expect("reqwest client builds"),
            base_url: base_url.into(),
            chat_path: chat_path.into(),
            api_key: api_key.into(),
            model: model.into(),
        }
    }

    pub fn from_config(cfg: &SttConfig) -> Self {
        Self::new(&cfg.base_url, &cfg.chat_path, &cfg.api_key, &cfg.model)
    }

    fn url(&self) -> String {
        format!("{}{}", self.base_url.trim_end_matches('/'), self.chat_path)
    }
}

#[async_trait]
impl SttProvider for OpenAiSttClient {
    async fn transcribe(&self, pcm16k: &[i16]) -> Result<String, HarnessError> {
        // Silent audio is a no-op turn — don't pay a network round trip.
        if pcm16k.iter().all(|s| s.abs() < SILENCE_EPSILON) {
            return Ok(String::new());
        }

        let wav = wav_from_pcm(pcm16k, 16_000);
        let file_part = reqwest::multipart::Part::bytes(wav)
            .file_name("audio.wav")
            .mime_str("audio/wav")
            .map_err(|e| HarnessError::protocol(format!("bad mime: {e}")))?;
        let form = reqwest::multipart::Form::new()
            .text("model", self.model.clone())
            .part("file", file_part);

        let mut req = self.http.post(self.url()).multipart(form);
        if !self.api_key.is_empty() {
            req = req.bearer_auth(&self.api_key);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| HarnessError::Network(format!("stt request failed: {e}")))?;
        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| HarnessError::Network(format!("stt body read failed: {e}")))?;
        if !status.is_success() {
            return Err(HarnessError::upstream("stt", status.as_u16(), body));
        }
        parse_transcript(&body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_header_is_canonical_44_bytes() {
        let wav = wav_from_pcm(&[100, -200, 300], 16_000);
        assert_eq!(wav.len(), 44 + 6, "44-byte header + 3 samples × 2 bytes");
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(&wav[12..16], b"fmt ");
        assert_eq!(&wav[36..40], b"data");
        // RIFF size = 36 + data_len
        assert_eq!(u32::from_le_bytes([wav[4], wav[5], wav[6], wav[7]]), 42);
        // PCM format, mono
        assert_eq!(u16::from_le_bytes([wav[20], wav[21]]), 1);
        assert_eq!(u16::from_le_bytes([wav[22], wav[23]]), 1);
        // sample rate
        assert_eq!(
            u32::from_le_bytes([wav[24], wav[25], wav[26], wav[27]]),
            16_000
        );
        // 16-bit
        assert_eq!(u16::from_le_bytes([wav[34], wav[35]]), 16);
        // data chunk size
        assert_eq!(u32::from_le_bytes([wav[40], wav[41], wav[42], wav[43]]), 6);
        // payload round-trips
        let payload = &wav[44..];
        assert_eq!([payload[0], payload[1]], 100i16.to_le_bytes());
        assert_eq!([payload[2], payload[3]], (-200i16).to_le_bytes());
        assert_eq!([payload[4], payload[5]], 300i16.to_le_bytes());
    }

    #[test]
    fn parse_openai_and_verbose_shapes() {
        let plain = parse_transcript(r#"{"text": "hi there"}"#).expect("plain parses");
        assert_eq!(plain, "hi there");
        let verbose = parse_transcript(
            r#"{"task":"transcribe","language":"en","duration":1.5,"text":"hello world"}"#,
        )
        .expect("verbose parses");
        assert_eq!(verbose, "hello world");
        let err = parse_transcript(r#"{"nope": true}"#).expect_err("must fail");
        assert!(err.to_string().contains("text"), "unexpected: {err}");
    }
}
