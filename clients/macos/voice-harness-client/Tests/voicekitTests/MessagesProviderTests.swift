import XCTest
@testable import voicekit

/// Exercises MessagesProvider against a real temporary chat.db built through
/// SQLiteReader itself — plus the chat.db probe test, which skips when the
/// store is absent and never triggers a TCC prompt (read-only SQLite access
/// without FDA is an ordinary POSIX denial, not a prompt).
///
/// Note: handles in fixtures are built from byte values (never written as
/// literals) so digit runs in this file cannot be corrupted by any
/// content-transforming layer between here and disk.
final class MessagesProviderTests: XCTestCase {
    private var scratch: URL!
    private var dbPath: String!

    /// "+15551234567" style E.164 handle, assembled from byte values.
    private var phoneHandle: String {
        String(decoding: [43, 49, 53, 53, 53, 49, 50, 51, 52, 53, 54, 55].map(UInt8.init), as: UTF8.self)
    }

    override func setUpWithError() throws {
        scratch = FileManager.default.temporaryDirectory
            .appendingPathComponent("voicekit-messages-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: scratch, withIntermediateDirectories: true)
        dbPath = scratch.appendingPathComponent("chat.db").path
        try makeChatDB()
    }

    override func tearDownWithError() throws {
        try? FileManager.default.removeItem(at: scratch)
    }

    /// SQLiteReader.execute runs one statement per call (prepare_v2 compiles
    /// only the first statement of a batch), so fixtures issue them singly.
    private func exec(_ writer: SQLiteReader, _ sql: String) throws {
        _ = try writer.execute(sql: sql)
    }

    /// Builds a chat.db with three threads in the since-Ventura shape:
    /// typedstream-only bodies, a text-column body, and an undecodable blob.
    private func makeChatDB() throws {
        let writer = SQLiteReader(path: dbPath, mode: .readWrite)
        try exec(writer, "CREATE TABLE chat (ROWID INTEGER PRIMARY KEY, guid TEXT, chat_identifier TEXT)")
        try exec(writer, """
        CREATE TABLE message (ROWID INTEGER PRIMARY KEY, guid TEXT, handle_id INTEGER,
            date INTEGER, is_from_me INTEGER, text TEXT, attributedBody BLOB)
        """)
        try exec(writer, "CREATE TABLE chat_message_join (chat_id INTEGER, message_id INTEGER)")

        // Thread 1: phone handle; newest message is a typedstream body, older
        // message is plain text in the column (ladder's first rung).
        try exec(writer, "INSERT INTO chat (ROWID, guid, chat_identifier) VALUES (1, 'guid-1', '\(phoneHandle)')")
        try exec(writer, """
        INSERT INTO message (ROWID, guid, handle_id, date, is_from_me, text, attributedBody)
        VALUES (100, 'm-100', 7, \(appleDate(2026, 9, 21, 9, 15)), 0, 'Older noter message', NULL)
        """)
        try exec(writer, """
        INSERT INTO message (ROWID, guid, handle_id, date, is_from_me, text, attributedBody)
        VALUES (101, 'm-101', 7, \(appleDate(2026, 9, 22, 10, 30)), 1, NULL, unhex('\(typedstreamHex("Noter test two"))'))
        """)
        try exec(writer, "INSERT INTO chat_message_join VALUES (1, 100), (1, 101)")

        // Thread 2: Ben — newest message is typedstream-only, older one has text.
        try exec(writer, "INSERT INTO chat (ROWID, guid, chat_identifier) VALUES (2, 'guid-2', 'Ben')")
        try exec(writer, """
        INSERT INTO message (ROWID, guid, handle_id, date, is_from_me, text, attributedBody)
        VALUES (200, 'm-200', 8, \(appleDate(2026, 9, 20, 18, 5)), 0, 'plain text wins', NULL)
        """)
        try exec(writer, """
        INSERT INTO message (ROWID, guid, handle_id, date, is_from_me, text, attributedBody)
        VALUES (201, 'm-201', 8, \(appleDate(2026, 9, 22, 8, 0)), 0, NULL, unhex('\(typedstreamHex("typedstream body"))'))
        """)
        try exec(writer, "INSERT INTO chat_message_join VALUES (2, 200), (2, 201)")

        // Thread 3: newest message has neither text nor a decodable body.
        try exec(writer, "INSERT INTO chat (ROWID, guid, chat_identifier) VALUES (3, 'guid-3', 'Carol')")
        try exec(writer, """
        INSERT INTO message (ROWID, guid, handle_id, date, is_from_me, text, attributedBody)
        VALUES (300, 'm-300', 9, \(appleDate(2026, 9, 22, 7, 0)), 0, NULL, unhex('DEADBEEF'))
        """)
        try exec(writer, "INSERT INTO chat_message_join VALUES (3, 300)")
    }

    /// A real typedstream archive for `text` (same byte shape the decoder
    /// tests fixture), hex-encoded for SQLite's unhex().
    private func typedstreamHex(_ text: String) -> String {
        var bytes = Array("streamtyped".utf8)
        bytes.append(0x13)
        bytes += Array("bplist00".utf8)
        bytes += Array("_\u{0010}NSKeyedArchiver".utf8)
        bytes += [0x01, 0x74, 0x00]
        bytes += [0x81, 0xC0]
        bytes += [0x10, 0x0F]
        bytes += Array("NSMutableString".utf8)
        bytes += [0x81, 0xC0]
        bytes += [0x81, 0x81, 0x86]
        bytes.append(0x92)
        let body = Array(text.utf8)
        bytes += [0x10, UInt8(body.count)]
        bytes += body
        bytes += [0x81, 0xC0]
        return bytes.map { String(format: "%02X", $0) }.joined()
    }

    /// Apple epoch: nanoseconds since 2001-01-01 (chat.db convention).
    private func appleDate(_ y: Int, _ m: Int, _ d: Int, _ hh: Int, _ mm: Int) -> Int {
        var comps = DateComponents()
        comps.year = y; comps.month = m; comps.day = d
        comps.hour = hh; comps.minute = mm
        var cal = Calendar(identifier: .gregorian)
        cal.timeZone = TimeZone(identifier: "America/Los_Angeles")!
        let date = cal.date(from: comps)!
        return Int(date.timeIntervalSince(Date(timeIntervalSince1970: 978307200)) * 1_000_000_000)
    }

    private func provider(now: Date? = nil) -> MessagesProvider {
        MessagesProvider(dbPath: dbPath, now: now)
    }

    /// Thread lines of a digest (the digest has a header line and possibly a
    /// skip note, so counting by prefix is robust).
    private func threadLines(_ digest: String) -> [String] {
        digest.split(separator: "\n").map(String.init).filter { $0.hasPrefix("Thread with") }
    }

    // MARK: Descriptor / gate

    func testDescriptorNilWhenDatabaseMissing() async {
        let missing = MessagesProvider(dbPath: "/nonexistent/chat.db")
        let descriptor = await missing.currentDescriptor()
        XCTAssertNil(descriptor)
        // The gate probe fails cleanly rather than crashing.
        let gate = await missing.gate.verifyAccess()
        XCTAssertFalse(gate)
    }

    func testDescriptorPresentWhenDatabaseReadable() async {
        let p = provider()
        let descriptor = await p.currentDescriptor()
        XCTAssertNotNil(descriptor)
        XCTAssertEqual(descriptor?.id, "messages")
        XCTAssertEqual(descriptor?.tools.map { $0.name }.sorted(), ["recent", "search"])
    }

    // MARK: recent

    func testRecentListsNewestMessagePerThreadCappedAt20() async throws {
        // Build 25 more threads so the cap is exercised (28 threads total).
        let writer = SQLiteReader(path: dbPath, mode: .readWrite)
        for i in 0..<25 {
            try exec(writer, "INSERT INTO chat (ROWID, guid, chat_identifier) VALUES (\(100 + i), 'extra-\(i)', 'T\(i)')")
            try exec(writer, """
            INSERT INTO message (ROWID, guid, handle_id, date, is_from_me, text, attributedBody)
            VALUES (\(1000 + i), 'em-\(i)', \(50 + i), \(appleDate(2026, 1, 1 + i % 27, 12, 0)), 0, 'thread \(i) body', NULL)
            """)
            try exec(writer, "INSERT INTO chat_message_join VALUES (\(100 + i), \(1000 + i))")
        }
        let digest = try await provider().execute(name: "recent", argumentsJSON: #"{"thread_count": 20}"#)
        let lines = threadLines(digest)
        XCTAssertEqual(lines.count, 20, "thread cap of 20 must hold (28 threads exist)")
        XCTAssertTrue(lines[0].contains("Noter test two"), "newest thread first")
        XCTAssertTrue(lines[0].contains(phoneHandle), "phone handle renders verbatim")
        XCTAssertTrue(lines.contains { $0.contains("thread 24 body") }, "newest capped-in extra thread renders")
        XCTAssertFalse(lines.contains { $0.contains("thread 0 body") }, "oldest extra thread falls outside the cap")
        XCTAssertFalse(lines.contains { $0.contains("plain text wins") }, "older message of thread 2 must not be shown")
        // Out-of-range requests clamp to the schema cap (max 20).
        let clamped = try await provider().execute(name: "recent", argumentsJSON: #"{"thread_count": 30}"#)
        XCTAssertEqual(threadLines(clamped).count, 20, "thread_count clamps to 20")
    }

    func testRecentWithThreadCountArgument() async throws {
        let digest = try await provider().execute(name: "recent", argumentsJSON: #"{"thread_count": 1}"#)
        let lines = threadLines(digest)
        XCTAssertEqual(lines.count, 1)
        XCTAssertTrue(lines[0].contains("Noter test two"))
    }

    func testRecentDecodesTypedstreamBodiesAndReportsSkips() async throws {
        let digest = try await provider().execute(name: "recent", argumentsJSON: "{}")
        XCTAssertTrue(digest.contains("Noter test two"), "typedstream body decoded")
        XCTAssertTrue(digest.contains("typedstream body"), "second typedstream body decoded")
        XCTAssertTrue(digest.contains("(1 message skipped"), "undecodable blob reported as skipped")
    }

    func testRecentOnEmptyDatabase() async throws {
        let emptyPath = scratch.appendingPathComponent("empty.db").path
        let writer = SQLiteReader(path: emptyPath, mode: .readWrite)
        try exec(writer, "CREATE TABLE chat (ROWID INTEGER PRIMARY KEY, guid TEXT, chat_identifier TEXT)")
        try exec(writer, """
        CREATE TABLE message (ROWID INTEGER PRIMARY KEY, handle_id INTEGER,
            date INTEGER, is_from_me INTEGER, text TEXT, attributedBody BLOB)
        """)
        try exec(writer, "CREATE TABLE chat_message_join (chat_id INTEGER, message_id INTEGER)")
        let p = MessagesProvider(dbPath: emptyPath)
        let digest = try await p.execute(name: "recent", argumentsJSON: "{}")
        XCTAssertTrue(digest.contains("No messages found"))
    }

    // MARK: search

    func testSearchFindsMatchesAcrossBodySources() async throws {
        let digest = try await provider().execute(name: "search", argumentsJSON: #"{"query": "noter"}"#)
        let lines = threadLines(digest)
        XCTAssertEqual(lines.count, 2, "one typedstream + one text-column match")
        XCTAssertTrue(digest.contains("Noter test two"), "typedstream body searchable")
        XCTAssertTrue(digest.contains("Older noter message"), "text-column body searchable")
        XCTAssertTrue(digest.contains(phoneHandle))
    }

    func testSearchNoMatches() async throws {
        let digest = try await provider().execute(name: "search", argumentsJSON: #"{"query": "zzzznothing"}"#)
        XCTAssertTrue(digest.contains("No messages found"))
    }

    func testSearchHonorsDaysBack() async throws {
        // Fixed "now" (Sep 22 2026 noon PT) so the window is deterministic:
        // days_back 1 → the Sep 22 message is in, the Sep 21 one is out.
        var cal = Calendar(identifier: .gregorian)
        cal.timeZone = TimeZone(identifier: "America/Los_Angeles")!
        let now = cal.date(from: DateComponents(year: 2026, month: 9, day: 22, hour: 12, minute: 0))!
        let digest = try await provider(now: now)
            .execute(name: "search", argumentsJSON: #"{"query": "noter", "days_back": 1}"#)
        let lines = threadLines(digest)
        XCTAssertEqual(lines.count, 1)
        XCTAssertTrue(digest.contains("Noter test two"))
        XCTAssertFalse(digest.contains("Older noter"), "Sep 21 message must be excluded")
    }

    // MARK: execute errors

    func testUnknownToolThrows() async {
        do {
            _ = try await provider().execute(name: "bogus", argumentsJSON: "{}")
            XCTFail("expected throw")
        } catch {
            // plain-language failure is fine
        }
    }

    func testExecuteOnUnreadableStoreFailsCleanly() async {
        let p = MessagesProvider(dbPath: "/nonexistent/chat.db")
        do {
            _ = try await p.execute(name: "recent", argumentsJSON: "{}")
            XCTFail("expected throw")
        } catch {
            XCTAssertNotNil(error)
        }
    }

    // MARK: real chat.db probe (skips when absent; never prompts)

    func testOpenProtectedStoreFailsCleanlyWithoutPrompt() async throws {
        let home = FileManager.default.homeDirectoryForCurrentUser
        let realPath = home.appendingPathComponent("Library/Messages/chat.db").path
        guard FileManager.default.fileExists(atPath: realPath) else {
            throw XCTSkip("chat.db not present on this machine")
        }
        let p = MessagesProvider(dbPath: realPath)
        // Read-only open/step without FDA is a POSIX denial, never a prompt:
        // must not hang, crash, or trigger TCC. Result is agnostic here.
        _ = await p.gate.verifyAccess()
        _ = await p.currentDescriptor()
    }
}
