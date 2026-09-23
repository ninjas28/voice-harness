import Foundation
import SQLite3

/// Personal-context provider over the Messages raw store (`chat.db`). All
/// access is read-only and bounded (SQLiteReader's 3 s busy timeout + 3 s
/// statement bound). The gate probes the database before anything is
/// announced — read-only SQLite access without FDA fails as an ordinary POSIX
/// denial, so probing never triggers a TCC prompt.
///
/// The whole type is macOS-gated in one block ("at construction", not
/// scattered through the file): `voicekit` must still compile for iOS, where
/// the raw stores do not exist.
#if os(macOS)
public final class MessagesProvider: PersonalContextProvider, @unchecked Sendable {
    public let providerId = "messages"
    private let gateProvider: PersonalContextGate
    private let reader: SQLiteReader
    /// Test seam: overrides "now" for search windows (nil = wall clock).
    private let nowOverride: Date?
    /// Upper bound on candidate rows examined per query (bounded everything).
    private static let candidateCap = 500

    /// - Parameters:
    ///   - dbPath: chat.db path; defaults to `~/Library/Messages/chat.db`.
    ///   - statementTimeout: per-statement bound handed to the reader.
    ///   - now: test-only wall-clock override for search windows.
    init(dbPath: String? = nil, statementTimeout: TimeInterval = 3.0,
         now: Date? = nil) {
        let home = FileManager.default.homeDirectoryForCurrentUser
        self.reader = SQLiteReader(
            path: dbPath ?? home.appendingPathComponent("Library/Messages/chat.db").path,
            statementTimeout: statementTimeout)
        self.gateProvider = MessagesGate(reader: reader)
        self.nowOverride = now
    }

    public var gate: PersonalContextGate { gateProvider }

    public func currentDescriptor() async -> ProviderDescriptor? {
        guard await gate.verifyAccess() else { return nil }
        return Self.descriptor()
    }

    static func descriptor() -> ProviderDescriptor {
        ProviderDescriptor(id: "messages", tools: [
            ToolDescriptor(
                name: "recent",
                description: "List recent message threads, newest first.",
                parameters: [
                    "type": .string("object"),
                    "properties": .object([
                        "thread_count": .object([
                            "type": .string("integer"),
                            "minimum": .int(1), "maximum": .int(20), "default": .int(5),
                            "description": .string("How many threads to include."),
                        ]),
                    ]),
                ]),
            ToolDescriptor(
                name: "search",
                description: "Search message bodies from the last few days.",
                parameters: [
                    "type": .string("object"),
                    "properties": .object([
                        "query": .object([
                            "type": .string("string"),
                            "description": .string("Text to look for in message bodies."),
                        ]),
                        "days_back": .object([
                            "type": .string("integer"),
                            "minimum": .int(1), "maximum": .int(90), "default": .int(14),
                            "description": .string("How many days back to search."),
                        ]),
                        "limit": .object([
                            "type": .string("integer"),
                            "minimum": .int(1), "maximum": .int(20), "default": .int(10),
                            "description": .string("Maximum number of matching messages."),
                        ]),
                    ]),
                ]),
        ])
    }

    public func execute(name: String, argumentsJSON: String) async throws -> String {
        guard await gate.verifyAccess() else {
            throw PersonalContextError.notAuthorized("messages")
        }
        let args = try PersonalContextArguments.parse(argumentsJSON)
        switch name {
        case "recent":
            let threadCount = PersonalContextArguments.int(args, "thread_count", default: 5, min: 1, max: 20)
            return try await recentDigest(threadCount: threadCount)
        case "search":
            guard let query = PersonalContextArguments.string(args, "query") else {
                throw PersonalContextError.invalidArguments("search needs a query")
            }
            let daysBack = PersonalContextArguments.int(args, "days_back", default: 14, min: 1, max: 90)
            let limit = PersonalContextArguments.int(args, "limit", default: 10, min: 1, max: 20)
            return try await searchDigest(query: query, daysBack: daysBack, limit: limit)
        default:
            throw PersonalContextError.failed("unknown messages tool '\(name)'")
        }
    }

    // MARK: - Queries → models (thin, unmocked)

    /// Apple epoch: nanoseconds since 2001-01-01 (chat.db convention).
    private static let appleEpoch = Date(timeIntervalSince1970: 978307200)

    private func recentDigest(threadCount: Int) async throws -> String {
        // Newest message per chat, via a grouped subselect on (chat, max date).
        // attributedBody is selected as hex(): the blobs contain NUL bytes
        // the reader's C-string conversion would truncate; hex round-trips
        // the exact bytes through the String-based reader.
        let rows = try await queryRows(sql: """
        SELECT c.chat_identifier, m.date, m.is_from_me, m.text, hex(m.attributedBody)
        FROM chat c
        JOIN chat_message_join cmj ON cmj.chat_id = c.ROWID
        JOIN message m ON m.ROWID = cmj.message_id
        WHERE m.date = (
            SELECT MAX(m2.date) FROM message m2
            JOIN chat_message_join cmj2 ON cmj2.message_id = m2.ROWID
            WHERE cmj2.chat_id = c.ROWID
        )
        ORDER BY m.date DESC
        LIMIT \(threadCount)
        """)
        let (threads, skipped) = Self.models(from: rows)
        var digest = MessagesDigestFormatter.threadsDigest(threads)
        digest = Self.appendSkipNote(digest, skipped: skipped)
        return digest
    }

