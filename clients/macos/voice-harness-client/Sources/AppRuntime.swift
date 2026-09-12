import AVFoundation
import Combine
import Foundation
import voicekit

/// Owns the live session: mic capture → WS client → audio playback, and
/// republishes the session model for SwiftUI (menu bar icon + panel).
@MainActor
final class AppRuntime: ObservableObject {
    static let shared = AppRuntime()

    @Published private(set) var running = false
    @Published private(set) var isBusy = false
    @Published private(set) var phase: SessionPhase = .idle
    @Published private(set) var transcript: String?
    @Published private(set) var responseText = ""
    @Published private(set) var errorMessage: String?
    let serverURL = AppSettings.serverURL

    /// The wire→UI state machine; single source of truth, mirrored above.
    let model = HarnessSessionModel()

    private var mic: MicCapture?
    private var client: HarnessClient?
    private var player: AudioPlayer?
    private var drainTask: Task<Void, Never>?
    /// Set when `turn.completed` arrived while audio was still pending.
    private var turnCompletedPending = false
    private var cancellables = Set<AnyCancellable>()
    /// VH_DEBUG_MIC=1 → periodic mic chunk-rate/peak logging (os_log "vhdebug").
    private let debugMic = ProcessInfo.processInfo.environment["VH_DEBUG_MIC"] == "1"
    private var debugChunks = 0
    private var debugPeak: Int16 = 0

    private init() {
        // @Published projections emit *after* mutation — safe to mirror here.
        model.$phase.dropFirst().sink { [weak self] in self?.phase = $0 }.store(in: &cancellables)
        model.$transcript.dropFirst().sink { [weak self] in
            self?.transcript = $0.isEmpty ? nil : $0
        }.store(in: &cancellables)
        model.$responseText.dropFirst().sink { [weak self] in self?.responseText = $0 }.store(in: &cancellables)
        model.$errorMessage.dropFirst().sink { [weak self] in self?.errorMessage = $0 }.store(in: &cancellables)
    }

    func toggle() async {
        if running { stop() } else { await start() }
    }

    func start() async {
        guard !running, !isBusy else { return }
        isBusy = true
        defer { isBusy = false }
        errorMessage = nil
        model.reset()
        if debugMic { DebugMicStats.reset() }

        let player = AudioPlayer()
        do {
            try player.start()
        } catch {
            errorMessage = "Audio output unavailable: \(error.localizedDescription)"
            return
        }

        let transport = URLSessionTransport(url: serverURL)
        let client = HarnessClient(transport: transport, model: model)
        do {
            try await client.start(deviceId: deviceName())
        } catch {
            await transport.close()
            errorMessage = "Can't reach harness server at \(serverURL.host ?? "?"): \(error.localizedDescription)"
            return
        }
        client.onChunk = { [weak player] base64, _ in
            player?.scheduleChunk(base64: base64)
        }
        // `turn.completed` arriving while audio is still queued is absorbed by
        // the model (stays speaking); remember it so the drain watcher can
        // complete the turn → listening transition once playback drains.
        client.onMessage = { [weak self] message in
            guard case .turnCompleted = message else { return }
            Task { @MainActor in self?.noteTurnCompleted() }
        }

        let mic = MicCapture()
        do {
            try mic.start { [weak client, weak self] event in
                guard case .chunk16k(let base64) = event, let client else { return }
                if let self, self.debugMic {
                    let samples = base64ToPCM16(base64)
                    var peak: Int16 = 0
                    for v in samples { if abs(Int32(v)) > abs(Int32(peak)) { peak = v } }
                    self.debugMicChunk(peak: peak, samples: samples.count)
                }
                Task { await client.sendAudio(base64: base64) }
            }
        } catch {
            await client.stop()
            player.stop()
            errorMessage = "Microphone unavailable: \(error.localizedDescription)"
            return
        }
        if debugMic {
            NSLog("vhdebug: mic native format: \(mic.nativeFormatDescription)")
        }

        self.player = player
        self.client = client
        self.mic = mic
        turnCompletedPending = false
        running = true
        drainTask = Task { [weak self] in await self?.drainWatcher() }
    }

    func stop() {
        guard running else { return }
        if debugMic { DebugMicStats.shared.summary() }
        drainTask?.cancel()
        drainTask = nil
        mic?.stop()
        mic = nil
        let client = self.client
        self.client = nil
        Task { await client?.stop() } // session.stop + close, off the hot path
        player?.stop()
        player = nil
        turnCompletedPending = false
        running = false
        model.reset()
    }

    private func noteTurnCompleted() {
        if model.audioPending { turnCompletedPending = true }
    }

    /// Watches playback drain so `turn.completed` that arrived while audio was
    /// still playing lands once the queue empties (phase → listening).
    private func drainWatcher() async {
        while !Task.isCancelled {
            if model.audioPending {
                await player?.waitUntilDrained()
                guard !Task.isCancelled else { return }
                model.audioDidFinish()
                if turnCompletedPending {
                    turnCompletedPending = false
                    model.apply(.turnCompleted) // audioPending now false → listening
                }
            }
            try? await Task.sleep(for: .milliseconds(100))
        }
    }

    private func deviceName() -> String {
        Host.current().localizedName ?? ProcessInfo.processInfo.hostName
    }

    /// Debug-mic accounting: logs a summary every 20 chunks (~1.2 s at 16 kHz).
    /// Called from the audio tap queue — counters are main-actor-published via
    /// the mirroring in init, so accumulate in local storage instead.
    nonisolated private func debugMicChunk(peak: Int16, samples: Int) {
        DebugMicStats.shared(peak: peak, samples: samples)
    }
}

/// Lock-protected accumulator for VH_DEBUG_MIC logging (audio-thread safe).
/// File-based: unified logging redacts dynamic values, so peaks go to
/// /tmp/vh-mic-debug.log instead.
final class DebugMicStats: @unchecked Sendable {
    static let shared = DebugMicStats()
    private let lock = NSLock()
    private var chunks = 0
    private var maxPeak: Int16 = 0
    private let logPath = "/tmp/vh-mic-debug.log"

    static func reset() {
        try? FileManager.default.removeItem(atPath: "/tmp/vh-mic-debug.log")
    }

    private func append(_ line: String) {
        let data = Data((line + "\n").utf8)
        if let fh = FileHandle(forWritingAtPath: logPath) {
            defer { try? fh.close() }
            fh.seekToEndOfFile()
            fh.write(data)
        } else {
            try? data.write(to: URL(fileURLWithPath: logPath))
        }
    }

    private func record(peak: Int16, samples: Int) {
        lock.lock()
        chunks += 1
        if abs(Int32(peak)) > abs(Int32(maxPeak)) { maxPeak = peak }
        let c = chunks
        lock.unlock()
        append("chunk \(c) peak=\(peak) samples=\(samples)")
    }

    func summary() {
        lock.lock()
        let c = chunks, mp = maxPeak
        lock.unlock()
        append("SUMMARY chunks=\(c) maxpeak=\(mp)")
    }

    func callAsFunction(peak: Int16, samples: Int) { record(peak: peak, samples: samples) }
}
