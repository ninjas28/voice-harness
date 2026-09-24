import Foundation
import Security

/// Entitlement probe used by `ICloudAccountId` to avoid constructing
/// `CKContainer` on binaries without the iCloud entitlement.
///
/// `CKContainer.default()` throws an uncatchable Objective-C `CKException`
/// ('containerIdentifier can not be nil') when the running binary lacks the
/// entitlement (e.g. the hand-built unsigned macOS bundle) — the process
/// aborts. `SecTaskCopyValueForEntitlement` reads the same entitlements
/// CloudKit checks, without touching CloudKit, so the failure becomes a
/// silent nil instead of a crash.
enum SecEntitlementProbe {
    /// The entitlement CloudKit requires for `CKContainer.default()`.
    private static let icloudContainerKey = "com.apple.developer.icloud-container-identifiers"

    /// True when the running binary holds the iCloud container entitlement.
    static func hasICloudContainerEntitlement() -> Bool {
        guard let task = SecTaskCreateFromSelf(nil) else { return false }
        guard let value = SecTaskCopyValueForEntitlement(task, icloudContainerKey as CFString, nil) else {
            return false
        }
        // The key holds an array of container ids; any non-empty value counts.
        if let array = value as? [Any] {
            return !array.isEmpty
        }
        return (value as? String)?.isEmpty == false
    }
}
