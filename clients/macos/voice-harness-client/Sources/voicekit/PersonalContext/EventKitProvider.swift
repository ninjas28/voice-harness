import Foundation
@preconcurrency import EventKit

/// Personal-context provider over EventKit: calendar events + reminders.
/// Authorization is lazy — the TCC prompt happens only when `authorize()` is
/// called (or a tool runs); never at session start. An actor so the shared
/// `EKEventStore` is isolated and the Sendable conformance is sound.
actor EventKitProvider: PersonalContextProvider {
    let providerId = "calendar"
    private let store = EKEventStore()

    static func descriptor() -> ProviderDescriptor {
        ProviderDescriptor(id: "calendar", tools: [
            ToolDescriptor(
                name: "events",
                description: "List upcoming calendar events for a range of days.",
                parameters: [
                    "type": .string("object"),
                    "properties": .object([
                        "days_ahead": .object([
                            "type": .string("integer"),
                            "minimum": .int(0), "maximum": .int(30), "default": .int(1),
                            "description": .string("Days ahead of today to include."),
                        ]),
                        "days_back": .object([
                            "type": .string("integer"),
                            "minimum": .int(0), "maximum": .int(7), "default": .int(0),
                            "description": .string("Days back from today to include."),
                        ]),
                        "calendar": .object([
                            "type": .string("string"),
                            "description": .string("Only events from the calendar with this exact name."),
                        ]),
                    ]),
                ]),
            ToolDescriptor(
                name: "reminders",
                description: "List reminders (overdue, today, upcoming).",
                parameters: [
                    "type": .string("object"),
                    "properties": .object([
                        "include_completed": .object([
                            "type": .string("boolean"),
                            "default": .bool(false),
                            "description": .string("Also include completed reminders."),
                        ]),
                        "days_ahead": .object([
                            "type": .string("integer"),
                            "default": .int(7),
                            "description": .string("Days ahead of today to include."),
                        ]),
                    ]),
                ]),
        ])
    }

    func currentDescriptor() async -> ProviderDescriptor? {
        await authorize() ? Self.descriptor() : nil
    }

    func execute(name: String, argumentsJSON: String) async throws -> String {
        guard await authorize() else { throw PersonalContextError.notAuthorized("calendar") }
        let args = try PersonalContextArguments.parse(argumentsJSON)
        switch name {
        case "events":
            let daysAhead = PersonalContextArguments.int(args, "days_ahead", default: 1, min: 0, max: 30)
            let daysBack = PersonalContextArguments.int(args, "days_back", default: 0, min: 0, max: 7)
            let calendarName = PersonalContextArguments.string(args, "calendar")
            return eventsDigest(daysAhead: daysAhead, daysBack: daysBack, calendarName: calendarName)
        case "reminders":
            let includeCompleted = (args["include_completed"] == .bool(true))
            let daysAhead = PersonalContextArguments.int(args, "days_ahead", default: 7, min: 0, max: 60)
            return await remindersDigest(includeCompleted: includeCompleted, daysAhead: daysAhead)
        default:
            throw PersonalContextError.failed("unknown calendar tool '\(name)'")
        }
    }

    /// Requests full events + reminders access. Read-only access exists but
    /// v1 keeps one code path (plan decision). No throw — a denied answer is
    /// just `false`.
    private func authorize() async -> Bool {
        if #available(macOS 14.0, iOS 17.0, *) {
            let eventsGranted = (try? await store.requestFullAccessToEvents()) ?? false
            let remindersGranted = (try? await store.requestFullAccessToReminders()) ?? false
            return eventsGranted && remindersGranted
        } else {
            return (try? await store.requestAccess(to: .event)) == true
        }
    }

    // MARK: - Fetch → model mapping (thin, unmocked)

    private func eventsDigest(daysAhead: Int, daysBack: Int, calendarName: String?) -> String {
        let cal = Calendar.current
        let now = Date()
        let start = cal.startOfDay(for: cal.date(byAdding: .day, value: -daysBack, to: now)!)
        let end = cal.date(byAdding: .day, value: daysAhead, to: cal.startOfDay(for: now))!

        let calendars = store.calendars(for: .event)
            .filter { calendarName == nil || $0.title == calendarName }
        guard !calendars.isEmpty else {
            return calendarName == nil
                ? "No events found for the range."
                : "No calendar named \(calendarName ?? "") was found."
        }
        let predicate = store.predicateForEvents(withStart: start, end: end, calendars: calendars)
        let models = store.events(matching: predicate).map { event in
            EventModel(title: event.title.isEmpty ? "Untitled event" : event.title,
                       start: event.startDate,
                       minutes: event.isAllDay ? nil : Int(event.endDate.timeIntervalSince(event.startDate) / 60),
                       calendarName: event.calendar.title,
                       isAllDay: event.isAllDay)
        }
        return CalendarDigestFormatter.eventsDigest(
            models, rangeLabel: rangeLabel(daysBack: daysBack, daysAhead: daysAhead))
    }

    private func remindersDigest(includeCompleted: Bool, daysAhead: Int) async -> String {
        let cal = Calendar.current
        let now = Date()
        let deadline = Date().addingTimeInterval(5)
        let cutoff = cal.date(byAdding: .day, value: daysAhead, to: cal.startOfDay(for: now))!
        let box = ReminderBox()
        // fetchReminders is callback-based; bridge it with a bounded wait
        // (never an unbounded block — AGENTS.md "bounded everything").
        store.fetchReminders(matching: store.predicateForReminders(in: store.calendars(for: .reminder))) { items in
            for item in items ?? [] {
                if item.isCompleted && !includeCompleted { continue }
                if let due = item.dueDateComponents?.date, due > cutoff { continue }
                box.append(ReminderModel(title: item.title ?? "Untitled reminder",
                                         due: item.dueDateComponents?.date,
                                         isCompleted: item.isCompleted))
            }
            box.finish()
        }
        while !box.isFinished, Date() < deadline {
            try? await Task.sleep(for: .milliseconds(20))
        }
        return CalendarDigestFormatter.remindersDigest(box.models, now: now)
    }

    private func rangeLabel(daysBack: Int, daysAhead: Int) -> String {
        switch (daysBack, daysAhead) {
        case (0, 0): return "for today"
        case (0, 1): return "for the next day"
        case (0, let ahead): return "for the next \(ahead) days"
        case (let back, 0): return "for the past \(back) days"
        case (let back, let ahead): return "from \(back) days back through the next \(ahead) days"
        }
    }
}

/// Lock-guarded accumulator for the callback-based fetchReminders bridge
/// (the callback runs on a nonisolated queue and cannot await an actor).
private final class ReminderBox: @unchecked Sendable {
    private let lock = NSLock()
    private var storage: [ReminderModel] = []
    private var finished = false

    var models: [ReminderModel] {
        lock.lock(); defer { lock.unlock() }
        return storage
    }

    var isFinished: Bool {
        lock.lock(); defer { lock.unlock() }
        return finished
    }

    func append(_ model: ReminderModel) {
        lock.lock(); storage.append(model); lock.unlock()
    }

    func finish() {
        lock.lock(); finished = true; lock.unlock()
    }
}
