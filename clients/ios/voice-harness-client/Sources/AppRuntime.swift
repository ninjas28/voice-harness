import AVFoundation
import Combine
import Foundation
import UIKit
import voicekit

/// iOS twin of the macOS AppRuntime (clients/macos/.../Sources/AppRuntime.swift):
/// owns the live session — mic capture → WS client → audio playback.
/// Platform deltas vs macOS: mic permission, AVAudioSession, UIDevice name.
@MainActor
final class AppRuntime: ObservableObject {
    static let shared = AppRuntime()

    @Published private(set) var running = false
    @Published private(set) var isBusy = false
    @Published private(set) var phase: SessionPhase = .idle
    @Published private(set) var transcript: String?
    @Published private(set) var responseText = ""
    @Published private(set) var errorMessage: String?
    /// Mirrors `model.activity`: what the assistant is doing under `thinking`.
    @Published private(set) var activity: HarnessSessionModel.Activity = .idle
    /// TTS playback speed — persisted; applied to the live player too.
    @Published var playbackRate: Float = AppSettings.ttsRate {
        didSet {
            AppSettings.setTTSRate(playbackRate)
            player?.rate = playbackRate
        }
    }
    /// Editable server URL for the settings sheet. Persisted via AppSettings;
    /// changing it while running stops the live session so the next start
    /// reconnects to the new endpoint.
    @Published var serverURLString: String =
        AppSettings.storedServerURLString ?? AppSettings.defaultServerURLString
    /// Editable API key for the settings sheet. Persisted via `AppSettings`
    /// and committed together with the URL (both need a reconnect to apply).
    @Published var apiKeyString: String = AppSettings.apiKey
    /// Whether personal-context tools (calendar, contacts, photos) are
    /// enabled. Persisted; flipped only through `setPersonalContextEnabled`,
    /// which owns the authorization + announce side effects.
    @Published private(set) var personalContextEnabled = AppSettings.personalContextEnabled

    /// The wire→UI state machine; single source of truth, mirrored above.
    let model = HarnessSessionModel()

    private var mic: MicCapture?
    private var client: HarnessClient?
    private var player: AudioPlayer?
    private var drainTask: Task<Void, Never>?
    /// Set when `turn.completed` arrived while audio was still pending.
    private var turnCompletedPending = false
    private var cancellables = Set<AnyCancellable>()
    /// Personal-context tool server (calendar + contacts + photos providers),
    /// built lazily — only once the user enables the feature — so opt-out
    /// users get zero TCC prompts and the provider objects are never even
    /// constructed. Cached: authorization is sticky, so one instance serves
    /// every session. No raw-store tier on iOS (chat.db/Mail/Notes scripting
    /// don't exist here) — the provider list is exactly the v1 set.
    private var personalContextServer: PersonalContextServer?

    /// Identity keys announced with `context.announce` (federation). Computed
    /// once: the resolver probes CloudKit only while the `AppSettings` cache
    /// is empty, then the keys persist until re-resolve.
    @Published private(set) var identityKeys: [String] = AppSettings.identityKeys

    /// Test seam: builds the production resolver (icloud > platform_uuid >
    /// me_email > manual). On iOS the platform-uuid and me-email providers
    /// compile to no-ops (they always yield nil — macOS-only APIs), and the
    /// icloud provider requires the iCloud entitlement (unavailable on free
    /// personal teams). The manual key is therefore the working cross-device
    /// bridge on iOS. Unsafe-annotated like `AppSettings.defaults` — replaced
    /// only from test setup, read at start.
    nonisolated(unsafe) static var identityResolverFactory: () -> IdentityResolver = {
        IdentityResolver(providers: [
            ICloudAccountId(),
            PlatformUUIDProvider(),
            MeEmailProvider(),
            ManualKeyProvider(),
        ])
    }

