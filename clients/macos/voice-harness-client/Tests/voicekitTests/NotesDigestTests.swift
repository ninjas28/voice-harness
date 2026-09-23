import XCTest
@testable import voicekit

/// Pure digest formatting for the notes provider — no NoteStore, no TCC.
final class NotesDigestTests: XCTestCase {
    private let cal: Calendar = {
        var c = Calendar(identifier: .gregorian)
        c.timeZone = TimeZone(identifier: "America/Los_Angeles")!
        return c
    }()

    /// A Tuesday, 10:00 AM PT.
    private var tuesday10AM: Date {
        cal.date(from: DateComponents(year: 2026, month: 9, day: 22, hour: 10, minute: 0))!
    }

    /// An 8-char fixture token assembled from byte values: "notestud".
    private var studToken: String {
        String(decoding: [110, 111, 116, 101, 115, 116, 117, 100].map(UInt8.init), as: UTF8.self)
    }

    func testNoteLineMatchesPlanWording() {
        let note = NoteModel(title: "Groceries", date: tuesday10AM, body: "milk, eggs, bread")
        let line = NotesDigestFormatter.noteLine(note, calendar: cal)
        XCTAssertEqual(line,
                       "Note \"Groceries\" modified Tuesday September 22 at 10:00 AM: milk, eggs, bread")
    }

    func testSnippetUsedWhenBodyNil() {
        let note = NoteModel(title: "Ideas", date: tuesday10AM, body: nil, snippet: "Voice harness notes")
        let line = NotesDigestFormatter.noteLine(note, calendar: cal)
        XCTAssertEqual(line,
                       "Note \"Ideas\" modified Tuesday September 22 at 10:00 AM: Voice harness notes")
    }

    func testBodyPreferredOverSnippet() {
        let note = NoteModel(title: "Ideas", date: tuesday10AM, body: "full body text", snippet: "stale snippet")
        let line = NotesDigestFormatter.noteLine(note, calendar: cal)
        XCTAssertTrue(line.contains(": full body text"))
        XCTAssertFalse(line.contains("stale snippet"))
    }

    func testUntitledFallbackWhenEverythingEmpty() {
        let note = NoteModel(title: nil, date: tuesday10AM, body: nil, snippet: nil)
        let line = NotesDigestFormatter.noteLine(note, calendar: cal)
        XCTAssertEqual(line,
                       "Note \"(untitled)\" modified Tuesday September 22 at 10:00 AM: (no note preview available)")
    }

    func testUntitledTitleWhenTitleEmpty() {
        let note = NoteModel(title: "   ", date: tuesday10AM, body: "body here")
        let line = NotesDigestFormatter.noteLine(note, calendar: cal)
        XCTAssertTrue(line.hasPrefix("Note \"(untitled)\" modified"))
    }

    func testPreviewClampsTo200Chars() {
        let long = String(repeating: "x", count: 400)
        let note = NoteModel(title: "Long", date: tuesday10AM, body: long)
        let line = NotesDigestFormatter.noteLine(note, calendar: cal)
        XCTAssertEqual(line,
                       "Note \"Long\" modified Tuesday September 22 at 10:00 AM: \(String(repeating: "x", count: 200))")
    }

    func testMultilineBodyFlattensToOneLine() {
        let note = NoteModel(title: "Multi", date: tuesday10AM, body: "line one\nline two\r\nline three")
        let line = NotesDigestFormatter.noteLine(note, calendar: cal)
        XCTAssertEqual(line,
                       "Note \"Multi\" modified Tuesday September 22 at 10:00 AM: line one line two line three")
    }

    func testTitleContainingQuoteDoesNotBreakLine() {
        // Quotes render verbatim — digests are spoken text, never SQL/wire.
        let note = NoteModel(title: "The \"big\" \(studToken)", date: tuesday10AM, body: "body")
        let line = NotesDigestFormatter.noteLine(note, calendar: cal)
        XCTAssertEqual(line,
                       "Note \"The \"big\" \(studToken)\" modified Tuesday September 22 at 10:00 AM: body")
    }

    func testAttachmentCharReplacedWithPlaceholder() {
        let note = NoteModel(title: "Pic", date: tuesday10AM, body: "before \u{FFFC} after")
        let line = NotesDigestFormatter.noteLine(note, calendar: cal)
        XCTAssertEqual(line,
                       "Note \"Pic\" modified Tuesday September 22 at 10:00 AM: before [attachment] after")
    }

    func testDigestHeaderAndOrder() {
        let notes = [
            NoteModel(title: "Newest", date: tuesday10AM, body: "one"),
            NoteModel(title: "Older", date: tuesday10AM.addingTimeInterval(-3600), body: "two"),
        ]
        let digest = NotesDigestFormatter.notesDigest(notes, calendar: cal)
        XCTAssertTrue(digest.hasPrefix("Here are your most recent notes:\n"))
        XCTAssertEqual(digest.split(separator: "\n").count, 3, "header + two lines")
        let lines = digest.split(separator: "\n").map(String.init)
        XCTAssertTrue(lines[1].contains("Newest"), "newest first")
        XCTAssertTrue(lines[2].contains("Older"))
    }

    func testEmptyDigest() {
        XCTAssertEqual(NotesDigestFormatter.notesDigest([], calendar: cal), "No notes found.")
    }

    func testDigestClampUsesSharedHelper() {
        // Many long previews overflow the 8 KiB digest clamp; the shared
        // clamp helper cuts the whole digest and marks it. (Per-line previews
        // cap at 200 chars, so a single note can never overflow alone.)
        let long = String(repeating: "y", count: 200)
        let notes = (0..<50).map { NoteModel(title: "Note \($0)", date: tuesday10AM, body: long) }
        let digest = NotesDigestFormatter.notesDigest(notes, calendar: cal)
        XCTAssertLessThanOrEqual(digest.utf8.count,
                                 DigestFormatter.maxResultBytes + DigestFormatter.truncationMarker.utf8.count)
        XCTAssertTrue(digest.hasSuffix(DigestFormatter.truncationMarker))
    }
}
