import AVFoundation
import Foundation

/// Activates the process-wide audio session for voice use on iOS. No-op on
/// macOS (no AVAudioSession there; the OS routes automatically).
public enum AudioSessionConfig {
    /// Play-and-record with Apple's voice processing (AEC/AGC) — the mic then
    /// hears less of our own TTS on-device. Speaker output, not earpiece.
    public static func activateForVoice() throws {
        #if os(iOS)
        let session = AVAudioSession.sharedInstance()
        try session.setCategory(.playAndRecord, mode: .voiceChat, options: [.defaultToSpeaker])
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
