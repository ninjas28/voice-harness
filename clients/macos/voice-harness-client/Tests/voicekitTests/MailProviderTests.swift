import XCTest
@testable import voicekit

/// Exercises MailProvider with a stub AppleScript runner — no real Mail.app,
/// no automation permission, no TCC prompts.
final class MailProviderTests: XCTestCase {
    /// A Tuesday, 10:00 AM PT.
    private var tuesday10AM: Date {
        var cal = Calendar(identifier: .gregorian)
        cal.timeZone = TimeZone(identifier: "America/Los_Angeles")!
        return cal.date(from: DateComponents(year: 2026, month: 9, day: 22, hour: 10, minute: 0))!
    }

    /// Lock-guarded script capture box (the runner closure is Sendable and
    /// runs off the test's executor).
    private final class ScriptCapture: @unchecked Sendable {
        private let lock = NSLock()
        private var storage: [String] = []
        func append(_ script: String) {
            lock.lock(); storage.append(script); lock.unlock()
        }
        var scripts: [String] {
            lock.lock(); defer { lock.unlock() }
            return storage
        }
    }

    /// Stub runner returning fixed lines in osascript "item" field order.
    private func stubRunner(lines: [String]) -> AppleScriptRunner {
        return { [lines] _ in lines.joined(separator: "\n") }
    }

    private let twoMessageLines = [
        // Fields: sender|subject|ISO date|content|read flag (true = read).
        "Ben|Kickoff|2026-09-22T10:00:00|Agenda attached|true",
        "Ops|Alert|2026-09-21T16:30:00|See dashboard|false",
    ]

    // MARK: Script shape

    func testRecentScriptShape() async throws {
        let capture = ScriptCapture()
        let runner: AppleScriptRunner = { script in
            capture.append(script)
            return ""
        }
        let provider = MailProvider(runner: runner)
        _ = try await provider.execute(name: "recent", argumentsJSON: "{}")
        let script = try XCTUnwrap(capture.scripts.first)
        XCTAssertTrue(script.contains("application \"Mail\""), "targets Mail: \(script)")
        XCTAssertTrue(script.contains("not running"), "gate probe first: \(script)")
        XCTAssertTrue(script.contains("inbox"), "reads the inbox: \(script)")
        XCTAssertTrue(script.contains("sender"), "reads sender: \(script)")
        XCTAssertTrue(script.contains("subject"), "reads subject: \(script)")
        XCTAssertTrue(script.contains("date received"), "reads date received: \(script)")
        XCTAssertTrue(script.contains("content"), "reads content: \(script)")
    }

    func testSearchScriptIncludesDaysBackFilter() async throws {
        let capture = ScriptCapture()
        let runner: AppleScriptRunner = { script in
            capture.append(script)
            return ""
        }
        let provider = MailProvider(runner: runner)
        _ = try await provider.execute(name: "search",
                                       argumentsJSON: #"{"query": "kickoff", "days_back": 7}"#)
        let script = try XCTUnwrap(capture.scripts.first)
        XCTAssertTrue(script.contains("days"), "AppleScript date arithmetic present: \(script)")
        XCTAssertTrue(script.contains("kickoff"), "query filter present: \(script)")
    }

    // MARK: Digest mapping

    func testRecentMapsStubOutputToDigest() async throws {
        let provider = MailProvider(runner: stubRunner(lines: twoMessageLines))
        let digest = try await provider.execute(name: "recent", argumentsJSON: "{}")
        // Expected stamps are computed from the same parse the provider uses
        // (the ISO string's zone is the machine's current zone), so this
        // verifies field mapping, ordering, and the unread marker without
        // hardcoding a wall clock.
        let benDate = try XCTUnwrap(MailProvider.parseISO("2026-09-22T10:00:00"))
        let opsDate = try XCTUnwrap(MailProvider.parseISO("2026-09-21T16:30:00"))
        let benStamp = "\(CalendarDigestFormatter.dayHeader(for: benDate)) at \(CalendarDigestFormatter.timeString(benDate))"
        let opsStamp = "\(CalendarDigestFormatter.dayHeader(for: opsDate)) at \(CalendarDigestFormatter.timeString(opsDate))"
        XCTAssertTrue(digest.contains("Email from Ben received \(benStamp): Kickoff — Agenda attached"))
        XCTAssertTrue(digest.contains("[unread] Email from Ops received \(opsStamp): Alert — See dashboard"))
    }

    func testSearchMapsStubOutputToDigest() async throws {
        let provider = MailProvider(runner: stubRunner(lines: twoMessageLines))
        let digest = try await provider.execute(name: "search",
                                                argumentsJSON: #"{"query": "alert"}"#)
        // The stub ignores the script, so both messages come back; the
        // query-side filter itself is verified in the script-shape test.
        XCTAssertTrue(digest.contains("Alert"))
        XCTAssertTrue(digest.contains("Kickoff"))
    }

    // MARK: Mail not running

    func testNotRunningProducesSpokenDigest() async throws {
        let runner: AppleScriptRunner = { _ in MailProvider.notRunningSentinel }
        let provider = MailProvider(runner: runner)
        let digest = try await provider.execute(name: "recent", argumentsJSON: "{}")
        XCTAssertEqual(digest, "Email is unavailable right now — Mail isn't running.")
    }

    func testGateFailsWhenRunnerReportsNotRunning() async {
        let runner: AppleScriptRunner = { _ in MailProvider.notRunningSentinel }
        let provider = MailProvider(runner: runner)
        let descriptor = await provider.currentDescriptor()
        XCTAssertNil(descriptor)
    }

    func testGateSucceedsWhenRunnerReturnsAccountCount() async {
        let runner: AppleScriptRunner = { _ in "2" }
        let provider = MailProvider(runner: runner)
        let descriptor = await provider.currentDescriptor()
        XCTAssertNotNil(descriptor)
        XCTAssertEqual(descriptor?.id, "mail")
    }

    // MARK: Robustness

    func testRunnerThrowsSurfaceAsPersonalContextError() async {
        let runner: AppleScriptRunner = { _ in throw Osascript.Error.timeout }
        let provider = MailProvider(runner: runner)
        do {
            _ = try await provider.execute(name: "recent", argumentsJSON: "{}")
            XCTFail("expected throw")
        } catch let error as PersonalContextError {
            guard case .failed = error else {
                return XCTFail("expected .failed, got \(error)")
            }
        } catch {
            XCTFail("unexpected error type: \(error)")
        }
    }

    func testMalformedLinesAreSkippedNotFatal() async throws {
        let lines = twoMessageLines + ["garbage-without-pipes", ""]
        let provider = MailProvider(runner: stubRunner(lines: lines))
        let digest = try await provider.execute(name: "recent", argumentsJSON: "{}")
        XCTAssertEqual(digest.split(separator: "\n").count, 3, "header + two well-formed lines")
    }

    func testUnknownToolThrows() async {
        let provider = MailProvider(runner: stubRunner(lines: []))
        do {
            _ = try await provider.execute(name: "bogus", argumentsJSON: "{}")
            XCTFail("expected throw")
        } catch {
            // fine
        }
    }

}
