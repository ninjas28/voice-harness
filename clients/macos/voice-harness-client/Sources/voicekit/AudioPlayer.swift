import AVFoundation
import Foundation

/// Streams `audio.chunk` payloads through a 16 kHz `AVAudioPlayerNode`.
///
/// All scheduling/drain bookkeeping runs on `queue`; the real-time completion
/// callback only forwards the count onto it. Drain accounting is exercised by
/// unit tests via `schedulePCMForTest`/`drainForTest` (no audio hardware);
/// actual sound is verified live in T9.
public final class AudioPlayer: @unchecked Sendable {
    private let engine = AVAudioEngine()
    private let node = AVAudioPlayerNode()
    private let format = AVAudioFormat(commonFormat: .pcmFormatInt16, sampleRate: 16000,
                                       channels: 1, interleaved: true)!
    private let queue = DispatchQueue(label: "vh.play")
    private var scheduled = 0
    private var drainContinuation: CheckedContinuation<Void, Never>?
    private(set) var isDrained = true

    public init() {
        engine.attach(node)
        engine.connect(node, to: engine.mainMixerNode, format: format)
    }

    public func start() throws {
        guard !engine.isRunning else { return }
        try engine.start()
        node.play()
    }

    public func stop() {
        queue.sync {
            node.stop()
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
                self.scheduled -= 1
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

    private func makeBuffer(_ samples: [Int16]) -> AVAudioPCMBuffer? {
        let buffer = samples.withUnsafeBufferPointer { p in
            AVAudioPCMBuffer(pcmFormat: format, frameCapacity: AVAudioFrameCount(p.count))
        }
        guard let buffer else { return nil }
        buffer.frameLength = AVAudioFrameCount(samples.count)
        samples.withUnsafeBufferPointer { p in
            buffer.int16ChannelData!.pointee.update(from: p.baseAddress!, count: samples.count)
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
