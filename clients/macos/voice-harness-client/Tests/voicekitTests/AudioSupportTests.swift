import XCTest
@testable import voicekit

final class AudioSupportTests: XCTestCase {
    func testPCM16RoundTripAndDownsample() {
        // 4:1 simple decimation 48k → 12k; verify value positions survive.
        let src: [Int16] = [0, 100, 200, 300, 400, 500, 600, 700]
        let out = decimateByAveraging(src, factor: 4)
        // 4:1: each output is the mean of its 4-sample block (600/4=150, 2200/4=550).
        XCTAssertEqual(out, [150, 550])
        // 3:1 mirrors the plan's real 48k → 16k case.
        XCTAssertEqual(decimateByAveraging([0, 3, 6, 9, 12, 15], factor: 3), [3, 12])
        let bytes = pcm16ToBase64(out)
        let back = base64ToPCM16(bytes)
        XCTAssertEqual(back, out)
    }

    func testBase64ToPCM16HandlesOddByteCount() {
        XCTAssertEqual(base64ToPCM16(pcm16ToBase64([-3, 7, -32000])), [-3, 7, -32000])
    }
}
