import SwiftUI
import voicekit

struct StatusIcon: View {
    let phase: SessionPhase

    var body: some View {
        switch phase {
        case .idle: Image(systemName: "waveform")
        case .listening: Image(systemName: "mic")
        case .speech: Image(systemName: "waveform.circle.fill")
        case .thinking: Image(systemName: "brain") // fallback exists: "ellipsis.bubble"
        case .speaking: Image(systemName: "speaker.wave.2.fill")
        }
    }
}
