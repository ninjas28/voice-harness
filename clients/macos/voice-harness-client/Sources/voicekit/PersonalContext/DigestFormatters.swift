import Foundation

/// Pure digest formatting for the personal-context providers. These functions
/// take plain value models (never framework types) so they are fully
/// unit-testable without touching EventKit/Contacts/PhotoKit authorization.
enum DigestFormatter {
    /// Shared clamp: digests are plain spoken text, ≤ 8 KiB, with an explicit
    /// marker when cut (mirrors the server-side clamp).
    static let maxResultBytes = 8192
    static let truncationMarker = "…(truncated)"

    static func clamp(_ text: String, maxBytes: Int = DigestFormatter.maxResultBytes) -> String {
        guard text.utf8.count > maxBytes else { return text }
        var truncated = text
        while truncated.utf8.count > maxBytes {
            truncated.removeLast()
        }
        truncated += truncationMarker
        return truncated
    }
}

/// One calendar event, mapped from `EKEvent` at the provider edge.
struct EventModel: Equatable, Sendable {
    var title: String
    var start: Date
    /// Duration in minutes; nil for all-day events.
    var minutes: Int?
    var calendarName: String
    var isAllDay: Bool = false
}

/// One reminder, mapped from `EKReminder` at the provider edge.
struct ReminderModel: Equatable, Sendable {
    var title: String
    var due: Date?
    var isCompleted: Bool
}

enum CalendarDigestFormatter {
    /// Spoken-style digest of calendar events, grouped by day.
    /// Days render as "Thursday September 24", times as 12-hour "9:30 AM",
    /// all-day events as "All-day: Trip".
    static func eventsDigest(_ events: [EventModel], rangeLabel: String, now: Date = Date(),
                             calendar: Calendar = .current) -> String {
        guard !events.isEmpty else { return "No events found \(rangeLabel)." }
        var order: [String] = []
        var groups: [String: (date: Date, lines: [String])] = [:]
        for event in events.sorted(by: { $0.start < $1.start }) {
            let key = dayKey(event.start, calendar: calendar)
            if groups[key] == nil {
                order.append(key)
                groups[key] = (event.start, [])
            }
            groups[key]!.lines.append(line(for: event, calendar: calendar))
        }
        var out = "Here's what's coming up \(rangeLabel):\n"
        for (index, key) in order.enumerated() {
            let group = groups[key]!
            let header = dayHeader(for: group.date, calendar: calendar)
            out += index == 0 ? "\(header):\n" : "\n\(header):\n"
            for line in group.lines {
                out += "\(line)\n"
            }
        }
        return DigestFormatter.clamp(out)
    }

    /// One event line: "9:30 AM Dentist (Home, 30 min)" / "All-day: Trip (Home)".
    static func line(for event: EventModel, calendar: Calendar = .current) -> String {
        var parts: [String] = []
        if event.isAllDay {
            parts.append("All-day: \(event.title)")
        } else {
            parts.append("\(timeString(event.start, calendar: calendar)) \(event.title)")
        }
        var meta: [String] = [event.calendarName]
        if let minutes = event.minutes {
            meta.append(minutesLabel(minutes))
        }
        parts.append("(\(meta.joined(separator: ", ")))")
        return parts.joined(separator: " ")
    }

    static func remindersDigest(_ reminders: [ReminderModel], now: Date,
                                calendar: Calendar = .current) -> String {
        let open = reminders.filter { !$0.isCompleted }
        guard !open.isEmpty else { return "No reminders found." }

        let startOfToday = calendar.startOfDay(for: now)
        var overdue: [String] = []
        var today: [String] = []
        var upcoming: [String] = []
        for reminder in open {
            let title = "• \(reminder.title)"
            guard let due = reminder.due else {
                upcoming.append(title) // no date: surface as upcoming
                continue
            }
            if due < startOfToday {
                overdue.append("\(title) (was due \(dayHeader(for: due, calendar: calendar)))")
            } else if calendar.isDate(due, inSameDayAs: now) {
                today.append("\(title) at \(timeString(due, calendar: calendar))")
            } else {
                upcoming.append("\(title) (\(dayHeader(for: due, calendar: calendar)))")
            }
        }

        var out = "Here are your reminders:\n"
        if !overdue.isEmpty {
            out += "Overdue:\n"
            out += overdue.map { "\($0)\n" }.joined()
        }
        if !today.isEmpty {
            out += "\nToday:\n"
            out += today.map { "\($0)\n" }.joined()
        }
        if !upcoming.isEmpty {
            out += "\nUpcoming:\n"
            out += upcoming.map { "\($0)\n" }.joined()
        }
        return DigestFormatter.clamp(out)
    }

    /// "Thursday September 24" (weekday + spoken month/day). Symbols are
    /// pinned to English so the spoken digest is deterministic regardless of
    /// the device locale.
    static func dayHeader(for date: Date, calendar: Calendar = .current) -> String {
        let components = calendar.dateComponents([.weekday, .month, .day], from: date)
        let weekday = englishWeekdays[components.weekday! - 1]
        let month = englishMonths[components.month! - 1]
        return "\(weekday) \(month) \(components.day!)"
    }

    private static let englishWeekdays = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"]
    private static let englishMonths = ["January", "February", "March", "April", "May", "June", "July",
                                        "August", "September", "October", "November", "December"]

    /// English weekday name for a weekday index (1 = Sunday) — used by the
    /// photos provider when building `PhotoStats.byWeekday`.
    static func englishWeekday(_ index: Int) -> String {
        englishWeekdays[index - 1]
    }

    /// "9:30 AM" 12-hour style (no leading zero on the hour).
    static func timeString(_ date: Date, calendar: Calendar = .current) -> String {
        let components = calendar.dateComponents([.hour, .minute], from: date)
        let hour = components.hour!
        let minute = components.minute!
        if hour == 0 { return "12:\(twoDigit(minute)) AM" }
        if hour < 12 { return "\(hour):\(twoDigit(minute)) AM" }
        if hour == 12 { return "12:\(twoDigit(minute)) PM" }
        return "\(hour - 12):\(twoDigit(minute)) PM"
    }

    static func minutesLabel(_ minutes: Int) -> String {
        if minutes < 60 { return "\(minutes) min" }
        let hours = minutes / 60
        let rest = minutes % 60
        if rest == 0 { return hours == 1 ? "1 hour" : "\(hours) hours" }
        return "\(hours) hr \(rest) min"
    }

    private static func twoDigit(_ value: Int) -> String {
        value < 10 ? "0\(value)" : "\(value)"
    }

    private static func dayKey(_ date: Date, calendar: Calendar) -> String {
        let c = calendar.dateComponents([.year, .month, .day], from: date)
        return "\(c.year!)-\(c.month!)-\(c.day!)"
    }
}
