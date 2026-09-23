import XCTest
import zlib
@testable import voicekit

#if os(macOS)
/// Exercises NotesProvider against a real temporary NoteStore-shaped SQLite
/// database built through SQLiteReader itself, plus the live NoteStore probe,
/// which skips when the store is absent or TCC-blocked and never triggers a
/// TCC prompt (read-only SQLite access without FDA is an ordinary POSIX
/// denial, not a prompt).
///
/// Note: fixture strings are built from byte values (never written as
/// literals) so digit runs in this file cannot be corrupted by any
/// content-transforming layer between here and disk.
final class NotesProviderTests: XCTestCase {
    private var scratch: URL!
    private var dbPath: String!

    /// An 8-char fixture token assembled from byte values.
    private var alphaToken: String {
        String(decoding: [110, 111, 116, 101, 116, 101, 115, 116].map(UInt8.init), as: UTF8.self)
    }
    private var betaToken: String {
        String(decoding: [110, 111, 116, 101, 115, 115, 101, 97].map(UInt8.init), as: UTF8.self)
    }

    override func setUpWithError() throws {
        scratch = FileManager.default.temporaryDirectory
            .appendingPathComponent("voicekit-notes-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: scratch, withIntermediateDirectories: true)
        dbPath = scratch.appendingPathComponent("NoteStore.sqlite").path
        try makeNoteStore()
    }

    override func tearDownWithError() throws {
        try? FileManager.default.removeItem(at: scratch)
    }

    /// SQLiteReader.execute runs one statement per call (prepare_v2 compiles
    /// only the first statement of a batch), so fixtures issue them singly.
    private func exec(_ writer: SQLiteReader, _ sql: String) throws {
        _ = try writer.execute(sql: sql)
    }

    /// The pinned NoteStore.sqlite subset (verified against public references;
    /// the live store was TCC-blocked for this environment).
    private func makeNoteStore() throws {
        let writer = SQLiteReader(path: dbPath, mode: .readWrite)
        try exec(writer, """
        CREATE TABLE ZICCLOUDSYNCINGOBJECT (Z_PK INTEGER PRIMARY KEY, ZTITLE1 TEXT,
            ZSNIPPET TEXT, ZFOLDER INTEGER, ZMODIFICATIONDATE1 TIMESTAMP,
            ZMARKEDFORDELETION INTEGER, ZARCHIVED INTEGER)
        """)
        try exec(writer, "CREATE TABLE ZICNOTEDATA (Z_PK INTEGER PRIMARY KEY, ZNOTE INTEGER, ZDATA BLOB)")
        try exec(writer, "CREATE TABLE ZFOLDER (Z_PK INTEGER PRIMARY KEY, ZTITLE2 TEXT)")

        // Folder 1: normal; Folder 2: Recently Deleted.
        try exec(writer, "INSERT INTO ZFOLDER (Z_PK, ZTITLE2) VALUES (1, 'Notes'), (2, 'Recently Deleted')")

        // Note 1: title only, snippet NULL, no note data → ladder rung 2.
        try exec(writer, """
        INSERT INTO ZICCLOUDSYNCINGOBJECT (Z_PK, ZTITLE1, ZSNIPPET, ZFOLDER, ZMODIFICATIONDATE1, ZMARKEDFORDELETION)
        VALUES (11, '\(alphaToken) plan', NULL, 1, \(coreDataDate(2026, 9, 22, 9, 15)), 0)
        """)

        // Note 2: snippet present, no note data → ladder rung 1.
        try exec(writer, """
        INSERT INTO ZICCLOUDSYNCINGOBJECT (Z_PK, ZTITLE1, ZSNIPPET, ZFOLDER, ZMODIFICATIONDATE1, ZMARKEDFORDELETION)
        VALUES (12, 'Snippets', 'fresh snippet text', 1, \(coreDataDate(2026, 9, 21, 14, 30)), 0)
        """)

        // Note 3: gzip+protobuf blob body → ladder rung 3 (newest, shows first).
        try exec(writer, """
        INSERT INTO ZICCLOUDSYNCINGOBJECT (Z_PK, ZTITLE1, ZSNIPPET, ZFOLDER, ZMODIFICATIONDATE1, ZMARKEDFORDELETION)
        VALUES (13, 'Blob note', NULL, 1, \(coreDataDate(2026, 9, 22, 18, 45)), 0)
        """)
        try exec(writer, "INSERT INTO ZICNOTEDATA (Z_PK, ZNOTE, ZDATA) VALUES (3, 13, unhex('\(gzipHex("blob body line"))'))")

        // Note 4: no title, blob has no text → "(untitled)" everywhere.
        try exec(writer, """
        INSERT INTO ZICCLOUDSYNCINGOBJECT (Z_PK, ZTITLE1, ZSNIPPET, ZFOLDER, ZMODIFICATIONDATE1, ZMARKEDFORDELETION)
        VALUES (14, NULL, NULL, 1, \(coreDataDate(2026, 9, 20, 8, 0)), 0)
        """)
        try exec(writer, "INSERT INTO ZICNOTEDATA (Z_PK, ZNOTE, ZDATA) VALUES (4, 14, unhex('\(gzipHex(""))'))")

        // Note 5: corrupt blob (no gzip magic) → falls to rung 2 (title only).
        try exec(writer, """
        INSERT INTO ZICCLOUDSYNCINGOBJECT (Z_PK, ZTITLE1, ZSNIPPET, ZFOLDER, ZMODIFICATIONDATE1, ZMARKEDFORDELETION)
        VALUES (15, 'Corrupt blob note', NULL, 1, \(coreDataDate(2026, 9, 19, 12, 0)), 0)
        """)
        try exec(writer, "INSERT INTO ZICNOTEDATA (Z_PK, ZNOTE, ZDATA) VALUES (5, 15, unhex('DEADBEEF'))")

        // Note 6: lives in Recently Deleted → excluded from every query.
        try exec(writer, """
        INSERT INTO ZICCLOUDSYNCINGOBJECT (Z_PK, ZTITLE1, ZSNIPPET, ZFOLDER, ZMODIFICATIONDATE1, ZMARKEDFORDELETION)
        VALUES (16, 'Trashed note', 'should never appear', 2, \(coreDataDate(2026, 9, 22, 20, 0)), 0)
        """)

        // Note 7: marked for deletion → excluded.
        try exec(writer, """
        INSERT INTO ZICCLOUDSYNCINGOBJECT (Z_PK, ZTITLE1, ZSNIPPET, ZFOLDER, ZMODIFICATIONDATE1, ZMARKEDFORDELETION, ZARCHIVED)
        VALUES (17, 'Marked note', 'gone soon', 1, \(coreDataDate(2026, 9, 22, 21, 0)), 1, 0)
        """)

        // Note 9: archived → excluded.
        try exec(writer, """
        INSERT INTO ZICCLOUDSYNCINGOBJECT (Z_PK, ZTITLE1, ZSNIPPET, ZFOLDER, ZMODIFICATIONDATE1, ZMARKEDFORDELETION, ZARCHIVED)
        VALUES (19, 'Archived note', 'filed away', 1, \(coreDataDate(2026, 9, 22, 22, 0)), 0, 1)
        """)

        // Note 8: two blob rows (last-writer-wins join) → the newer Z_PK wins.
        try exec(writer, """
        INSERT INTO ZICCLOUDSYNCINGOBJECT (Z_PK, ZTITLE1, ZSNIPPET, ZFOLDER, ZMODIFICATIONDATE1, ZMARKEDFORDELETION, ZARCHIVED)
        VALUES (18, 'Two blob rows', NULL, 1, \(coreDataDate(2026, 9, 18, 9, 0)), 0, 0)
        """)
        try exec(writer, "INSERT INTO ZICNOTEDATA (Z_PK, ZNOTE, ZDATA) VALUES (8, 18, unhex('\(gzipHex("old body"))'))")
        try exec(writer, "INSERT INTO ZICNOTEDATA (Z_PK, ZNOTE, ZDATA) VALUES (9, 18, unhex('\(gzipHex("new body wins"))'))")
    }

    /// Core Data epoch seconds since 2001-01-01.
    private func coreDataDate(_ y: Int, _ m: Int, _ d: Int, _ hh: Int, _ mm: Int) -> Int {
        var comps = DateComponents()
        comps.year = y; comps.month = m; comps.day = d
        comps.hour = hh; comps.minute = mm
        var cal = Calendar(identifier: .gregorian)
        cal.timeZone = TimeZone(identifier: "America/Los_Angeles")!
        let date = cal.date(from: comps)!
        return Int(date.timeIntervalSince(Date(timeIntervalSince1970: 978307200)))
    }

    /// Real gzip-wrapped protobuf document for the given note text, hex-encoded
    /// for SQLite's unhex(). Shape: Document{version(2)} → Version{data(3)} →
    /// String{string(2)}.
    private func gzipHex(_ text: String) -> String {
        let textBytes = Array(text.utf8)
        let stringMessage = [UInt8(2 << 3 | 2)] + varint(textBytes.count) + textBytes
        let version = [UInt8(3 << 3 | 2)] + varint(stringMessage.count) + stringMessage
        let document = [UInt8(2 << 3 | 2)] + varint(version.count) + version
        return GzipTestsHelper.gzipDeflateHex(Data(document))
    }

    private func varint(_ value: Int) -> [UInt8] {
        var v = UInt64(value)
        var out: [UInt8] = []
        repeat {
            var byte = UInt8(v & 0x7F)
            v >>= 7
            if v != 0 { byte |= 0x80 }
            out.append(byte)
        } while v != 0
        return out
    }

    private func provider() -> NotesProvider {
        NotesProvider(storePath: dbPath)
    }

    /// Note lines of a digest (the digest has a header line and possibly a
    /// skip note, so counting by prefix is robust).
    private func noteLines(_ digest: String) -> [String] {
        digest.split(separator: "\n").map(String.init).filter { $0.hasPrefix("Note \"") }
    }

    // MARK: Descriptor / gate

    func testDescriptorNilWhenStoreMissing() async {
        let missing = NotesProvider(storePath: "/nonexistent/NoteStore.sqlite")
        let descriptor = await missing.currentDescriptor()
        XCTAssertNil(descriptor)
        let gate = await missing.gate.verifyAccess()
        XCTAssertFalse(gate)
    }

    func testDescriptorPresentWhenStoreReadable() async {
        let p = provider()
        let descriptor = await p.currentDescriptor()
        XCTAssertNotNil(descriptor)
        XCTAssertEqual(descriptor?.id, "notes")
        XCTAssertEqual(descriptor?.tools.map { $0.name }.sorted(), ["recent", "search"])
    }

    // MARK: recent

    func testRecentListsNewestFirstExcludesTrashAndCapHolds() async throws {
        // Build 22 more plain notes so the 20 cap is exercised (27 live notes).
        let writer = SQLiteReader(path: dbPath, mode: .readWrite)
        for i in 0..<22 {
            try exec(writer, """
            INSERT INTO ZICCLOUDSYNCINGOBJECT (Z_PK, ZTITLE1, ZSNIPPET, ZFOLDER, ZMODIFICATIONDATE1, ZMARKEDFORDELETION)
            VALUES (\(100 + i), 'Filler \(i)', NULL, 1, \(coreDataDate(2026, 1, 1 + i % 27, 12, 0)), 0)
            """)
        }
        let digest = try await provider().execute(name: "recent", argumentsJSON: "{\"count\": 20}")
        let lines = noteLines(digest)
        XCTAssertEqual(lines.count, 20, "cap of 20 must hold (27 live notes exist)")
        XCTAssertTrue(lines[0].contains("Blob note"), "newest note first")
        XCTAssertTrue(lines[0].contains("blob body line"), "blob body decoded in the recent digest")
        XCTAssertTrue(lines.contains { $0.contains("Filler 21") })
        XCTAssertFalse(lines.contains { $0.contains("Filler 0") }, "oldest falls outside the cap")
        XCTAssertFalse(digest.contains("Trashed note"), "Recently Deleted folder excluded")
        XCTAssertFalse(digest.contains("Marked note"), "ZMARKEDFORDELETION excluded")
        XCTAssertFalse(digest.contains("Archived note"), "ZARCHIVED excluded")
        // Out-of-range requests clamp to the schema cap (max 20).
        let clamped = try await provider().execute(name: "recent", argumentsJSON: "{\"count\": 50}")
        XCTAssertEqual(noteLines(clamped).count, 20, "count clamps to 20")
    }

    func testRecentBodyLadder() async throws {
        let digest = try await provider().execute(name: "recent", argumentsJSON: "{}")
        // Blob note: full body from the gzip-protobuf blob.
        XCTAssertTrue(digest.contains("blob body line"))
        // Snippets note: snippet rung.
        XCTAssertTrue(digest.contains("fresh snippet text"))
        // alphaToken plan: title rung (snippet NULL, no blob).
        XCTAssertTrue(digest.contains("plan"))
        // Untitled note with an empty blob: "(untitled)" + no-preview line.
        XCTAssertTrue(digest.contains("(untitled)"))
        XCTAssertTrue(digest.contains("(no note preview available)"))
        // Corrupt blob: falls back to the title rung.
        XCTAssertTrue(digest.contains("Corrupt blob note"))
    }

    func testRecentOnEmptyStore() async throws {
        let emptyPath = scratch.appendingPathComponent("empty.sqlite").path
        let writer = SQLiteReader(path: emptyPath, mode: .readWrite)
        try exec(writer, """
        CREATE TABLE ZICCLOUDSYNCINGOBJECT (Z_PK INTEGER PRIMARY KEY, ZTITLE1 TEXT,
            ZSNIPPET TEXT, ZFOLDER INTEGER, ZMODIFICATIONDATE1 TIMESTAMP,
            ZMARKEDFORDELETION INTEGER, ZARCHIVED INTEGER)
        """)
        try exec(writer, "CREATE TABLE ZICNOTEDATA (Z_PK INTEGER PRIMARY KEY, ZNOTE INTEGER, ZDATA BLOB)")
        try exec(writer, "CREATE TABLE ZFOLDER (Z_PK INTEGER PRIMARY KEY, ZTITLE2 TEXT)")
        let p = NotesProvider(storePath: emptyPath)
        let digest = try await p.execute(name: "recent", argumentsJSON: "{}")
        XCTAssertTrue(digest.contains("No notes found"))
    }

    // MARK: search

    func testSearchMatchesBlobSnippetAndTitleRungs() async throws {
        let digest = try await provider().execute(name: "search", argumentsJSON: "{\"query\": \"\(alphaToken)\"}")
        let lines = noteLines(digest)
        XCTAssertEqual(lines.count, 1, "title-rung note matches")
        XCTAssertTrue(digest.contains("plan"))
        XCTAssertTrue(digest.contains("modified"), "date stamp present")

        let blobSearch = try await provider().execute(name: "search", argumentsJSON: "{\"query\": \"blob body\"}")
        XCTAssertTrue(blobSearch.contains("Blob note"), "blob body searchable")
    }

    func testSearchUsesNewestBlobRowWhenNoteHasSeveral() async throws {
        // Two ZICNOTEDATA rows for one note: the newest Z_PK's body wins.
        let digest = try await provider().execute(name: "search", argumentsJSON: "{\"query\": \"body\"}")
        XCTAssertTrue(digest.contains("new body wins"))
        XCTAssertFalse(digest.contains("old body"))
    }

    func testSearchLowercaseContainsMatching() async throws {
        let digest = try await provider().execute(name: "search", argumentsJSON: "{\"query\": \"FRESH SNIPPET\"}")
        XCTAssertTrue(digest.contains("fresh snippet text"), "case-insensitive matching")
    }

    func testSearchNoMatches() async throws {
        let digest = try await provider().execute(name: "search", argumentsJSON: "{\"query\": \"zzzznothing\"}")
        XCTAssertTrue(digest.contains("No notes found"))
    }

    func testSearchHonorsLimitAndSkipsNote() async throws {
        let digest = try await provider().execute(name: "search", argumentsJSON: "{\"query\": \"note\", \"limit\": 2}")
        // Matches: "alphaToken plan" (title), "Blob note" (title), plus
        // "Corrupt blob note" (title) — the untitled note matches nothing.
        let lines = noteLines(digest)
        XCTAssertEqual(lines.count, 2, "limit honored")
        // newest first: Blob note (18:45), then alphaToken plan (9:15) — Corrupt (Sep 19) is third, cut.
        XCTAssertTrue(lines[0].contains("Blob note"))
        XCTAssertTrue(digest.contains("(1 note skipped"), "skipped matches counted")
    }

    func testSearchRequiresQuery() async {
        do {
            _ = try await provider().execute(name: "search", argumentsJSON: "{}")
            XCTFail("expected throw")
        } catch {
            XCTAssertNotNil(error)
        }
    }

    // MARK: execute errors

    func testUnknownToolThrows() async {
        do {
            _ = try await provider().execute(name: "bogus", argumentsJSON: "{}")
            XCTFail("expected throw")
        } catch {
            XCTAssertNotNil(error)
        }
    }

    func testExecuteOnMissingStoreFailsCleanly() async {
        let p = NotesProvider(storePath: "/nonexistent/NoteStore.sqlite")
        do {
            _ = try await p.execute(name: "recent", argumentsJSON: "{}")
            XCTFail("expected throw")
        } catch {
            XCTAssertNotNil(error)
        }
    }

    // MARK: live NoteStore probe (skips when absent/TCC-blocked; never prompts)

    func testLiveNoteStoreProbeSkipsWhenBlocked() async throws {
        let home = FileManager.default.homeDirectoryForCurrentUser
        let realPath = home.appendingPathComponent(
            "Library/Group Containers/group.com.apple.notes/NoteStore.sqlite").path
        guard FileManager.default.fileExists(atPath: realPath) else {
            throw XCTSkip("NoteStore.sqlite not present on this machine")
        }
        let p = NotesProvider(storePath: realPath)
        // Read-only open/step without FDA is a POSIX denial, never a prompt:
        // must not hang, crash, or trigger TCC. Result is agnostic here.
        _ = await p.gate.verifyAccess()
        _ = await p.currentDescriptor()
    }
}

/// Gzip fixture builder shared by the provider tests (libz, via the same
/// code path the production Gzip decompressor inverts).
enum GzipTestsHelper {
    static func gzipDeflateHex(_ data: Data) -> String {
        var stream = z_stream()
        let status = deflateInit2_(&stream, 9, Z_DEFLATED, 15 + 16, 8,
                                   Z_DEFAULT_STRATEGY, "1.2.11",
                                   Int32(MemoryLayout<z_stream>.size))
        XCTAssertEqual(status, Z_OK, "deflateInit2_ failed")
        defer { _ = deflateEnd(&stream) }
        let bound = Int(compressBound(uLong(max(data.count, 1)))) + 64
        let output = UnsafeMutablePointer<UInt8>.allocate(capacity: bound)
        defer { output.deallocate() }
        var input = [UInt8](data)
        let compressed = input.withUnsafeMutableBufferPointer { input -> Data in
            stream.next_in = input.baseAddress
            stream.avail_in = UInt32(input.count)
            stream.next_out = output
            stream.avail_out = UInt32(bound)
            let result = deflate(&stream, Z_FINISH)
            XCTAssertEqual(result, Z_STREAM_END, "deflate Z_FINISH failed")
            return Data(bytes: output, count: bound - Int(stream.avail_out))
        }
        return compressed.map { String(format: "%02X", $0) }.joined()
    }
}
#endif
