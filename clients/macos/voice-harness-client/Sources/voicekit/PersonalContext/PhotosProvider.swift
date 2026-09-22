import Foundation
import Photos

/// Personal-context provider over PhotoKit: shallow statistics (counts, day
/// histogram, favorites) for a time window. Photos content itself is never
/// accessed — digests only. Authorization is lazy (TCC).
struct PhotosProvider: PersonalContextProvider {
    let providerId = "photos"
    /// Implementation cap on scanned assets (bounded-everything house rule).
    private static let fetchCap = 500

    static func descriptor() -> ProviderDescriptor {
        ProviderDescriptor(id: "photos", tools: [
            ToolDescriptor(
                name: "digest",
                description: "Summarize the user's photo library for recent days (counts, favorites, days).",
                parameters: [
                    "type": .string("object"),
                    "properties": .object([
                        "days_back": .object([
                            "type": .string("integer"),
                            "minimum": .int(1), "maximum": .int(90), "default": .int(7),
                            "description": .string("How many days back to summarize."),
                        ]),
                    ]),
                ]),
        ])
    }

    func currentDescriptor() async -> ProviderDescriptor? {
        await authorize() ? Self.descriptor() : nil
    }

    func execute(name: String, argumentsJSON: String) async throws -> String {
        guard await authorize() else { throw PersonalContextError.notAuthorized("photos") }
        guard name == "digest" else {
            throw PersonalContextError.failed("unknown photos tool '\(name)'")
        }
        let args = try PersonalContextArguments.parse(argumentsJSON)
        let daysBack = PersonalContextArguments.int(args, "days_back", default: 7, min: 1, max: 90)
        return digest(daysBack: daysBack)
    }

    /// Requests `.readWrite` — PhotoKit has no read-only level.
    private func authorize() async -> Bool {
        switch PHPhotoLibrary.authorizationStatus(for: .readWrite) {
        case .authorized, .limited: return true
        case .notDetermined:
            return await PHPhotoLibrary.requestAuthorization(for: .readWrite) == .authorized
        default: return false
        }
    }

    // MARK: - Fetch → model mapping (thin, unmocked)

    private func digest(daysBack: Int) -> String {
        let cal = Calendar.current
        let start = cal.startOfDay(for: cal.date(byAdding: .day, value: -daysBack + 1, to: Date())!)
        let options = PHFetchOptions()
        options.predicate = NSPredicate(format: "creationDate > %@", start as NSDate)
        options.sortDescriptors = [NSSortDescriptor(key: "creationDate", ascending: false)]
        options.fetchLimit = Self.fetchCap
        let fetch = PHAsset.fetchAssets(with: options)

        var total = 0
        var favorites = 0
        var oldest: Date?
        var newest: Date?
        var byWeekday: [String: Int] = [:]
        fetch.enumerateObjects { asset, _, _ in
            total += 1
            if asset.isFavorite { favorites += 1 }
            guard let date = asset.creationDate else { return }
            if oldest == nil || date < oldest! { oldest = date }
            if newest == nil || date > newest! { newest = date }
            let index = cal.component(.weekday, from: date)
            byWeekday[CalendarDigestFormatter.englishWeekday(index), default: 0] += 1
        }
        let stats = PhotoStats(total: total, favorites: favorites, oldest: oldest,
                               newest: newest, byWeekday: byWeekday)
        return PhotosDigestFormatter.digest(
            stats, now: Date(), daysLabel: daysBack == 1 ? "the last day" : "the last \(daysBack) days")
    }
}
