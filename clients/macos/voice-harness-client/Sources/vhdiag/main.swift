// Diagnostic: capture 5 s of mic via MicCapture and report chunk rate + peaks.
// Usage: swift run vhdiag
import Foundation
import AVFoundation
import voicekit

setvbuf(stdout, nil, _IOLBF, 8192) // line-buffered: survives crashes

final class Stats: @unchecked Sendable {
    let lock = NSLock()
    var chunks = 0
    var samplesTotal = 0
    var lastPeak: Int16 = 0
    var peakGlobal: Int16 = 0
    func record(_ s: [Int16]) {
        var peak: Int16 = 0
        for v in s { if abs(Int32(v)) > abs(Int32(peak)) { peak = v } }
        lock.lock()
        chunks += 1
        samplesTotal += s.count
        lastPeak = peak
        if abs(Int32(peak)) > abs(Int32(peakGlobal)) { peakGlobal = peak }
        lock.unlock()
    }
    func snapshot() -> (Int, Int, Int16, Int16) {
        lock.lock(); defer { lock.unlock() }
        return (chunks, samplesTotal, lastPeak, peakGlobal)
    }
}

// 1. Ask for mic permission BEFORE creating any audio objects — creating
//    AVAudioEngine pre-grant and starting post-grant crashes on some setups.
let granted: Bool
if #available(macOS 14.0, *) {
    let sem = DispatchSemaphore(value: 0)
    var result = false
    AVAudioApplication.requestRecordPermission { ok in
        result = ok
        sem.signal()
    }
    sem.wait()
    granted = result
} else {
    granted = true // pre-14 fallback: just try
}
print("mic permission granted: \(granted)")
guard granted else { print("cannot proceed without mic"); exit(2) }

// 2. Now safe to inspect/attach hardware.
let stats = Stats()
let mic = MicCapture()
print("native input format: \(AVAudioEngine().inputNode.inputFormat(forBus: 0))")

do {
    try mic.start { event in
        guard case .chunk16k(let b64) = event else { return }
        stats.record(base64ToPCM16(b64))
    }
} catch {
    print("mic start failed: \(error)")
    exit(1)
}

print("capturing 5 seconds — please SPEAK NOW…")
for i in 1...5 {
    Thread.sleep(forTimeInterval: 1)
    let (c, s, lp, pg) = stats.snapshot()
    print("  t+\(i)s: chunks=\(c) samples=\(s) lastChunkPeak=\(lp) peakSoFar=\(pg)")
}
mic.stop()
let (_, _, _, pg) = stats.snapshot()
print("done: global peak \(pg) (≈32767 is loud speech, <500 is silence)")
