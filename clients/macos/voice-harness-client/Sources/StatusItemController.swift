import AppKit
import Combine
import Foundation
import SwiftUI
import voicekit

/// AppKit glue: NSStatusItem + transient NSPopover. The popover is anchored
/// to the status item with a real arrow, dismisses on outside click (transient
/// behavior), and hosts the same PanelView SwiftUI view via
/// NSHostingController — one source of truth for the popover's content.
@MainActor
final class StatusItemController: NSObject {
    private let statusItem = NSStatusBar.system.statusItem(
        withLength: NSStatusItem.variableLength)
    private let popover = NSPopover()
    private var cancellables = Set<AnyCancellable>()

    init(runtime: AppRuntime) {
        super.init()

        popover.behavior = .transient
        popover.contentSize = NSSize(width: 340, height: 200)
        popover.contentViewController = NSHostingController(
            rootView: PanelView(runtime: runtime).frame(width: 340))

        if let button = statusItem.button {
            button.image = NSImage(
                systemSymbolName: swiftSymbolName(for: .idle),
                accessibilityDescription: "Voice assistant")
            button.target = self
            button.action = #selector(togglePopover)
        }

        // Keep the status-item image in sync with the session phase.
        runtime.$phase.dropFirst().sink { [weak self] phase in
            self?.updateIcon(phase)
        }.store(in: &cancellables)
    }

    private func updateIcon(_ phase: SessionPhase) {
        guard let button = statusItem.button else { return }
        button.image = NSImage(
            systemSymbolName: swiftSymbolName(for: phase),
            accessibilityDescription: "Voice assistant")
    }

    @objc private func togglePopover() {
        guard let button = statusItem.button else { return }
        if popover.isShown {
            popover.performClose(nil)
        } else {
            popover.show(relativeTo: button.bounds, of: button, preferredEdge: .minY)
        }
    }
}
