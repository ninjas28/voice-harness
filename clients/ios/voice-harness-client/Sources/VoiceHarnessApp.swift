import SwiftUI
import voicekit

@main
struct VoiceHarnessApp: App {
    @StateObject private var runtime = AppRuntime.shared
    @Environment(\.scenePhase) private var scenePhase

    var body: some Scene {
        WindowGroup { ContentView(runtime: runtime) }
            .onChange(of: scenePhase) { _, phase in
                // Control Center tap → widget extension wrote an autostart
                // request; the app (foregrounded by the intent) consumes it
                // here and jumps straight into listening.
                if phase == .active, AutoStartRequest.consume() {
                    Task { await runtime.start() }
                }
            }
    }
}
