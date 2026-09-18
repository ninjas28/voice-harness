import AppKit
import Foundation
import voicekit

/// App entry point. AppKit owns the status item (anchored popover with a real
/// arrow, outside-click dismissal); see StatusItemController.
final class AppDelegate: NSObject, NSApplicationDelegate {
    private var controller: StatusItemController?

    func applicationDidFinishLaunching(_ notification: Notification) {
        controller = StatusItemController(runtime: AppRuntime.shared)
    }
}

@main
struct VoiceHarnessApp {
    static func main() {
        let app = NSApplication.shared
        let delegate = AppDelegate()
        app.delegate = delegate
        app.setActivationPolicy(.accessory) // menu-bar app: no dock icon
        app.run()
    }
}
