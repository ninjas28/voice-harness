import AVFoundation
import Foundation

/// Captures microphone audio via `AVAudioEngine` and forwards 16 kHz mono
/// PCM16 chunks (base64) to a sink — the harness wire contract.
///
/// All mutable state is confined to `queue`. The tap callback converts using a
/// converter captured at tap-install time (touched solely on the audio
/// callback thread) and hands only a `String` across the queue boundary.
/// Verified live in T9 — no mic in the test host.
public final class MicCapture: @unchecked Sendable {
    public enum Event: Sendable {
        case chunk16k(base64: String)
    }

    enum CaptureError: Error {
        case unsupportedInputFormat(AVAudioFormat)
    }

    private let engine = AVAudioEngine()
    private let queue = DispatchQueue(label: "vh.mic")
    private var sink: (@Sendable (Event) -> Void)?
    private var running = false
    /// Description of the input node's native format (set during `start`).
    public private(set) var nativeFormatDescription = "not started"

    public init() {}

    public var isRunning: Bool { queue.sync { running } }

    public func start(sink: @escaping @Sendable (Event) -> Void) throws {
        try queue.sync {
            guard !running else { return }
            let input = engine.inputNode
            let native = input.inputFormat(forBus: 0)
            let target = AVAudioFormat(commonFormat: .pcmFormatInt16, sampleRate: 16000,
                                       channels: 1, interleaved: true)!
            guard let converter = AVAudioConverter(from: native, to: target) else {
                throw CaptureError.unsupportedInputFormat(native)
            }
            nativeFormatDescription = native.description
            self.sink = sink
            input.installTap(onBus: 0, bufferSize: 4800, format: native) { [weak self, converter] buffer, _ in
                guard let self else { return }
                let samples = Self.convert(buffer, with: converter)
                guard !samples.isEmpty else { return } // converter may emit zero frames while warm-up
                let payload = pcm16ToBase64(samples)
                self.queue.async { self.emit(payload) }
            }
            engine.prepare()
            try engine.start()
            running = true
        }
    }

    public func stop() {
        queue.sync {
            guard running else { return }
            engine.inputNode.removeTap(onBus: 0)
            engine.stop()
            sink = nil
            running = false
        }
    }

    private func emit(_ base64: String) {
        VHSendLog.log("emit \(base64.count) chars, sink=\(sink != nil)")
        sink?(.chunk16k(base64: base64))
    }

    /// Converts one native-format tap buffer to 16 kHz mono Int16 samples.
    /// Zero-frame converter output (internal buffering early in a stream) is dropped.
    private static func convert(_ buffer: AVAudioPCMBuffer, with converter: AVAudioConverter) -> [Int16] {
        let ratio = converter.outputFormat.sampleRate / buffer.format.sampleRate
        let capacity = AVAudioFrameCount(Double(buffer.frameLength) * ratio) + 32
        guard capacity > 0,
              let output = AVAudioPCMBuffer(pcmFormat: converter.outputFormat, frameCapacity: capacity)
        else { return [] }
        var fed = false
        var conversionError: NSError?
        let status = converter.convert(to: output, error: &conversionError) { _, outStatus in
            if fed {
                outStatus.pointee = .noDataNow
                return nil
            }
            fed = true
            outStatus.pointee = .haveData
            return buffer
        }
        guard status != .error, conversionError == nil,
              output.frameLength > 0,
              let int16 = output.int16ChannelData
        else { return [] }
        return Array(UnsafeBufferPointer(start: int16[0], count: Int(output.frameLength)))
    }
}
