import Foundation

/// One identity source for personal-context federation (plan
/// 2026-09-21_235410). A provider yields a stable, device- or
/// account-scoped string key; the server groups clients sharing a key
/// into one canonical user.
public protocol IdentityProvider: Sendable {
    /// Namespaced key type, e.g. `icloud`, `platform_uuid`, `me_email`.
    /// Keys travel as `<keyId>:<value>` so unlike types never collide.
    var keyId: String { get }
    /// The current key value, or nil when the source is unavailable
    /// (no account, offline, unauthorized — never a prompt).
    func currentKey() async -> String?
}

/// Assembles identity keys from providers in a fixed precedence order:
/// `icloud` > `platform_uuid` > `me_email`, regardless of construction
/// order. Nil (unavailable) sources are skipped, duplicates deduped.
public struct IdentityResolver: Sendable {
    private let providers: [any IdentityProvider]

    public init(providers: [any IdentityProvider]) {
        self.providers = providers
    }

    /// Non-nil keys in fixed precedence order, deduped.
    public func identityKeys() async -> [String] {
        var ranked: [Ranked] = []
        for (index, provider) in providers.enumerated() {
            guard let raw = await provider.currentKey() else { continue }
            let value = raw.trimmingCharacters(in: .whitespacesAndNewlines)
            guard !value.isEmpty else { continue }
            // Canonical order comes from the precedence rank; ties (two
            // providers with the same keyId) keep construction order.
            let rank = Self.precedenceRank(for: provider.keyId, at: index)
            ranked.append((rank, "\(provider.keyId):\(value)"))
        }
        var seen = Set<String>()
        return ranked
            .sorted { $0.rank < $1.rank }
            .map(\.key)
            .filter { seen.insert($0).inserted }
    }

    private typealias Ranked = (rank: Int, key: String)

    /// Precedence rank per keyId; unknown keyIds sort after the known ones
    /// (stable), keeping the resolver open to future sources.
    private static func precedenceRank(for keyId: String, at index: Int) -> Int {
        switch keyId {
        case "icloud": return 0
        case "platform_uuid": return 1
        case "me_email": return 2
        default: return 3 + index
        }
    }
}
