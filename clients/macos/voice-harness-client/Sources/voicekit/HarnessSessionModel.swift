import Foundation
import Combine

@MainActor
public final class HarnessSessionModel: ObservableObject {
    public enum Activity: Equatable, Sendable {
        case idle
        case thinking
        case callingTools
    }

    @Published public private(set) var phase: SessionPhase = .idle
    @Published public private(set) var transcript = ""
    @Published public private(set) var responseText = ""
    @Published public private(set) var errorMessage: String?
    /// True while the player still has scheduled-but-unplayed audio.
    @Published public private(set) var audioPending = false
    /// Sub-state under `thinking`: what the model is doing right now.
    /// Deliberately separate from `phase` so the audioPending absorption
    /// rules above stay untouched.
    @Published public private(set) var activity: Activity = .idle

    /// How long the completion watchdog waits after playback drains before
    /// self-completing the turn. The server sends `turn.completed` (+
    /// trailing states) back-to-back with the final `audio.chunk` — live-
    /// verified — so a healthy stream always lands it before drain. If it
    /// never arrives (stalled upstream: the provider HTTP clients have no
    /// read timeout, a hung TTS/LLM body read stalls the turn task), the UI
    /// would stick in `speaking` forever. Settable for tests.
    public var completionWatchdogDelay: Duration = .seconds(2.5)
    private var completionWatchdog: Task<Void, Never>?

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
            activity = (s == .thinking) ? .thinking : .idle
            if s == .thinking { responseText = "" }
        case .stateThinking(let detail):
            activity = (detail == .callingTools) ? .callingTools : .thinking
        case .transcript(let t): transcript = t
        case .responseTextDelta(let d): responseText += d
        case .responseText(let t): responseText = t
        case .audioChunk:
            audioPending = true
            if phase != .speaking { phase = .speaking }
        case .turnCompleted:
            if !audioPending { phase = .listening }
            completionWatchdog?.cancel()
            completionWatchdog = nil
        case .toolCall:
            // Personal-context call forwarded to PersonalContextServer by the
            // runtime. UI-wise already covered by the preceding
            // `state.thinking(calling_tools)` frame — nothing to change here.
            break
        case .error(let _, let m):
            errorMessage = m
            phase = .idle
            activity = .idle
            completionWatchdog?.cancel()
            completionWatchdog = nil
        }
    }

    /// Called when playback drains (runtime drain watcher). Arms a watchdog
    /// that self-completes the turn when the server's end-of-turn burst never
    /// arrived — a stalled turn leaves the UI in `speaking` forever otherwise.
    public func audioDidFinish() {
        audioPending = false
        guard completionWatchdog == nil else { return }
        completionWatchdog = Task { [weak self] in
            try? await Task.sleep(for: self?.completionWatchdogDelay ?? .seconds(2.5))
            guard let self, !Task.isCancelled else { return }
            // Clear BEFORE deciding: a new turn's audio may have started while
            // we slept — this drain's watchdog is spent either way, and the
            // next drain must be able to arm a fresh one.
            self.completionWatchdog = nil
            // Only recover the exact stuck condition: drained, no completion,
            // still marked speaking. If the user already started talking again
            // (phase moved to speech/thinking), a fire here would stomp the
            // new turn's phase back to listening.
            guard !self.audioPending, self.phase == .speaking else { return }
            self.apply(.turnCompleted)
        }
    }
    public func reset() {
        phase = .idle
        transcript = ""
        responseText = ""
        errorMessage = nil
        audioPending = false
        activity = .idle
        completionWatchdog?.cancel()
        completionWatchdog = nil
    }
}
