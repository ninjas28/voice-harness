import SwiftUI

@main
struct VoiceHarnessApp: App {
    @StateObject private var runtime = AppRuntime.shared

    var body: some Scene {
        WindowGroup { ContentView(runtime: runtime) }
    }
}
