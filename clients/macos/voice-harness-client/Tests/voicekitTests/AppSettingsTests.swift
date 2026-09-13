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
            AppSettings.sanitizeServerURLString("  ws://host:8090/v1/realtime  "),
            "ws://host:8090/v1/realtime")
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
        AppSettings.setServerURLString("ws://host:1/x")
        XCTAssertEqual(AppSettings.storedServerURLString, "ws://host:1/x")
    }
}
