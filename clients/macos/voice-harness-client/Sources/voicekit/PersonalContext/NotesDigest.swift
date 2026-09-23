import Foundation

/// One note as surfaced in a notes digest, mapped from NoteStore.sqlite at the
/// provider edge. `body` is the full decoded text when the gzip-protobuf blob
/// yielded one; `snippet` is the store's pre-computed ZSNIPPET fallback.
struct NoteModel: Equatable, Sendable {
    var title: String?
    var date: Date
    var body: String?
    var snippet: String?
}

/// Pure digest formatting for NotesProvider — no NoteStore, no TCC.
enum NotesDigestFormatter {
    /// Cap on the per-note preview in a digest line.
    static let previewLimit = 200
    /// The attachment marker character (U+FFFC) Notes puts where media sits.
    static let attachmentChar: Character = "\u{FFFC}"
    static let attachmentPlaceholder = "[attachment]"

    /// "Note "<title>" modified <weekday date time>: <first 200 chars>"
    /// (plan Task 5 wording, mail/message style). English weekday/month names
    /// and hand-rolled 12-hour times come from the shared formatter.
    static func noteLine(_ note: NoteModel, calendar: Calendar = .current) -> String {
        let stamp = "\(CalendarDigestFormatter.dayHeader(for: note.date, calendar: calendar)) at \(CalendarDigestFormatter.timeString(note.date, calendar: calendar))"
        let title = note.title?.trimmingCharacters(in: .whitespaces)
        let titlePart = (title?.isEmpty == false) ? title! : "(untitled)"
        return "Note \"\(titlePart)\" modified \(stamp): \(preview(note))"
    }

    /// Spoken-style digest listing notes, newest first.
    static func notesDigest(_ notes: [NoteModel], calendar: Calendar = .current) -> String {
        guard !notes.isEmpty else { return "No notes found." }
        var out = "Here are your most recent notes:\n"
        for note in notes {
            out += noteLine(note, calendar: calendar) + "\n"
        }
        return DigestFormatter.clamp(out)
    }

    /// First 200 characters, flattened to one line, or a spoken fallback.
    /// Body preferred; the store's snippet is the fallback rung.
    static func preview(_ note: NoteModel) -> String {
        let raw = note.body ?? note.snippet
        guard var text = raw?.trimmingCharacters(in: .whitespacesAndNewlines),
              !text.isEmpty else {
            return "(no note preview available)"
        }
        text = text.replacingOccurrences(of: String(attachmentChar), with: attachmentPlaceholder)
        text = text.replacingOccurrences(of: "\r\n", with: " ")
        text = text.replacingOccurrences(of: "\n", with: " ")
        text = text.replacingOccurrences(of: "\r", with: " ")
        if text.count > previewLimit {
            text = String(text.prefix(previewLimit))
        }
        return text
    }
}
