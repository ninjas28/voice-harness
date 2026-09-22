import Foundation

/// Small settings persisted in `UserDefaults`.
///
/// `defaults` is a test seam: tests inject a scratch `UserDefaults(suiteName:)`
/// domain so they never touch the app's real settings.
public enum AppSettings {
    /// Thread-safe by `UserDefaults`'s own guarantee; unsafe-annotated only
    /// because Swift 6 cannot see that for a global var.
    public nonisolated(unsafe) static var defaults: UserDefaults = .standard

    public static let serverURLKey = "server_url"
    public static let defaultServerURLString = "ws://127.0.0.1:8090/v1/realtime"

    public static let apiKeyKey = "api_key"

    public static var serverURL: URL {
        let sanitized = sanitizeServerURLString(storedServerURLString ?? "") ?? ""
        return URL(string: sanitized) ?? URL(string: defaultServerURLString)!
    }

    /// The raw stored string (nil when unset) — the panel can display it.
    public static var storedServerURLString: String? {
        let raw = defaults.string(forKey: serverURLKey)?
            .trimmingCharacters(in: .whitespacesAndNewlines)
        guard let raw, !raw.isEmpty else { return nil }
        return raw
    }

    /// Validates and normalizes a user-entered server URL.
    ///
    /// Returns the trimmed string when it is a well-formed ws:// or wss:// URL
    /// with a host, `""` for an empty/clearing entry, and `nil` when the entry
    /// is unusable (callers must ignore it — never store a broken URL).
    ///
    /// Cleartext `ws://` is only allowed to loopback, private/link-local
    /// (RFC1918 + 169.254, or a `*.local` mDNS name) hosts — sending
    /// unencrypted microphone audio across the public internet is never
    /// acceptable. `wss://` is valid for any host.
    public static func sanitizeServerURLString(_ value: String) -> String? {
        let trimmed = value.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { return "" }
        guard let url = URL(string: trimmed),
              let scheme = url.scheme?.lowercased(),
              scheme == "ws" || scheme == "wss",
              let host = url.host, !host.isEmpty
        else { return nil }
        if scheme == "ws", !isLocalCleartextHost(host) { return nil }
        return trimmed
    }

    /// True when a cleartext `ws://` host is acceptable: loopback, private
    /// (RFC1918), link-local (169.254), or a `*.local` mDNS name.
    private static func isLocalCleartextHost(_ host: String) -> Bool {
        let lowered = host.lowercased()
        if lowered == "localhost" { return true }
        if lowered.hasSuffix(".local") { return true }

        // `url.host` may keep IPv6 brackets (older Foundation) or strip them
        // (swift-foundation); parse the bare address either way.
        var bare = lowered
        if bare.hasPrefix("["), bare.hasSuffix("]") {
            bare = String(bare.dropFirst().dropLast())
        }

        var v4 = in_addr()
        if inet_pton(AF_INET, bare, &v4) == 1 {
            let octets = withUnsafeBytes(of: v4) { Array($0) }
            switch octets[0] {
            case 127: // loopback
                return true
            case 10: // RFC1918 10/8
                return true
            case 172: // RFC1918 172.16/12
                return (16...31).contains(octets[1])
            case 192: // RFC1918 192.168/16
                return octets[1] == 168
            case 169: // link-local 169.254/16
                return octets[1] == 254
            default:
                return false
            }
        }

        var v6 = in6_addr()
        if inet_pton(AF_INET6, bare, &v6) == 1 {
            // IPv6 loopback (::1): fifteen zero bytes, last byte 1.
            let bytes = withUnsafeBytes(of: v6) { Array($0) }
            return bytes[15] == 1 && bytes.prefix(15).allSatisfy { $0 == 0 }
        }

        // Non-literal hostname: treat as public for cleartext.
        return false
    }

    public static func setServerURLString(_ value: String) {
        guard let trimmed = sanitizeServerURLString(value) else { return }
        if trimmed.isEmpty {
            defaults.removeObject(forKey: serverURLKey)
        } else {
            defaults.set(trimmed, forKey: serverURLKey)
        }
    }

    // MARK: - API key

    /// Harness server API key, sent as `Authorization: Bearer <key>` on the
    /// websocket handshake. Empty (the default) means no-auth localhost use.
    public static var apiKey: String {
        let raw = defaults.string(forKey: apiKeyKey)?
            .trimmingCharacters(in: .whitespacesAndNewlines)
        return raw ?? ""
    }

    public static func setAPIKey(_ value: String) {
        let trimmed = value.trimmingCharacters(in: .whitespacesAndNewlines)
        if trimmed.isEmpty {
            defaults.removeObject(forKey: apiKeyKey)
        } else {
            defaults.set(trimmed, forKey: apiKeyKey)
        }
    }

    // MARK: - Personal context toggle

    /// Master switch for client-executed personal-context tools (calendar,
    /// contacts, photos). Default off: enabling it is the one deliberate TCC
    /// prompt, so users who never opt in get zero authorization requests.
    public static let personalContextEnabledKey = "personal_context_enabled"

    public static var personalContextEnabled: Bool {
        defaults.bool(forKey: personalContextEnabledKey)
    }

    public static func setPersonalContextEnabled(_ value: Bool) {
        defaults.set(value, forKey: personalContextEnabledKey)
    }

    // MARK: - TTS playback rate

    public static let ttsRateKey = "tts_rate"

    /// Available playback speeds for the panel picker.
    public static let ttsRateChoices: [Float] = [1.0, 1.2, 1.5, 2.0]

    public static var ttsRate: Float {
        let stored = defaults.object(forKey: ttsRateKey) as? Float
        return stored ?? 1.0
    }

    public static func setTTSRate(_ value: Float) {
        defaults.set(value, forKey: ttsRateKey)
    }
}
