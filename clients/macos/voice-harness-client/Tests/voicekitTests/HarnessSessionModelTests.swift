import XCTest
@testable import voicekit

@MainActor
final class HarnessSessionModelTests: XCTestCase {
    func testServerFramesDriveModel() async {
        let model = HarnessSessionModel()
        model.apply(.state(.listening))
        XCTAssertEqual(model.phase, .listening)
        model.apply(.state(.speech))
        model.apply(.transcript("what time is it"))
        XCTAssertEqual(model.transcript, "what time is it")
        model.apply(.state(.thinking))
        model.apply(.responseTextDelta("It's "))
        model.apply(.responseTextDelta("2:37."))
        XCTAssertEqual(model.responseText, "It's 2:37.")
        model.apply(.audioChunk(pcm: "QUJD", seq: 0))
        XCTAssertEqual(model.phase, .speaking)
        model.apply(.responseText("It's 2:37."))
        model.apply(.turnCompleted)
        // audio not drained yet → still speaking
        XCTAssertEqual(model.phase, .speaking)
    }

    func testErrorFrameSurfacesAndResets() {
        let model = HarnessSessionModel()
        model.apply(.error(code: "x", message: "boom"))
        XCTAssertEqual(model.errorMessage, "boom")
        XCTAssertEqual(model.phase, .idle)
    }

    func testStateThinkingDrivesActivity() {
        let model = HarnessSessionModel()
        model.apply(.state(.thinking))
        XCTAssertEqual(model.activity, .thinking)
        model.apply(.stateThinking(.callingTools))
        XCTAssertEqual(model.activity, .callingTools)
        XCTAssertEqual(model.phase, .thinking) // unchanged
        model.apply(.state(.listening))
        XCTAssertEqual(model.activity, .idle)
    }

    func testTurnCompletedAfterDrainReturnsToListening() async {
        let model = HarnessSessionModel()
        model.apply(.audioChunk(pcm: "QUJD", seq: 0))
        model.audioDidFinish()   // player drain callback
        model.apply(.turnCompleted)
        XCTAssertEqual(model.phase, .listening)
    }

    // MARK: Completion watchdog (stalled-turn recovery)

    /// Regression: if the server's end-of-turn burst (`turn.completed`) never
    /// arrives — a stalled TTS/LLM upstream hangs the turn task, and the
    /// provider HTTP clients have no read timeout — the UI stuck in
    /// `speaking` forever after the audio drained. The model must
    /// self-complete shortly after drain when no completion shows up.
    func testDrainWithoutTurnCompletedSelfCompletes() async {
        let model = HarnessSessionModel()
        model.completionWatchdogDelay = .milliseconds(80)
        model.apply(.audioChunk(pcm: "QUJD", seq: 0))
        model.apply(.responseText("The answer."))
        XCTAssertEqual(model.phase, .speaking)
        model.audioDidFinish()
        await waitForPhase(.listening, on: model, timeout: 3)
        XCTAssertEqual(model.responseText, "The answer.", "self-complete must not clear the response")
    }

    /// A `turn.completed` arriving normally cancels the watchdog — no late
    /// re-application (a late apply(.turnCompleted) would yank a new turn's
    /// phase back to listening).
    func testTurnCompletedCancelsWatchdog() async {
        let model = HarnessSessionModel()
        model.completionWatchdogDelay = .milliseconds(80)
        model.apply(.audioChunk(pcm: "QUJD", seq: 0))
        model.audioDidFinish()
        model.apply(.turnCompleted)
        XCTAssertEqual(model.phase, .listening)
        // Well past the watchdog window: nothing may fire. Simulate the next
        // turn starting; a late watchdog would stomp it back to .listening.
        try? await Task.sleep(for: .milliseconds(300))
        model.apply(.state(.speech))
        XCTAssertEqual(model.phase, .speech)
    }

    /// The watchdog must not fire when a new turn's audio is already pending
    /// (echo-triggered speech + fresh audio.chunk can land inside the window).
    func testWatchdogSuppressedWhenNewAudioPending() async {
        let model = HarnessSessionModel()
        model.completionWatchdogDelay = .milliseconds(80)
        model.apply(.audioChunk(pcm: "QUJD", seq: 0))
        model.audioDidFinish() // arms watchdog
        // New turn's audio (mic heard echo → new response) before it fires:
        model.apply(.audioChunk(pcm: "QUJD", seq: 1))
        try? await Task.sleep(for: .milliseconds(300))
        XCTAssertEqual(model.phase, .speaking, "watchdog must not complete while new audio plays")
        XCTAssertEqual(model.audioPending, true)
    }

    /// `reset()` cancels the watchdog: after a reset the phase is `.idle`, and
    /// a firing watchdog would apply(.turnCompleted) → `.listening`.
    func testResetCancelsWatchdog() async {
        let model = HarnessSessionModel()
        model.completionWatchdogDelay = .milliseconds(80)
        model.apply(.audioChunk(pcm: "QUJD", seq: 0))
        model.audioDidFinish() // arms watchdog
        model.reset()
        try? await Task.sleep(for: .milliseconds(300))
        XCTAssertEqual(model.phase, .idle, "reset must cancel the pending watchdog")
    }

    @discardableResult
    private func waitForPhase(_ phase: SessionPhase, on model: HarnessSessionModel,
                              timeout: TimeInterval) async -> Bool {
        let deadline = Date().addingTimeInterval(timeout)
        // Yield via Task.sleep (NOT RunLoop pumping): the test task must
        // suspend so the main actor can service the model's watchdog task.
        while model.phase != phase && Date() < deadline {
            try? await Task.sleep(for: .milliseconds(10))
        }
        XCTAssertEqual(model.phase, phase, "phase not reached within \(timeout)s")
        return model.phase == phase
    }
}
