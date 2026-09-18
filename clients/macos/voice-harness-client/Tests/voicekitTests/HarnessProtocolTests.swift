import XCTest
@testable import voicekit

final class HarnessProtocolTests: XCTestCase {
    func testDecodesEveryServerFrame() throws {
        let cases: [(String, ServerMessage)] = [
            (#"{"type":"state","state":"thinking"}"#, .state(.thinking)),
            (#"{"type":"transcript","text":"hi"}"#, .transcript("hi")),
            (#"{"type":"response.text.delta","text":"He"}"#, .responseTextDelta("He")),
            (#"{"type":"response.text","text":"Hello"}"#, .responseText("Hello")),
            (#"{"type":"audio.chunk","pcm":"QUJD","seq":3}"#, .audioChunk(pcm: "QUJD", seq: 3)),
            (#"{"type":"turn.completed"}"#, .turnCompleted),
            (#"{"type":"error","code":"bad","message":"nope"}"#, .error(code: "bad", message: "nope")),
            (#"{"type":"state.thinking","detail":"calling_tools"}"#, .stateThinking(.callingTools)),
            (#"{"type":"state.thinking"}"#, .stateThinking(.thinking)), // bare shape: `detail` defaults to plain thinking
        ]
        for (json, expected) in cases {
            XCTAssertEqual(try ServerMessage.decode(json), expected, "failed for \(json)")
        }
    }

    func testEncodesClientFrames() {
        XCTAssertEqual(ClientMessage.sessionStart(deviceId: "kitchen", sampleRate: 16000).encode(),
                       #"{"type":"session.start","device_id":"kitchen","sample_rate":16000}"#)
        XCTAssertEqual(ClientMessage.audioData(pcm: "QUJD").encode(),
                       #"{"type":"audio.data","pcm":"QUJD"}"#)
        XCTAssertEqual(ClientMessage.speechEnd.encode(), #"{"type":"speech.end"}"#)
    }
}
