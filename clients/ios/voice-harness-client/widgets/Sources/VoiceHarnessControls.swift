import AppIntents
import SwiftUI
import WidgetKit
import voicekit

/// Control Center control (iOS 18+): "Voice Harness" button that foregrounds
/// the app and auto-starts listening. The intent runs in this extension's
/// process — it cannot reach the app's live session — so it writes an
/// autostart request to the shared app group; the app consumes it on
/// foreground (VoiceHarnessApp.scenePhase).
@available(iOS 18.0, *)
struct StartListeningControl: ControlWidget {
    static let kind = "wtf.geese.VoiceHarness.StartListening"

    var body: some ControlWidgetConfiguration {
        StaticControlConfiguration(kind: Self.kind) {
            ControlWidgetButton(action: StartListeningControlIntent()) {
                Label("Voice Harness", systemImage: "waveform")
            }
        }
        .displayName("Voice Harness")
    }
}

@available(iOS 18.0, *)
struct StartListeningControlIntent: AppIntent {
    static let title: LocalizedStringResource = "Start Voice Harness Listening"
    static let description = IntentDescription("Opens Voice Harness and starts the microphone session.")
    static let openAppWhenRun = true

    @MainActor
    func perform() async throws -> some IntentResult & ProvidesDialog {
        AutoStartRequest.request()
        return .result(dialog: "Starting Voice Harness…")
    }
}

@main
@available(iOS 18.0, *)
struct VoiceHarnessWidgetBundle: WidgetBundle {
    var body: some Widget {
        StartListeningControl()
    }
}
