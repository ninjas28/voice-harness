import Foundation

/// One locally-executing personal-context provider (calendar, contacts,
/// photos). Descriptors carry BARE tool names — the server namespaces them as
/// `personal.<provider>.<tool>` when advertising to the LLM.
public protocol PersonalContextProvider: Sendable {
    /// Stable provider id used in `personal.<id>.<tool>` routing and in the
    /// announced descriptor (`descriptor.id` must equal this).
    var providerId: String { get }
    /// The provider's catalog when currently usable (authorized), else nil —
    /// unauthorized providers are omitted from `context.announce`.
    func currentDescriptor() async -> ProviderDescriptor?
    /// Execute a bare tool name with the raw JSON arguments string.
    /// Returns plain spoken-style digest text or throws.
    func execute(name: String, argumentsJSON: String) async throws -> String
    /// Access gate consulted before the provider is announced. Defaults to a
    /// passthrough (always true) so v1 providers need zero changes; raw-store
    /// providers supply a real probe.
    var gate: PersonalContextGate { get }
}

/// Failures providers surface; the server converts them into plain-language
/// `tool.result` text the LLM can speak.
public enum PersonalContextError: Error, Equatable {
    /// Access was denied / not yet granted (TCC).
    case notAuthorized(String)
    /// The arguments string could not be understood.
    case invalidArguments(String)
    /// Anything else, already in plain language.
    case failed(String)
}

/// Argument parsing shared by the providers: decodes the JSON object and
/// extracts bounded integers / optional strings.
enum PersonalContextArguments {
    static func parse(_ argumentsJSON: String) throws -> [String: JSONValue] {
        guard !argumentsJSON.trimmingCharacters(in: .whitespaces).isEmpty else { return [:] }
        guard let value = JSONValue(jsonString: argumentsJSON), case .object(let dict) = value else {
            throw PersonalContextError.invalidArguments("arguments must be a JSON object")
        }
        return dict
    }

    static func int(_ dict: [String: JSONValue], _ key: String,
                    default fallback: Int, min minimum: Int, max maximum: Int) -> Int {
        guard case .int(let raw) = dict[key] else { return fallback }
        return min(max(raw, minimum), maximum)
    }

    static func string(_ dict: [String: JSONValue], _ key: String) -> String? {
        guard case .string(let value) = dict[key] else { return nil }
        let trimmed = value.trimmingCharacters(in: .whitespaces)
        return trimmed.isEmpty ? nil : trimmed
    }
}
