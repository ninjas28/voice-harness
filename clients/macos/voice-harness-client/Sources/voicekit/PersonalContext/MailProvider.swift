import Foundation

/// Closure seam for AppleScript execution (real runner uses `Osascript.run`).
public typealias AppleScriptRunner = @Sendable (String) async throws -> String

/// Personal-context provider over Mail.app via bounded osascript. The gate
/// probe (`application "Mail" is running` + a trivial read) doubles as the
/// automation TCC moment: the first real scripted access fires the prompt —
/// only when the user flips the raw-stores toggle, never in tests (tests use
/// a stub runner). Mail-not-running is a distinct, spoken signal.
///
/// The whole type is macOS-gated in one block: `voicekit` must still compile
/// for iOS, where Mail scripting does not exist.
#if os(macOS)
public actor MailProvider: PersonalContextProvider {
    public let providerId = "mail"

    /// The sentinel the AppleScript prints when Mail isn't running.
    static let notRunningSentinel = "NOT_RUNNING"

    private let runner: AppleScriptRunner

    /// - Parameter runner: AppleScript execution seam; defaults to the real
    ///   bounded osascript runner.
    public init(runner: @escaping AppleScriptRunner = { try await Osascript.run($0, timeout: .seconds(10)) }) {
        self.runner = runner
    }

    // nonisolated: the protocol requirement is nonisolated; the gate value
    // itself is Sendable (the actor reference inside it is too).
    nonisolated public var gate: PersonalContextGate { MailGate(provider: self) }

    public func currentDescriptor() async -> ProviderDescriptor? {
        guard await gate.verifyAccess() else { return nil }
        return Self.descriptor()
    }

    static func descriptor() -> ProviderDescriptor {
        ProviderDescriptor(id: "mail", tools: [
            ToolDescriptor(
                name: "recent",
                description: "List recent emails from the inbox, newest first.",
                parameters: [
                    "type": .string("object"),
                    "properties": .object([
                        "count": .object([
                            "type": .string("integer"),
                            "minimum": .int(1), "maximum": .int(20), "default": .int(5),
                            "description": .string("How many emails to include."),
                        ]),
                    ]),
                ]),
            ToolDescriptor(
                name: "search",
                description: "Search recent emails by subject or content.",
                parameters: [
                    "type": .string("object"),
                    "properties": .object([
                        "query": .object([
                            "type": .string("string"),
                            "description": .string("Text to look for in subject or content."),
                        ]),
                        "days_back": .object([
                            "type": .string("integer"),
                            "minimum": .int(1), "maximum": .int(90), "default": .int(14),
                            "description": .string("How many days back to search."),
                        ]),
                        "limit": .object([
                            "type": .string("integer"),
                            "minimum": .int(1), "maximum": .int(20), "default": .int(5),
                            "description": .string("Maximum number of matching emails."),
                        ]),
                    ]),
                ]),
        ])
    }

    public func execute(name: String, argumentsJSON: String) async throws -> String {
        let args = try PersonalContextArguments.parse(argumentsJSON)
        switch name {
        case "recent":
            let count = PersonalContextArguments.int(args, "count", default: 5, min: 1, max: 20)
            return try await runDigest(script: Self.recentScript(count: count))
        case "search":
            guard let query = PersonalContextArguments.string(args, "query") else {
                throw PersonalContextError.invalidArguments("search needs a query")
            }
            let daysBack = PersonalContextArguments.int(args, "days_back", default: 14, min: 1, max: 90)
            let limit = PersonalContextArguments.int(args, "limit", default: 5, min: 1, max: 20)
            return try await runDigest(script: Self.searchScript(query: query, daysBack: daysBack, limit: limit))
        default:
            throw PersonalContextError.failed("unknown mail tool '\(name)'")
        }
    }

    /// Runs one mail script and maps its output to a spoken digest. The
    /// not-running sentinel becomes the distinct spoken signal instead of an
    /// error (plan wording).
    private func runDigest(script: String) async throws -> String {
        let output: String
        do {
            output = try await runner(script)
        } catch {
            throw PersonalContextError.failed("Email lookup failed: \(errorText(error))")
        }
        if output.contains(Self.notRunningSentinel) {
            return MailDigestFormatter.notRunningDigest
        }
        let mails = Self.parse(output)
        return MailDigestFormatter.mailDigest(mails)
    }

    private func errorText(_ error: Swift.Error) -> String {
        if let osascriptError = error as? Osascript.Error {
            switch osascriptError {
            case .timeout: return "the script timed out"
            case .missingBinary: return "osascript is missing"
            case .script(let message): return message
            }
        }
        if let contextError = error as? PersonalContextError, case .failed(let message) = contextError {
            return message
        }
        return "\(error)"
    }

    // MARK: - AppleScript construction (pure, testable)

    /// Gate probe + digest script in one bounded round-trip: checks Mail is
    /// running, then reads inbox messages newest-first. Prints the sentinel
    /// when Mail isn't running (checked BEFORE any property read, so the
    /// not-running case never fires the automation prompt).
    static func recentScript(count: Int) -> String {
        let quotedCount = max(1, min(20, count))
        return """
        if application "Mail" is not running then
            return "\(notRunningSentinel)"
        end if
        set _out to ""
        repeat with _acct in accounts
            set _msgs to messages of inbox of _acct
            set _n to count of _msgs
            set _take to \(quotedCount)
            if _n < _take then set _take to _n
            repeat with _i from 1 to _take
                set _m to item _i of _msgs
                set _out to _out & (sender of _m) & "|" & (subject of _m) & "|" & ¬
                    ((date received of _m) as «class isot» as string) & "|" & (content of _m) & "|" & ¬
                    ((read flag of _m) as string) & linefeed
            end repeat
        end repeat
        return _out
        """
    }

    /// Same shape, filtered query-side by date and subject/content match.
    /// AppleScript string comparisons are case-insensitive by default, which
    /// is what the search wants.
    static func searchScript(query: String, daysBack: Int, limit: Int) -> String {
        // AppleScript string escaping: backslashes and quotes.
        var escaped = query.replacingOccurrences(of: "\\", with: "\\\\")
        escaped = escaped.replacingOccurrences(of: "\"", with: "\\\"")
        let quotedLimit = max(1, min(20, limit))
        let quotedDays = max(1, min(90, daysBack))
        return """
        if application "Mail" is not running then
            return "\(notRunningSentinel)"
        end if
        set _cutoff to (current date) - \(quotedDays) * days
        set _out to ""
        set _found to 0
        repeat with _acct in accounts
            if _found ≥ \(quotedLimit) then exit repeat
            set _msgs to messages of inbox of _acct
            repeat with _m in _msgs
                if _found ≥ \(quotedLimit) then exit repeat
                if (date received of _m) ≥ _cutoff then
                    set _haystack to (subject of _m) & " " & (content of _m)
                    if _haystack contains "\(escaped)" then
                        set _out to _out & (sender of _m) & "|" & (subject of _m) & "|" & ¬
                            ((date received of _m) as «class isot» as string) & "|" & (content of _m) & "|" & ¬
                            ((read flag of _m) as string) & linefeed
                        set _found to _found + 1
                    end if
                end if
            end repeat
        end repeat
        return _out
        """
    }

    // MARK: - Output parsing (pure, testable)

    /// Parses the pipe-delimited lines into MailModels. Date arrives as an
    /// ISO-8601 string from AppleScript's «class isot» coercion. Malformed
    /// lines are skipped, never fatal.
    static func parse(_ output: String) -> [MailModel] {
        var models: [MailModel] = []
        for line in output.split(separator: "\n", omittingEmptySubsequences: true) {
            let parts = line.components(separatedBy: "|")
            guard parts.count >= 5 else { continue }
            let sender = parts[0].trimmingCharacters(in: .whitespaces)
            let subject = parts[1]
            let isoDate = parts[2]
            let content = parts[3]
            let isUnread = parts[4].trimmingCharacters(in: .whitespaces).lowercased() == "false"
            guard let date = Self.parseISO(isoDate) else { continue }
            guard !sender.isEmpty else { continue }
            models.append(MailModel(sender: sender, date: date, subject: subject,
                                    content: content, isUnread: isUnread))
        }
        return models
    }

    /// AppleScript «class isot» → "2026-09-22T10:00:00" (offset-naive local).
    static func parseISO(_ text: String) -> Date? {
        let trimmed = text.trimmingCharacters(in: .whitespaces)
        guard trimmed.count >= 19 else { return nil }
        let formatter = DateFormatter()
        formatter.locale = Locale(identifier: "en_US_POSIX")
        formatter.dateFormat = "yyyy-MM-dd'T'HH:mm:ss"
        formatter.isLenient = false
        return formatter.date(from: String(trimmed.prefix(19)))
    }

    /// Gate probe: Mail running + trivial read, one bounded round-trip.
    /// Distinct not-running signal surfaces through the sentinel.
    private struct MailGate: PersonalContextGate {
        let provider: MailProvider
        func verifyAccess() async -> Bool {
            do {
                let run = await provider.runner
                let output = try await run(MailProvider.gateScript)
                return !output.contains(MailProvider.notRunningSentinel)
            } catch {
                return false
            }
        }
    }

    /// The gate script: running check + one trivial property read (count of
    /// accounts). First scripted access may fire the automation prompt — by
    /// design, only outside tests.
    static let gateScript = """
    if application "Mail" is not running then
        return "\(notRunningSentinel)"
    end if
    return (count of accounts) as string
    """
}
#endif
