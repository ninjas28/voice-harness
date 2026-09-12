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

    public init(url: URL) {
        task = URLSession.shared.webSocketTask(with: url)
        // Connect eagerly: a send() on a never-resumed task suspends forever.
        task.resume()
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
                    self?.onChunk?(pcm, seq)
                }
                self?.onMessage?(message)
            }
        }
        try await transport.send(ClientMessage.sessionStart(deviceId: deviceId, sampleRate: 16000).encode())
    }

    public func sendAudio(base64: String) async {
        VHSendLog.log("sendAudio \(base64.count) chars")
        do {
            try await transport.send(ClientMessage.audioData(pcm: base64).encode())
            VHSendLog.log("sendAudio ok")
        } catch {
            VHSendLog.log("sendAudio ERROR: \(error)")
        }
    }

    public func sendSpeechEnd() async {
        try? await transport.send(ClientMessage.speechEnd.encode())
    }

    public func stop() async {
        try? await transport.send(ClientMessage.sessionStop.encode())
        await transport.close()
    }
}

/// File-based send-path tracing (VH_DEBUG_SEND=1): unified logging redacts
/// dynamic values, so attempts/results go to /tmp/vh-send.log instead.
public enum VHSendLog {
    static let path = "/tmp/vh-send.log"
    public static var enabled: Bool {
        ProcessInfo.processInfo.environment["VH_DEBUG_SEND"] == "1"
    }
    public static func log(_ line: String) {
        guard enabled else { return }
        let data = Data((line + "\n").utf8)
        if let fh = FileHandle(forWritingAtPath: path) {
            defer { try? fh.close() }
            fh.seekToEndOfFile()
            fh.write(data)
        } else {
            try? data.write(to: URL(fileURLWithPath: path))
        }
    }
    public static func reset() {
        try? FileManager.default.removeItem(atPath: path)
    }
}
