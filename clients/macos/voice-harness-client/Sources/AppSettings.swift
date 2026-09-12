import Foundation

/// Small settings persisted in `UserDefaults`.
enum AppSettings {
    static let serverURLKey = "server_url"
    static let defaultServerURLString = "ws://127.0.0.1:8090/v1/realtime"

    static var serverURL: URL {
        storedServerURLString.flatMap(URL.init(string:)) ?? URL(string: defaultServerURLString)!
    }

    /// The raw stored string (nil when unset) — the panel can display it.
    static var storedServerURLString: String? {
        let raw = UserDefaults.standard.string(forKey: serverURLKey)?
            .trimmingCharacters(in: .whitespacesAndNewlines)
        guard let raw, !raw.isEmpty else { return nil }
        return raw
    }

    static func setServerURLString(_ value: String) {
        let trimmed = value.trimmingCharacters(in: .whitespacesAndNewlines)
        if trimmed.isEmpty {
            UserDefaults.standard.removeObject(forKey: serverURLKey)
        } else {
            UserDefaults.standard.set(trimmed, forKey: serverURLKey)
        }
    }
}
