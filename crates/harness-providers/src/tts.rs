//! TTS provider: OpenAI-compatible `/v1/audio/speech` client.
//!
//! [`TtsProvider::synthesize`] returns **16 kHz mono PCM16** bytes regardless
//! of what the upstream sends: a `RIFF`-tagged response is parsed as WAV
//! (16/8-bit PCM or 32-bit float, any channel count), downmixed to mono and
//! linearly resampled to 16 kHz ([`resample_to_16k`]); a raw (non-RIFF) body
//! is assumed to already be 16 kHz mono PCM16 and passes through untouched.

use async_trait::async_trait;

use harness_core::config::TtsConfig;
use harness_core::error::HarnessError;

/// Text-to-speech provider producing normalized 16 kHz mono PCM16 audio.
#[async_trait]
pub trait TtsProvider: Send + Sync {
    async fn synthesize(&self, text: &str) -> Result<Vec<u8>, HarnessError>;
}

/// OpenAI-compatible TTS client.
pub struct OpenAiTtsClient {
    http: reqwest::Client,
    base_url: String,
    speech_path: String,
    api_key: String,
    model: String,
    voice: String,
    response_format: String,
    /// Sample rate to assume for raw (non-WAV) response bodies; 16 kHz means
    /// pass-through. WAV responses carry their rate in the header instead.
    raw_sample_rate: u32,
}

/// Overall per-request bound: connect (10 s) + status + full body read. A TTS
/// call that exceeds this is a stalled upstream — the turn must error instead
/// of hanging. Without a read timeout a hung body read froze the turn task
/// and the end-of-turn `turn.completed` burst never went out (clients stuck
/// in `speaking`; see the stalled-turn lesson in AGENTS.md).
const DEFAULT_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// One parsed fmt chunk plus its body length, from the WAV chunk walk.
type ParsedFmt<'a> = (&'a [u8], usize);

