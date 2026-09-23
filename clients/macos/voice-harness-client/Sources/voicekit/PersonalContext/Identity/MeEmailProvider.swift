import Foundation
import Contacts

/// Identity source backed by the resolved "me" contact card's first email —
/// a weak cross-device fallback key (works when iCloud is unavailable but the
/// same account/card is on both devices).
///
/// Never a prompt: when Contacts is not already authorized this provider
/// returns nil without calling `requestAccess` — identity is strictly a
/// read-only side effect of the authorization the v1 personal-context
/// toggle already asked for.
///
/// Platform note: Apple marks `unifiedMeContactWithKeys(toFetch:)`
/// macOS-only (unavailable on iOS), so on iOS this provider yields nil.
public struct MeEmailProvider: IdentityProvider {
    public let keyId = "me_email"

    public init() {}

    public func currentKey() async -> String? {
#if os(macOS)
        // .notDetermined → nil: identity never requests access itself.
        guard CNContactStore.authorizationStatus(for: .contacts) == .authorized else {
            return nil
        }
        // The fetch is a synchronous local store read — run it off the
        // cooperative pool; the store and key descriptors are built inside
        // so nothing non-Sendable crosses the closure boundary.
        return await Task.detached {
            let keys = [CNContactEmailAddressesKey] as [CNKeyDescriptor]
            let store = CNContactStore()
            guard let contact = try? store.unifiedMeContactWithKeys(toFetch: keys),
                  let email = contact.emailAddresses.first?.value as String?
            else { return nil }
            let trimmed = email.trimmingCharacters(in: .whitespacesAndNewlines)
            return trimmed.isEmpty ? nil : trimmed.lowercased()
        }.value
#else
        return nil // iOS: API unavailable by Apple's declaration
#endif
    }
}
