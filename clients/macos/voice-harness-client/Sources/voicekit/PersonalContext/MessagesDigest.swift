import Foundation

/// One conversation thread as surfaced in a messages digest: the handle of the
/// other party, the time of the newest message, and its decoded plain body.
struct ThreadModel: Equatable, Sendable {
    var handle: String
    var date: Date
    /// Plain text of the newest message, or nil when neither the `text`
    /// column nor the typedstream body yielded anything.
    var body: String?
}

/// Pure digest formatting for MessagesProvider — no chat.db, no TCC.
enum MessagesDigestFormatter {
    /// Cap on the per-message preview in a digest line.
    static let previewLimit = 200

    /// "Thread with <handle>, last message <weekday date time>: <first 200 chars>"
    /// (plan Task 3 wording). English weekday/month names and hand-rolled
    /// 12-hour times come from the shared formatter, pinned to en-US.
    static func threadLine(_ thread: ThreadModel, calendar: Calendar = .current) -> String {
        let stamp = "\(CalendarDigestFormatter.dayHeader(for: thread.date, calendar: calendar)) at \(CalendarDigestFormatter.timeString(thread.date, calendar: calendar))"
        let preview = preview(thread.body)
        return "Thread with \(thread.handle), last message \(stamp): \(preview)"
    }

    /// Spoken-style digest listing threads, newest first.
    static func threadsDigest(_ threads: [ThreadModel], calendar: Calendar = .current) -> String {
        guard !threads.isEmpty else { return "No messages found." }
        var out = "Here are your most recent message threads:\n"
        for thread in threads {
            out += threadLine(thread, calendar: calendar) + "\n"
        }
        return DigestFormatter.clamp(out)
    }

    /// First 200 characters, flattened to one line, or a spoken fallback.
    static func preview(_ body: String?) -> String {
        guard var text = body?.trimmingCharacters(in: .whitespacesAndNewlines),
              !text.isEmpty else {
            return "(no text preview available)"
        }
        text = text.replacingOccurrences(of: "\r\n", with: " ")
        text = text.replacingOccurrences(of: "\n", with: " ")
        text = text.replacingOccurrences(of: "\r", with: " ")
        if text.count > previewLimit {
            text = String(text.prefix(previewLimit))
        }
        return text
    }
}
