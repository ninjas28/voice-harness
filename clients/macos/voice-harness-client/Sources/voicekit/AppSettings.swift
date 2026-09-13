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
    public static func sanitizeServerURLString(_ value: String) -> String? {
        let trimmed = value.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { return "" }
        guard let url = URL(string: trimmed),
              let scheme = url.scheme?.lowercased(),
              scheme == "ws" || scheme == "wss",
              let host = url.host, !host.isEmpty
        else { return nil }
        return trimmed
    }

    public static func setServerURLString(_ value: String) {
        guard let trimmed = sanitizeServerURLString(value) else { return }
        if trimmed.isEmpty {
            defaults.removeObject(forKey: serverURLKey)
        } else {
            defaults.set(trimmed, forKey: serverURLKey)
        }
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