/// Parse a WAV file into mono PCM16 samples plus its sample rate.
pub fn wav_to_pcm16(wav: &[u8]) -> Result<(Vec<i16>, u32), HarnessError> {
    if wav.len() < 12 || &wav[0..4] != b"RIFF" || &wav[8..12] != b"WAVE" {
        return Err(HarnessError::protocol("not a RIFF/WAVE file"));
    }

    // Walk chunks; the 44-byte canonical layout is fmt(16) + data, but real
    // encoders emit extra chunks (LIST, fact, PEAK) — chunk-walking survives.
    let mut i = 12usize;
    let (mut fmt, mut data): (Option<ParsedFmt<'_>>, Option<&[u8]>) = (None, None);
    while i + 8 <= wav.len() {
        let id = &wav[i..i + 4];
        let len = u32::from_le_bytes([wav[i + 4], wav[i + 5], wav[i + 6], wav[i + 7]]) as usize;
        let body_end = (i + 8 + len).min(wav.len()); // tolerate a short data chunk
        let body = &wav[i + 8..body_end];
        if id == b"fmt " {
            fmt = Some((body, body_end - (i + 8)));
        } else if id == b"data" {
            data = Some(body);
        }
        i = i + 8 + len + (len & 1); // chunks are word-aligned
    }

    let Some((fmt_body, fmt_len)) = fmt else {
        return Err(HarnessError::protocol("wav missing fmt chunk"));
    };
    if fmt_len < 16 {
        return Err(HarnessError::protocol("wav fmt chunk too short"));
    }
    let Some(data) = data else {
        return Err(HarnessError::protocol("wav missing data chunk"));
    };

    let audio_format = u16::from_le_bytes([fmt_body[0], fmt_body[1]]);
    let channels = u16::from_le_bytes([fmt_body[2], fmt_body[3]]) as usize;
    let sample_rate = u32::from_le_bytes([fmt_body[4], fmt_body[5], fmt_body[6], fmt_body[7]]);
    let bits = u16::from_le_bytes([fmt_body[14], fmt_body[15]]) as usize;
    if channels == 0 {
        return Err(HarnessError::protocol("wav with zero channels"));
    }

    // Convert whatever the encoder gave us into mono f64 samples.
    let mono: Vec<f64> = match (audio_format, bits) {
        (1, 16) => data
            .chunks_exact(2 * channels)
            .map(|frame| frame_average(frame, 2, |b| i16::from_le_bytes([b[0], b[1]]) as f64))
            .collect(),
        (1, 8) => data
            .chunks_exact(channels)
            .map(|frame| frame_average(frame, 1, |b| (b[0] as f64 - 128.0) / 128.0))
            .collect(),
        (3, 32) => data
            .chunks_exact(4 * channels)
            .map(|frame| {
                frame_average(frame, 4, |b| {
                    f32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f64
                })
            })
            .collect(),
        (1, 24) => data
            .chunks_exact(3 * channels)
            .map(|frame| {
                frame_average(frame, 3, |b| {
                    let v = i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8; // sign-extend
                    v as f64
                })
            })
            .collect(),
        _ => {
            return Err(HarnessError::protocol(format!(
                "unsupported wav format {audio_format}/{bits}-bit"
            )))
        }
    };

    // Scale to i16 by source depth (int formats keep their native range,
    // 32-bit float is normalized to ±1.0).
    let scale: f64 = match (audio_format, bits) {
        (1, 8) => 32768.0 / 128.0,
        (1, 24) => 32768.0 / 8388608.0,
        (3, 32) => 32768.0,
        _ => 1.0,
    };
    let samples: Vec<i16> = mono
        .into_iter()
        .map(|v| (v * scale).clamp(i16::MIN as f64, i16::MAX as f64) as i16)
        .collect();

    Ok((samples, sample_rate))
}

/// Average one multichannel frame channel-by-channel into a mono value.
fn frame_average(frame: &[u8], width: usize, read: impl Fn(&[u8]) -> f64) -> f64 {
    let n = frame.len() / width;
    frame.chunks_exact(width).map(read).sum::<f64>() / n.max(1) as f64
}

/// Linearly resample PCM16 samples to 16 kHz (identity when already 16 kHz).
pub fn resample_to_16k(samples: &[i16], from_rate: u32) -> Vec<i16> {
    if from_rate == 16_000 || samples.is_empty() {
        return samples.to_vec();
    }
    let out_len = samples.len() as u64 * 16_000 / from_rate as u64;
    let mut out = Vec::with_capacity(out_len as usize);
    let step = from_rate as f64 / 16_000.0;
    for k in 0..out_len {
        let t = k as f64 * step;
        let i = t.floor() as usize;
        let frac = t - i as f64;
        let s0 = samples[i];
        let s1 = samples[(i + 1).min(samples.len() - 1)];
        out.push((s0 as f64 + (s1 - s0) as f64 * frac) as i16);
    }
    out
}

/// Normalize an upstream response body to 16 kHz mono PCM16 little-endian
/// bytes. WAV responses are parsed and converted per their header; raw
/// bodies are treated as mono PCM16 at `raw_rate` (16 kHz → pass-through).
pub fn normalize_to_16k_mono(bytes: &[u8], raw_rate: u32) -> Result<Vec<u8>, HarnessError> {
    if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WAVE" {
        let (samples, rate) = wav_to_pcm16(bytes)?;
        let out = resample_to_16k(&samples, rate);
        return Ok(out.iter().flat_map(|s| s.to_le_bytes()).collect());
    }
    let samples: Vec<i16> = bytes
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect();
    let out = resample_to_16k(&samples, raw_rate);
    Ok(out.iter().flat_map(|s| s.to_le_bytes()).collect())
}

