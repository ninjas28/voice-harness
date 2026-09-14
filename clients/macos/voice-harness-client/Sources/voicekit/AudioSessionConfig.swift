import AVFoundation
import Foundation

/// Activates the process-wide audio session for voice use on iOS. No-op on
/// macOS (no AVAudioSession there; the OS routes automatically).
public enum AudioSessionConfig {
    /// Play-and-record with Apple's voice processing (AEC/AGC), output to the
    /// loudspeaker, Bluetooth HFP allowed.
    ///
    /// Mode must be `.videoChat` (NOT `.voiceChat`): `.defaultToSpeaker` is
    /// documented to have no effect in voiceChat mode — that mode pins output
    /// to the earpiece, so TTS was only audible through the phone receiver.
    /// videoChat enables the same voice-processing (AEC/AGC) path as
    /// voiceChat while honoring the speaker option.
    public static func activateForVoice() throws {
        #if os(iOS)
        let session = AVAudioSession.sharedInstance()
        try session.setCategory(
            .playAndRecord, mode: .videoChat,
            options: [.allowBluetooth, .defaultToSpeaker])
        try session.setPreferredSampleRate(16000)
        try session.setPreferredIOBufferDuration(0.03)
        try session.setActive(true)
        #endif
    }

    public static func deactivate() {
        #if os(iOS)
        try? AVAudioSession.sharedInstance().setActive(false, options: .notifyOthersOnDeactivation)
        #endif
    }
}
