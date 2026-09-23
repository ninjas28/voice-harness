import XCTest
@testable import voicekit

/// Fixed-digest provider used as the routing test double. Its execute output
/// embeds the inputs ("EXEC:<name>:<args>") so assertions verify what the
/// server actually routed — no shared mutable state needed.
private struct EchoProvider: PersonalContextProvider {
    let id: String
    let descriptor: ProviderDescriptor?
    let error: Error?

    var providerId: String { id }
    func currentDescriptor() async -> ProviderDescriptor? { descriptor }

    func execute(name: String, argumentsJSON: String) async throws -> String {
        if let error { throw error }
        return "EXEC:\(name):\(argumentsJSON)"
    }
}

/// Collects the ClientMessages the server hands to the send closure.
private actor MessageSink {
    var messages: [ClientMessage] = []
    func add(_ message: ClientMessage) { messages.append(message) }
}

final class PersonalContextServerTests: XCTestCase {
    private func descriptor(id: String, tool: String) -> ProviderDescriptor {
        .init(id: id, tools: [.init(name: tool, description: "List \(tool).",
                                    parameters: ["type": .string("object")])])
    }

    func testToolCallRoutesToProviderAndReplies() async {
        let sink = MessageSink()
        let server = PersonalContextServer(providers: [
            EchoProvider(id: "calendar", descriptor: descriptor(id: "calendar", tool: "events"), error: nil),
        ])
        await server.handle(
            .toolCall(callId: 3, name: "personal.calendar.events", argumentsJSON: "{\"days_ahead\":1}"),
            send: { await sink.add($0) })
        let sent = await sink.messages
        XCTAssertEqual(sent, [.toolResult(callId: 3, ok: true, text: "EXEC:events:{\"days_ahead\":1}")])
    }

    func testUnknownToolSendsErrorResult() async {
        let sink = MessageSink()
        let server = PersonalContextServer(providers: [
            EchoProvider(id: "calendar", descriptor: descriptor(id: "calendar", tool: "events"), error: nil),
        ])
        await server.handle(
            .toolCall(callId: 9, name: "personal.nosuch.tool", argumentsJSON: "{}"),
            send: { await sink.add($0) })
        let sent = await sink.messages
        guard sent.count == 1, case .toolResult(let callId, let ok, let text) = sent[0] else {
            return XCTFail("expected exactly one tool.result, got \(sent)")
        }
        XCTAssertEqual(callId, 9)
        XCTAssertFalse(ok)
        XCTAssertTrue(text.localizedLowercase.contains("unknown"), "text was: \(text)")
    }

    func testUnknownToolOnKnownProviderSendsErrorResult() async {
        // Provider id matches but the bare tool was never announced.
        let sink = MessageSink()
        let server = PersonalContextServer(providers: [
            EchoProvider(id: "calendar", descriptor: descriptor(id: "calendar", tool: "events"), error: nil),
        ])
        await server.handle(
            .toolCall(callId: 4, name: "personal.calendar.nosuch", argumentsJSON: "{}"),
            send: { await sink.add($0) })
        let sent = await sink.messages
        guard sent.count == 1, case .toolResult(_, let ok, let text) = sent[0] else {
            return XCTFail("expected exactly one tool.result, got \(sent)")
        }
        XCTAssertFalse(ok)
        XCTAssertTrue(text.localizedLowercase.contains("unknown"), "text was: \(text)")
    }

    func testProviderFailureSendsErrorResult() async {
        let sink = MessageSink()
        let server = PersonalContextServer(providers: [
            EchoProvider(id: "calendar", descriptor: descriptor(id: "calendar", tool: "events"),
                         error: PersonalContextError.failed("calendar is locked")),
        ])
        await server.handle(
            .toolCall(callId: 5, name: "personal.calendar.events", argumentsJSON: "{}"),
            send: { await sink.add($0) })
        let sent = await sink.messages
        guard sent.count == 1, case .toolResult(let callId, let ok, let text) = sent[0] else {
            return XCTFail("expected exactly one tool.result, got \(sent)")
        }
        XCTAssertEqual(callId, 5)
        XCTAssertFalse(ok)
        XCTAssertTrue(text.contains("calendar is locked"), "text was: \(text)")
    }

    func testNotAuthorizedFailureSendsSpeakableText() async {
        let sink = MessageSink()
        let server = PersonalContextServer(providers: [
            EchoProvider(id: "calendar", descriptor: descriptor(id: "calendar", tool: "events"),
                         error: PersonalContextError.notAuthorized("calendar")),
        ])
        await server.handle(
            .toolCall(callId: 6, name: "personal.calendar.events", argumentsJSON: "{}"),
            send: { await sink.add($0) })
        let sent = await sink.messages
        guard sent.count == 1, case .toolResult(_, let ok, let text) = sent[0] else {
            return XCTFail("expected exactly one tool.result, got \(sent)")
        }
        XCTAssertFalse(ok)
        XCTAssertTrue(text.contains("calendar access has not been granted"), "text was: \(text)")
    }

    func testNonPersonalToolCallIsIgnored() async {
        let sink = MessageSink()
        let server = PersonalContextServer(providers: [])
        await server.handle(
            .toolCall(callId: 1, name: "time.now_utc", argumentsJSON: "{}"),
            send: { await sink.add($0) })
        await server.handle(.turnCompleted, send: { await sink.add($0) })
        let sent = await sink.messages
        XCTAssertTrue(sent.isEmpty, "non-personal frames must be ignored, got \(sent)")
    }

    func testOversizedDigestIsClamped() async {
        let sink = MessageSink()
        let server = PersonalContextServer(providers: [
            EchoProvider(id: "calendar", descriptor: descriptor(id: "calendar", tool: "events"), error: nil),
        ])
        // EchoProvider embeds the arguments, so a huge arguments string makes
        // a huge digest without another test double.
        await server.handle(
            .toolCall(callId: 7, name: "personal.calendar.events",
                      argumentsJSON: String(repeating: "x", count: 10_000)),
            send: { await sink.add($0) })
        let sent = await sink.messages
        guard sent.count == 1, case .toolResult(_, let ok, let text) = sent[0] else {
            return XCTFail("expected exactly one tool.result, got \(sent)")
        }
        XCTAssertTrue(ok)
        XCTAssertTrue(text.contains("…(truncated)"), "digest must carry the truncation marker")
        XCTAssertLessThanOrEqual(text.utf8.count, 8192 + "…(truncated)".utf8.count)
    }

    func testAnnounceMessageOmitsUnauthorizedProviders() async {
        let server = PersonalContextServer(providers: [
            EchoProvider(id: "calendar", descriptor: nil, error: nil),
            EchoProvider(id: "contacts", descriptor: descriptor(id: "contacts", tool: "search"), error: nil),
        ])
        let message = await server.announceMessage()
        XCTAssertEqual(message, .contextAnnounce(providers: [descriptor(id: "contacts", tool: "search")], identityKeys: []))
    }

    func testAnnounceWithNoAuthorizedProvidersIsEmptyList() async {
        let server = PersonalContextServer(providers: [
            EchoProvider(id: "calendar", descriptor: nil, error: nil),
        ])
        let message = await server.announceMessage()
        XCTAssertEqual(message, .contextAnnounce(providers: [], identityKeys: []))
    }
}
