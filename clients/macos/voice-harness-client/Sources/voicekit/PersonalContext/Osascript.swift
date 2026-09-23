import Foundation

#if os(macOS)
/// Bounded AppleScript execution through `/usr/bin/osascript`.
/// macOS-only: `Process` does not exist in iOS Foundation (the iOS SDK does
/// not export it), and AppleScript automation only exists on macOS anyway.
///
/// Every run is bounded: the child gets a wall-clock limit (10 s default) and
/// is killed via `terminate()` when it overruns, so a wedged `delay` can
/// never hang the session. Stdout and stderr are drained concurrently from
/// spawn, so a chatty stderr cannot deadlock stdout. Scripts are passed via
/// `osascript -e` and stdin is a closed pipe — nothing is ever read from
/// stdin, so the plan's 5 s stdin bound holds structurally.
public enum Osascript {
    public enum Error: Swift.Error, Equatable {
        /// osascript is not present/executable.
        case missingBinary
        /// The script exceeded its wall-clock bound and was killed.
        case timeout
        /// osascript exited non-zero; carries the stderr text.
        case script(String)
    }

    private static let binaryPath = "/usr/bin/osascript"

    /// Runs one AppleScript and returns its trimmed stdout.
    public static func run(_ script: String, timeout: Duration = .seconds(10)) async throws -> String {
        guard FileManager.default.isExecutableFile(atPath: binaryPath) else {
            throw Error.missingBinary
        }

        let process = Process()
        process.executableURL = URL(fileURLWithPath: binaryPath)
        process.arguments = ["-e", script]
        let stdoutPipe = Pipe()
        let stderrPipe = Pipe()
        process.standardOutput = stdoutPipe
        process.standardError = stderrPipe
        // Closed stdin: scripts arrive via argv, so osascript reading stdin
        // would hit EOF immediately (no interactive hang possible).
        process.standardInput = Pipe()

        try process.run()

        // Process/Pipe are thread-safe for this usage pattern; strict
        // concurrency cannot know that, so opt out locally.
        nonisolated(unsafe) let stdoutHandle = stdoutPipe.fileHandleForReading
        nonisolated(unsafe) let stderrHandle = stderrPipe.fileHandleForReading
        nonisolated(unsafe) let child = process
        let timeoutBox = TimeoutBox()

        return try await withThrowingTaskGroup(of: String.self) { group in
            group.addTask {
                // Concurrent pipe reads both directions.
                let outTask = Task.detached(priority: .userInitiated) {
                    stdoutHandle.readDataToEndOfFile()
                }
                let errTask = Task.detached(priority: .userInitiated) {
                    stderrHandle.readDataToEndOfFile()
                }
                await child.waitUntilExit()
                let stdout = String(decoding: await outTask.value, as: UTF8.self)
                    .trimmingCharacters(in: .whitespacesAndNewlines)
                let stderr = String(decoding: await errTask.value, as: UTF8.self)
                    .trimmingCharacters(in: .whitespacesAndNewlines)
                if timeoutBox.isTimedOut { throw Error.timeout }
                if child.terminationStatus != 0 {
                    throw Error.script(stderr.isEmpty ? "osascript exited \(child.terminationStatus)" : stderr)
                }
                return stdout
            }
            group.addTask {
                try await Task.sleep(for: timeout)
                timeoutBox.mark()
                child.terminate() // bound enforcement: kill the runaway child
                throw Error.timeout
            }
            let first = try await group.next()!
            group.cancelAll()
            return first
        }
    }

    /// Cross-task flag so the worker can tell a killed run (timeout) from a
    /// natural non-zero exit.
    private final class TimeoutBox: @unchecked Sendable {
        private let lock = NSLock()
        private var timedOut = false

        func mark() {
            lock.lock(); timedOut = true; lock.unlock()
        }

        var isTimedOut: Bool {
            lock.lock(); defer { lock.unlock() }
            return timedOut
        }
    }
}
#endif
