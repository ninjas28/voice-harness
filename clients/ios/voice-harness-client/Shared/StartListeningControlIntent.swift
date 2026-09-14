import AppIntents
import Foundation
import voicekit

/// "Start Voice Harness Listening" — the Control Center control's intent.
///
/// MUST be compiled into BOTH targets (app + widget extension): the system
/// only honors `openAppWhenRun` and runs the intent in the app's process when
/// the intent type exists in the app target. With it present in both, a
/// control tap foregrounds the app and perform() runs in-app, where it starts
/// the session directly; the app-group flag write stays as a fallback for the
/// extension-process path (the app consumes it on foreground).
@available(iOS 18.0, *)
struct StartListeningControlIntent: AppIntent {
    static let title: LocalizedStringResource = "Start Voice Harness Listening"
    static let description = IntentDescription("Opens Voice Harness and starts the microphone session.")
    static let openAppWhenRun = true

    @MainActor
    func perform() async throws -> some IntentResult & ProvidesDialog {
        AutoStartRequest.request()
        #if APP
        // Running in the app process (intent compiled into the app target):
        // start immediately — no scenePhase round-trip needed, and this also
        // covers the already-foregrounded case where scenePhase won't refire.
        // start() is a no-op when a session is already running.
        await AppRuntime.shared.start()
        #endif
        return .result(dialog: "Starting Voice Harness…")
    }
}
