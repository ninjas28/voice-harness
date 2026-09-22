import XCTest
@testable import voicekit

final class DigestFormatterTests: XCTestCase {
    /// The day the plan's example pins: Thursday September 24 (2026).
    private func thursday0930(minutes: Int = 30) -> Date {
        Calendar.current.date(from: DateComponents(year: 2026, month: 9, day: 24, hour: 9, minute: 30))!
    }

    private func date(_ year: Int, _ month: Int, _ day: Int, _ hour: Int = 0, _ minute: Int = 0) -> Date {
        Calendar.current.date(from: DateComponents(year: year, month: month, day: day, hour: hour, minute: minute))!
    }

    // MARK: - Calendar digest

    func testCalendarDigestGroupsByDayAndSpeaks24hAs12h() {
        let events = [
            EventModel(title: "Dentist", start: thursday0930(), minutes: 30, calendarName: "Home"),
        ]
        let digest = CalendarDigestFormatter.eventsDigest(events, rangeLabel: "for the next day")
        XCTAssertTrue(digest.contains("Thursday September 24"), "digest was: \(digest)")
        XCTAssertTrue(digest.contains("9:30 AM"), "digest was: \(digest)")
        XCTAssertTrue(digest.contains("Dentist"), "digest was: \(digest)")
        XCTAssertTrue(digest.contains("Home"), "digest was: \(digest)")
    }

    func testCalendarDigestMultipleDaysAndAllDay() {
        let events = [
            EventModel(title: "Trip", start: date(2026, 9, 25), minutes: nil, calendarName: "Home", isAllDay: true),
            EventModel(title: "Dentist", start: thursday0930(), minutes: 30, calendarName: "Home"),
            EventModel(title: "Dinner", start: date(2026, 9, 25, 19, 0), minutes: 90, calendarName: "Work"),
        ]
        let digest = CalendarDigestFormatter.eventsDigest(events, rangeLabel: "for the next two days")
        XCTAssertTrue(digest.contains("Thursday September 24"), "digest was: \(digest)")
        XCTAssertTrue(digest.contains("Friday September 25"), "digest was: \(digest)")
        XCTAssertTrue(digest.contains("All-day: Trip"), "digest was: \(digest)")
        XCTAssertTrue(digest.contains("7:00 PM"), "digest was: \(digest)")
        XCTAssertTrue(digest.contains("Work"), "digest was: \(digest)")
    }

    func testCalendarDigestSaysNoEvents() {
        XCTAssertEqual(CalendarDigestFormatter.eventsDigest([], rangeLabel: "for the next day"),
                       "No events found for the next day.")
    }

    // MARK: - Reminder digest

    func testReminderDigestSeparatesOverdueTodayUpcoming() {
        let reminders = [
            ReminderModel(title: "Submit taxes", due: date(2026, 9, 19), isCompleted: false),   // overdue (before today)
            ReminderModel(title: "Pay rent", due: date(2026, 9, 21, 23, 30), isCompleted: false), // today
            ReminderModel(title: "Call mom", due: date(2026, 9, 22, 9, 0), isCompleted: false),    // upcoming
            ReminderModel(title: "Stretch", due: nil, isCompleted: false),                          // no date → upcoming
        ]
        let digest = CalendarDigestFormatter.remindersDigest(reminders, now: date(2026, 9, 21, 18, 0))
        XCTAssertTrue(digest.contains("Overdue:"), "digest was: \(digest)")
        XCTAssertTrue(digest.contains("Submit taxes"), "digest was: \(digest)")
        XCTAssertTrue(digest.contains("was due Saturday September 19"), "digest was: \(digest)")
        XCTAssertTrue(digest.contains("Today:"), "digest was: \(digest)")
        XCTAssertTrue(digest.contains("Pay rent"), "digest was: \(digest)")
        XCTAssertTrue(digest.contains("Upcoming:"), "digest was: \(digest)")
        XCTAssertTrue(digest.contains("Call mom"), "digest was: \(digest)")
    }

    func testReminderDigestSaysNoReminders() {
        let digest = CalendarDigestFormatter.remindersDigest([], now: date(2026, 9, 21, 18, 0))
        XCTAssertTrue(digest.contains("No reminders"), "digest was: \(digest)")
    }

