import SwiftUI
import voicekit

@main
struct VoiceHarnessApp: App {
    @StateObject private var runtime = AppRuntime.shared

    var body: some Scene {
        MenuBarExtra {
            PanelView(runtime: runtime)
                .frame(width: 340)
        } label: {
            StatusIcon(phase: runtime.phase)
        }
        .menuBarExtraStyle(.window)
    }
}
