import XCTest
@testable import voicekit

@MainActor
final class HarnessClientTests: XCTestCase {
    actor StubTransport: Transport {
        private(set) var sent: [String] = []
        private(set) var receiveHandler: (@Sendable (String) async -> Void)?
        private(set) var closed = false

        func send(_ text: String) async throws {
            sent.append(text)
        }

        func setReceiveHandler(_ handler: (@Sendable (String) async -> Void)?) async {
            receiveHandler = handler
        }

        func close() async {
            closed = true
        }

        /// Delivers one frame through the installed handler, in order.
        func simulateIncoming(_ text: String) async {
            await receiveHandler?(text)
        }
    }

    @MainActor
    private func makeClient() -> (HarnessClient, StubTransport, HarnessSessionModel) {
        let stub = StubTransport()
        let model = HarnessSessionModel()
        let client = HarnessClient(transport: stub, model: model)
        return (client, stub, model)
    }

    func testSendFailsFastWhenServerUnreachable() async {
        // Regression: URLSessionWebSocketTask.send() suspends forever if the
        // task was never resumed — the transport must connect eagerly.
        let transport = URLSessionTransport(url: URL(string: "ws://127.0.0.1:9/v1/realtime")!)
        let sent = expectation(description: "send completed (success or throw)")
        Task {
            _ = try? await transport.send("ping")
            sent.fulfill()
        }
        await fulfillment(of: [sent], timeout: 5.0)
    }

