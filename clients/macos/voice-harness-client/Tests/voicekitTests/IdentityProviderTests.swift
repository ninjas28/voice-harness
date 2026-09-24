import XCTest
@testable import voicekit

/// Identity-key assembly for personal-context federation (plan
/// 2026-09-21_235410): stubbed providers only — the CloudKit / IOKit /
/// Contacts real paths are compile-verified, never executed here (no
/// network, no TCC prompts from tests).
final class IdentityProviderTests: XCTestCase {
    /// Scratch defaults domain: manual-key tests persist through AppSettings,
    /// which must never touch the app's real settings from tests.
    private let suiteName = "voicekit.IdentityProviderTests"
    private var defaults: UserDefaults!

    override func setUp() {
        defaults = UserDefaults(suiteName: suiteName)
        defaults.removePersistentDomain(forName: suiteName)
        AppSettings.defaults = defaults
    }

    override func tearDown() {
        AppSettings.defaults = .standard
        defaults.removePersistentDomain(forName: suiteName)
    }

    /// Stub identity provider: fixed key tag + value (nil = unavailable).
    private struct StubIdentityProvider: IdentityProvider {
        let keyId: String
        let value: String?

        func currentKey() async -> String? { value }
    }

    // MARK: - IdentityResolver (fixed precedence, dedupe, nil-skip)

    /// Keys come out in the fixed precedence order icloud > platform_uuid >
    /// me_email no matter how the providers were handed to the resolver.
    func testFixedPrecedenceRegardlessOfConstructionOrder() async {
        let resolver = IdentityResolver(providers: [
            StubIdentityProvider(keyId: "me_email", value: "me@example.com"),
            StubIdentityProvider(keyId: "platform_uuid", value: "UUID-1"),
            StubIdentityProvider(keyId: "icloud", value: "rec-1"),
        ])
        let keys = await resolver.identityKeys()
        XCTAssertEqual(keys, ["icloud:rec-1", "platform_uuid:UUID-1", "me_email:me@example.com"])
    }

    /// Unavailable sources (nil) fall back to the next key silently.
    func testNilProvidersAreSkipped() async {
        let resolver = IdentityResolver(providers: [
            StubIdentityProvider(keyId: "icloud", value: nil),
            StubIdentityProvider(keyId: "platform_uuid", value: "UUID-1"),
            StubIdentityProvider(keyId: "me_email", value: "me@example.com"),
        ])
        let keys = await resolver.identityKeys()
        XCTAssertEqual(keys, ["platform_uuid:UUID-1", "me_email:me@example.com"])
    }

    /// Every source unavailable → no keys at all (offline-tolerant).
    func testAllNilYieldsEmpty() async {
        let resolver = IdentityResolver(providers: [
            StubIdentityProvider(keyId: "icloud", value: nil),
            StubIdentityProvider(keyId: "me_email", value: nil),
        ])
        let empty = await resolver.identityKeys()
        XCTAssertEqual(empty, [])
    }

    /// Two providers reporting the same key collapse to one entry.
    func testDuplicateKeysAreDeduped() async {
        let resolver = IdentityResolver(providers: [
            StubIdentityProvider(keyId: "icloud", value: "rec-1"),
            StubIdentityProvider(keyId: "icloud", value: "rec-1"),
            StubIdentityProvider(keyId: "platform_uuid", value: "UUID-1"),
        ])
        let keys = await resolver.identityKeys()
        XCTAssertEqual(keys, ["icloud:rec-1", "platform_uuid:UUID-1"])
    }

    /// Keys are namespaced `<keyId>:<value>`, so the same raw value under
    /// different key tags never dedupes across types.
    func testSameValueUnderDifferentKeyIdsIsNotDeduped() async {
        let resolver = IdentityProviderTests.makeResolver()
        let named = IdentityResolver(providers: [
            StubIdentityProvider(keyId: "icloud", value: "shared"),
            StubIdentityProvider(keyId: "platform_uuid", value: "shared"),
        ])
        let keys = await named.identityKeys()
        XCTAssertEqual(keys, ["icloud:shared", "platform_uuid:shared"])
    }

