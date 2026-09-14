import XCTest
@testable import voicekit

@MainActor
final class AutoStartRequestTests: XCTestCase {
    private let suiteName = "voicekit.AutoStartRequestTests"
    private var defaults: UserDefaults!

    override func setUp() {
        defaults = UserDefaults(suiteName: suiteName)
        defaults.removePersistentDomain(forName: suiteName)
        AutoStartRequest.defaultsOverride = defaults
    }

    override func tearDown() {
        AutoStartRequest.defaultsOverride = nil
        defaults.removePersistentDomain(forName: suiteName)
    }

    func testConsumeReturnsFalseWhenNothingRequested() {
        XCTAssertFalse(AutoStartRequest.consume())
    }

    func testRequestThenConsumeReturnsTrueOnce() {
        AutoStartRequest.request()
        XCTAssertTrue(AutoStartRequest.consume())
        XCTAssertFalse(AutoStartRequest.consume(), "consume must clear the request")
    }

    func testRepeatedRequestIsIdempotent() {
        AutoStartRequest.request()
        AutoStartRequest.request()
        XCTAssertTrue(AutoStartRequest.consume())
        XCTAssertFalse(AutoStartRequest.consume())
    }
}
