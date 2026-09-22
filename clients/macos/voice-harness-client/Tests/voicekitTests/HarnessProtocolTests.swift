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

    // MARK: Personal-context wire messages (Task 6)

    func testToolCallDecode() throws {
        let msg = try ServerMessage.decode(
            #"{"type":"tool.call","call_id":7,"name":"personal.calendar.events","arguments":"{\"days_ahead\":1}"}"#)
        XCTAssertEqual(msg, .toolCall(callId: 7, name: "personal.calendar.events", argumentsJSON: "{\"days_ahead\":1}"))
    }

    func testToolResultEncode() {
        XCTAssertEqual(
            ClientMessage.toolResult(callId: 7, ok: true, text: "3 events").encode(),
            #"{"type":"tool.result","call_id":7,"ok":true,"text":"3 events"}"#)
    }

    func testToolResultEncodesQuotesSafely() {
        let json = ClientMessage.toolResult(callId: 1, ok: false, text: "quote \" and \\ backslash").encode()
        let obj = try! JSONSerialization.jsonObject(with: Data(json.utf8)) as! [String: Any]
        XCTAssertEqual(obj["text"] as? String, "quote \" and \\ backslash")
    }

    func testContextAnnounceEncode() throws {
        let json = ClientMessage.contextAnnounce(providers: [
            .init(id: "calendar", tools: [.init(name: "events", description: "List events.", parameters: ["type": .string("object")])])
        ]).encode()
        XCTAssertTrue(json.hasPrefix(#"{"type":"context.announce","providers":"#))
        // decode-back check via JSONSerialization for id/tool name round-trip
        let obj = try XCTUnwrap(JSONSerialization.jsonObject(with: Data(json.utf8)) as? [String: Any])
        let providers = try XCTUnwrap(obj["providers"] as? [[String: Any]])
        XCTAssertEqual(providers.first?["id"] as? String, "calendar")
        let tools = try XCTUnwrap(providers.first?["tools"] as? [[String: Any]])
        XCTAssertEqual(tools.first?["name"] as? String, "events")
        XCTAssertEqual(tools.first?["description"] as? String, "List events.")
    }

    func testJSONValueRoundTrip() throws {
        let value = JSONValue.object([
            "str": .string("hi"), "int": .int(-3), "dbl": .double(2.5),
            "bool": .bool(true), "nil": .null,
            "arr": .array([.int(1), .string("two")]),
            "obj": .object(["nested": .bool(false)]),
        ])
        let data = try JSONEncoder().encode(value)
        let back = try JSONDecoder().decode(JSONValue.self, from: data)
        XCTAssertEqual(back, value)
    }
}
