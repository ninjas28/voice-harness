import Foundation

/// Personal-context provider over the Notes raw store (NoteStore.sqlite). All
/// access is read-only and bounded (SQLiteReader's 3 s busy timeout + 3 s
/// statement bound). The gate probes the database before anything is
/// announced — read-only SQLite access without FDA fails as an ordinary POSIX
/// denial, so probing never triggers a TCC prompt.
///
/// Body ladder (plan Task 5): ZSNIPPET → title column → gzip-decompress the
/// note blob (0x1f8b, 1 MiB cap, libz) → minimal protobuf walk for text runs
/// → "(untitled)". Decode failures never fail the digest.
///
/// The whole type is macOS-gated in one block ("at construction", not
/// scattered through the file): `voicekit` must still compile for iOS, where
/// the raw stores do not exist.
#if os(macOS)
public final class NotesProvider: PersonalContextProvider, @unchecked Sendable {
    public let providerId = "notes"
    private let gateProvider: PersonalContextGate
    private let reader: SQLiteReader
    /// Upper bound on candidate rows examined per query (bounded everything).
    private static let candidateCap = 500

    /// - Parameters:
    ///   - storePath: NoteStore.sqlite path; defaults to the Notes group
    ///     container on this Mac.
    ///   - statementTimeout: per-statement bound handed to the reader.
    init(storePath: String? = nil, statementTimeout: TimeInterval = 3.0) {
        self.reader = SQLiteReader(path: storePath ?? NoteStoreReader.defaultPath,
                                   statementTimeout: statementTimeout)
        self.gateProvider = NotesGate(reader: reader)
    }

    public var gate: PersonalContextGate { gateProvider }

    public func currentDescriptor() async -> ProviderDescriptor? {
        guard await gate.verifyAccess() else { return nil }
        return Self.descriptor()
    }

    static func descriptor() -> ProviderDescriptor {
        ProviderDescriptor(id: "notes", tools: [
            ToolDescriptor(
                name: "recent",
                description: "List recent notes, newest modification first.",
                parameters: [
                    "type": .string("object"),
                    "properties": .object([
                        "count": .object([
                            "type": .string("integer"),
                            "minimum": .int(1), "maximum": .int(20), "default": .int(5),
                            "description": .string("How many notes to include."),
                        ]),
                    ]),
                ]),
            ToolDescriptor(
                name: "search",
                description: "Search notes by title or content.",
                parameters: [
                    "type": .string("object"),
                    "properties": .object([
                        "query": .object([
                            "type": .string("string"),
                            "description": .string("Text to look for in note titles or bodies."),
                        ]),
                        "limit": .object([
                            "type": .string("integer"),
                            "minimum": .int(1), "maximum": .int(20), "default": .int(5),
                            "description": .string("Maximum number of matching notes."),
                        ]),
                    ]),
                ]),
        ])
    }

    public func execute(name: String, argumentsJSON: String) async throws -> String {
        guard await gate.verifyAccess() else {
            throw PersonalContextError.notAuthorized("notes")
        }
        let args = try PersonalContextArguments.parse(argumentsJSON)
        switch name {
        case "recent":
            let count = PersonalContextArguments.int(args, "count", default: 5, min: 1, max: 20)
            return try await recentDigest(count: count)
        case "search":
            guard let query = PersonalContextArguments.string(args, "query") else {
                throw PersonalContextError.invalidArguments("search needs a query")
            }
            let limit = PersonalContextArguments.int(args, "limit", default: 5, min: 1, max: 20)
            return try await searchDigest(query: query, limit: limit)
        default:
            throw PersonalContextError.failed("unknown notes tool '\(name)'")
        }
    }

    // MARK: - Queries → models (thin, unmocked)

    private func recentDigest(count: Int) async throws -> String {
        let rows = try await queryRows(limit: count)
        let (models, skipped) = Self.models(from: rows)
        var digest = NotesDigestFormatter.notesDigest(models)
        digest = Self.appendSkipNote(digest, skipped: skipped)
        return digest
    }

