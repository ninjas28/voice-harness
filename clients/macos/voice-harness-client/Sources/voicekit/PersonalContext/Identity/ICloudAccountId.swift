import Foundation
import CloudKit

/// Identity source backed by the iCloud account: the CloudKit `userRecordID`
/// recordName is stable per Apple ID **per app container** — the same value
/// on every device signed into the same account, which is exactly what
/// cross-device federation needs to group them.
///
/// Offline-tolerant: no account / offline / no entitlement → the fetch fails
/// and `currentKey()` returns nil (error path, never a prompt).
public struct ICloudAccountId: IdentityProvider {
    public let keyId = "icloud"

    /// Test seam: injectable fetch closure so tests never touch CloudKit.
    private let fetchUserRecordID: @Sendable () async -> String?

    public init(fetchUserRecordID: @escaping @Sendable () async -> String?) {
        self.fetchUserRecordID = fetchUserRecordID
    }

    /// Production initializer over `CKContainer.default()`.
    public init() {
        self.init(fetchUserRecordID: Self.makeDefaultFetch())
    }

    private static func makeDefaultFetch() -> @Sendable () async -> String? {
        {
            do {
                let recordID = try await CKContainer.default().userRecordID()
                let recordName = recordID.recordName
                // A missing/placeholder recordName carries no grouping value.
                guard !recordName.isEmpty else { return nil }
                return recordName
            } catch {
                return nil // no account, offline, no entitlement — silent
            }
        }
    }

    public func currentKey() async -> String? {
        await fetchUserRecordID()
    }
}
