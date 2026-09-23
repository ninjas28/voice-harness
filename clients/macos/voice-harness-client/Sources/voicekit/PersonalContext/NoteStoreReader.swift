import Foundation

/// Minimal read-mostly accessor over the pinned NoteStore.sqlite subset. All
/// SQL lives here so NotesProvider stays a thin tool/digest layer. Blobs are
/// fetched via SQL `hex()` and hex-decoded Swift-side — SQLiteReader reads
/// columns as C strings, so raw NUL-bearing blob bytes would be truncated.
/// (Same pattern MessagesProvider uses for chat.db attributedBody.)
///
/// The whole type is macOS-gated in one block: `voicekit` must still compile
/// for iOS, where the Notes store does not exist.
#if os(macOS)
enum NoteStoreReader {
    /// Pinned schema (public references; live store was TCC-blocked for the
    /// implementing session — see plan Task 5 verification note):
    /// - ZICCLOUDSYNCINGOBJECT: note/folder metadata. ZTITLE1 note title,
    ///   ZSNIPPET text preview, ZFOLDER → ZFOLDER.Z_PK,
    ///   ZMODIFICATIONDATE1 Core Data seconds-since-2001, ZMARKEDFORDELETION
    ///   soft-delete flag.
    /// - ZICNOTEDATA: ZNOTE → ZICCLOUDSYNCINGOBJECT.Z_PK, ZDATA the
    ///   gzip-wrapped protobuf document blob.
    /// - ZFOLDER: ZTITLE2 folder title ("Recently Deleted" lives here).

    static let defaultPath = FileManager.default.homeDirectoryForCurrentUser
        .appendingPathComponent("Library/Group Containers/group.com.apple.notes/NoteStore.sqlite")
        .path

    /// Core Data epoch: seconds since 2001-01-01.
    static let coreDataEpoch = Date(timeIntervalSince1970: 978307200)

    /// One projected note row: ZICCLOUDSYNCINGOBJECT metadata + the note's
    /// newest note-data blob as a hex string (nil when it has none).
    struct NoteRow {
        var title: String?
        var snippet: String?
        var modificationDate: Date?
        var blobHex: String?
    }

    /// Gate probe: bounded read against the read-only handle. A missing file
    /// or TCC denial surfaces as a per-statement error, never a prompt.
    static func probeAccess(_ reader: SQLiteReader) -> Bool {
        (try? reader.execute(sql: "SELECT count(*) FROM ZICCLOUDSYNCINGOBJECT LIMIT 1")) != nil
    }

    /// Live notes (not trashed, not marked for deletion, folder not
    /// "Recently Deleted" — best effort), newest modification first, capped.
    static func rows(limit: Int, reader: SQLiteReader) throws -> [NoteRow] {
        let sql = """
        SELECT o.ZTITLE1, o.ZSNIPPET, o.ZMODIFICATIONDATE1, hex(nd.ZDATA)
        FROM ZICCLOUDSYNCINGOBJECT o
        LEFT JOIN ZICNOTEDATA nd
            ON nd.ZNOTE = o.Z_PK
            AND nd.Z_PK = (
                SELECT nd2.Z_PK FROM ZICNOTEDATA nd2
                WHERE nd2.ZNOTE = o.Z_PK
                ORDER BY nd2.Z_PK DESC LIMIT 1
            )
        WHERE (o.ZMARKEDFORDELETION IS NULL OR o.ZMARKEDFORDELETION = 0)
          AND (o.ZARCHIVED IS NULL OR o.ZARCHIVED = 0)
          AND (o.ZFOLDER IS NULL OR o.ZFOLDER NOT IN (
              SELECT f.Z_PK FROM ZFOLDER f WHERE f.ZTITLE2 = 'Recently Deleted'))
        ORDER BY o.ZMODIFICATIONDATE1 DESC
        LIMIT \(limit)
        """
        return try reader.execute(sql: sql).compactMap(Self.noteRow(from:))
    }

    /// One raw query row → NoteRow (nil for shape-mangled rows).
    static func noteRow(from row: [String?]) -> NoteRow? {
        guard row.count >= 4 else { return nil }
        var note = NoteRow(title: row[0], snippet: row[1], modificationDate: nil, blobHex: row[3])
        if let secondsText = row[2], let seconds = TimeInterval(secondsText) {
            note.modificationDate = coreDataEpoch.addingTimeInterval(seconds)
        }
        return note
    }
}
#endif
