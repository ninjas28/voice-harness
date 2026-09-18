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
}
