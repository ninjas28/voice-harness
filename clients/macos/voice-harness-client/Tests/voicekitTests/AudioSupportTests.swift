import XCTest
@testable import voicekit

final class AudioSupportTests: XCTestCase {
    func testPcm16ToBase64RoundTrips() {
        let samples: [Int16] = [-3000, 0, 3000, 12345]
        let decoded = base64ToPCM16(pcm16ToBase64(samples))
        XCTAssertEqual(decoded, samples)
    }

    /// Regression: at least one known input encoded an *empty* base64 string
    /// (empty .string frame), so the server never saw audio.data at all.
    func testPcm16ToBase64NeverEmptyForRealSamples() {
        let samples: [Int16] = (0..<4832).map { Int16(3000.0 * sin(Double($0) * 2.0 * .pi * 220.0 / 16000.0)) }
        XCTAssertEqual(samples.count, 4832)
        let encoded = pcm16ToBase64(samples)
        XCTAssertFalse(encoded.isEmpty, "pcm16ToBase64 produced an empty payload for \(samples.count) samples")
        XCTAssertEqual(encoded.count, 12888, "9664 bytes must encode to 12888 base64 chars")
        XCTAssertEqual(base64ToPCM16(encoded).count, 4832)
    }

    func testDecimateByAveraging() {
        XCTAssertEqual(decimateByAveraging([0, 100, 200, 300], factor: 2), [50, 250])
        XCTAssertEqual(decimateByAveraging([0, 100, 200, 300], factor: 4), [150])
        XCTAssertEqual(decimateByAveraging([1, 2, 3], factor: 1), [1, 2, 3])
    }
}
