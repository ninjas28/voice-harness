import Foundation
import SQLite3

/// Minimal read-mostly SQLite helper over the libsqlite3 C API for the
/// raw-store providers (chat.db, NoteStore.sqlite). Zero SPM dependencies —
/// the system `SQLite3` module from the macOS/iOS SDK. Everything is bounded:
/// 3 s busy timeout, and every statement carries a wall-clock bound via
/// `sqlite3_progress_handler` so a wedged read can never hang the session.
///
/// The API mirrors the providers' needs: `execute(sql:) -> [[String?]]` where
/// each row is a list of column values and SQL NULL maps to Swift nil.
final class SQLiteReader: @unchecked Sendable {
    enum OpenMode {
        /// READONLY — the raw-store default (digests never write).
        case readOnly
        /// READWRITE — used by tests to build fixture databases.
        case readWrite
    }

    enum Error: Swift.Error, Equatable {
        /// `sqlite3_open_v2` failed (missing file with no create flag is NOT
        /// an error — SQLite succeeds lazily; errors surface per-statement).
        case open(String)
        /// prepare/step/column conversion failed; message from sqlite3_errmsg.
        case sql(String)
        /// The statement exceeded its wall-clock bound.
        case timeout
    }

    private let path: String
    private let mode: OpenMode
    private let statementTimeout: TimeInterval
    private let lock = NSLock()
    private var db: OpaquePointer?
    /// Wall-clock deadline for the in-flight statement, read by the progress
    /// handler (lock-guarded, valid only while `execute` holds the lock).
    private var activeDeadline: Date = .distantPast

    /// - Parameters:
    ///   - path: database file path.
    ///   - mode: `.readOnly` (default) opens with SQLITE_OPEN_READONLY.
    ///   - statementTimeout: wall-clock bound per statement, enforced by
    ///     the progress handler. Default 3 s.
    init(path: String, mode: OpenMode = .readOnly, statementTimeout: TimeInterval = 3.0) {
        self.path = path
        self.mode = mode
        self.statementTimeout = statementTimeout
    }

    deinit {
        if let db { sqlite3_close_v2(db) }
    }

    /// True when the last open attempt failed outright (file unusable at the
    /// OS level). Diagnostics only — statement execution surfaces errors.
    var isOpenFailed: Bool {
        lock.lock(); defer { lock.unlock() }
        return openFailed
    }

    private var openFailed = false

    /// Opens the database lazily on first use (guarded by `lock`).
    private func database() throws -> OpaquePointer {
        lock.lock()
        if let db {
            lock.unlock()
            return db
        }
        lock.unlock()
        return try openDatabase()
    }

    private func openDatabase() throws -> OpaquePointer {
        lock.lock()
        defer { lock.unlock() }
        if let db { return db }
        openFailed = false
        let flags: Int32 = mode == .readOnly
            ? SQLITE_OPEN_READONLY
            : SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE
        var handle: OpaquePointer?
        let rc = sqlite3_open_v2(path, &handle, flags, nil)
        if rc != SQLITE_OK {
            openFailed = true
            let message = handle.map { String(cString: sqlite3_errmsg($0)) } ?? "rc=\(rc)"
            if let handle { sqlite3_close_v2(handle) }
            throw Error.open(message)
        }
        // Bounded contention: give up after 3 s instead of blocking forever.
        sqlite3_busy_timeout(handle!, 3000)
        db = handle
        return handle!
    }

    /// Runs one SQL statement (or batch) and collects all rows. Values map:
    /// INTEGER/TEXT/REAL -> String, NULL -> nil, BLOB -> UTF-8 lossy.
    func execute(sql: String) throws -> [[String?]] {
        let db = try database()
        lock.lock()
        defer { lock.unlock() }

        var statement: OpaquePointer?
        guard sqlite3_prepare_v2(db, sql, -1, &statement, nil) == SQLITE_OK,
              let statement else {
            throw Error.sql(String(cString: sqlite3_errmsg(db)))
        }
        defer { sqlite3_finalize(statement) }

        // Wall-clock bound for the whole statement: the progress handler
        // fires during step; past the deadline we force-abort via
        // sqlite3_interrupt so a wedged read cannot hang the session. The
        // deadline lives on `self` and the context pointer is an unretained
        // self pointer — safe because the lock is held for the entire
        // statement and a C function pointer cannot capture Swift context.
        activeDeadline = Date().addingTimeInterval(statementTimeout)
        defer { activeDeadline = .distantPast }
        let bound: @convention(c) (UnsafeMutableRawPointer?) -> Int32 = { context in
            guard let context else { return 0 }
            let reader = Unmanaged<SQLiteReader>.fromOpaque(context).takeUnretainedValue()
            return Date() >= reader.activeDeadline ? 1 : 0
        }
        sqlite3_progress_handler(db, 10_000, bound,
                                 Unmanaged.passUnretained(self).toOpaque())
        defer { sqlite3_progress_handler(db, 0, nil, nil) }

        var rows: [[String?]] = []
        while true {
            let rc = sqlite3_step(statement)
            switch rc {
            case SQLITE_ROW:
                rows.append(Self.row(statement))
            case SQLITE_DONE:
                return rows
            case SQLITE_INTERRUPT:
                throw Error.timeout
            default:
                throw Error.sql(String(cString: sqlite3_errmsg(db)))
            }
        }
    }

    /// One row -> [String?], column count from the prepared statement.
    private static func row(_ statement: OpaquePointer) -> [String?] {
        let count = sqlite3_column_count(statement)
        var values: [String?] = []
        values.reserveCapacity(Int(count))
        for i in 0..<count {
            if sqlite3_column_type(statement, i) == SQLITE_NULL {
                values.append(nil)
            } else if let bytes = sqlite3_column_text(statement, i) {
                values.append(String(cString: bytes))
            } else {
                values.append(nil)
            }
        }
        return values
    }
}
