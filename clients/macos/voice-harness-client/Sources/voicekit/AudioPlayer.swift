import AVFoundation
import Combine
import Foundation

/// Streams `audio.chunk` payloads through a 16 kHz `AVAudioPlayerNode`.
///
/// All scheduling/drain bookkeeping runs on `queue`; the real-time completion
/// callback only forwards the count onto it. Drain accounting is exercised by
/// unit tests via `schedulePCMForTest`/`drainForTest` (no audio hardware);
/// actual sound is verified live in T9.
public final class AudioPlayer: ObservableObject, @unchecked Sendable {
    /// Exposed so the mic capture can share this engine: two AVAudioEngines on
    /// a Bluetooth device fight over the route and playback goes silent.
    public let engine = AVAudioEngine()
    private let node = AVAudioPlayerNode()
    /// Pitch-preserving playback speed (1.0 = normal; up to 2.0 for skimming).
    private let timePitch = AVAudioUnitTimePitch()
    private let format = AVAudioFormat(commonFormat: .pcmFormatFloat32, sampleRate: 16000,
                                       channels: 1, interleaved: false)!
    private let queue = DispatchQueue(label: "vh.play")
    private var scheduled = 0
    private var drainContinuation: CheckedContinuation<Void, Never>?
    private(set) var isDrained = true

    /// TTS playback speed, published for the SwiftUI picker. Applied to the
    /// time-pitch unit live — mid-playback changes are immediate.
    @Published public var rate: Float = 1.0 {
        didSet { timePitch.rate = rate }
    }

    public init() {
        engine.attach(node)
        engine.attach(timePitch)
        // node → timePitch → mixer so rate changes apply to everything queued.
        engine.connect(node, to: timePitch, format: format)
        engine.connect(timePitch, to: engine.mainMixerNode, format: format)
    }

    public func start() throws {
        guard !engine.isRunning else { return }
        try engine.start()
        node.play()
    }

    public func stop() {
        queue.sync {
            node.stop()
            // Shared-engine mode: the player owns the engine — stop it here so
            // the Bluetooth route is released when the session ends.
            if engine.isRunning { engine.stop() }
            scheduled = 0
            setDrainedLocked(true)
        }
    }

    /// Schedules one `audio.chunk` payload for playback.
    public func scheduleChunk(base64: String) {
        let samples = base64ToPCM16(base64)
        guard !samples.isEmpty else { return }
        queue.async { self._schedule(samples) }
    }

    /// Resumes when all scheduled buffers have finished playing.
    public func waitUntilDrained() async {
        let already = queue.sync { isDrained }
        if already { return }
        await withCheckedContinuation { continuation in
            queue.async {
                if self.isDrained {
                    continuation.resume()
                } else {
                    // Replace any earlier waiter — one is enough; it resumes on drain.
                    self.drainContinuation = continuation
                }
            }
        }
    }

    // MARK: - Internals (queue-confined)

    private func _schedule(_ samples: [Int16]) {
        guard let buffer = makeBuffer(samples) else { return }
        scheduled += 1
        if scheduled == 1 { setDrainedLocked(false) }
        node.scheduleBuffer(buffer, completionCallbackType: .dataPlayedBack) { [weak self] _ in
            guard let self else { return }
            self.queue.async {
                self.scheduled = max(0, self.scheduled - 1)
                if self.scheduled == 0 { self.setDrainedLocked(true) }
            }
        }
    }

    /// Must run on `queue`. Resumes exactly one registered waiter.
    private func setDrainedLocked(_ value: Bool) {
        isDrained = value
        guard value, let continuation = drainContinuation else { return }
        drainContinuation = nil
        continuation.resume()
    }

    /// Int16 wire samples → Float32 canonical buffer (effect units like
    /// AVAudioUnitTimePitch reject Int16 connections with -10868).
    private func makeBuffer(_ samples: [Int16]) -> AVAudioPCMBuffer? {
        let buffer = AVAudioPCMBuffer(pcmFormat: format, frameCapacity: AVAudioFrameCount(samples.count))
        guard let buffer else { return nil }
        buffer.frameLength = AVAudioFrameCount(samples.count)
        if let dst = buffer.floatChannelData?[0] {
            for (i, s) in samples.enumerated() { dst[i] = Float(s) / 32768.0 }
        }
        return buffer
    }

    // MARK: - Test hooks (no audio hardware)

    func schedulePCMForTest(_ samples: [Int16], seq: Int) {
        queue.sync { _schedule(samples) }
    }

    func drainForTest() {
        queue.sync {
            scheduled = 0
            setDrainedLocked(true)
        }
    }
}