    /// Commits the manual identity key (cross-device bridge): persists it,
    /// invalidates the key cache, and re-announces on the live session so the
    /// server regroups immediately. Empty clears the key.
    func setManualIdentityKey(_ value: String) {
        AppSettings.setManualIdentityKey(value)
        AppSettings.setIdentityKeys([]) // force re-resolve on next start
        identityKeys = AppSettings.identityKeys
        if let client {
            Task { [weak self] in
                guard let self else { return }
                let keys = await Self.identityResolverFactory().identityKeys()
                AppSettings.setIdentityKeys(keys)
                self.identityKeys = keys
                let descriptors = await self.personalContextServer?.announceProviders() ?? []
                try? await client.sendAnnounce(providers: descriptors,
                                               identityKeys: keys)
            }
        }
    }

    private init() {
        // @Published projections emit *after* mutation — safe to mirror here.
        model.$phase.dropFirst().sink { [weak self] in self?.phase = $0 }.store(in: &cancellables)
        model.$transcript.dropFirst().sink { [weak self] in
            self?.transcript = $0.isEmpty ? nil : $0
        }.store(in: &cancellables)
        model.$responseText.dropFirst().sink { [weak self] in self?.responseText = $0 }.store(in: &cancellables)
        model.$activity.dropFirst().sink { [weak self] in self?.activity = $0 }.store(in: &cancellables)
        model.$errorMessage.dropFirst().sink { [weak self] in self?.errorMessage = $0 }.store(in: &cancellables)
    }

    /// Resolves identity keys exactly once (federation): the detached
    /// resolver task runs only while the `AppSettings` cache is empty —
    /// an empty result stays uncached so the next start re-resolves
    /// (offline-tolerant: iCloud may work later). The announce always sends
    /// whatever is known at start time; the detached task's result lands in
    /// the cache for subsequent sessions.
    private func resolveIdentityKeysIfNeeded() {
        guard AppSettings.identityKeys.isEmpty else {
            identityKeys = AppSettings.identityKeys
            return
        }
        let resolver = Self.identityResolverFactory()
        Task.detached(priority: .utility) { [weak self] in
            let keys = await resolver.identityKeys()
            guard !keys.isEmpty else { return } // stay uncached → re-resolve later
            AppSettings.setIdentityKeys(keys)
            await MainActor.run { self?.identityKeys = keys }
        }
    }

    func toggle() async {
        if running { stop() } else { await start() }
    }

    /// Enables/disables the personal-context tools (calendar, contacts,
    /// photos). Enabling is the one deliberate TCC moment: requests access for
    /// all three providers up front, then announces the authorized set on the
    /// live session (or defers to the next `start` when offline). Disabling
    /// announces an empty provider list to clear the server-side catalog.
    func setPersonalContextEnabled(_ enabled: Bool) {
        AppSettings.setPersonalContextEnabled(enabled)
        personalContextEnabled = enabled
        guard enabled else {
            // Clear the catalog server-side; stop routing tool calls.
            if let client {
                Task { [weak self] in
                    try? await client.sendAnnounce(providers: [],
                                                   identityKeys: self?.identityKeys ?? [])
                }
            }
            return
        }
        // Building the server and asking for descriptors fires the TCC
        // prompts (first time) — off the button tap so the UI stays live.
        Task { [weak self] in
            guard let self else { return }
            let server = self.getOrBuildPersonalContextServer()
            let descriptors = await server.announceProviders()
            guard self.personalContextEnabled else { return } // toggled off mid-prompt
            if let client = self.client {
                try? await client.sendAnnounce(providers: descriptors,
                                               identityKeys: self.identityKeys)
            }
        }
    }

    /// Lazily constructs the personal-context server (one instance, cached).
    /// The v1 provider set only — no raw-store tier on iOS (chat.db, Mail,
    /// Notes scripting don't exist here). Constructing the providers is
    /// prompt-free; authorization fires when `announceProviders()` walks the
    /// gates, which only the toggle handler (or an enabled session start)
    /// triggers.
    private func getOrBuildPersonalContextServer() -> PersonalContextServer {
        if let personalContextServer { return personalContextServer }
        let server = PersonalContextServer(providers: [
            EventKitProvider(),
            ContactsProvider(),
            PhotosProvider(),
        ])
        personalContextServer = server
        return server
    }

