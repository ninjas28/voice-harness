//! VAD / utterance segmentation.
//!
//! The FSM is detector-agnostic: [`VadDetector`] is the pluggable voice-activity
//! probe (real [`WebrtcVad`] in production, synthetic RMS detectors in tests).
//! [`UtteranceAssembler`] consumes fixed-duration PCM16 frames and applies the
//! session policy: pre-speech ring buffer → speech → silence endpoint →
//! min/max caps → finalized [`Utterance`].

use std::collections::VecDeque;

/// Pluggable voice-activity detector.
pub trait VadDetector {
    /// Decide whether one fixed-length frame contains speech.
    fn is_speech(&mut self, frame: &[i16], sample_rate: u32) -> bool;
}

/// Endpointing/segmentation policy (mirrors `[session]` config).
#[derive(Debug, Clone)]
pub struct VadPolicy {
    pub silence_ms: u64,
    pub min_utterance_ms: u64,
    pub max_utterance_ms: u64,
    pub pre_speech_ms: u64,
}

/// A finalized utterance: raw PCM16 mono samples at `sample_rate`.
#[derive(Debug, Clone, PartialEq)]
pub struct Utterance {
    pub pcm: Vec<i16>,
    pub sample_rate: u32,
}

impl Utterance {
    pub fn duration_ms(&self) -> u64 {
        self.pcm.len() as u64 * 1000 / self.sample_rate.max(1) as u64
    }
}

enum State {
    /// No utterance open; ring-buffering pre-speech audio.
    Listening,
    /// Utterance open (pre-speech prefix already in `buf`).
    Speech,
}

/// Segments a frame stream into utterances.
pub struct UtteranceAssembler<D: VadDetector> {
    policy: VadPolicy,
    detector: D,
    sample_rate: u32,
    state: State,
    /// Pre-speech ring: (frame, ms) pairs, total ≤ `policy.pre_speech_ms`.
    pre_speech: VecDeque<(Vec<i16>, u64)>,
    pre_speech_total_ms: u64,
    /// Utterance accumulation including the pre-speech prefix and trailing silence.
    buf: Vec<i16>,
    /// Total ms currently in `buf`.
    buf_ms: u64,
    /// Ms of actual speech content in `buf` (excludes pre-speech prefix and
    /// trailing silence) — min-utterance and max-duration are measured on this.
    speech_ms: u64,
    /// Consecutive trailing silence (ms) while in Speech.
    silence_ms: u64,
    /// An utterance is open (state == Speech); kept as a flag for clarity.
    active: bool,
}

impl<D: VadDetector> UtteranceAssembler<D> {
    pub fn new(policy: VadPolicy, detector: D) -> Self {
        Self {
            policy,
            detector,
            sample_rate: 16_000,
            state: State::Listening,
            pre_speech: VecDeque::new(),
            pre_speech_total_ms: 0,
            buf: Vec::new(),
            buf_ms: 0,
            speech_ms: 0,
            silence_ms: 0,
            active: false,
        }
    }

    /// Override the assumed input sample rate (audio contract: 16 kHz).
    pub fn set_sample_rate(&mut self, sample_rate: u32) {
        self.sample_rate = sample_rate;
    }

    /// Feed one frame; returns the finalized utterance when the endpoint fires
    /// or the max-duration cap is hit.
    pub fn push(&mut self, pcm: &[i16], _now_ms: u64) -> Option<Utterance> {
        if pcm.is_empty() {
            return None;
        }
        let frame_ms = pcm.len() as u64 * 1000 / self.sample_rate.max(1) as u64;
        let speech = self.detector.is_speech(pcm, self.sample_rate);

        match self.state {
            State::Listening => {
                if speech {
                    // Open the utterance with the pre-speech prefix + this frame.
                    self.buf.clear();
                    self.buf_ms = 0;
                    for (f, ms) in self.pre_speech.drain(..) {
                        self.buf.extend_from_slice(&f);
                        self.buf_ms += ms;
                    }
                    self.buf.extend_from_slice(pcm);
                    self.buf_ms += frame_ms;
                    self.speech_ms = frame_ms;
                    self.silence_ms = 0;
                    self.active = true;
                    self.state = State::Speech;
                    self.maybe_cap()
                } else {
                    // Ring-buffer pre-speech audio, trimming to the policy window.
                    self.pre_speech.push_back((pcm.to_vec(), frame_ms));
                    self.pre_speech_total_ms += frame_ms;
                    while self.pre_speech_total_ms > self.policy.pre_speech_ms {
                        if let Some((_, ms)) = self.pre_speech.pop_front() {
                            self.pre_speech_total_ms -= ms;
                        } else {
                            break;
                        }
                    }
                    None
                }
            }
            State::Speech => {
                self.buf.extend_from_slice(pcm);
                self.buf_ms += frame_ms;
                if speech {
                    self.speech_ms += frame_ms;
                    self.silence_ms = 0;
                    self.maybe_cap()
                } else {
                    self.silence_ms += frame_ms;
                    if self.silence_ms >= self.policy.silence_ms {
                        self.finalize()
                    } else {
                        self.maybe_cap()
                    }
                }
            }
        }
    }

