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
    case toolCall(callId: Int, name: String, argumentsJSON: String)
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
        let callId: Int?
        let name: String?
        let arguments: String?

        private enum CodingKeys: String, CodingKey {
            case type, state, detail, text, pcm, seq, code, message, name, arguments
            case callId = "call_id"
        }

        var value: ServerMessage {
            switch type {
            case "state": .state(state ?? .idle)
            case "state.thinking": .stateThinking(detail ?? .thinking)
            case "transcript": .transcript(text ?? "")
            case "response.text.delta": .responseTextDelta(text ?? "")
            case "response.text": .responseText(text ?? "")
            case "audio.chunk": .audioChunk(pcm: pcm ?? "", seq: seq ?? 0)
            case "turn.completed": .turnCompleted
            case "tool.call": .toolCall(callId: callId ?? 0, name: name ?? "", argumentsJSON: arguments ?? "")
            case "error": .error(code: code ?? "", message: message ?? "")
            default: .error(code: "unknown_type", message: type)
            }
        }
    }
}

/// A tool exposed by a client-side personal-context provider.
public struct ToolDescriptor: Codable, Equatable, Sendable {
    public let name: String
    public let description: String
    /// JSON schema for the arguments object.
    public let parameters: JSONValue

    public init(name: String, description: String, parameters: JSONValue) {
        self.name = name
        self.description = description
        self.parameters = parameters
    }
}

/// One client-side context provider announced via `context.announce`.
public struct ProviderDescriptor: Codable, Equatable, Sendable {
    public let id: String
    public let tools: [ToolDescriptor]

    public init(id: String, tools: [ToolDescriptor]) {
        self.id = id
        self.tools = tools
    }
}

public enum ClientMessage: Equatable, Sendable {
    case sessionStart(deviceId: String?, sampleRate: Int)
    case audioData(pcm: String)
    case speechEnd
    case sessionStop
    case toolResult(callId: Int, ok: Bool, text: String)
    case contextAnnounce(providers: [ProviderDescriptor])

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
        case .toolResult(let callId, let ok, let text):
            o = ["{\"type\":\"tool.result\",\"call_id\":\(callId),\"ok\":\(ok),",
                 "\"text\":\(Self.jsonEscaped(text))}"]
        case .contextAnnounce(let providers):
            o = ["{\"type\":\"context.announce\",\"providers\":",
                 Self.providersJSON(providers), "}"]
        }
        return o.joined()
    }

    /// JSON-escapes a string for safe inclusion in a hand-rolled frame.
    private static func jsonEscaped(_ s: String) -> String {
        guard let data = try? JSONEncoder().encode([s]),
              var element = String(data: data, encoding: .utf8) else {
            return "\"\""
        }
        element.removeFirst()
        element.removeLast()
        return element
    }

    /// Encodes the providers array via Codable (shaped exactly like the Rust
    /// `ProviderDescriptor`), spliced into the hand-rolled envelope.
    private static func providersJSON(_ providers: [ProviderDescriptor]) -> String {
        guard let data = try? JSONEncoder().encode(providers) else { return "[]" }
        return String(decoding: data, as: UTF8.self)
    }
}
