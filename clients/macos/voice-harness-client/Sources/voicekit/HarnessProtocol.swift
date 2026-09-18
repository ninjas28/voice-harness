import Foundation

public enum SessionPhase: String, Codable, Sendable {
    case idle // client-only
    case listening, speech, thinking, speaking
}

public enum ThinkingDetail: String, Codable, Equatable, Sendable {
    case thinking
    case callingTools = "calling_tools"
}

public enum ServerMessage: Equatable, Sendable {
    case state(SessionPhase)
    case stateThinking(ThinkingDetail)
    case transcript(String)
    case responseTextDelta(String)
    case responseText(String)
    case audioChunk(pcm: String, seq: Int)
    case turnCompleted
    case error(code: String, message: String)

    public static func decode(_ json: String) throws -> ServerMessage {
        try JSONDecoder().decode(Wire.self, from: Data(json.utf8)).value
    }

    struct Wire: Decodable {
        let type: String
        let state: SessionPhase?
        let detail: ThinkingDetail?
        let text: String?
        let pcm: String?
        let seq: Int?
        let code: String?
        let message: String?
        var value: ServerMessage {
            switch type {
            case "state": .state(state ?? .idle)
            case "state.thinking": .stateThinking(detail ?? .thinking)
            case "transcript": .transcript(text ?? "")
            case "response.text.delta": .responseTextDelta(text ?? "")
            case "response.text": .responseText(text ?? "")
            case "audio.chunk": .audioChunk(pcm: pcm ?? "", seq: seq ?? 0)
            case "turn.completed": .turnCompleted
            case "error": .error(code: code ?? "", message: message ?? "")
            default: .error(code: "unknown_type", message: type)
            }
        }
    }
}

public enum ClientMessage: Equatable, Sendable {
    case sessionStart(deviceId: String?, sampleRate: Int)
    case audioData(pcm: String)
    case speechEnd
    case sessionStop

    public func encode() -> String {
        var o = [String]()
        switch self {
        case .sessionStart(let d, let r):
            o = ["{\"type\":\"session.start\""]
            if let d { o.append(",\"device_id\":\"\(d)\"") }
            o.append(",\"sample_rate\":\(r)}")
        case .audioData(let p): o = ["{\"type\":\"audio.data\",\"pcm\":\"\(p)\"}"]
        case .speechEnd: o = ["{\"type\":\"speech.end\"}"]
        case .sessionStop: o = ["{\"type\":\"session.stop\"}"]
        }
        return o.joined()
    }
}
