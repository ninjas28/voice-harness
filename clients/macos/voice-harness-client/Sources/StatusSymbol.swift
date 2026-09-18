import Foundation
import voicekit

/// Single source of truth for the status-item symbol per session state.
/// Consumed by the AppKit status item (StatusItemController) so the icon
/// always matches the session phase.
func swiftSymbolName(for phase: SessionPhase) -> String {
    switch phase {
    case .idle: "waveform"
    case .listening: "mic"
    case .speech: "waveform.circle.fill"
    case .thinking: "brain" // fallback exists: "ellipsis.bubble"
    case .speaking: "speaker.wave.2.fill"
    }
}
