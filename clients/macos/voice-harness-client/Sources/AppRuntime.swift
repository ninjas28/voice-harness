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
    /// TTS playback speed — persisted; applied to the live player too.
    @Published var playbackRate: Float = AppSettings.ttsRate {
        didSet {
            AppSettings.setTTSRate(playbackRate)
            player?.rate = playbackRate
        }
    }
    /// Editable server URL for the settings field. Persisted via
    /// `AppSettings`; changing it while running stops the live session so the
    /// next start reconnects to the new endpoint.
    @Published var serverURLString: String = AppSettings.storedServerURLString ?? AppSettings.defaultServerURLString

    /// The wire→UI state machine; single source of truth, mirrored above.
    let model = HarnessSessionModel()

    private var mic: MicCapture?
    private var client: HarnessClient?
    private var player: AudioPlayer?
    private var drainTask: Task<Void, Never>?
    /// Set when `turn.completed` arrived while audio was still pending.
    private var turnCompletedPending = false
    private var cancellables = Set<AnyCancellable>()

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

    /// Commits the settings field to `AppSettings`. Returns an error message
    /// for invalid input (nothing is stored), or nil on success. Clearing the
    /// field restores the default URL. A running session is stopped so the
    /// next start reconnects to the committed endpoint.
    @discardableResult
    func commitServerURL(_ raw: String) -> String? {
        guard let sanitized = AppSettings.sanitizeServerURLString(raw) else {
            return "Enter a ws:// or wss:// URL with a host (e.g. \(AppSettings.defaultServerURLString))"
        }
        AppSettings.setServerURLString(sanitized)
        serverURLString = sanitized.isEmpty ? AppSettings.defaultServerURLString : sanitized
        if running { stop() }
        return nil
    }

    func start() async {
        guard !running, !isBusy else { return }
        isBusy = true
        defer { isBusy = false }
        errorMessage = nil
        model.reset()

        let serverURL = AppSettings.serverURL
        // Order matters on macOS: the input tap must be installed and the
        // engine prepared BEFORE engine.start() — tapping a running engine's
        // input node silently never fires. So: create player + client, install
        // mic tap on the shared engine (prepared, not started), then one
        // engine start negotiates the Bluetooth route for both directions.
        let player = AudioPlayer()
        player.rate = playbackRate
        let transport = URLSessionTransport(url: serverURL)
        let client = HarnessClient(transport: transport, model: model)
        do {
            try await client.start(deviceId: deviceName())
        } catch {
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

        let mic = MicCapture(engine: player.engine)
        do {
            try mic.start { [weak client] event in
                guard case .chunk16k(let base64) = event, let client else { return }
                Task { await client.sendAudio(base64: base64) }
            }
        } catch {
            await client.stop()
            errorMessage = "Microphone unavailable: \(error.localizedDescription)"
            return
        }
        do {
            try player.start()
        } catch {
            await client.stop()
            mic.stop()
            errorMessage = "Audio output unavailable: \(error.localizedDescription)"
            return
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
}
