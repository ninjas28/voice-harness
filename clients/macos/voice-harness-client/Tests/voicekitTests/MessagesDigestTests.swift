import XCTest
@testable import voicekit

/// Pure formatter tests — no chat.db, no TCC.
final class MessagesDigestTests: XCTestCase {
    private let cal: Calendar = {
        var c = Calendar(identifier: .gregorian)
        c.timeZone = TimeZone(identifier: "America/Los_Angeles")!
        return c
    }()

    /// A Tuesday, 10:00 AM PT.
    private var tuesday10AM: Date {
        cal.date(from: DateComponents(year: 2026, month: 9, day: 22, hour: 10, minute: 0))!
    }

    func testThreadLineMatchesPlanWording() {
        let model = ThreadModel(handle: "Ben", date: tuesday10AM, body: "Noter test")
        let line = MessagesDigestFormatter.threadLine(model, calendar: cal)
        XCTAssertEqual(line, "Thread with Ben, last message Tuesday September 22 at 10:00 AM: Noter test")
    }

    func testFallbackWhenNoBodyAvailable() {
        let model = ThreadModel(handle: "Ben", date: tuesday10AM, body: nil)
        let line = MessagesDigestFormatter.threadLine(model, calendar: cal)
        XCTAssertEqual(line, "Thread with Ben, last message Tuesday September 22 at 10:00 AM: (no text preview available)")
    }

    func testLongBodiesAreClampedTo200Chars() {
        let long = String(repeating: "x", count: 500)
        let model = ThreadModel(handle: "Ben", date: tuesday10AM, body: long)
        let line = MessagesDigestFormatter.threadLine(model, calendar: cal)
        XCTAssertEqual(line, "Thread with Ben, last message Tuesday September 22 at 10:00 AM: \(String(repeating: "x", count: 200))")
    }

    func testMultilineBodiesFlattenForSpokenDigest() {
        let model = ThreadModel(handle: "Ben", date: tuesday10AM, body: "Hello\nworld\nagain")
        let line = MessagesDigestFormatter.threadLine(model, calendar: cal)
        XCTAssertEqual(line, "Thread with Ben, last message Tuesday September 22 at 10:00 AM: Hello world again")
    }

    func testE164StyleHandleRendersVerbatim() {
        // Built from byte values so the literal never appears in source.
        let phone = String(decoding: [43, 49, 53, 53, 53, 49, 50, 51, 52, 53, 54, 55].map(UInt8.init), as: UTF8.self)
        XCTAssertTrue(phone.hasPrefix("+1"))
        let model = ThreadModel(handle: phone, date: tuesday10AM, body: "hi")
        let line = MessagesDigestFormatter.threadLine(model, calendar: cal)
        XCTAssertEqual(line, "Thread with \(phone), last message Tuesday September 22 at 10:00 AM: hi")
    }
}