    /// Commits the settings field to AppSettings. Returns an error message for
    /// invalid input (nothing stored), nil on success. A running session is
    /// stopped so the next start reconnects to the committed endpoint.
    @discardableResult
    func commitServerURL(_ raw: String, apiKey: String? = nil) -> String? {
        guard let sanitized = AppSettings.sanitizeServerURLString(raw) else {
            return "Enter a ws:// or wss:// URL with a host (e.g. \(AppSettings.defaultServerURLString))"
        }
        AppSettings.setServerURLString(sanitized)
        serverURLString = sanitized.isEmpty ? AppSettings.defaultServerURLString : sanitized
        if let apiKey {
            AppSettings.setAPIKey(apiKey)
            apiKeyString = AppSettings.apiKey
        }
        if running { stop() }
        return nil
    }

    func start() async {
        guard !running, !isBusy else { return }
        isBusy = true
        defer { isBusy = false }
        errorMessage = nil
        model.reset()

        // Identity federation: resolve keys once (detached, cache-gated) so
        // they ride every announce this session and later ones. Runs before
        // the mic prompt so the resolver task works in parallel with it.
        resolveIdentityKeysIfNeeded()

        // iOS DELTA: permission must be granted and the session category set
        // BEFORE any engine work, or the input tap yields silence.
        guard await Self.requestMicPermission() else {
            errorMessage = "Microphone permission denied — enable it in Settings."
            return
        }
        do {
            try AudioSessionConfig.activateForVoice()
        } catch {
            errorMessage = "Audio session unavailable: \(error.localizedDescription)"
            return
        }

        // One AVAudioEngine total: MicCapture shares the player's engine
        // (two engines fight over the Bluetooth route — playback goes silent).
        let serverURL = AppSettings.serverURL
        let player = AudioPlayer()
        player.rate = playbackRate
        let transport = URLSessionTransport(url: serverURL, apiKey: AppSettings.apiKey)
        let client = HarnessClient(transport: transport, model: model)
        // Personal-context tools ride this session when enabled: the client
        // routes incoming `personal.*` tool calls to the server, which answers
        // via the same transport. Fresh session ⇒ fresh announce right after
        // `session.start` (the server resets its per-session catalog there).
        if AppSettings.personalContextEnabled {
            client.personalContextServer = getOrBuildPersonalContextServer()
        }
        do {
            try await client.start(deviceId: deviceName())
        } catch {
            errorMessage = "Can't reach harness server at \(serverURL.host ?? "?"): \(error.localizedDescription)"
            return
        }
        // The announce covers the full installed catalog — identity keys ride
        // along (federation), whatever is known at this point.
        if AppSettings.personalContextEnabled, let server = client.personalContextServer {
            do {
                try await client.sendAnnounce(providers: await server.announceProviders(),
                                              identityKeys: identityKeys)
            } catch {
                // Non-fatal: the session still works, just without personal
                // tools until the next announce (toggle flip or reconnect).
            }
        }
        client.onChunk = { [weak player] base64, _ in
            player?.scheduleChunk(base64: base64)
        }
        // `turn.completed` while audio is queued is absorbed by the model;
        // the drain watcher completes the turn → listening on playback drain.
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
        AudioSessionConfig.deactivate() // iOS DELTA
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
                client?.playbackDidDrain() // reopen the mic gate (echo drop)
                if turnCompletedPending {
                    turnCompletedPending = false
                    model.apply(.turnCompleted) // audioPending now false → listening
                }
            }
            try? await Task.sleep(for: .milliseconds(100))
        }
    }

    private func deviceName() -> String { UIDevice.current.name } // iOS DELTA

    private static func requestMicPermission() async -> Bool {
        await withCheckedContinuation { cont in
            AVAudioApplication.requestRecordPermission { granted in
                cont.resume(returning: granted)
            }
        }
    }
}