    private func searchDigest(query: String, daysBack: Int, limit: Int) async throws -> String {
        let now = nowOverride ?? Date()
        let cutoff = now.addingTimeInterval(-Double(daysBack) * 86_400)
        let appleCutoff = Int(cutoff.timeIntervalSince(Self.appleEpoch) * 1_000_000_000)
        // Fetch the newest candidates in the window and match in Swift: since
        // Ventura bodies live in typedstream blobs that SQL LIKE cannot match
        // reliably, and the plan keeps matching query-side simple.
        // Same hex() trick as recentDigest (NUL-safe blob round-trip).
        let rows = try await queryRows(sql: """
        SELECT c.chat_identifier, m.date, m.is_from_me, m.text, hex(m.attributedBody)
        FROM chat c
        JOIN chat_message_join cmj ON cmj.chat_id = c.ROWID
        JOIN message m ON m.ROWID = cmj.message_id
        WHERE m.date >= \(appleCutoff)
        ORDER BY m.date DESC
        LIMIT \(Self.candidateCap)
        """)
        var matches: [ThreadModel] = []
        var skipped = 0
        for row in rows {
            guard let model = Self.thread(from: row) else { continue }
            guard let body = model.body else {
                skipped += 1 // undecodable blob with no text column
                continue
            }
            if body.range(of: query, options: .caseInsensitive) != nil {
                matches.append(model)
                if matches.count == limit { break }
            }
        }
        guard !matches.isEmpty else { return "No messages found for '\(query)'." }
        var digest = MessagesDigestFormatter.threadsDigest(matches)
        digest = Self.appendSkipNote(digest, skipped: skipped)
        return digest
    }

    /// Runs the reader on the cooperative pool (blocking sqlite3_step must
    /// not sit on an arbitrary executor thread).
    private func queryRows(sql: String) async throws -> [[String?]] {
        let reader = self.reader
        return try await Task.detached(priority: .userInitiated) {
            try reader.execute(sql: sql)
        }.value
    }

    /// One query row -> ThreadModel. Body ladder: `text` column → typedstream
    /// decode of `attributedBody` (arriving as a hex string from `hex()`) →
    /// nil (the message is skipped). Decode failures never fail the digest
    /// (plan decision 5).
    static func thread(from row: [String?]) -> ThreadModel? {
        guard row.count >= 5,
              let handle = row[0],
              let dateText = row[1] else { return nil }
        let nanoseconds = TimeInterval(dateText) ?? 0
        let date = appleEpoch.addingTimeInterval(nanoseconds / 1_000_000_000)
        var body = row[3]
        if body == nil || body?.isEmpty == true {
            body = row[4].flatMap { hexBlob in
                blobData(fromHex: hexBlob).flatMap(TypedStreamDecoder.decode)
            }
        }
        return ThreadModel(handle: handle, date: date, body: body)
    }

    /// Hex string (from SQLite `hex()`) → bytes, or nil for odd/empty input.
    private static func blobData(fromHex hex: String) -> Data? {
        let chars = Array(hex.utf8)
        guard chars.count >= 2, chars.count % 2 == 0 else { return nil }
        var bytes = [UInt8]()
        bytes.reserveCapacity(chars.count / 2)
        func nibble(_ c: UInt8) -> UInt8? {
            switch c {
            case 0x30...0x39: return c - 0x30
            case 0x41...0x46: return c - 0x41 + 10
            case 0x61...0x66: return c - 0x61 + 10
            default: return nil
            }
        }
        var i = 0
        while i < chars.count {
            guard let hi = nibble(chars[i]), let lo = nibble(chars[i + 1]) else { return nil }
            bytes.append(hi << 4 | lo)
            i += 2
        }
        return Data(bytes)
    }

    /// Maps rows to models and counts undecodable-body skips.
    private static func models(from rows: [[String?]]) -> ([ThreadModel], skipped: Int) {
        var models: [ThreadModel] = []
        var skipped = 0
        for row in rows {
            guard let model = thread(from: row) else { continue }
            if model.body == nil { skipped += 1 }
            models.append(model)
        }
        return (models, skipped)
    }

    /// Spoken-style note when bodies had to be skipped (plan decision 5).
    private static func appendSkipNote(_ digest: String, skipped: Int) -> String {
        guard skipped > 0 else { return digest }
        let noun = skipped == 1 ? "message" : "messages"
        return digest + "(\(skipped) \(noun) skipped: their content could not be read.)\n"
    }

    /// Gate probing real chat.db access: a bounded `SELECT count(*) FROM
    /// message` against the read-only handle.
    private struct MessagesGate: PersonalContextGate {
        let reader: SQLiteReader
        func verifyAccess() async -> Bool {
            let reader = self.reader
            return (try? await Task.detached(priority: .userInitiated) {
                try reader.execute(sql: "SELECT count(*) FROM message LIMIT 1")
            }.value) != nil
        }
    }
}
#endif
