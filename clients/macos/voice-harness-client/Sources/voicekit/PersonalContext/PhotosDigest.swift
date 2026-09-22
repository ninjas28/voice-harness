import Foundation

/// Aggregate photo-library statistics for one time window, mapped from
/// `PHFetchResult<PHAsset>` at the provider edge.
struct PhotoStats: Equatable, Sendable {
    var total: Int
    var favorites: Int
    var oldest: Date?
    var newest: Date?
    /// Assets per weekday name ("Monday": 4).
    var byWeekday: [String: Int]
}

enum PhotosDigestFormatter {
    /// Spoken-style digest of photo-library activity in the window.
    static func digest(_ stats: PhotoStats, now: Date,
                       calendar: Calendar = .current,
                       daysLabel: String = "the last 7 days") -> String {
        guard stats.total > 0 else {
            return "No photos found in \(daysLabel)."
        }
        var out = "You have \(stats.total) item\(stats.total == 1 ? "" : "s") from \(daysLabel)"
        if stats.favorites > 0 {
            out += ", including \(stats.favorites) favorite\(stats.favorites == 1 ? "" : "s")"
        }
        out += ".\n"
        if !stats.byWeekday.isEmpty {
            let counts = stats.byWeekday.sorted { $0.value > $1.value }
            let parts = counts.map { "\($0.value) on \($0.key)" }
            out += "By day: \(parts.joined(separator: ", ")).\n"
        }
        if let newest = stats.newest {
            if calendar.isDate(newest, inSameDayAs: now) {
                out += "The newest is from today at \(CalendarDigestFormatter.timeString(newest, calendar: calendar))."
            } else {
                out += "The newest is from \(CalendarDigestFormatter.dayHeader(for: newest, calendar: calendar))."
            }
        } else if let oldest = stats.oldest {
            out += "The oldest is from \(CalendarDigestFormatter.dayHeader(for: oldest, calendar: calendar))."
        }
        return DigestFormatter.clamp(out)
    }
}
