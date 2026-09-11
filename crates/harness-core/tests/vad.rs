//! VAD / utterance FSM tests (RED phase — UtteranceAssembler not implemented yet).
//!
//! Uses a synthetic detector (speech = RMS above threshold) so the FSM is tested
//! deterministically, independent of the real WebRTC VAD.

use harness_core::vad::{UtteranceAssembler, VadDetector, VadPolicy};

/// 30 ms @ 16 kHz — the plan's canonical frame (480 samples).
const FRAME: usize = 480;
const RATE: u32 = 16_000;

/// Speech = RMS > threshold. Silence frames are near-zero, speech frames are loud.
struct SyntheticVad {
    threshold: f64,
}

impl VadDetector for SyntheticVad {
    fn is_speech(&mut self, frame: &[i16], _sample_rate: u32) -> bool {
        if frame.is_empty() {
            return false;
        }
        let rms = (frame.iter().map(|s| (*s as f64) * (*s as f64)).sum::<f64>()
            / frame.len() as f64)
            .sqrt();
        rms > self.threshold
    }
}

fn policy() -> VadPolicy {
    VadPolicy {
        silence_ms: 700,
        min_utterance_ms: 300,
        max_utterance_ms: 30_000,
        pre_speech_ms: 500,
    }
}

fn speech_frame() -> Vec<i16> {
    (0..FRAME)
        .map(|i| (3000.0 * (2.0 * std::f64::consts::PI * i as f64 / 16.0).sin()) as i16)
        .collect()
}

fn silence_frame() -> Vec<i16> {
    vec![0i16; FRAME]
}

/// ms timestamp of frame n (30 ms frames)
fn ts(n: usize) -> u64 {
    n as u64 * 30
}

#[test]
fn speech_then_700ms_silence_finalizes() {
    let mut asm = UtteranceAssembler::new(policy(), SyntheticVad { threshold: 100.0 });

    // 17 silence frames = 510 ms; ring keeps the last <=500 ms (16 frames, 480 ms).
    for n in 0..17 {
        assert!(asm.push(&silence_frame(), ts(n)).is_none());
    }
    // 34 speech frames = 1020 ms of speech.
    for n in 17..51 {
        assert!(
            asm.push(&speech_frame(), ts(n)).is_none(),
            "no endpoint mid-speech"
        );
    }
    // Silence tail: endpoint fires when silence reaches 700 ms → on the 24th frame (720 ms).
    for n in 51..74 {
        assert!(
            asm.push(&silence_frame(), ts(n)).is_none(),
            "not yet 700 ms of silence"
        );
    }
    let utt = asm
        .push(&silence_frame(), ts(74))
        .expect("endpoint fires at 700 ms silence");

    // 480 (pre-speech) + 1020 (speech) + 720 (trailing silence) = 2220 ms.
    assert_eq!(utt.duration_ms(), 2220);
    assert_eq!(utt.pcm.len(), 35_520); // 2220 ms * 16 samples/ms
    assert_eq!(utt.sample_rate, RATE);
}

#[test]
fn blip_below_min_utterance_discarded() {
    let mut asm = UtteranceAssembler::new(policy(), SyntheticVad { threshold: 100.0 });

    for n in 0..17 {
        assert!(asm.push(&silence_frame(), ts(n)).is_none());
    }
    // 4 speech frames = 120 ms blip (< 300 ms min utterance).
    for n in 17..21 {
        assert!(asm.push(&speech_frame(), ts(n)).is_none());
    }
    // Full silence tail — must finalize-and-discard, returning None, not Some.
    for n in 21..46 {
        assert!(
            asm.push(&silence_frame(), ts(n)).is_none(),
            "blip must be discarded by min-utterance policy"
        );
    }
}

#[test]
fn force_end_shortcuts_endpoint() {
    let mut asm = UtteranceAssembler::new(policy(), SyntheticVad { threshold: 100.0 });

    for n in 0..17 {
        assert!(asm.push(&silence_frame(), ts(n)).is_none());
    }
    // force_end with no speech → None.
    assert!(asm.force_end().is_none());

    // 1 s of speech, then client says speech.end.
    for n in 17..51 {
        assert!(asm.push(&speech_frame(), ts(n)).is_none());
    }
    let utt = asm
        .force_end()
        .expect("force_end returns the open utterance");
    assert_eq!(utt.duration_ms(), 480 + 1020);

    // force_end again with nothing open → None.
    assert!(asm.force_end().is_none());
}

#[test]
fn max_utterance_cap_finalizes_without_silence() {
    let mut asm = UtteranceAssembler::new(policy(), SyntheticVad { threshold: 100.0 });

    for n in 0..17 {
        assert!(asm.push(&silence_frame(), ts(n)).is_none());
    }
    // Feed 31 s of continuous speech; cap must fire at 30 s, not 31 s.
    let mut result = None;
    let mut fired_at = None;
    for n in 17..17 + 1034 {
        // 30000 ms cap → 480 ms pre + 30000 ms speech = 30480 ms at the 1000th frame.
        if let Some(u) = asm.push(&speech_frame(), ts(n)) {
            result = Some(u);
            fired_at = Some(n);
            break;
        }
    }
    let n = fired_at.expect("cap must fire");
    assert_eq!(
        n,
        17 + 999,
        "fires on the 1000th speech frame (480+30000 ms)"
    );
    let utt = result.unwrap();
    assert_eq!(utt.duration_ms(), 30_480);
    // Assembler must be reusable after the cap: new speech can start.
    for n in n + 1..n + 40 {
        assert!(asm.push(&speech_frame(), ts(n)).is_none());
    }
    for n in n + 40..n + 63 {
        assert!(asm.push(&silence_frame(), ts(n)).is_none());
    }
    // 24th silence frame reaches 720 ms ≥ 700 ms → endpoint fires here.
    let utt2 = asm
        .push(&silence_frame(), ts(n + 63))
        .expect("second utterance after cap");
    assert!(utt2.duration_ms() > 1000);
}

#[test]
fn mid_utterance_pause_does_not_split() {
    let mut asm = UtteranceAssembler::new(policy(), SyntheticVad { threshold: 100.0 });

    for n in 0..17 {
        assert!(asm.push(&silence_frame(), ts(n)).is_none());
    }
    // 500 ms speech, 150 ms pause (< 700 ms → no split), 500 ms speech, then silence tail.
    for n in 17..34 {
        assert!(asm.push(&speech_frame(), ts(n)).is_none());
    }
    for n in 34..39 {
        assert!(asm.push(&silence_frame(), ts(n)).is_none());
    }
    for n in 39..56 {
        assert!(asm.push(&speech_frame(), ts(n)).is_none());
    }
    for n in 56..79 {
        assert!(asm.push(&silence_frame(), ts(n)).is_none());
    }
    // 24th silence frame reaches 720 ms ≥ 700 ms → endpoint fires here.
    let utt = asm
        .push(&silence_frame(), ts(79))
        .expect("one utterance across the pause");
    // 480 + 510 + 150 + 510 + 720 = 2370 ms.
    assert_eq!(utt.duration_ms(), 2370);
}

#[test]
fn webrtc_vad_classifies_real_frames() {
    // The real C-backed detector must compile and classify obvious frames.
    let mut vad = harness_core::vad::WebrtcVad::new();
    assert!(
        !vad.is_speech(&vec![0i16; FRAME], RATE),
        "silence is not speech"
    );
    let loud = speech_frame();
    assert!(vad.is_speech(&loud, RATE), "loud 1 kHz tone is speech");
}
