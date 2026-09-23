import Foundation
#if os(macOS)
import IOKit
#endif

/// Identity source backed by the macOS platform UUID: the `IOPlatformUUID`
/// of `IOPlatformExpertDevice` via IOKit — stable per machine, never shared
/// between two devices, so it can never falsely group two people.
///
/// macOS-only: the iOS client gets no platform-uuid key (YAGNI decision in
/// the plan). Compile-gated with `#if os(macOS)` — the type still exists on
/// iOS but yields nothing, so shared runtime code compiles on both.
public struct PlatformUUIDProvider: IdentityProvider {
    public let keyId = "platform_uuid"

    public init() {}

    public func currentKey() async -> String? {
        Self.platformUUID()
    }

    /// Reads `IOPlatformUUID` from the platform expert device. Synchronous
    /// IOKit registry read (no prompt, no network); nil when the entry is
    /// absent or malformed.
    static func platformUUID() -> String? {
#if os(macOS)
        // kIOMainPortDefault is the modern spelling of the default main
        // port (kIOMasterPortDefault is the deprecated alias).
        let service = IOServiceGetMatchingService(
            kIOMainPortDefault,
            IOServiceMatching("IOPlatformExpertDevice"))
        guard service != 0 else { return nil }
        defer { IOObjectRelease(service) }

        guard let property = IORegistryEntryCreateCFProperty(
            service,
            "IOPlatformUUID" as CFString,
            kCFAllocatorDefault,
            0)
        else { return nil }
        // Live-checked on this Mac: the registry value is a string like
        // "E1DA311F-2D73-57BC-8D9C-EB7E7840E67C", not raw 16 bytes.
        guard let uuidString = property.takeRetainedValue() as? String,
              UUID(uuidString: uuidString) != nil
        else { return nil }
        return uuidString
#else
        return nil // iOS: no platform-uuid identity key
#endif
    }
}
