import Foundation
import Security

/// Entitlement probe used by `ICloudAccountId` to avoid constructing
/// `CKContainer` on binaries without the iCloud entitlement.
///
/// `CKContainer.default()` throws an uncatchable Objective-C `CKException`
/// ('containerIdentifier can not be nil') when the running binary lacks the
/// entitlement (e.g. the hand-built unsigned macOS bundle) — the process
/// aborts. The probe reads the same entitlements CloudKit checks, without
/// touching CloudKit, so the failure becomes a silent nil instead of a crash.
///
/// Platform sources for the entitlement:
/// - macOS: `SecTaskCopyValueForEntitlement` (SecTask APIs are macOS-only —
///   they do not exist in the iOS SDK).
/// - iOS: the `Entitlements` dict inside the embedded provisioning profile
///   (device builds; simulator builds carry no profile, and CloudKit is not
///   usable there).
enum SecEntitlementProbe {
    /// The entitlement CloudKit requires for `CKContainer.default()`.
    private static let icloudContainerKey = "com.apple.developer.icloud-container-identifiers"

    /// True when the running binary holds the iCloud container entitlement.
    static func hasICloudContainerEntitlement() -> Bool {
        #if os(macOS)
        guard let task = SecTaskCreateFromSelf(nil) else { return false }
        guard let value = SecTaskCopyValueForEntitlement(task, icloudContainerKey as CFString, nil) else {
            return false
        }
        // The key holds an array of container ids; any non-empty value counts.
        if let array = value as? [Any] {
            return !array.isEmpty
        }
        return (value as? String)?.isEmpty == false
        #else
        hasICloudContainerInEmbeddedProvisioningProfile()
        #endif
    }

    #if !os(macOS)
    /// iOS path: parse `Entitlements` from the embedded provisioning profile.
    private static func hasICloudContainerInEmbeddedProvisioningProfile() -> Bool {
        guard let profileURL = Bundle.main.url(forResource: "embedded", withExtension: "mobileprovision"),
              let blob = try? Data(contentsOf: profileURL) else {
            return false // no profile (simulator) or unreadable: treat as unentitled
        }
        guard let plistData = Self.embeddedPlistData(in: blob),
              let plist = try? PropertyListSerialization.propertyList(from: plistData, format: nil)
                  as? [String: Any],
              let entitlements = plist["Entitlements"] as? [String: Any] else {
            return false
        }
        if let array = entitlements[icloudContainerKey] as? [Any] {
            return !array.isEmpty
        }
        return (entitlements[icloudContainerKey] as? String)?.isEmpty == false
    }

    /// A mobileprovision is a CMS blob with a plain XML plist embedded; locate
    /// and extract it (no crypto verification needed — we only read
    /// entitlement claims to decide whether CloudKit is safe to construct).
    private static func embeddedPlistData(in blob: Data) -> Data? {
        guard let start = blob.range(of: Data("<?xml".utf8)),
              let end = blob.range(of: Data("</plist>".utf8), in: start.upperBound..<blob.endIndex) else {
            return nil
        }
        return blob.subdata(in: start.lowerBound..<end.upperBound)
    }
    #endif
}