impl OpenAiTtsClient {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        base_url: impl Into<String>,
        speech_path: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
        voice: impl Into<String>,
        response_format: impl Into<String>,
    ) -> Self {
        Self::with_raw_sample_rate(
            base_url,
            speech_path,
            api_key,
            model,
            voice,
            response_format,
            16_000,
        )
    }

    /// Like [`OpenAiTtsClient::new`], but declares the sample rate of raw
    /// (non-WAV) upstream bodies so they can be resampled to 16 kHz.
    #[allow(clippy::too_many_arguments)]
    pub fn with_raw_sample_rate(
        base_url: impl Into<String>,
        speech_path: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
        voice: impl Into<String>,
        response_format: impl Into<String>,
        raw_sample_rate: u32,
    ) -> Self {
        Self::with_raw_sample_rate_and_timeout(
            base_url,
            speech_path,
            api_key,
            model,
            voice,
            response_format,
            raw_sample_rate,
            DEFAULT_REQUEST_TIMEOUT,
        )
    }

    /// Like [`Self::with_raw_sample_rate`] with an explicit request timeout —
    /// the test seam for the hung-upstream regression (tests set a short knob
    /// instead of sleeping past the production default).
    #[allow(clippy::too_many_arguments)] // the existing 7-arg ctor + the timeout knob
    pub fn with_raw_sample_rate_and_timeout(
        base_url: impl Into<String>,
        speech_path: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
        voice: impl Into<String>,
        response_format: impl Into<String>,
        raw_sample_rate: u32,
        request_timeout: std::time::Duration,
    ) -> Self {
        Self {
            http: reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(10))
                .timeout(request_timeout)
                .build()
                .expect("reqwest client builds"),
            base_url: base_url.into(),
            speech_path: speech_path.into(),
            api_key: api_key.into(),
            model: model.into(),
            voice: voice.into(),
            response_format: response_format.into(),
            raw_sample_rate,
        }
    }

    pub fn from_config(cfg: &TtsConfig) -> Self {
        Self::with_raw_sample_rate(
            &cfg.base_url,
            &cfg.speech_path,
            &cfg.api_key,
            &cfg.model,
            &cfg.voice,
            &cfg.response_format,
            cfg.raw_sample_rate,
        )
    }

    fn url(&self) -> String {
        format!(
            "{}{}",
            self.base_url.trim_end_matches('/'),
            self.speech_path
        )
    }
}

#[async_trait]
impl TtsProvider for OpenAiTtsClient {
    async fn synthesize(&self, text: &str) -> Result<Vec<u8>, HarnessError> {
        let body = serde_json::json!({
            "model": self.model,
            "voice": self.voice,
            "input": text,
            "response_format": self.response_format,
        });
        let mut req = self.http.post(self.url()).json(&body);
        if !self.api_key.is_empty() {
            req = req.bearer_auth(&self.api_key);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| HarnessError::Network(format!("tts request failed: {e}")))?;
        let status = resp.status();
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| HarnessError::Network(format!("tts body read failed: {e}")))?;
        if !status.is_success() {
            return Err(HarnessError::upstream(
                "tts",
                status.as_u16(),
                String::from_utf8_lossy(&bytes).into_owned(),
            ));
        }
        normalize_to_16k_mono(&bytes, self.raw_sample_rate)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal canonical WAV with the given rate/format/samples.
    fn wav_bytes(
        rate: u32,
        channels: u16,
        audio_format: u16,
        bits: u16,
        frames: &[Vec<u8>],
    ) -> Vec<u8> {
        let frame_len = channels as usize * bits as usize / 8;
        let mut w = Vec::new();
        let data_len = frames.len() * frame_len;
        w.extend_from_slice(b"RIFF");
        w.extend_from_slice(&((36 + data_len) as u32).to_le_bytes());
        w.extend_from_slice(b"WAVE");
        w.extend_from_slice(b"fmt ");
        w.extend_from_slice(&16u32.to_le_bytes());
        w.extend_from_slice(&audio_format.to_le_bytes());
        w.extend_from_slice(&channels.to_le_bytes());
        w.extend_from_slice(&rate.to_le_bytes());
        let byte_rate = rate * channels as u32 * bits as u32 / 8;
        w.extend_from_slice(&byte_rate.to_le_bytes());
        w.extend_from_slice(&(channels * bits / 8).to_le_bytes());
        w.extend_from_slice(&bits.to_le_bytes());
        w.extend_from_slice(b"data");
        w.extend_from_slice(&(data_len as u32).to_le_bytes());
        for f in frames {
            for b in f {
                w.push(*b);
            }
        }
        w
    }

