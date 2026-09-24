import Foundation

/// Cross-process "start listening" handoff, shared by the iOS app and its
/// Control Center widget extension via an app-group `UserDefaults`.
///
/// The extension cannot reach the app's live session (separate process), so a
/// control tap writes `request()` here; the app consumes it on foreground and
/// auto-starts the session.
public enum AutoStartRequest {
    /// The app-group ID must match the app target and the widget extension
    /// (same entitlements on both) — one source of truth.
    public static let appGroupID = "group.com.voiceharness.harness"

    static let flagKey = "autostart_requested"

    /// Test seam: injected scratch domain; nil in production.
    nonisolated(unsafe) static var defaultsOverride: UserDefaults?

    private static var defaults: UserDefaults {
        defaultsOverride ?? UserDefaults(suiteName: appGroupID) ?? .standard
    }

    /// Called by the widget extension's control intent.
    public static func request() {
        defaults.set(true, forKey: flagKey)
    }

    /// Called by the app (foreground): returns and clears the flag.
    @discardableResult
    public static func consume() -> Bool {
        let v = defaults.bool(forKey: flagKey)
        if v { defaults.set(false, forKey: flagKey) }
        return v
    }
}
