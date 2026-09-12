// Isolation test: replicates the app's exact send pattern — 4832-sample
// chunks every ~16 ms, each sent via `Task { await client.sendAudio(...) }`
// spawned from a DispatchQueue callback (mirrors MicCapture.emit).
// Run: swift run vhws --url ws://127.0.0.1:8091/v1/realtime --seconds 3
import Foundation
import voicekit

struct Args {
    var url = "ws://127.0.0.1:8091/v1/realtime"
    var seconds = 3.0
    init() {
        var it = CommandLine.arguments.dropFirst().makeIterator()
        while let a = it.next() {
            if a == "--url", let v = it.next() { url = v }
            if a == "--seconds", let v = it.next(), let d = Double(v) { seconds = d }
        }
    }
}

let args = Args()
let sem = DispatchSemaphore(value: 0)

let transport = URLSessionTransport(url: URL(string: args.url)!)
let client = HarnessClient(transport: transport, model: HarnessSessionModel())

// Detached: top-level main.swift is main-actor isolated; a plain Task would
// inherit it and deadlock against sem.wait() below.
let runTask = Task.detached {
    do {
        try await client.start(deviceId: "vhws-isolation")
        print("session.start sent", terminator: "\n")
        // 4832-sample chunk with real (non-zero) content: 16 ms @ 16 kHz.
        let samples: [Int16] = (0..<4832).map { Int16(3000.0 * sin(Double($0) * 2.0 * .pi * 220.0 / 16000.0)) }
        let payload = pcm16ToBase64(samples)
        print("payload: \(payload.count) base64 chars", terminator: "\n")
        let q = DispatchQueue(label: "test.mic")
        let n = Int(args.seconds * 60)
        for i in 0..<n {
            q.asyncAfter(deadline: .now() + Double(i) / 60.0) {
                Task { await client.sendAudio(base64: payload) }
            }
        }
        let total = n
        try await Task.sleep(nanoseconds: UInt64((args.seconds + 1) * 1_000_000_000))
        print("\(total) audio chunks dispatched", terminator: "\n")
        // Direct transport send as a control: does ANY large frame get through?
        try await transport.send("{\"type\":\"audio.data\",\"pcm\":\"\(String(repeating: "AQID", count: 1600))\"}")
        print("direct control frame sent", terminator: "\n")
        await client.stop()
        print("session.stop sent", terminator: "\n")
    } catch {
        print("FAILED: \(error)", terminator: "\n")
    }
    sem.signal()
}

sem.wait()
runTask.cancel()
