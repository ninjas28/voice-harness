import Foundation
import Combine

@MainActor
public final class HarnessSessionModel: ObservableObject {
    @Published public private(set) var phase: SessionPhase = .idle
    @Published public private(set) var transcript = ""
    @Published public private(set) var responseText = ""
    @Published public private(set) var errorMessage: String?
    /// True while the player still has scheduled-but-unplayed audio.
    @Published public private(set) var audioPending = false

    public init() {}

    public func apply(_ msg: ServerMessage) {
        switch msg {
        case .state(let s):
            // While client-side playback is pending, the client owns the phase:
            // the server cannot know when queued audio finishes, so its
            // listening/thinking bookkeeping must not cut playback short. The
            // drain watcher completes the turn (audioDidFinish + turn.completed
            // → listening). Server `speech` during pending audio is also
            // absorbed — the mic hears the client's own TTS (no AEC), so it's
            // self-echo, not a user interruption.
            if audioPending && s != .idle { break }
            phase = s
            if s == .thinking { responseText = "" }
        case .transcript(let t): transcript = t
        case .responseTextDelta(let d): responseText += d
        case .responseText(let t): responseText = t
        case .audioChunk:
            audioPending = true
            if phase != .speaking { phase = .speaking }
        case .turnCompleted:
            if !audioPending { phase = .listening }
        case .error(let _, let m):
            errorMessage = m
            phase = .idle
        }
    }

    public func audioDidFinish() { audioPending = false }
    public func reset() {
        phase = .idle
        transcript = ""
        responseText = ""
        errorMessage = nil
        audioPending = false
    }
}
