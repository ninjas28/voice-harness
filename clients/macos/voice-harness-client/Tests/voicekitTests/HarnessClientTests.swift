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
}
