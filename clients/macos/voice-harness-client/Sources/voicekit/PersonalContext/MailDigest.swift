import Foundation

/// One email as surfaced in a mail digest, mapped from AppleScript at the
/// provider edge.
struct MailModel: Equatable, Sendable {
    var sender: String
    var date: Date
    var subject: String
    var content: String
    var isUnread: Bool = false
}

/// Pure digest formatting for MailProvider — no Mail.app, no AppleScript.
enum MailDigestFormatter {
    /// Cap on the per-message content preview.
    static let previewLimit = 200

    /// The distinct not-running signal, verbatim from the plan.
    static let notRunningDigest = "Email is unavailable right now — Mail isn't running."

    /// "Email from <sender> received <weekday date at time>: <subject> — <first 200 chars>"
    /// (plan Task 4 wording). Unread mail gets a spoken "[unread]" marker.
    static func mailLine(_ mail: MailModel, calendar: Calendar = .current) -> String {
        let stamp = "\(CalendarDigestFormatter.dayHeader(for: mail.date, calendar: calendar)) at \(CalendarDigestFormatter.timeString(mail.date, calendar: calendar))"
        let subject = mail.subject.trimmingCharacters(in: .whitespaces)
        let subjectPart = subject.isEmpty ? "(no subject)" : subject
        var line = "Email from \(mail.sender) received \(stamp): \(subjectPart)"
        let content = preview(mail.content)
        if !content.isEmpty {
            line += " — \(content)"
        }
        return mail.isUnread ? "[unread] \(line)" : line
    }

    /// Spoken-style digest listing emails, newest first.
    static func mailDigest(_ mails: [MailModel], calendar: Calendar = .current) -> String {
        guard !mails.isEmpty else { return "No emails found." }
        var out = "Here are your most recent emails:\n"
        for mail in mails {
            out += mailLine(mail, calendar: calendar) + "\n"
        }
        return DigestFormatter.clamp(out)
    }

    /// First 200 characters, flattened to one line.
    static func preview(_ content: String) -> String {
        var text = content
            .replacingOccurrences(of: "\r\n", with: " ")
            .replacingOccurrences(of: "\n", with: " ")
            .replacingOccurrences(of: "\r", with: " ")
            .trimmingCharacters(in: .whitespaces)
        if text.count > previewLimit {
            text = String(text.prefix(previewLimit))
        }
        return text
    }
}
