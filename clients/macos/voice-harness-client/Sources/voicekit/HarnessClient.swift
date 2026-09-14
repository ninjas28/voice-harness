import Foundation

/// A bidirectional text-frame transport for the realtime session.
///
/// The receive handler is part of the protocol so `HarnessClient` owns a
/// single decode → dispatch pump regardless of transport implementation
/// (URLSession in production, a stub in tests) — no duplicated pumps.
public protocol Transport: Actor {
    /// Sends one text frame. Throws when the connection is unusable.
    func send(_ text: String) async throws
    /// Installs the single incoming-frame handler; call with `nil` to clear.
    /// Must invoke the handler (awaiting it) for every subsequent text frame,
    /// in arrival order.
    func setReceiveHandler(_ handler: (@Sendable (String) async -> Void)?) async
    /// Closes the connection.
    func close() async
}

/// Production transport over `URLSessionWebSocketTask`.
///
/// One long-lived receive pump: each completed receive re-arms the next,
/// forwarding every text frame to the installed handler.
public actor URLSessionTransport: Transport {
    private let task: URLSessionWebSocketTask
    private var receiveHandler: (@Sendable (String) async -> Void)?
    private var receiveLoopRunning = false

    public init(url: URL, apiKey: String = "") {
        task = URLSession.shared.webSocketTask(with: Self.makeHandshakeRequest(url: url, apiKey: apiKey))
        // Connect eagerly: a send() on a never-resumed task suspends forever.
        task.resume()
    }

    /// Builds the websocket handshake request. The API key travels ONLY in
    /// the `Authorization: Bearer <key>` header — never in the URL, whose
    /// query string must survive untouched (deployed clients store
    /// `?token=…` in the server URL). An empty/whitespace key omits the
    /// header entirely, preserving the no-auth localhost workflow.
    public static func makeHandshakeRequest(url: URL, apiKey: String) -> URLRequest {
        var request = URLRequest(url: url)
        let key = apiKey.trimmingCharacters(in: .whitespacesAndNewlines)
        if !key.isEmpty {
            request.setValue("Bearer \(key)", forHTTPHeaderField: "Authorization")
        }
        return request
    }

    public func setReceiveHandler(_ handler: (@Sendable (String) async -> Void)?) {
        receiveHandler = handler
        if handler != nil, !receiveLoopRunning {
            receiveLoopRunning = true
            pump()
        }
    }

    public func send(_ text: String) async throws {
        try await task.send(.string(text))
    }

    public func close() {
        receiveHandler = nil
        task.cancel(with: .goingAway, reason: nil)
    }

    /// Single long-lived pump: awaits one frame, delivers it to the handler
    /// (in arrival order), then re-arms. Stops when the handler is cleared.
    private func pump() {
        task.receive { [weak self] result in
            guard let self else { return }
            Task { await self.deliver(result) }
        }
    }

    private func deliver(_ result: Result<URLSessionWebSocketTask.Message, Error>) {
        guard let handler = receiveHandler else {
            receiveLoopRunning = false // closed
            return
        }
        switch result {
        case .success(.string(let text)):
            Task {
                await handler(text)
                await self.pump() // re-arm only after the handler finished
            }
        case .success: // non-text frame (pong/binary): keep pumping
            pump()
        case .failure:
            receiveLoopRunning = false // socket gone; do not spin an error loop
        }
    }
}

/// Drives one realtime session: owns the transport, decodes incoming server
/// frames onto the model, and forwards audio chunks to `onChunk`.
///
/// Plain class (not @MainActor) with async entry points — the model itself is
/// @MainActor, so all state mutation is still serialized on the main actor.
public final class HarnessClient: @unchecked Sendable {
    private let transport: any Transport
    private let model: HarnessSessionModel
    /// Mic gate: closed from the first `audio.chunk` until the runtime calls
    /// `playbackDidDrain()` (its drain watcher runs it right after
    /// `model.audioDidFinish()`). While closed, outgoing audio.data/speech.end
    /// are dropped — the mic hears our own TTS (no perfect AEC) and the server
    /// VAD would treat the echo as user speech, re-triggering the turn loop.
    private let gateLock = NSLock()
    private var micGated = false
    /// Called for each `audio.chunk` (base64 PCM16, seq).
    public var onChunk: (@Sendable (String, Int) -> Void)?
    /// Called for every decoded server message *after* it was applied to the
    /// model (on the main actor). Lets the runtime observe frames the model
    /// absorbs silently (e.g. `turn.completed` while audio is pending).
    public var onMessage: (@Sendable (ServerMessage) -> Void)?

    public init(transport: any Transport, model: HarnessSessionModel) {
        self.transport = transport
        self.model = model
    }

    /// Connects and sends `session.start`. Throws if the server is
    /// unreachable — the caller should surface that, not hang.
    public func start(deviceId: String?) async throws {
        let model = self.model
        await transport.setReceiveHandler { [weak self, weak model] text in
            guard let model, let message = try? ServerMessage.decode(text) else { return }
            await MainActor.run {
                model.apply(message)
                if case .audioChunk(let pcm, let seq) = message {
                    self?.notePlaybackStarted()
                    self?.onChunk?(pcm, seq)
                }
                self?.onMessage?(message)
            }
        }
        try await transport.send(ClientMessage.sessionStart(deviceId: deviceId, sampleRate: 16000).encode())
    }

    /// Called by the runtime when queued playback has drained (the drain
    /// watcher runs it right after `model.audioDidFinish()`); reopens the mic
    /// gate so real user speech streams again.
    public func playbackDidDrain() {
        gateLock.lock()
        micGated = false
        gateLock.unlock()
    }

    private func notePlaybackStarted() {
        gateLock.lock()
        micGated = true
        gateLock.unlock()
    }

    /// True while outgoing mic audio must be suppressed (playback pending).
    private func micIsGated() -> Bool {
        gateLock.lock()
        defer { gateLock.unlock() }
        return micGated
    }

    public func sendAudio(base64: String) async {
        guard !micIsGated() else { return } // echo of our own TTS — drop
        try? await transport.send(ClientMessage.audioData(pcm: base64).encode())
    }

    public func sendSpeechEnd() async {
        guard !micIsGated() else { return } // echo, not user speech — drop
        try? await transport.send(ClientMessage.speechEnd.encode())
    }

    public func stop() async {
        try? await transport.send(ClientMessage.sessionStop.encode())
        await transport.close()
    }
}
