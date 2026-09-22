import XCTest
@testable import voicekit

@MainActor
final class AppSettingsTests: XCTestCase {
    private let suiteName = "voicekit.AppSettingsTests"
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

    // MARK: - sanitizeServerURLString

    func testSanitizeTrimsWhitespace() {
        XCTAssertEqual(
            AppSettings.sanitizeServerURLString("  ws://192.168.10.93:8090/v1/realtime  "),
            "ws://192.168.10.93:8090/v1/realtime")
    }

    func testSanitizeAcceptsWss() {
        XCTAssertEqual(
            AppSettings.sanitizeServerURLString("wss://example.com/v1/realtime"),
            "wss://example.com/v1/realtime")
    }

    func testSanitizeEmptyClears() {
        XCTAssertEqual(AppSettings.sanitizeServerURLString(""), "")
        XCTAssertEqual(AppSettings.sanitizeServerURLString("   "), "")
    }

    func testSanitizeRejectsGarbage() {
        XCTAssertNil(AppSettings.sanitizeServerURLString("not a url"))
    }

    func testSanitizeRejectsNonWebSocketScheme() {
        XCTAssertNil(AppSettings.sanitizeServerURLString("http://example.com/v1/realtime"))
        XCTAssertNil(AppSettings.sanitizeServerURLString("example.com:8090/v1/realtime"))
    }

    func testSanitizeRejectsEmptyHost() {
        XCTAssertNil(AppSettings.sanitizeServerURLString("ws:///v1/realtime"))
    }

    // MARK: - cleartext ws:// host policy

    func testSanitizeAcceptsLoopbackWS() {
        XCTAssertEqual(
            AppSettings.sanitizeServerURLString("ws://127.0.0.1"),
            "ws://127.0.0.1")
        XCTAssertEqual(
            AppSettings.sanitizeServerURLString("ws://127.0.0.1:8090/v1/realtime"),
            "ws://127.0.0.1:8090/v1/realtime")
        XCTAssertEqual(
            AppSettings.sanitizeServerURLString("ws://[::1]:8090/v1/realtime"),
            "ws://[::1]:8090/v1/realtime")
        XCTAssertEqual(
            AppSettings.sanitizeServerURLString("ws://localhost:8090/v1/realtime"),
            "ws://localhost:8090/v1/realtime")
    }

    func testSanitizeAcceptsPrivateAndLinkLocalWS() {
        XCTAssertEqual(
            AppSettings.sanitizeServerURLString("ws://192.168.10.93:8090/v1/realtime"),
            "ws://192.168.10.93:8090/v1/realtime")
        XCTAssertEqual(
            AppSettings.sanitizeServerURLString("ws://10.1.2.3:8090/v1/realtime"),
            "ws://10.1.2.3:8090/v1/realtime")
        XCTAssertEqual(
            AppSettings.sanitizeServerURLString("ws://172.16.5.5:8090/v1/realtime"),
            "ws://172.16.5.5:8090/v1/realtime")
        XCTAssertEqual(
            AppSettings.sanitizeServerURLString("ws://172.31.255.1:8090/v1/realtime"),
            "ws://172.31.255.1:8090/v1/realtime")
        XCTAssertEqual(
            AppSettings.sanitizeServerURLString("ws://169.254.10.20:8090/v1/realtime"),
            "ws://169.254.10.20:8090/v1/realtime")
        XCTAssertEqual(
            AppSettings.sanitizeServerURLString("ws://voicebox.local:8090/v1/realtime"),
            "ws://voicebox.local:8090/v1/realtime")
    }

    func testSanitizeRejectsPublicCleartextWS() {
        XCTAssertNil(
            AppSettings.sanitizeServerURLString("ws://voicebox.zippystation.com/v1/realtime"))
        XCTAssertNil(AppSettings.sanitizeServerURLString("ws://example.com/v1/realtime"))
        XCTAssertNil(AppSettings.sanitizeServerURLString("ws://8.8.8.8:8090/v1/realtime"))
        // RFC1918 boundaries: 172.32.0.0/12-adjacent addresses are public.
        XCTAssertNil(AppSettings.sanitizeServerURLString("ws://172.32.0.1:8090/v1/realtime"))
        XCTAssertNil(AppSettings.sanitizeServerURLString("ws://11.0.0.1:8090/v1/realtime"))
        // A non-IPv4 hostname is treated as public for cleartext.
        XCTAssertNil(AppSettings.sanitizeServerURLString("ws://myserver.example/v1/realtime"))
    }

    func testSanitizeAcceptsWssAnyHost() {
        XCTAssertEqual(
            AppSettings.sanitizeServerURLString("wss://voicebox.zippystation.com/v1/realtime"),
            "wss://voicebox.zippystation.com/v1/realtime")
    }

    func testSanitizePreservesTokenQueryRoundTrip() {
        let stored = "ws://192.168.10.93:8090/v1/realtime?token=s3cr3t-token-value"
        XCTAssertEqual(AppSettings.sanitizeServerURLString(stored), stored)
        AppSettings.setServerURLString(stored)
        XCTAssertEqual(AppSettings.storedServerURLString, stored)
        XCTAssertEqual(AppSettings.serverURL.absoluteString, stored)
    }

    // MARK: - setServerURLString / serverURL round trip

    func testSetThenReadRoundTrips() {
        AppSettings.setServerURLString("wss://example.com/v1/realtime")
        XCTAssertEqual(AppSettings.serverURL, URL(string: "wss://example.com/v1/realtime"))
    }

    func testSetInvalidIsIgnored() {
        AppSettings.setServerURLString("wss://example.com/v1/realtime")
        AppSettings.setServerURLString("garbage")
        XCTAssertEqual(AppSettings.serverURL, URL(string: "wss://example.com/v1/realtime"))
    }

    func testClearFallsBackToDefault() {
        AppSettings.setServerURLString("wss://example.com/v1/realtime")
        AppSettings.setServerURLString("")
        XCTAssertEqual(AppSettings.serverURL.absoluteString, AppSettings.defaultServerURLString)
        XCTAssertNil(AppSettings.storedServerURLString)
    }

    func testStoredServerURLStringReturnsStoredValue() {
        XCTAssertNil(AppSettings.storedServerURLString)
        AppSettings.setServerURLString("ws://127.0.0.1:1/x")
        XCTAssertEqual(AppSettings.storedServerURLString, "ws://127.0.0.1:1/x")
    }

    // MARK: - API key

    func testAPIKeyDefaultsToEmpty() {
        XCTAssertEqual(AppSettings.apiKey, "")
    }

    func testAPIKeyRoundTrips() {
        AppSettings.setAPIKey("s3cr3t-key")
        XCTAssertEqual(AppSettings.apiKey, "s3cr3t-key")
    }

    func testAPIKeyTrimsWhitespace() {
        AppSettings.setAPIKey("  s3cr3t-key  ")
        XCTAssertEqual(AppSettings.apiKey, "s3cr3t-key")
    }

    func testAPIKeyClearsToEmpty() {
        AppSettings.setAPIKey("s3cr3t-key")
        AppSettings.setAPIKey("   ")
        XCTAssertEqual(AppSettings.apiKey, "")
    }

    // MARK: - Personal context toggle (Task 9)

    func testPersonalContextDefaultsToDisabled() {
        XCTAssertFalse(AppSettings.personalContextEnabled)
    }

    func testPersonalContextRoundTrips() {
        AppSettings.setPersonalContextEnabled(true)
        XCTAssertTrue(AppSettings.personalContextEnabled)
        AppSettings.setPersonalContextEnabled(false)
        XCTAssertFalse(AppSettings.personalContextEnabled)
    }
}