    fn i16_le_bytes(v: i16) -> Vec<u8> {
        v.to_le_bytes().to_vec()
    }

    #[test]
    fn parses_16bit_mono_wav() {
        let w = wav_bytes(
            16_000,
            1,
            1,
            16,
            &[i16_le_bytes(100), i16_le_bytes(-56), i16_le_bytes(257)],
        );
        let (samples, rate) = wav_to_pcm16(&w).expect("parses");
        assert_eq!(rate, 16_000);
        assert_eq!(samples, vec![100, -56, 257]);
    }

    #[test]
    fn downmixes_stereo_to_mono() {
        // L=100, R=-100 → 0; L=300, R=300 → 300
        let frames = vec![
            [100i16.to_le_bytes(), (-100i16).to_le_bytes()].concat(),
            [300i16.to_le_bytes(), 300i16.to_le_bytes()].concat(),
        ];
        let w = wav_bytes(16_000, 2, 1, 16, &frames);
        let (samples, rate) = wav_to_pcm16(&w).expect("parses");
        assert_eq!(rate, 16_000);
        assert_eq!(samples, vec![0, 300]);
    }

    #[test]
    fn converts_32bit_float_wav() {
        let frames = vec![
            0.5f32.to_le_bytes().to_vec(),
            (-0.25f32).to_le_bytes().to_vec(),
        ];
        let w = wav_bytes(24_000, 1, 3, 32, &frames);
        let (samples, rate) = wav_to_pcm16(&w).expect("parses");
        assert_eq!(rate, 24_000);
        assert_eq!(samples, vec![16384, -8192]);
    }

    #[test]
    fn truncated_wav_is_protocol_error() {
        // RIFF/WAVE tags but no fmt/data chunks.
        let w = b"RIFF\x10\x00\x00\x00WAVE".to_vec();
        let err = wav_to_pcm16(&w).expect_err("must fail");
        assert!(err.to_string().contains("fmt"), "unexpected: {err}");
    }

    #[test]
    fn resample_sine_24k_to_16k_preserves_frequency_and_length() {
        // 2400 samples of a 240 Hz sine at 24 kHz — 24 full cycles.
        let sine: Vec<i16> = (0..2400)
            .map(|i| {
                (2400.0 * (2.0 * std::f64::consts::PI * 240.0 * i as f64 / 24_000.0).sin()) as i16
            })
            .collect();
        let out = resample_to_16k(&sine, 24_000);
        assert_eq!(out.len(), 1600, "length scales by 2/3");
        // Frequency is preserved: output spans 0.1 s at 16 kHz; count zero
        // crossings (2 per cycle) with tolerance for boundary phase.
        let zero_crossings = out
            .windows(2)
            .filter(|w| (w[0] >= 0) != (w[1] >= 0))
            .count();
        let freq = zero_crossings as f64 / 2.0 / 0.1;
        assert!(
            (235.0..=245.0).contains(&freq),
            "240 Hz must survive resampling, got {freq} Hz"
        );
        // Peak amplitude is preserved through linear interpolation.
        let peak = out.iter().map(|s| s.abs()).max().unwrap_or(0);
        assert!(peak > 2000, "peak must survive resampling, got {peak}");
    }

    #[test]
    fn resample_same_rate_is_identity() {
        let s = vec![1, -2, 3, -4, 5];
        assert_eq!(resample_to_16k(&s, 16_000), s);
    }
}