    /// Client sent an explicit `speech.end`: close the open utterance now.
    pub fn force_end(&mut self) -> Option<Utterance> {
        if self.active {
            self.finalize()
        } else {
            None
        }
    }

    /// Whether an utterance is currently open (VAD said speech and no
    /// endpoint has fired yet) — used by the WS layer to emit `State(Speech)`
    /// exactly once on the Listening → Speech transition.
    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Finalize at the max-duration cap (fires even mid-speech). The cap is
    /// measured on speech content, so the emitted utterance is the speech plus
    /// its pre-speech lead-in.
    fn maybe_cap(&mut self) -> Option<Utterance> {
        if self.active && self.speech_ms >= self.policy.max_utterance_ms {
            self.finalize()
        } else {
            None
        }
    }

    /// Close the current utterance: emit if the speech content is long enough,
    /// otherwise discard the blip.
    fn finalize(&mut self) -> Option<Utterance> {
        self.state = State::Listening;
        self.active = false;
        self.silence_ms = 0;
        let pcm = std::mem::take(&mut self.buf);
        let speech_ms = self.speech_ms;
        self.buf_ms = 0;
        self.speech_ms = 0;
        if speech_ms >= self.policy.min_utterance_ms {
            Some(Utterance {
                pcm,
                sample_rate: self.sample_rate,
            })
        } else {
            None // blip: below min utterance, dropped
        }
    }
}

/// Real detector: WebRTC VAD (C) in aggressive mode at 16 kHz.
///
/// Only 10/20/30 ms frames are valid for the underlying library; other frame
/// lengths report silence (`Err(())` → `false`).
pub struct WebrtcVad {
    vad: webrtc_vad::Vad,
}

impl WebrtcVad {
    pub fn new() -> Self {
        Self {
            vad: webrtc_vad::Vad::new_with_rate_and_mode(
                webrtc_vad::SampleRate::Rate16kHz,
                webrtc_vad::VadMode::Aggressive,
            ),
        }
    }
}

impl Default for WebrtcVad {
    fn default() -> Self {
        Self::new()
    }
}

impl VadDetector for WebrtcVad {
    fn is_speech(&mut self, frame: &[i16], _sample_rate: u32) -> bool {
        self.vad.is_voice_segment(frame).unwrap_or(false)
    }
}

// The C VAD handle is only ever used behind `&mut self` (exclusive access), so
// moving it between threads is sound; needed to hold the assembler in tokio tasks.
unsafe impl Send for WebrtcVad {}
// Same argument for `&T` shared access: the vad crate's own `Sync` impl is
// missing (raw `*mut` inside), but the C library's entry points only mutate
// through `&mut` wrappers. `ConnState` (which owns an assembler) must be
// `Send` for tokio::spawn — `Sync` on the detector keeps async fns taking
// `&ConnState` Send-safe. No shared access actually happens: the assembler is
// always behind `&mut` or moved.
//
// SAFETY: webrtc_vad's Fvad functions take `*mut Fvad`; all access from Rust
// goes through `Vad::is_voice_segment(&mut self)`. Exclusive access only ⇒
// sharing &Self across threads never mutates concurrently through aliasing.
unsafe impl Sync for WebrtcVad {}
