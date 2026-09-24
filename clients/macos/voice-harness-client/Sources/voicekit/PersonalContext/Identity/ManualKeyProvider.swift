import Foundation

/// Identity source backed by a user-set shared key (manual identity bridge):
/// the user types the same value into every device's panel — e.g. the home
/// alias — and the server groups all devices announcing that key into one
/// canonical user. Zero entitlements, zero signing, works everywhere.
///
/// This is the fallback identity bridge when the iCloud path is unavailable
/// (free personal development teams cannot use the CloudKit capability).
/// The `manual` keyId is unknown to `IdentityResolver`'s precedence table,
/// so it sorts after the automatic sources; a single manual value is still
/// enough for the server to group devices.
public struct ManualKeyProvider: IdentityProvider {
    public let keyId = "manual"

    public init() {}

    /// The persisted value, or nil when unset/empty (unavailable source).
    var currentKeyValue: String? {
        let value = AppSettings.manualIdentityKey
        return value.isEmpty ? nil : value
    }

    public func currentKey() async -> String? {
        currentKeyValue
    }
}
