/// Access gate for a personal-context provider. Raw-store providers (v1.5)
/// probe real access (SQLite open, osascript round-trip) and only pass after
/// the user flips the "Raw stores" toggle — the deliberate TCC moment. v1
/// providers keep the default passthrough gate, so nothing changes for them.
public protocol PersonalContextGate: Sendable {
    /// Returns true when the provider may read its backing store. Probes are
    /// bounded and never surface an error — a failure is just `false`.
    func verifyAccess() async -> Bool
}

/// The v1 default: no additional gate beyond the provider's own lazy
/// authorization.
public struct PassthroughGate: PersonalContextGate {
    public init() {}
    public func verifyAccess() async -> Bool { true }
}

extension PersonalContextProvider {
    /// Default gate for v1 providers — always passes. Lives in a protocol
    /// extension backing a PROTOCOL REQUIREMENT (not a plain extension
    /// member) so it dispatches through `any PersonalContextProvider`.
    public var gate: PersonalContextGate { PassthroughGate() }
}