    func testStartSendsSessionStart() async throws {
        let (client, stub, _) = makeClient()
        try await client.start(deviceId: "test")
        let sent = await stub.sent
        XCTAssertEqual(sent.first,
                       #"{"type":"session.start","device_id":"test","sample_rate":16000}"#)
        XCTAssertEqual(sent.count, 1)
        await client.stop()
        let closed = await stub.closed
        XCTAssertTrue(closed)
    }

    func testIncomingFramesAreMappedOntoModel() async throws {
        let (client, stub, model) = makeClient()
        try await client.start(deviceId: nil)
        await stub.simulateIncoming(#"{"type":"transcript","text":"hi"}"#)
        await stub.simulateIncoming(#"{"type":"state","state":"thinking"}"#)
        await stub.simulateIncoming(#"{"type":"response.text.delta","text":"He"}"#)
        XCTAssertEqual(model.transcript, "hi")
        XCTAssertEqual(model.responseText, "He")
        XCTAssertEqual(model.phase, .thinking)
        await client.stop()
    }

    func testAudioChunkReachesOnChunkAndModel() async throws {
        let (client, stub, model) = makeClient()
        final class ChunkBox: @unchecked Sendable {
            let lock = NSLock()
            var chunks: [(base64: String, seq: Int)] = []
            func append(_ b: String, _ s: Int) {
                lock.lock(); chunks.append((b, s)); lock.unlock()
            }
        }
        let box = ChunkBox()
        try await client.start(deviceId: nil)
        client.onChunk = { base64, seq in box.append(base64, seq) }
        await stub.simulateIncoming(#"{"type":"audio.chunk","pcm":"QUJD","seq":3}"#)
        let collected = box.lock.withLock { box.chunks }
        XCTAssertEqual(collected.map(\.base64), ["QUJD"])
        XCTAssertEqual(collected.map(\.seq), [3])
        XCTAssertEqual(model.phase, .speaking)
        XCTAssertTrue(model.audioPending)
        await client.stop()
    }

    func testSendersEmitClientFrames() async throws {
        let (client, stub, _) = makeClient()
        try await client.start(deviceId: nil)
        await client.sendAudio(base64: "QUJD")
        await client.sendSpeechEnd()
        await client.stop()
        let sent = await stub.sent
        XCTAssertEqual(sent.dropFirst().first, #"{"type":"audio.data","pcm":"QUJD"}"#)
        XCTAssertEqual(sent.dropFirst(2).first, #"{"type":"speech.end"}"#)
        XCTAssertEqual(sent.last, #"{"type":"session.stop"}"#)
    }

    /// Regression: the server emits state:listening immediately after
    /// turn.completed — it cannot know when the client finishes playing the
    /// queued audio. While audio is pending, the client must OWN the
    /// speaking→listening transition (the drain watcher does it), or the UI
    /// flips back to listening mid-playback and kills the response early.
    func testStateListeningIgnoredWhileAudioPending() async throws {
        let (client, stub, model) = makeClient()
        try await client.start(deviceId: nil)
        await stub.simulateIncoming(#"{"type":"audio.chunk","pcm":"QUJD","seq":0}"#)
        XCTAssertEqual(model.phase, .speaking)
        XCTAssertTrue(model.audioPending)
        // Server-side turn bookkeeping arrives while audio still queued:
        await stub.simulateIncoming(#"{"type":"turn.completed"}"#)
        XCTAssertEqual(model.phase, .speaking, "turn.completed absorbed while audio pending")
        await stub.simulateIncoming(#"{"type":"state","state":"listening"}"#)
        XCTAssertEqual(model.phase, .speaking, "state:listening must NOT override pending audio")
        await stub.simulateIncoming(#"{"type":"state","state":"thinking"}"#)
        XCTAssertEqual(model.phase, .speaking, "state:thinking must NOT override pending audio either")
        // Server VAD hears the client's own TTS through the mic (no AEC) and
        // emits state:speech — that's self-echo, not the user interrupting.
        await stub.simulateIncoming(#"{"type":"state","state":"speech"}"#)
        XCTAssertEqual(model.phase, .speaking, "state:speech must NOT override pending audio (self-echo)")
        // While pending, the client owns the phase entirely.
        await stub.simulateIncoming(#"{"type":"state","state":"listening"}"#)
        XCTAssertEqual(model.phase, .speaking)
        // Client finishes playback → drain watcher calls audioDidFinish:
        model.audioDidFinish()
        model.apply(.turnCompleted)
        XCTAssertEqual(model.phase, .listening, "drain watcher completes the transition after playback")
        await client.stop()
    }

    /// Regression: while the client is playing TTS (audio pending), outgoing
    /// audio.data must be dropped — the mic hears our own speaker and the
    /// server VAD treats the echo as user speech, re-triggering the turn loop
    /// (loud on iOS where the speaker sits next to the mic). The gate reopens
    /// when playback drains so normal mic streaming resumes.
    func testOutgoingAudioDroppedWhilePlaybackPending() async throws {
        let (client, stub, model) = makeClient()
        try await client.start(deviceId: nil)
        // Server sends TTS chunk → playback pending → mic gate closes.
        await stub.simulateIncoming(#"{"type":"audio.chunk","pcm":"QUJD","seq":0}"#)
        await client.sendAudio(base64: "RWNobw==") // mic picks up our own TTS
        await client.sendSpeechEnd()
        var sent = await stub.sent
        XCTAssertEqual(sent.filter { $0.contains("audio.data") }.count, 0,
                       "audio.data must be dropped while playback is pending")
        XCTAssertEqual(sent.filter { $0.contains("speech.end") }.count, 0,
                       "speech.end must be dropped too — echo is not user speech")
        // Playback drains (runtime drain watcher → audioDidFinish +
        // playbackDidDrain): gate reopens.
        model.audioDidFinish()
        client.playbackDidDrain()
        await client.sendAudio(base64: "SGVsbG8=")
        sent = await stub.sent
        XCTAssertTrue(sent.contains(#"{"type":"audio.data","pcm":"SGVsbG8="}"#),
                      "mic audio must flow again after playback drains")
        await client.stop()
    }

    /// Regression: mic and playback MUST share one AVAudioEngine. Two engines
    /// on a Bluetooth headset fight over the audio route (A2DP vs HFP) and the
    /// playback engine's route is invalidated — buffers "complete" without a
    /// sound. One engine negotiates the route once for both directions.
    func testMicSharesPlayerEngine() {
        let player = AudioPlayer()
        let mic = MicCapture(engine: player.engine)
        XCTAssertTrue(mic.engine === player.engine)
        // Default init still creates its own engine.
        let standalone = MicCapture()
        XCTAssertFalse(standalone.engine === player.engine)
    }

    // MARK: - Personal context (Task 9)

    /// Fixed-digest provider double: execute output embeds the inputs
    /// ("EXEC:<name>:<args>") so assertions verify what actually routed.
    private struct StubPersonalProvider: PersonalContextProvider {
        let id: String
        let descriptor: ProviderDescriptor?

        var providerId: String { id }
        func currentDescriptor() async -> ProviderDescriptor? { descriptor }

        func execute(name: String, argumentsJSON: String) async throws -> String {
            "EXEC:\(name):\(argumentsJSON)"
        }
    }

    /// After `start`, an installed personalContextServer answers a
    /// `tool.call` with a `tool.result` written to the transport.
    func testToolCallRoutesThroughPersonalContextServer() async throws {
        let (client, stub, _) = makeClient()
        try await client.start(deviceId: nil)
        client.personalContextServer = PersonalContextServer(providers: [
            StubPersonalProvider(id: "calendar",
                                 descriptor: .init(id: "calendar", tools: [
                                    .init(name: "events", description: "List events.",
                                          parameters: ["type": .string("object")]),
                                 ])),
        ])
        await stub.simulateIncoming(
            #"{"type":"tool.call","call_id":11,"name":"personal.calendar.events","arguments":"{}"}"#)
        // The tool.result is written from a detached task off the receive
        // pump — poll for it, bounded (house rule: never an unbounded wait).
        let deadline = Date().addingTimeInterval(2)
        var results: [String] = []
        while Date() < deadline {
            let sent = await stub.sent
            results = sent.filter { $0.contains(#""type":"tool.result""#) }
            if !results.isEmpty { break }
            try await Task.sleep(for: .milliseconds(20))
        }
        XCTAssertEqual(results.count, 1, "expected exactly one tool.result")
        if results.count == 1 {
            XCTAssertTrue(results[0].contains(#""call_id":11"#), "frame was: \(results[0])")
            XCTAssertTrue(results[0].contains(#""ok":true"#), "frame was: \(results[0])")
            XCTAssertTrue(results[0].contains("EXEC:events:{}"), "frame was: \(results[0])")
        }
        await client.stop()
    }

    /// The announce helper encodes through the transport unmodified.
    func testSendAnnounceEncodesContextAnnounce() async throws {
        let (client, stub, _) = makeClient()
        try await client.start(deviceId: nil)
        let descriptor = ProviderDescriptor(id: "calendar", tools: [
            .init(name: "events", description: "List events.",
                  parameters: ["type": .string("object")]),
        ])
        try await client.sendAnnounce(providers: [descriptor])
        let sent = await stub.sent
        XCTAssertTrue(sent.last?.contains(#""type":"context.announce""#) ?? false,
                      "last frame was: \(sent.last ?? "nil")")
        XCTAssertTrue(sent.last?.contains(#""id":"calendar""#) ?? false)
        await client.stop()
    }

    /// An empty announce still encodes and sends (clears the server catalog).
    func testSendAnnounceSendsEmptyProviderList() async throws {
        let (client, stub, _) = makeClient()
        try await client.start(deviceId: nil)
        try await client.sendAnnounce(providers: [])
        let sent = await stub.sent
        XCTAssertEqual(sent.last,
                       #"{"type":"context.announce","providers":[]}"#)
        await client.stop()
    }
}
