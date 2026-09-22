import Foundation

/// One matched contact, mapped from `CNContact` at the provider edge.
struct ContactModel: Equatable, Sendable {
    var name: String
    var phone: String?
    var email: String?
}

enum ContactsDigestFormatter {
    /// Spoken-style digest of matched contacts: one line per contact.
    static func digest(for query: String, matches: [ContactModel]) -> String {
        guard !matches.isEmpty else { return "No contacts found matching \(query)." }
        let lines = matches.map { line(for: $0) }
        let header = matches.count == 1 ? "Here's the contact I found for \(query):"
                                        : "Here are the \(matches.count) contacts I found for \(query):"
        var out = "\(header)\n"
        for line in lines {
            out += "• \(line)\n"
        }
        return DigestFormatter.clamp(out)
    }

    /// One contact line: "Sarah Nielsen — (415) 555-0132, sarah@…".
    static func line(for contact: ContactModel) -> String {
        var details: [String] = []
        if let phone = contact.phone, !phone.isEmpty { details.append(phone) }
        if let email = contact.email, !email.isEmpty { details.append(email) }
        if details.isEmpty { return contact.name }
        return "\(contact.name) — \(details.joined(separator: ", "))"
    }
}
