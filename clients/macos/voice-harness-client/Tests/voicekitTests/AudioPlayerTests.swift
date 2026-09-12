import Combine
import XCTest
@testable import voicekit

final class AudioPlayerTests: XCTestCase {
    func testDrainCountsScheduledChunks() {
        let player = AudioPlayer()
        player.schedulePCMForTest([1, 2, 3], seq: 0)
        player.schedulePCMForTest([4, 5], seq: 1)
        XCTAssertFalse(player.isDrained)
        player.drainForTest()
        XCTAssertTrue(player.isDrained)
    }

    func testEmptyAndUndecodableChunksAreSkipped() {
        let player = AudioPlayer()
        player.schedulePCMForTest([1, 2, 3], seq: 0)
        player.scheduleChunk(base64: "not valid base64!!!")
        player.scheduleChunk(base64: "")
        player.scheduleChunk(base64: "AA==") // decodes to 1 byte → not a whole PCM16 sample
        XCTAssertFalse(player.isDrained)    // only the real chunk is pending
        player.drainForTest()
        XCTAssertTrue(player.isDrained)
    }

    func testWaitUntilDrainedReturnsImmediatelyWhenAlreadyDrained() async {
        let player = AudioPlayer()
        await player.waitUntilDrained()
        XCTAssertTrue(player.isDrained)
    }

    func testWaitUntilDrainedResumesWhenDrainHappens() async {
        let player = AudioPlayer()
        player.schedulePCMForTest([7], seq: 0)
        let waiter = Task { await player.waitUntilDrained() }
        try? await Task.sleep(for: .milliseconds(50))
        XCTAssertFalse(player.isDrained)
        player.drainForTest()
        // Bounded: resumes via drain, or via the 2 s timeout cancelling the waiter.
        await withTaskGroup(of: Bool.self) { group in
            group.addTask { await waiter.value; return true }
            group.addTask {
                try? await Task.sleep(for: .seconds(2))
                waiter.cancel()
                return false
            }
            let drained = await group.next() ?? false
            group.cancelAll()
            XCTAssertTrue(drained, "waitUntilDrained did not resume")
        }
    }

    func testPlaybackRateIsSettableAndPublishable() {
        let player = AudioPlayer()
        var observed: [Float] = []
        let sub = player.$rate.dropFirst().sink { observed.append($0) }
        _ = sub
        XCTAssertEqual(player.rate, 1.0, "default rate")
        player.rate = 1.5
        XCTAssertEqual(player.rate, 1.5)
        XCTAssertEqual(observed.last, 1.5, "rate publishes for SwiftUI")
        // No crash/throw at the AVAudioUnitTimePitch layer even before start().
        player.rate = 2.0
        XCTAssertEqual(player.rate, 2.0)
    }
}
