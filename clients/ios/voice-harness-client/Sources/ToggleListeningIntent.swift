import AppIntents
import Foundation

/// "Toggle Voice Harness" — surfaces in Shortcuts automatically, so it can be
/// assigned to: Action Button (Settings → Action Button → Shortcut), a
/// home-screen icon (Shortcuts → ⓘ → Add to Home Screen), or Back Tap
/// (Settings → Accessibility → Touch → Back Tap). Parameterless by design.
@available(iOS 17, *)
struct ToggleListeningIntent: AppIntent {
    static let title: LocalizedStringResource = "Toggle Voice Harness"
    static let description = IntentDescription("Start or stop the voice harness microphone session.")
    static let openAppWhenRun = true

    @MainActor
    func perform() async throws -> some IntentResult & ProvidesDialog {
        await AppRuntime.shared.toggle()
        switch AppRuntime.shared.phase {
        case .listening, .speech: return .result(dialog: "Listening")
        default: return .result(dialog: "Stopped")
        }
    }
}

@available(iOS 17, *)
struct VoiceHarnessShortcuts: AppShortcutsProvider {
    static var appShortcuts: [AppShortcut] {
        AppShortcut(
            intent: ToggleListeningIntent(),
            phrases: [
                "Toggle \(.applicationName)",
                "Start \(.applicationName)",
                "Stop \(.applicationName)",
            ],
            shortTitle: "Voice Harness",
            systemImageName: "waveform")
    }
}
