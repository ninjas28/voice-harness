/// Routes server `tool.call` frames to the right personal-context provider
/// and turns the outcome into a `tool.result` frame. Also builds the
/// `context.announce` message from the currently-authorized providers.
public actor PersonalContextServer {
    /// Namespace the server assigns; descriptors carry bare tool names.
    public static let toolNamePrefix = "personal."
    /// Digest clamp: digests longer than this are cut and marked.
    public static let maxResultBytes = 8192
    static let truncationMarker = "…(truncated)"

    private let providers: [any PersonalContextProvider]

    public init(providers: [any PersonalContextProvider]) {
        self.providers = providers
    }

    /// Handle one server message; sends `tool.result` via `send` when
    /// applicable. Frames other than `personal.*` tool calls are ignored.
    public func handle(_ message: ServerMessage,
                       send: @Sendable (ClientMessage) async -> Void) async {
        guard case .toolCall(let callId, let name, let argumentsJSON) = message else { return }
        guard name.hasPrefix(Self.toolNamePrefix) else { return }

        let rest = name.dropFirst(Self.toolNamePrefix.count)
        guard let dot = rest.firstIndex(of: ".") else {
            await sendUnknownTool(callId, name, send: send)
            return
        }
        let providerId = String(rest[..<dot])
        let tool = String(rest[rest.index(after: dot)...])

        // Routing gate: the provider must exist, be authorized (a descriptor
        // is announced only when authorized), and have announced this tool —
        // exactly the set the server-side catalog can resolve.
        guard let provider = providers.first(where: { $0.providerId == providerId }) else {
            await sendUnknownTool(callId, name, send: send)
            return
        }
        guard let descriptor = await provider.currentDescriptor() else {
            await send(.toolResult(callId: callId, ok: false,
                                   text: "Your \(providerId) access has not been granted yet."))
            return
        }
        guard descriptor.tools.contains(where: { $0.name == tool }) else {
            await sendUnknownTool(callId, name, send: send)
            return
        }

        do {
            let digest = try await provider.execute(name: tool, argumentsJSON: argumentsJSON)
            await send(.toolResult(callId: callId, ok: true, text: Self.clamp(digest)))
        } catch {
            await send(.toolResult(callId: callId, ok: false, text: Self.errorText(error)))
        }
    }

    /// The `context.announce` message built from currently-authorized
    /// providers. An empty providers list is valid: it clears the server-side
    /// catalog (used when the user disables personal context). `identityKeys`
    /// ride along when the runtime computed them (federation); default empty
    /// keeps every existing construction v1-shaped.
    public func announceMessage(identityKeys: [String] = []) async -> ClientMessage {
        .contextAnnounce(providers: await announceProviders(),
                         identityKeys: identityKeys)
    }

    /// Descriptors of the currently-usable providers — the payload of
    /// `context.announce`. A provider is announced only when its gate passes
    /// AND it returns a descriptor (nil descriptors omit the provider).
    /// Calling this is also what triggers each provider's lazy authorization
    /// (TCC), so runtimes use it to pre-grant access.
    public func announceProviders() async -> [ProviderDescriptor] {
        var descriptors: [ProviderDescriptor] = []
        for provider in providers {
            guard await provider.gate.verifyAccess() else { continue }
            if let descriptor = await provider.currentDescriptor() {
                descriptors.append(descriptor)
            }
        }
        return descriptors
    }

    private func sendUnknownTool(_ callId: Int, _ name: String,
                                 send: @Sendable (ClientMessage) async -> Void) async {
        await send(.toolResult(callId: callId, ok: false,
                               text: "Unknown personal-context tool \(name)."))
    }

    // MARK: - Digest shaping

    /// Clamps a digest to `maxResultBytes` UTF-8 bytes, appending the
    /// truncation marker when cut.
    static func clamp(_ text: String, maxBytes: Int = PersonalContextServer.maxResultBytes) -> String {
        guard text.utf8.count > maxBytes else { return text }
        var truncated = text
        while truncated.utf8.count > maxBytes {
            truncated.removeLast()
        }
        truncated += truncationMarker
        return truncated
    }

    /// Plain-language text for a provider failure — the LLM speaks this.
    static func errorText(_ error: Error) -> String {
        switch error as? PersonalContextError {
        case .notAuthorized(let what):
            "Your \(what) access has not been granted."
        case .invalidArguments(let message):
            "That request was malformed: \(message)."
        case .failed(let message):
            "I couldn't do that: \(message)."
        case nil:
            "I couldn't do that: \(error.localizedDescription)."
        }
    }
}
