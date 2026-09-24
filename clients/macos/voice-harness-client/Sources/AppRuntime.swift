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
    /// Mirrors `model.activity`: what the assistant is doing under `thinking`.
    @Published private(set) var activity: HarnessSessionModel.Activity = .idle
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
    /// Editable API key for the settings field. Persisted via `AppSettings`
    /// and committed together with the URL (both need a reconnect to apply).
    @Published var apiKeyString: String = AppSettings.apiKey
    /// Whether personal-context tools (calendar, contacts, photos) are
    /// enabled. Persisted; flipped only through `setPersonalContextEnabled`,
    /// which owns the authorization + announce side effects.
    @Published private(set) var personalContextEnabled = AppSettings.personalContextEnabled
    /// Whether raw-store providers (messages, mail, notes) are enabled.
    /// Persisted; flipped only through `setRawStoresEnabled`, which owns the
    /// gate verification (the deliberate TCC moment) + announce side effects.
    @Published private(set) var rawStoresEnabled = AppSettings.rawStoresEnabled

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
    /// every session.
    private var personalContextServer: PersonalContextServer?

    /// Identity keys announced with `context.announce` (federation). Computed
    /// once: the resolver probes CloudKit / IOKit / Contacts only while the
    /// `AppSettings` cache is empty, then the keys persist until re-resolve.
    @Published private(set) var identityKeys: [String] = AppSettings.identityKeys

    /// Test seam: builds the production resolver (icloud > platform_uuid >
    /// me_email > manual). Tests replace it with stub providers.
    /// Unsafe-annotated like `AppSettings.defaults` — replaced only from test
    /// setup, read at start.
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
    /// server regroups immediately (no reconnect needed). Empty clears the
    /// key — the next announce simply omits it.
    func setManualIdentityKey(_ value: String) {
        AppSettings.setManualIdentityKey(value)
        AppSettings.setIdentityKeys([]) // force re-resolve on next start
        identityKeys = AppSettings.identityKeys
        if let client {
            Task { [weak self] in
                guard let self else { return }
                // Re-resolve synchronously (manual key reads settings; the
                // automatic providers are cheap probes) and announce.
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

    /// Resolves identity keys exactly once (plan Task 6): the detached
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

    /// Commits the settings field to `AppSettings`. Returns an error message
    /// for invalid input (nothing is stored), or nil on success. Clearing the
    /// field restores the default URL. A running session is stopped so the
    /// next start reconnects to the committed endpoint.
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
        // they ride every announce this session and later ones.
        resolveIdentityKeysIfNeeded()

        let serverURL = AppSettings.serverURL
        // Order matters on macOS: the input tap must be installed and the
        // engine prepared BEFORE engine.start() — tapping a running engine's
        // input node silently never fires. So: create player + client, install
        // mic tap on the shared engine (prepared, not started), then one
        // engine start negotiates the Bluetooth route for both directions.
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
        // The announce covers the full installed catalog (raw providers are
        // inside the server exactly when both toggles are on, and their
        // descriptors appear only when their gates pass — verified at the
        // toggle flip; probes here are quiet re-checks).
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

    /// Enables/disables the personal-context tools (calendar, contacts,
    /// photos). Enabling is the one deliberate TCC moment: requests access for
    /// all three providers up front, then announces the authorized set on the
    /// live session (or defers to the next `start` when offline). Disabling
    /// announces an empty provider list to clear the server-side catalog —
    /// for everything, including raw stores when they were riding along.
    func setPersonalContextEnabled(_ enabled: Bool) {
        AppSettings.setPersonalContextEnabled(enabled)
        personalContextEnabled = enabled
        guard enabled else {
            // Clear the catalog server-side; stop routing tool calls.
            if let client {
                Task { try? await client.sendAnnounce(providers: [], identityKeys: identityKeys) }
            }
            return
        }
        // Building the server and asking for descriptors fires the TCC
        // prompts (first time) — off the button tap so the panel stays live.
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

    #if os(macOS)
    /// Enables/disables the raw-store providers (messages, mail, notes).
    /// Enabling verifies their gates here — read-only SQLite opens and the
    /// osascript probe, the deliberate FDA/Automation TCC moment — then
    /// re-announces the live catalog. Disabling re-announces the remaining
    /// v1 catalog (or an empty list when the master toggle is off too),
    /// which clears the raw tools server-side. Gates fire only from this
    /// toggle flip, never in tests or unattended runs.
    func setRawStoresEnabled(_ enabled: Bool) {
        AppSettings.setRawStoresEnabled(enabled)
        rawStoresEnabled = enabled
        // The cached server must reflect the new provider set; rebuilding is
        // cheap (providers are stateless beyond sticky authorization).
        personalContextServer = nil
        guard enabled else {
            Task { [weak self] in
                guard let self else { return }
                guard self.personalContextEnabled, let client = self.client else { return }
                let server = self.getOrBuildPersonalContextServer()
                try? await client.sendAnnounce(providers: await server.announceProviders(),
                                               identityKeys: self.identityKeys)
            }
            return
        }
        // Verify gates + announce off the button tap: the probes may block
        // briefly, and first-time FDA/Automation prompts appear here only.
        Task { [weak self] in
            guard let self else { return }
            guard self.personalContextEnabled else { return } // master toggle off
            let server = self.getOrBuildPersonalContextServer() // now carries raw providers
            let descriptors = await server.announceProviders() // gates fire here
            guard self.rawStoresEnabled, self.personalContextEnabled else { return } // toggled off mid-probe
            if let client = self.client {
                try? await client.sendAnnounce(providers: descriptors,
                                               identityKeys: self.identityKeys)
            }
        }
    }
    #endif

    /// Lazily constructs the personal-context server (one instance, cached).
    /// On macOS, when the raw-stores tier is enabled, the provider list gains
    /// the raw-store providers (messages, mail, notes) so ONE server routes
    /// `personal.*` calls for everything. Raw providers are constructed only
    /// behind the second toggle (#if os(macOS); they do not exist on iOS).
    private func getOrBuildPersonalContextServer() -> PersonalContextServer {
        if let personalContextServer { return personalContextServer }
        var providers: [any PersonalContextProvider] = [
            EventKitProvider(),
            ContactsProvider(),
            PhotosProvider(),
        ]
        #if os(macOS)
        if AppSettings.rawStoresEnabled {
            providers.append(MessagesProvider())
            providers.append(MailProvider())
            providers.append(NotesProvider())
        }
        #endif
        let server = PersonalContextServer(providers: providers)
        personalContextServer = server
        return server
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
                client?.playbackDidDrain() // reopen the mic gate (echo drop)
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