    /// Empty/whitespace values are skipped; real values are trimmed.
    func testWhitespaceOnlyValuesAreSkippedAndRealValuesTrimmed() async {
        let resolver = IdentityResolver(providers: [
            StubIdentityProvider(keyId: "icloud", value: "   "),
            StubIdentityProvider(keyId: "platform_uuid", value: "  UUID-1  "),
        ])
        let keys = await resolver.identityKeys()
        XCTAssertEqual(keys, ["platform_uuid:UUID-1"])
    }

    /// No providers → no keys.
    func testNoProvidersYieldsEmpty() async {
        let empty = await IdentityResolver(providers: []).identityKeys()
        XCTAssertEqual(empty, [])
    }

    // MARK: - ManualKeyProvider (user-set shared identity key)

    func testManualKeyProviderYieldsPersistedValue() {
        AppSettings.setManualIdentityKey("our-home")
        let provider = ManualKeyProvider()
        XCTAssertEqual(provider.keyId, "manual")
        XCTAssertEqual(provider.currentKeyValue, "our-home")
        AppSettings.setManualIdentityKey("")
        XCTAssertNil(ManualKeyProvider().currentKeyValue, "cleared setting = unavailable")
    }

    /// The manual key sorts AFTER the known automatic sources (unknown keyId
    /// rule) but still participates in dedupe and precedence output.
    func testManualKeySortsAfterAutomaticSources() async {
        AppSettings.setManualIdentityKey("our-home")
        let resolver = IdentityResolver(providers: [
            ManualKeyProvider(),
            StubIdentityProvider(keyId: "platform_uuid", value: "UUID-1"),
        ])
        let keys = await resolver.identityKeys()
        XCTAssertEqual(keys, ["platform_uuid:UUID-1", "manual:our-home"])
    }

    /// Announce-critical (2026-09-23 federation regression): the manual key
    /// must appear in `identityKeys()` even when the automatic providers are
    /// still resolving (or never resolve) — a session start right after
    /// typing the key cannot wait on a detached resolver task.
    func testManualKeyPresentWhenAutomaticProvidersPending() async {
        AppSettings.setManualIdentityKey("our-home")
        // All-automatic provider unavailable (nil): the resolver would
        // otherwise return [] and stay uncached, dropping the manual bridge.
        let resolver = IdentityResolver(providers: [
            StubIdentityProvider(keyId: "icloud", value: nil),
            ManualKeyProvider(),
        ])
        let keys = await resolver.identityKeys()
        XCTAssertEqual(keys, ["manual:our-home"])
    }

    /// The synchronous announce key matches the resolver's manual entry.
    func testAnnounceKeyMatchesResolverOutput() async {
        AppSettings.setManualIdentityKey("our-home")
        XCTAssertEqual(ManualKeyProvider.announceKey(), "manual:our-home")
        AppSettings.setManualIdentityKey(" ")
        XCTAssertNil(ManualKeyProvider.announceKey(), "whitespace-only = unset")
    }

    // MARK: - ICloudAccountId (injected fetch seam — no CloudKit here)

    func testICloudAccountIdReturnsInjectedRecordName() async {
        let provider = ICloudAccountId(fetchUserRecordID: { "rec-abc" })
        XCTAssertEqual(provider.keyId, "icloud")
        let key = await provider.currentKey()
        XCTAssertEqual(key, "rec-abc")
    }

    func testICloudAccountIdNilWhenUnavailable() async {
        let provider = ICloudAccountId(fetchUserRecordID: { nil })
        let key = await provider.currentKey()
        XCTAssertNil(key)
    }

    private static func makeResolver() -> IdentityResolver {
        IdentityResolver(providers: [])
    }
}
