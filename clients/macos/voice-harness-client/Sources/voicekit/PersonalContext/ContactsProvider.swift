import Foundation
import Contacts

/// Personal-context provider over the Contacts store: name search with
/// case-insensitive substring matching. Authorization is lazy (TCC). An actor
/// so the shared `CNContactStore` is isolated and Sendable is sound.
public actor ContactsProvider: PersonalContextProvider {
    public let providerId = "contacts"
    private let store = CNContactStore()

    public init() {}

    static func descriptor() -> ProviderDescriptor {
        ProviderDescriptor(id: "contacts", tools: [
            ToolDescriptor(
                name: "search",
                description: "Search the user's contacts by name.",
                parameters: [
                    "type": .string("object"),
                    "properties": .object([
                        "query": .object([
                            "type": .string("string"),
                            "description": .string("Name fragment to search for (case-insensitive)."),
                        ]),
                        "limit": .object([
                            "type": .string("integer"),
                            "minimum": .int(1), "maximum": .int(10), "default": .int(3),
                            "description": .string("Maximum number of matches to return."),
                        ]),
                    ]),
                    "required": .array([.string("query")]),
                ]),
        ])
    }

    public func currentDescriptor() async -> ProviderDescriptor? {
        await authorize() ? Self.descriptor() : nil
    }

    public func execute(name: String, argumentsJSON: String) async throws -> String {
        guard await authorize() else { throw PersonalContextError.notAuthorized("contacts") }
        guard name == "search" else {
            throw PersonalContextError.failed("unknown contacts tool '\(name)'")
        }
        let args = try PersonalContextArguments.parse(argumentsJSON)
        guard let query = PersonalContextArguments.string(args, "query") else {
            throw PersonalContextError.invalidArguments("a 'query' is required")
        }
        let limit = PersonalContextArguments.int(args, "limit", default: 3, min: 1, max: 10)
        return digest(query: query, limit: limit)
    }

    /// Requests contacts read access. No throw — denied is just `false`.
    private func authorize() async -> Bool {
        switch CNContactStore.authorizationStatus(for: .contacts) {
        case .authorized: return true
        case .notDetermined:
            return (try? await store.requestAccess(for: .contacts)) == true
        default: return false
        }
    }

    // MARK: - Fetch → model mapping (thin, unmocked)

    private func digest(query: String, limit: Int) -> String {
        let keys = [CNContactGivenNameKey, CNContactMiddleNameKey, CNContactFamilyNameKey,
                    CNContactPhoneNumbersKey, CNContactEmailAddressesKey] as [CNKeyDescriptor]
        var matches: [ContactModel] = []
        let request = CNContactFetchRequest(keysToFetch: keys)
        request.mutableObjects = false
        try? store.enumerateContacts(with: request) { contact, stop in
            guard matches.count < limit else { stop.pointee = true; return }
            let name = [contact.givenName, contact.middleName, contact.familyName]
                .filter { !$0.isEmpty }
                .joined(separator: " ")
            guard !name.isEmpty, name.localizedCaseInsensitiveContains(query) else { return }
            let phone = contact.phoneNumbers.first?.value.stringValue
            let email = contact.emailAddresses.first?.value as String?
            matches.append(ContactModel(name: name, phone: phone, email: email))
        }
        return ContactsDigestFormatter.digest(for: query, matches: matches)
    }
}