    func testCompletedRemindersAreOmitted() {
        let reminders = [
            ReminderModel(title: "Done thing", due: date(2026, 9, 21, 12, 0), isCompleted: true),
            ReminderModel(title: "Open thing", due: nil, isCompleted: false),
        ]
        let digest = CalendarDigestFormatter.remindersDigest(reminders, now: date(2026, 9, 21, 18, 0))
        XCTAssertFalse(digest.contains("Done thing"), "digest was: \(digest)")
        XCTAssertTrue(digest.contains("Open thing"), "digest was: \(digest)")
    }

    // MARK: - Contacts digest

    func testContactsDigestMatchesNameCaseInsensitive() {
        // Pure formatter layer: matching was applied before this point. The
        // provider-level case-insensitive substring match is exercised in
        // testContactsFormatterModelLineShape (model→line) plus provider docs.
        let contacts = [
            ContactModel(name: "Sarah Nielsen", phone: "mobile (415) 555-0132", email: "sarah@nielsen.example"),
            ContactModel(name: "Morgan Nielsen", phone: nil, email: nil),
        ]
        let digest = ContactsDigestFormatter.digest(for: "mom", matches: contacts)
        XCTAssertTrue(digest.contains("Sarah Nielsen — mobile (415) 555-0132"), "digest was: \(digest)")
        XCTAssertTrue(digest.contains("Morgan Nielsen"), "digest was: \(digest)")
    }

    func testContactsDigestSaysNotFound() {
        XCTAssertEqual(ContactsDigestFormatter.digest(for: "zzz", matches: []),
                       "No contacts found matching zzz.")
    }

    func testContactsFormatterModelLineShape() {
        // Model → line: name + phone + email joined with commas after an em dash.
        XCTAssertEqual(ContactsDigestFormatter.line(for: ContactModel(name: "Sarah Nielsen",
                                                                     phone: "(415) 555-0132",
                                                                     email: "sarah@nielsen.example")),
                       "Sarah Nielsen — (415) 555-0132, sarah@nielsen.example")
        XCTAssertEqual(ContactsDigestFormatter.line(for: ContactModel(name: "Solo", phone: nil, email: nil)),
                       "Solo")
    }

    // MARK: - Photos digest

    func testPhotosDigestCountsByDayAndFavorites() {
        // Sept 20 2026 is a Sunday (verified via `date`), Sept 25 a Friday.
        let stats = PhotoStats(
            total: 42, favorites: 3, oldest: date(2026, 9, 19), newest: date(2026, 9, 26, 20, 15),
            byWeekday: ["Sunday": 8, "Friday": 12])
        let digest = PhotosDigestFormatter.digest(stats, now: date(2026, 9, 26, 21, 0))
        XCTAssertTrue(digest.contains("42 items"), "digest was: \(digest)")
        XCTAssertTrue(digest.contains("3 favorites"), "digest was: \(digest)")
        XCTAssertTrue(digest.contains("12 on Friday"), "digest was: \(digest)")
        XCTAssertTrue(digest.contains("8 on Sunday"), "digest was: \(digest)")
        XCTAssertTrue(digest.contains("The newest is from today at 8:15 PM"), "digest was: \(digest)")
    }

    func testPhotosDigestSaysNoPhotos() {
        let empty = PhotoStats(total: 0, favorites: 0, oldest: nil, newest: nil, byWeekday: [:])
        let digest = PhotosDigestFormatter.digest(empty, now: date(2026, 9, 26, 21, 0))
        XCTAssertTrue(digest.contains("No photos found in the last 7 days"), "digest was: \(digest)")
    }

    // MARK: - Truncation

    func testDigestsTruncateAt8KiB() {
        let giant = EventModel(title: String(repeating: "x", count: 10_000),
                               start: thursday0930(), minutes: 30, calendarName: "Home")
        let digest = CalendarDigestFormatter.eventsDigest([giant], rangeLabel: "for the next day")
        XCTAssertTrue(digest.contains("…(truncated)"), "digest must carry the truncation marker")
        XCTAssertLessThanOrEqual(digest.utf8.count, 8192 + "…(truncated)".utf8.count)
    }

    func testClampKeepsShortDigestIntact() {
        XCTAssertEqual(DigestFormatter.clamp("hello"), "hello")
    }
}