    private func searchDigest(query: String, limit: Int) async throws -> String {
        // Fetch the newest candidates and match in Swift: bodies live in
        // gzipped protobuf blobs SQL LIKE cannot see, and titles/snippets
        // match case-insensitively here.
        let rows = try await queryRows(limit: Self.candidateCap)
        var matches: [NoteModel] = []
        var skipped = 0
        for row in rows {
            let model = Self.note(from: row)
            let title = model.title ?? ""
            let body = model.body ?? model.snippet ?? ""
            if title.range(of: query, options: .caseInsensitive) != nil
                || body.range(of: query, options: .caseInsensitive) != nil {
                matches.append(model)
            } else if model.body == nil && (model.snippet ?? "").isEmpty && title.isEmpty {
                skipped += 1 // no readable content at any ladder rung
            }
        }
        guard !matches.isEmpty else { return "No notes found for '\(query)'." }
        if matches.count > limit { matches = Array(matches.prefix(limit)) }
        var digest = NotesDigestFormatter.notesDigest(matches)
        digest = Self.appendSkipNote(digest, skipped: skipped)
        return digest
    }

    /// Runs the reader on the cooperative pool (blocking sqlite3_step must
    /// not sit on an arbitrary executor thread).
    private func queryRows(limit: Int) async throws -> [NoteStoreReader.NoteRow] {
        let reader = self.reader
        return try await Task.detached(priority: .userInitiated) {
            try NoteStoreReader.rows(limit: limit, reader: reader)
        }.value
    }

    /// One projected row → NoteModel. Body ladder: ZSNIPPET → title column →
    /// gzip-decompress blob → protobuf text runs → nil (rendered as
    /// "(untitled)" with no preview). Decode failures never fail the digest.
    static func note(from row: NoteStoreReader.NoteRow) -> NoteModel {
        var body: String?
        if row.blobHex?.count ?? 0 >= 4, let hex = row.blobHex,
           let blob = Self.blobData(fromHex: hex) {
            body = Self.decodeBody(fromBlob: blob)
        }
        if body?.isEmpty == true { body = nil }
        return NoteModel(title: row.title, date: row.modificationDate ?? Date(),
                         body: body, snippet: row.snippet)
    }

    /// Gzip-inflates the blob and walks the protobuf document for text runs.
    /// Any failure → nil (the digest falls to the earlier rungs).
    static func decodeBody(fromBlob blob: Data) -> String? {
        guard let plain = Gzip.decompress(blob) else { return nil }
        return Self.textRuns(fromDocument: plain)
    }

    /// Walks Document{version(2)} → Version{data(3)} → String{string(2)}.
    /// Extra/unknown fields are ignored; any structural surprise → nil.
    static func textRuns(fromDocument plain: Data) -> String? {
        guard let document = ProtobufReader.parse(plain) else { return nil }
        for versionField in document where versionField.fieldNumber == 2 {
            guard case .lengthDelimited(let versionBytes) = versionField.value,
                  let version = ProtobufReader.parse(versionBytes) else { continue }
            for dataField in version where dataField.fieldNumber == 3 {
                guard case .lengthDelimited(let stringBytes) = dataField.value,
                      let stringMessage = ProtobufReader.parse(stringBytes) else { continue }
                for textField in stringMessage where textField.fieldNumber == 2 {
                    guard case .lengthDelimited(let textData) = textField.value else { continue }
                    return String(decoding: textData, as: UTF8.self)
                }
            }
        }
        return nil
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

    /// Maps rows to models and counts unreadable skips (no title, no snippet,
    /// no decodable body).
    private static func models(from rows: [NoteStoreReader.NoteRow]) -> ([NoteModel], skipped: Int) {
        var models: [NoteModel] = []
        var skipped = 0
        for row in rows {
            let model = note(from: row)
            let hasContent = model.title?.isEmpty == false
                || model.body?.isEmpty == false
                || model.snippet?.isEmpty == false
            if !hasContent { skipped += 1 }
            models.append(model)
        }
        return (models, skipped)
    }

    /// Spoken-style note when notes had to be skipped.
    private static func appendSkipNote(_ digest: String, skipped: Int) -> String {
        guard skipped > 0 else { return digest }
        let noun = skipped == 1 ? "note" : "notes"
        return digest + "(\(skipped) \(noun) skipped: their content could not be read.)\n"
    }

    /// Gate probing real NoteStore access: a bounded count against the
    /// read-only handle.
    private struct NotesGate: PersonalContextGate {
        let reader: SQLiteReader
        func verifyAccess() async -> Bool {
            let reader = self.reader
            return await (Task.detached(priority: .userInitiated) {
                NoteStoreReader.probeAccess(reader)
            }.value)
        }
    }
}
#endif
