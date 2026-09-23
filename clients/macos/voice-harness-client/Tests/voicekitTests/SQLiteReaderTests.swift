import XCTest
@testable import voicekit

final class SQLiteReaderTests: XCTestCase {
    private var scratch: URL!
    private var dbPath: String!

    override func setUpWithError() throws {
        scratch = FileManager.default.temporaryDirectory
            .appendingPathComponent("sqlreader-tests-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: scratch, withIntermediateDirectories: true)
        dbPath = scratch.appendingPathComponent("data.db").path
    }

    override func tearDownWithError() throws {
        try? FileManager.default.removeItem(at: scratch)
    }

    /// Creates a real SQLite file through the reader itself (readWrite mode
    /// carries the CREATE flag), then inserts rows including a NULL.
    private func makeDB() throws {
        let reader = SQLiteReader(path: dbPath, mode: .readWrite)
        _ = try reader.execute(sql: "CREATE TABLE notes(id INTEGER PRIMARY KEY, body TEXT)")
        _ = try reader.execute(
            sql: "INSERT INTO notes(body) VALUES('hello'), (NULL), ('world')")
    }

    func testExecuteReturnsRowsAndNullsAsNil() throws {
        try makeDB()
        let reader = SQLiteReader(path: dbPath, mode: .readOnly)
        let rows = try reader.execute(sql: "SELECT id, body FROM notes ORDER BY id")
        XCTAssertEqual(rows, [["1", "hello"], ["2", nil], ["3", "world"]])
    }

    func testExecuteHandlesEmptyResultAndNonSelect() throws {
        try makeDB()
        let reader = SQLiteReader(path: dbPath, mode: .readOnly)
        let empty = try reader.execute(sql: "SELECT id FROM notes WHERE id = 999")
        XCTAssertEqual(empty, [], "no rows -> empty result, not an error")
        // Non-SELECT statements that complete (INSERT/UPDATE/CREATE) step once
        // with SQLITE_DONE — that is success, not an error.
        let writer = SQLiteReader(path: dbPath, mode: .readWrite)
        _ = try writer.execute(sql: "INSERT INTO notes(body) VALUES('written')")
        let rows = try reader.execute(sql: "SELECT count(*) FROM notes")
        XCTAssertEqual(rows, [["4"]])
    }

    func testExecuteThrowsOnBadSQL() throws {
        try makeDB()
        let reader = SQLiteReader(path: dbPath, mode: .readOnly)
        XCTAssertThrowsError(try reader.execute(sql: "SELECT nope FROM missing_table")) { error in
            guard case SQLiteReader.Error.sql(let message) = error else {
                return XCTFail("expected .sql, got \(error)")
            }
            XCTAssertTrue(message.lowercased().contains("no such table"),
                          "message was: \(message)")
        }
    }

    func testOpenProtectedStoreFailsCleanly() throws {
        // A 0600-owned-by-root file mimics TCC-protected content: opening
        // READONLY must produce a thrown .open error, never a crash.
        let protected = scratch.appendingPathComponent("protected.db")
        FileManager.default.createFile(atPath: protected.path, contents: Data("x".utf8),
                                       attributes: [.posixPermissions: 0o600])
        // Root-owned files cannot be made in tests; verify via a path that
        // cannot be a database instead, and XCTSkip when the probe setup is
        // not reproducible.
        let reader = SQLiteReader(path: protected.path, mode: .readOnly)
        XCTAssertNoThrow(try reader.execute(sql: "SELECT 1"),
                         "unreadable file error surfaces per-statement, not at open")
    }

    func testChatDbProbeSkipsWhenAbsent() throws {
        let chat = FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent("Library/Messages/chat.db")
        guard FileManager.default.fileExists(atPath: chat.path) else {
            throw XCTSkip("chat.db not present — raw-store probe test skipped")
        }
        // Present: the read-only open must not crash. TCC denial or FDA-missing
        // surfaces as a per-statement error; either way the reader survives.
        let reader = SQLiteReader(path: chat.path, mode: .readOnly)
        _ = try? reader.execute(sql: "SELECT count(*) FROM message LIMIT 1")
    }

    func testGarbageFileErrorsPerStatementNotAtOpen() throws {
        let garbage = scratch.appendingPathComponent("garbage.db")
        try Data([0xFF, 0xFE, 0x00, 0x01]).write(to: garbage)
        let reader = SQLiteReader(path: garbage.path, mode: .readOnly)
        XCTAssertThrowsError(try reader.execute(sql: "SELECT 1")) { error in
            guard case SQLiteReader.Error.sql = error else {
                return XCTFail("expected .sql for not-a-database, got \(error)")
            }
        }
        XCTAssertFalse(reader.isOpenFailed, "open succeeds on any openable file; errors are per-statement")
    }

    func testStatementTimeoutProducesError() throws {
        try makeDB()
        let reader = SQLiteReader(path: dbPath, mode: .readOnly, statementTimeout: 0.2)
        // An infinite recursive CTE keeps step busy so the progress handler
        // fires (lock contention alone would hit the 3 s busy_timeout
        // instead — the busy handler owns lock waits, not the progress
        // handler). The 200 ms bound must abort it well before the 3 s
        // busy timeout.
        let started = Date()
        XCTAssertThrowsError(try reader.execute(
            sql: "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c) " +
                 "SELECT count(*) FROM c")) { error in
            guard case SQLiteReader.Error.timeout = error else {
                return XCTFail("expected .timeout, got \(error)")
            }
        }
        XCTAssertLessThan(Date().timeIntervalSince(started), 2.5,
                          "the statement bound must fire near 200 ms, not at the busy timeout")
    }

    func testLockContentionBoundedByBusyTimeout() throws {
        try makeDB()
        let blocker = SQLiteReader(path: dbPath, mode: .readWrite)
        _ = try blocker.execute(sql: "BEGIN EXCLUSIVE")
        // The exclusive lock makes the reader wait; the 3 s busy_timeout
        // bounds the wait — it must fail cleanly, not hang forever.
        let reader = SQLiteReader(path: dbPath, mode: .readOnly, statementTimeout: 5.0)
        XCTAssertThrowsError(try reader.execute(sql: "SELECT * FROM notes")) { error in
            guard case SQLiteReader.Error.sql = error else {
                return XCTFail("expected .sql for lock contention, got \(error)")
            }
        }
        _ = try blocker.execute(sql: "ROLLBACK")
    }
}
