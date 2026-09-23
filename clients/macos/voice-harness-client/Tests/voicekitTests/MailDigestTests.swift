import XCTest
@testable import voicekit

/// Pure digest formatting — no Mail.app, no AppleScript.
final class MailDigestTests: XCTestCase {
    private let cal: Calendar = {
        var c = Calendar(identifier: .gregorian)
        c.timeZone = TimeZone(identifier: "America/Los_Angeles")!
        return c
    }()

    /// A Tuesday, 10:00 AM PT.
    private var tuesday10AM: Date {
        cal.date(from: DateComponents(year: 2026, month: 9, day: 22, hour: 10, minute: 0))!
    }

    func testMailLineMatchesPlanWording() {
        let model = MailModel(sender: "Ben", date: tuesday10AM, subject: "Kickoff", content: "Agenda attached")
        let line = MailDigestFormatter.mailLine(model, calendar: cal)
        XCTAssertEqual(line, "Email from Ben received Tuesday September 22 at 10:00 AM: Kickoff — Agenda attached")
    }

    func testUnreadMarkerAndEmptySubject() {
        let model = MailModel(sender: "Ops", date: tuesday10AM, subject: "", content: "Disk full")
        let line = MailDigestFormatter.mailLine(model, calendar: cal)
        XCTAssertEqual(line, "Email from Ops received Tuesday September 22 at 10:00 AM: (no subject) — Disk full")
    }

    func testUnreadFlagAddsMarker() {
        let model = MailModel(sender: "Ops", date: tuesday10AM, subject: "Alert", content: "See dashboard", isUnread: true)
        let line = MailDigestFormatter.mailLine(model, calendar: cal)
        XCTAssertTrue(line.hasPrefix("[unread] Email from Ops"))
    }

    func testEmptyContentOmitsDashSection() {
        let model = MailModel(sender: "Ben", date: tuesday10AM, subject: "Hi", content: "")
        let line = MailDigestFormatter.mailLine(model, calendar: cal)
        XCTAssertEqual(line, "Email from Ben received Tuesday September 22 at 10:00 AM: Hi")
    }

    func testContentClampsTo200Chars() {
        let long = String(repeating: "y", count: 400)
        let model = MailModel(sender: "Ben", date: tuesday10AM, subject: "Hi", content: long)
        let line = MailDigestFormatter.mailLine(model, calendar: cal)
        XCTAssertEqual(line, "Email from Ben received Tuesday September 22 at 10:00 AM: Hi — \(String(repeating: "y", count: 200))")
    }

    func testMultilineContentFlattens() {
        let model = MailModel(sender: "Ben", date: tuesday10AM, subject: "Hi", content: "line one\nline two\r\nline three")
        let line = MailDigestFormatter.mailLine(model, calendar: cal)
        XCTAssertEqual(line, "Email from Ben received Tuesday September 22 at 10:00 AM: Hi — line one line two line three")
    }

    func testMailDigestHeaderAndClamp() {
        let models = [
            MailModel(sender: "Ben", date: tuesday10AM, subject: "Kickoff", content: "Agenda"),
            MailModel(sender: "Ops", date: tuesday10AM.addingTimeInterval(-3600), subject: "Alert", content: "See dashboard"),
        ]
        let digest = MailDigestFormatter.mailDigest(models, calendar: cal)
        XCTAssertTrue(digest.hasPrefix("Here are your most recent emails:\n"))
        XCTAssertEqual(digest.split(separator: "\n").count, 3, "header + two lines")
        XCTAssertTrue(digest.contains("Kickoff"))
        XCTAssertTrue(digest.contains("Alert"))
    }

    func testEmptyMailboxDigest() {
        let digest = MailDigestFormatter.mailDigest([], calendar: cal)
        XCTAssertEqual(digest, "No emails found.")
    }

    func testNotRunningDigest() {
        XCTAssertEqual(MailDigestFormatter.notRunningDigest,
                       "Email is unavailable right now — Mail isn't running.")
    }
}
