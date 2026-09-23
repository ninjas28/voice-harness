import XCTest
@testable import voicekit

/// Provider double whose gate and descriptor are injectable so announce
/// filtering can be exercised without touching any real framework.
private struct GatedProvider: PersonalContextProvider {
    let id: String
    let descriptor: ProviderDescriptor?
    let gate: PersonalContextGate

    var providerId: String { id }
    func currentDescriptor() async -> ProviderDescriptor? { descriptor }
    func execute(name: String, argumentsJSON: String) async throws -> String {
        "EXEC:\(name):\(argumentsJSON)"
    }
}

/// Provider double with NO gate of its own — proves the protocol-extension
/// default exists and passes for v1 providers that never mention gates.
private struct MinimalProvider: PersonalContextProvider {
    var providerId: String { "minimal" }
    func currentDescriptor() async -> ProviderDescriptor? { nil }
    func execute(name: String, argumentsJSON: String) async throws -> String { "" }
}

/// Gate double with a fixed answer.
private struct FixedGate: PersonalContextGate {
    let allowed: Bool
    func verifyAccess() async -> Bool { allowed }
}

final class PersonalContextGateTests: XCTestCase {
    func testPassthroughGateAlwaysTrue() async {
        let passed = await PassthroughGate().verifyAccess()
        XCTAssertTrue(passed, "the v1 default gate must always pass")
    }

    func testProtocolDefaultGateIsPassthrough() async {
        let passed = await MinimalProvider().gate.verifyAccess()
        XCTAssertTrue(passed, "v1 providers need zero changes: default gate passes")
    }

    func testFixedGateReturnsItsAnswer() async {
        let allowed = await FixedGate(allowed: true).verifyAccess()
        let denied = await FixedGate(allowed: false).verifyAccess()
        XCTAssertTrue(allowed)
        XCTAssertFalse(denied)
    }

    func testAnnounceOmitsProviderWhenGateFails() async {
        let server = PersonalContextServer(providers: [
            GatedProvider(id: "messages",
                          descriptor: descriptor(id: "messages", tool: "recent"),
                          gate: FixedGate(allowed: false)),
        ])
        let message = await server.announceMessage()
        XCTAssertEqual(message, .contextAnnounce(providers: [], identityKeys: []),
                       "a failed gate omits the provider even with a descriptor")
    }

    func testAnnounceIncludesProviderWhenGatePasses() async {
        let server = PersonalContextServer(providers: [
            GatedProvider(id: "messages",
                          descriptor: descriptor(id: "messages", tool: "recent"),
                          gate: FixedGate(allowed: true)),
        ])
        let message = await server.announceMessage()
        XCTAssertEqual(message, .contextAnnounce(
            providers: [descriptor(id: "messages", tool: "recent")], identityKeys: []))
    }

    func testAnnounceOmitsWhenGatePassesButDescriptorIsNil() async {
        // Both conditions must hold: gate passes AND descriptor != nil.
        let server = PersonalContextServer(providers: [
            GatedProvider(id: "messages", descriptor: nil, gate: FixedGate(allowed: true)),
        ])
        let message = await server.announceMessage()
        XCTAssertEqual(message, .contextAnnounce(providers: [], identityKeys: []))
    }

    func testAnnounceMixesGatedAndUngatedProviders() async {
        // A v1 provider (default passthrough gate) and a raw-store provider
        // (failing gate) together: only the v1 one is announced.
        let server = PersonalContextServer(providers: [
            GatedProvider(id: "messages", descriptor: descriptor(id: "messages", tool: "recent"),
                          gate: FixedGate(allowed: false)),
            GatedProvider(id: "contacts", descriptor: descriptor(id: "contacts", tool: "search"),
                          gate: PassthroughGate()),
        ])
        let message = await server.announceMessage()
        XCTAssertEqual(message, .contextAnnounce(
            providers: [descriptor(id: "contacts", tool: "search")], identityKeys: []))
    }

    private func descriptor(id: String, tool: String) -> ProviderDescriptor {
        .init(id: id, tools: [.init(name: tool, description: "List \(tool).",
                                    parameters: ["type": .string("object")])])
    }
}
