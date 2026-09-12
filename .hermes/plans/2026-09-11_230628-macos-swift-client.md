# Voice Harness — macOS Menu Bar Client (Implementation Plan)

> **For Hermes:** Use subagent-driven-development to implement this plan task-by-task.

**Goal:** A lightweight Swift/SwiftUI macOS menu bar app that streams mic audio to the harness's `WS /v1/realtime`, shows the live transcript as you speak, an animation while thinking, and plays the response audio as it arrives.

**Architecture:** One SwiftUI app (menu-bar only, no dock/window) using `URLSessionWebSocketTask` for the wire protocol and AVAudioEngine for mic capture and playback. State machine (`idle → listening → thinking → speaking`) drives one compact panel view; each server message maps 1:1 onto an observable-model update. `AVAudioConverter` feeds the harness's 16 kHz mono PCM16 contract; playback streams `audio.chunk` payloads as scheduled PCM buffers so audio starts before the turn completes.

**Tech Stack:** Swift 6.3, SwiftUI, AVFoundation (AVAudioEngine, AVAudioConverter, AVAudioPCMBuffer), URLSessionWebSocketTask, XCTest. No third-party dependencies.

---

## Verified context (do not re-derive)

- Repo: `/Users/trevor/Documents/Github/voice-harness`. Existing: 4-crate Rust workspace (server complete, 89 tests green, verified live end-to-end on 2026-09-11: text turn, audio round-trip, WS realtime). This plan adds a client — **do not modify server code**.
- **Wire protocol** (`crates/harness-server/src/ws.rs`, README.md): JSON text frames over WS.
  - Send: `{"type":"session.start","device_id":..., "sample_rate":16000}` → `{"type":"audio.data","pcm":"<base64 PCM16 LE mono>"}` → optional `{"type":"speech.end"}` → optional `{"type":"session.stop"}`.
  - Receive: `{"type":"state","state":"listening|speech|thinking|speaking"}`, `{"type":"transcript","text":...}`, `{"type":"response.text.delta","text":...}`, `{"type":"response.text","text":...}`, `{"type":"audio.chunk","pcm":"<base64 PCM16>","seq":N}`, `{"type":"turn.completed"}`, `{"type":"error","code":...,"message":...}`.
- **Audio contract:** 16 kHz, mono, PCM16 little-endian, both directions. Server does VAD endpointing (700 ms silence → turn fires).
- **Server:** runs at `127.0.0.1:8090` (config `server.bind`); WS endpoint `ws://127.0.0.1:8090/v1/realtime`; `server.api_keys` empty in the user's config → **no auth token required today** (send none; keep a `--token`-style setting for later).
- **Protocol edge cases** (observed in live tests): server emits `state: listening` again before `thinking` (VAD endpoint); `audio.chunk` frames arrive while `response.text.delta` still streaming; seq starts at 0 and is strictly increasing.
- Dev environment: Xcode 16+ with Swift 6.3.3 installed at `/Applications/Xcode.app`, target arm64 macOS 26. Rust binary must be running for integration tests (task T8 handles that).
- **Swift 6 concurrency:** strict by default — the plan's design keeps audio/WS on a single `@unchecked Sendable` engine class driven from one serial queue-ish actor pattern; any compile error of the form "actor-isolated … cannot be sent" should be resolved by following the ownership structure below, not by sprinkling `nonisolated(unsafe)`.

## Assumptions

- v1 targets the user's Mac only (menu bar, Apple Silicon). No iOS/catalyst, no localization, no Settings window — config is a small JSON file + a panel text field for the server URL.
- Push-to-talk is **out** (server VAD decides turn end, matching ESP32 behavior). Mic monitoring (hearing yourself) is off.
- App is un-sandboxed (direct dev build); if the user later distributes it, sandbox + mic entitlement work is a follow-up.
- TTS output is always 16 kHz mono PCM16 (verified live; server normalizes).

## Wire→UI mapping (single source of truth for all tasks)

| Server frame | Client action |
|---|---|
| `state` | `model.phase = state` (drives transcript banner + spinner/animation) |
| `transcript` | `model.transcript = text` (shown in "you said" section) |
| `response.text.delta` | append to `model.responseText` (live caption) |
| `response.text` | `model.responseText = text` (authoritative final) |
| `audio.chunk` | schedule PCM playback via `AudioPlayer.scheduleChunk(base64:)` |
| `turn.completed` | if nothing is playing/queued: `phase = listening` else wait for drain → `listening` |
| `error` | `model.errorMessage = message`, `phase = idle` |

## Layout

```
clients/macos/voice-harness-client/
├── Package.swift
├── Sources/
│   ├── voicekit/                      # logic lib (testable, no SwiftUI)
│   │   ├── HarnessProtocol.swift      # ClientMessage/ServerMessage Codable
│   │   ├── HarnessClient.swift        # WS session: connect/send/receive pump
│   │   ├── HarnessSessionModel.swift  # ObservableObject state machine
│   │   ├── MicCapture.swift           # AVAudioEngine tap → 16k PCM16 → base64
│   │   └── AudioPlayer.swift          # chunk scheduling + drain detection
│   ├── VoiceHarnessApp.swift          # @main, MenuBarExtra, panel wiring
│   ├── PanelView.swift                # transcript, thinking animation, response
│   └── StatusIcon.swift               # SF Symbol by phase (record.fill etc.)
└── Tests/voicekitTests/
    ├── HarnessProtocolTests.swift
    ├── HarnessClientTests.swift
    ├── HarnessSessionModelTests.swift
    └── AudioPlayerTests.swift
```

All paths below are relative to `/Users/trevor/Documents/Github/voice-harness/`.

---

## Tasks

### Task T1: Package scaffold

**Files:** Create `clients/macos/voice-harness-client/Package.swift`, `Sources/voicekit/HarnessProtocol.swift` (empty types file), `Sources/VoiceHarnessApp.swift` (minimal `@main`), `Tests/voicekitTests/HarnessProtocolTests.swift` (empty).

Steps:
1. `mkdir -p clients/macos/voice-harness-client/{Sources/voicekit,Tests/voicekitTests}`.
2. Write `Package.swift`:

```swift
// swift-tools-version:6.0
import PackageDescription

let package = Package(
    name: "voice-harness-client",
    platforms: [.macOS(.v13)],
    targets: [
        .target(name: "voicekit"),
        .executableTarget(
            name: "VoiceHarnessClient",
            dependencies: ["voicekit"]
        ),
        .testTarget(name: "voicekitTests", dependencies: ["voicekit"])
    ]
)
```

3. `cd clients/macos/voice-harness-client && swift build 2>&1 | tail -3` → `Build complete!` (create empty stub files first so targets are non-empty).
4. Commit: `git add clients/macos && git commit -m "chore(client): swift package scaffold"`.

### Task T2: Protocol types (TDD)

**Files:** `Sources/voicekit/HarnessProtocol.swift`, `Tests/voicekitTests/HarnessProtocolTests.swift`.

**Step 1: failing test** — `Tests/voicekitTests/HarnessProtocolTests.swift`:

```swift
import XCTest
@testable import voicekit

final class HarnessProtocolTests: XCTestCase {
    func testDecodesEveryServerFrame() throws {
        let cases: [(String, ServerMessage)] = [
            (#"{"type":"state","state":"thinking"}"#, .state(.thinking)),
            (#"{"type":"transcript","text":"hi"}"#, .transcript("hi")),
            (#"{"type":"response.text.delta","text":"He"}"#, .responseTextDelta("He")),
            (#"{"type":"response.text","text":"Hello"}"#, .responseText("Hello")),
            (#"{"type":"audio.chunk","pcm":"QUJD","seq":3}"#, .audioChunk(pcm: "QUJD", seq: 3)),
            (#"{"type":"turn.completed"}"#, .turnCompleted),
            (#"{"type":"error","code":"bad","message":"nope"}"#, .error(code: "bad", message: "nope")),
        ]
        for (json, expected) in cases {
            XCTAssertEqual(try ServerMessage.decode(json), expected, "failed for \(json)")
        }
    }

    func testEncodesClientFrames() {
        XCTAssertEqual(ClientMessage.sessionStart(deviceId: "kitchen", sampleRate: 16000).encode(),
                       #"{"type":"session.start","device_id":"kitchen","sample_rate":16000}"#)
        XCTAssertEqual(ClientMessage.audioData(pcm: "QUJD").encode(),
                       #"{"type":"audio.data","pcm":"QUJD"}"#)
        XCTAssertEqual(ClientMessage.speechEnd.encode(), #"{"type":"speech.end"}"#)
    }
}
```

**Step 2:** `swift test --filter HarnessProtocolTests` → compile error (`ServerMessage` undefined) = RED confirmed.

**Step 3: implement** — `Sources/voicekit/HarnessProtocol.swift`:

```swift
import Foundation

public enum SessionPhase: String, Codable, Sendable {
    case idle // client-only
    case listening, speech, thinking, speaking
}

public enum ServerMessage: Equatable, Sendable {
    case state(SessionPhase)
    case transcript(String)
    case responseTextDelta(String)
    case responseText(String)
    case audioChunk(pcm: String, seq: Int)
    case turnCompleted
    case error(code: String, message: String)

    public static func decode(_ json: String) throws -> ServerMessage {
        try JSONDecoder().decode(Wire.self, from: Data(json.utf8)).value
    }

    struct Wire: Decodable {
        let type: String
        let state: SessionPhase?
        let text: String?
        let pcm: String?
        let seq: Int?
        let code: String?
        let message: String?
        var value: ServerMessage {
            switch type {
            case "state": .state(state ?? .idle)
            case "transcript": .transcript(text ?? "")
            case "response.text.delta": .responseTextDelta(text ?? "")
            case "response.text": .responseText(text ?? "")
            case "audio.chunk": .audioChunk(pcm: pcm ?? "", seq: seq ?? 0)
            case "turn.completed": .turnCompleted
            case "error": .error(code: code ?? "", message: message ?? "")
            default: .error(code: "unknown_type", message: type)
            }
        }
    }
}

public enum ClientMessage: Equatable, Sendable {
    case sessionStart(deviceId: String?, sampleRate: Int)
    case audioData(pcm: String)
    case speechEnd
    case sessionStop

    public func encode() -> String {
        var o = [String]()
        switch self {
        case .sessionStart(let d, let r):
            o = ["{\"type\":\"session.start\""]
            if let d { o.append(",\"device_id\":\"\(d)\"") }
            o.append(",\"sample_rate\":\(r)}")
        case .audioData(let p): o = ["{\"type\":\"audio.data\",\"pcm\":\"\(p)\"}"]
        case .speechEnd: o = ["{\"type\":\"speech.end\"}"]
        case .sessionStop: o = ["{\"type\":\"session.stop\"}"]
        }
        return o.joined()
    }
}
```

**Step 4:** `swift test --filter HarnessProtocolTests` → all pass. Commit: `feat(client): wire protocol types with round-trip tests`.

### Task T3: Session model state machine (TDD)

**Files:** `Sources/voicekit/HarnessSessionModel.swift`, `Tests/voicekitTests/HarnessSessionModelTests.swift`.

**Step 1: failing test**:

```swift
import XCTest
@testable import voicekit

@MainActor
final class HarnessSessionModelTests: XCTestCase {
    func testServerFramesDriveModel() async {
        let model = HarnessSessionModel()
        model.apply(.state(.listening))
        XCTAssertEqual(model.phase, .listening)
        model.apply(.state(.speech))
        model.apply(.transcript("what time is it"))
        XCTAssertEqual(model.transcript, "what time is it")
        model.apply(.state(.thinking))
        model.apply(.responseTextDelta("It's "))
        model.apply(.responseTextDelta("2:37."))
        XCTAssertEqual(model.responseText, "It's 2:37.")
        model.apply(.audioChunk(pcm: "QUJD", seq: 0))
        XCTAssertEqual(model.phase, .speaking)
        model.apply(.responseText("It's 2:37."))
        model.apply(.turnCompleted)
        // audio not drained yet → still speaking
        XCTAssertEqual(model.phase, .speaking)
    }

    func testErrorFrameSurfacesAndResets() {
        let model = HarnessSessionModel()
        model.apply(.error(code: "x", message: "boom"))
        XCTAssertEqual(model.errorMessage, "boom")
        XCTAssertEqual(model.phase, .idle)
    }

    func testTurnCompletedAfterDrainReturnsToListening() async {
        let model = HarnessSessionModel()
        model.apply(.audioChunk(pcm: "QUJD", seq: 0))
        model.audioDidFinish()   // player drain callback
        model.apply(.turnCompleted)
        XCTAssertEqual(model.phase, .listening)
    }
}
```

**Step 2:** RED (compile error).

**Step 3: implement** — `Sources/voicekit/HarnessSessionModel.swift`:

```swift
import Foundation
import Combine

@MainActor
public final class HarnessSessionModel: ObservableObject {
    @Published public private(set) var phase: SessionPhase = .idle
    @Published public private(set) var transcript = ""
    @Published public private(set) var responseText = ""
    @Published public private(set) var errorMessage: String?
    /// True while the player still has scheduled-but-unplayed audio.
    @Published public private(set) var audioPending = false

    public init() {}

    public func apply(_ msg: ServerMessage) {
        switch msg {
        case .state(let s):
            phase = s
            if s == .thinking { responseText = "" }
        case .transcript(let t): transcript = t
        case .responseTextDelta(let d): responseText += d
        case .responseText(let t): responseText = t
        case .audioChunk:
            audioPending = true
            if phase != .speaking { phase = .speaking }
        case .turnCompleted:
            if !audioPending { phase = .listening }
        case .error(let _, let m):
            errorMessage = m
            phase = .idle
        }
    }

    public func audioDidFinish() { audioPending = false }
    public func reset() { phase = .idle; transcript = ""; responseText = ""; errorMessage = nil; audioPending = false }
}
```

**Step 4:** `swift test` → pass. Commit: `feat(client): session model state machine`.

### Task T4: Audio formats + base64 PCM helpers (TDD)

**Files:** `Sources/voicekit/AudioSupport.swift`, `Tests/voicekitTests/AudioSupportTests.swift`.

**Step 1: failing test** (pure-data, no AVFoundation):

```swift
import XCTest
@testable import voicekit

final class AudioSupportTests: XCTestCase {
    func testPCM16RoundTripAndDownsample() {
        // 4:1 simple decimation 48k → 12k; verify value positions survive.
        let src: [Int16] = [0, 100, 200, 300, 400, 500, 600, 700]
        let out = decimateByAveraging(src, factor: 4)
        XCTAssertEqual(out, [50, 250, 450, 650])
        let bytes = pcm16ToBase64(out)
        let back = base64ToPCM16(bytes)
        XCTAssertEqual(back, out)
    }

    func testBase64ToPCM16HandlesOddByteCount() {
        XCTAssertEqual(base64ToPCM16(pcm16ToBase64([-3, 7, -32000])), [-3, 7, -32000])
    }
}
```

**Step 2:** RED.

**Step 3: implement** — `Sources/voicekit/AudioSupport.swift`:

```swift
import Foundation

public func decimateByAveraging(_ x: [Int16], factor: Int) -> [Int16] {
    guard factor > 1 else { return x }
    return stride(from: 0, to: x.count - (x.count % factor), by: factor).map { i in
        var sum = 0
        for j in i..<(i + factor) { sum += Int(x[j]) }
        return Int16(clamping: sum / factor)
    }
}

public func pcm16ToBase64(_ samples: [Int16]) -> String {
    samples.withUnsafeBufferPointer { buf in
        Data(buffer: buf).base64EncodedString()
    }
}

public func base64ToPCM16(_ b64: String) -> [Int16] {
    guard let data = Data(base64Encoded: b64) else { return [] }
    return data.withUnsafeBytes { raw in
        stride(from: 0, to: raw.count - (raw.count % 2), by: 2).map {
            Int16(littleEndian: raw.loadUnaligned(fromByteOffset: $0, as: Int16.self))
        }
    }
}
```

**Step 4:** `swift test` → pass. Commit: `feat(client): PCM16/base64 audio helpers`.

### Task T5: Mic capture (AVAudioEngine → 16k PCM16 → audio.data)

**Files:** `Sources/voicekit/MicCapture.swift`.

Mic code is untestable in CI (no mic in test host) — write it directly with a compile+manual-verify gate (T8). Keep the conversion decision simple: request the engine input node's native format, then **average-decimate** to 16 kHz when the native rate is a multiple of it (48000 → factor 3, 44100 → use AVAudioConverter; see tradeoffs). v1 uses AVAudioConverter for all rates — correctness over cleverness:

```swift
import AVFoundation
import Foundation

/// Captures mic audio and forwards 16 kHz mono PCM16 (base64) to a sink.
public final class MicCapture {
    public enum Event { case level(Float), chunk16k(base64: String) }
    public private(set) var isRunning = false

    private let engine = AVAudioEngine()
    private let queue = DispatchQueue(label: "vh.mic")
    private var converter: AVAudioConverter?
    private var sink: ((Event) -> Void)?

    public init() {}

    public func start(sink: @escaping (Event) -> Void) throws {
        guard !isRunning else { return }
        self.sink = sink
        let input = engine.inputNode
        let native = input.inputFormat(forBus: 0)
        let target = AVAudioFormat(commonFormat: .pcmFormatInt16, sampleRate: 16000,
                                   channels: 1, interleaved: true)!
        converter = AVAudioConverter(from: native, to: target)

        input.installTap(onBus: 0, bufferSize: 4800, format: native) { [weak self] buffer, _ in
            self?.queue.async { self?.convert(buffer) }
        }
        engine.prepare()
        try engine.start()
        isRunning = true
    }

    public func stop() {
        guard isRunning else { return }
        engine.inputNode.removeTap(onBus: 0)
        engine.stop()
        isRunning = false
    }

    private func convert(_ buffer: AVAudioPCMBuffer) {
        guard let converter else { return }
        let ratio = 16000.0 / buffer.format.sampleRate
        let cap = AVAudioFrameCount(Double(buffer.frameLength) * ratio) + 32
        guard let out = AVAudioPCMBuffer(pcmFormat: converter.outputFormat, frameCapacity: cap) else { return }
        var fed = false
        var err: NSError?
        let status = converter.convert(to: out, error: &err) { _, outStatus in
            if fed { outStatus.pointee = .noDataNow; return nil }
            fed = true
            outStatus.pointee = .haveData
            return buffer
        }
        guard err == nil, status != .error, let int16 = out.int16ChannelData else { return }
        let samples = Array(UnsafeBufferPointer(start: int16[0], count: Int(out.frameLength)))
        sink?(.chunk16k(base64: pcm16ToBase64(samples)))
    }
}
```

Verify: `swift build` → `Build complete!`. Commit: `feat(client): mic capture with AVAudioConverter to 16k PCM16`.

### Task T6: Audio player for audio.chunk (TDD for drain logic, manual for sound)

**Files:** `Sources/voicekit/AudioPlayer.swift`, `Tests/voicekitTests/AudioPlayerTests.swift`.

**Step 1: failing test** (drain accounting only — AVFoundation parts are thin wrappers):

```swift
import XCTest
@testable import voicekit

final class AudioPlayerTests: XCTestCase {
    func testDrainCountsScheduledChunks() async {
        let player = AudioPlayer()
        await player.schedulePCMForTest([1, 2, 3], seq: 0)
        await player.schedulePCMForTest([4, 5], seq: 1)
        try? await Task.sleep(for: .milliseconds(50))
        await MainActor.run { XCTAssertFalse(player.isDrained) }
        await player.drainForTest()
        await MainActor.run { XCTAssertTrue(player.isDrained) }
    }
}
```

**Step 2:** RED.

**Step 3: implement** — `Sources/voicekit/AudioPlayer.swift`. Core idea: one 16 kHz `AVAudioEngine` output or `AVAudioPlayerNode` scheduling Int16 buffers; completion callbacks decrement `scheduledCount`; `isDrained = scheduledCount == 0 && !isPlaying`. Test hooks `schedulePCMForTest/drainForTest` manipulate the counter without hardware:

```swift
import AVFoundation
import Foundation

public final class AudioPlayer {
    private let engine = AVAudioEngine()
    private let node = AVAudioPlayerNode()
    private let format = AVAudioFormat(commonFormat: .pcmFormatInt16, sampleRate: 16000,
                                       channels: 1, interleaved: true)!
    private let queue = DispatchQueue(label: "vh.play")
    private var scheduled = 0
    private(set) var isDrained: Bool { didSet { drainContinuation?.resume() ; drainContinuation = nil } }

    private var drainContinuation: CheckedContinuation<Void, Never>?

    public init() {
        engine.attach(node)
        engine.connect(node, to: engine.mainMixerNode, format: format)
        isDrained = true
    }

    public func start() throws { try engine.start(); node.play() }

    /// Schedules one audio.chunk payload for playback.
    public func scheduleChunk(base64: String) {
        let samples = base64ToPCM16(base64)
        guard !samples.isEmpty else { return }
        queue.async { self._schedule(samples) }
    }

    private func _schedule(_ samples: [Int16]) {
        guard let buf = samples.withUnsafeBufferPointer({ p in
            AVAudioPCMBuffer(pcmFormat: format, frameCapacity: AVAudioFrameCount(p.count))
        }) else { return }
        buf.frameLength = AVAudioFrameCount(samples.count)
        samples.withUnsafeBufferPointer { p in
            buf.int16ChannelData!.pointee.update(from: p.baseAddress!, count: p.count)
        }
        scheduled += 1
        node.scheduleBuffer(buf) { [weak self] _ in
            self?.queue.async {
                self?.scheduled -= 1
                if self?.scheduled == 0 { self?.isDrained = true }
            }
        }
        if scheduled == 1 { isDrained = false }
    }

    /// Resumes when all scheduled buffers finished playing.
    public func waitUntilDrained() async {
        if isDrained { return }
        await withCheckedContinuation { c in drainContinuation = c }
    }

    // MARK: Test hooks (no hardware)
    func schedulePCMForTest(_ samples: [Int16], seq: Int) async {
        await MainActor.run { _ = seq }
        queue.sync { _schedule(samples) }
    }
    func drainForTest() async {
        queue.sync { scheduled = 0; isDrained = true }
    }
}
```

**Step 4:** `swift test` → pass. Commit: `feat(client): streaming audio.chunk playback with drain detection`.

### Task T7: HarnessClient WS session (TDD against a local mock server)

**Files:** `Sources/voicekit/HarnessClient.swift`, `Tests/voicekitTests/HarnessClientTests.swift`.

**Step 1: failing test** — spins a tiny in-test WS echo server so no Rust binary is needed. SwiftNIO is overkill; use `Network.framework` hand-rolled WS upgrade (30 lines) — or simpler: test `HarnessClient` against **the real harness binary** is reserved for T8; here test only the pump logic with a stubbed transport:

```swift
import XCTest
@testable import voicekit

final class HarnessClientTests: XCTestCase {
    actor StubTransport: Transport {
        var sent: [String] = []
        var onReceive: ((String) -> Void)?
        func send(_ text: String) { sent.append(text) }
        func simulateIncoming(_ text: String) { onReceive?(text) }
    }

    func testPumpMapsIncomingFramesToModelAndSendsSessionStart() async throws {
        let stub = StubTransport()
        let model = await HarnessSessionModel()
        let client = HarnessClient(transport: stub, model: model)
        await client.start(deviceId: "test")
        XCTAssertEqual(stub.sent.first, #"{"type":"session.start","device_id":"test","sample_rate":16000}"#)
        await stub.simulateIncoming(#"{"type":"transcript","text":"hi"}"#)
        let t = await model.transcript
        XCTAssertEqual(t, "hi")
        await client.stop()
    }
}
```

**Step 2:** RED.

**Step 3: implement** — `Sources/voicekit/HarnessClient.swift` with a `Transport` protocol so `URLSessionWebSocketTask` plugs in for production:

```swift
import Foundation

public protocol Transport: Actor {
    func send(_ text: String) async
}

public actor URLSessionTransport: Transport {
    private let task: URLSessionWebSocketTask
    public init(url: URL) {
        task = URLSession.shared.webSocketTask(with: url)
    }
    public func startReceiving(onReceive: @escaping @Sendable (String) -> Void) {
        task.resume()
        pump(onReceive)
    }
    public func send(_ text: String) async {
        try? await task.send(.string(text))
    }
    public func close() async { task.cancel(with: .goingAway, reason: nil) }

    private func pump(_ onReceive: @escaping @Sendable (String) -> Void) {
        task.receive { [weak self] result in
            if case .success(.string(let s)) = result { onReceive(s) }
            self?.pump(onReceive)
        }
    }
}

@MainActor
public final class HarnessClient {
    private let transport: any Transport
    private let model: HarnessSessionModel
    public var onChunk: ((String, Int) -> Void)?   // (base64 pcm, seq)

    public init(transport: any Transport, model: HarnessSessionModel) {
        self.transport = transport
        self.model = model
    }

    public func start(deviceId: String?) async {
        await transport.send(ClientMessage.sessionStart(deviceId: deviceId, sampleRate: 16000).encode())
        Task { await receiveLoop() }
    }

    public func sendAudio(base64: String) async { await transport.send(ClientMessage.audioData(pcm: base64).encode()) }
    public func sendSpeechEnd() async { await transport.send(ClientMessage.speechEnd.encode()) }

    public func stop() async { await transport.send(ClientMessage.sessionStop.encode()) }

    private func receiveLoop() async {
        // pump via transport-specific receive — see URLSessionTransport.startReceiving
    }
}
```

> Note for implementer: the stub test uses `StubTransport.simulateIncoming`; production uses `URLSessionTransport.startReceiving { [weak model] text in ... }` wiring `ServerMessage.decode → model.apply / onChunk`. Resolve this asymmetry inside `HarnessClient` by making the transport expose a receive closure (add `func setReceiveHandler(_:)` to the protocol) — do not duplicate pumps.

**Step 4:** `swift test` → pass. Commit: `feat(client): WS session client with injectable transport`.

### Task T8: Wire the app together (no new tests — integration verified in T9)

**Files:** `Sources/VoiceHarnessApp.swift`, `Sources/PanelView.swift`, `Sources/StatusIcon.swift`.

`VoiceHarnessApp.swift`:

```swift
import SwiftUI
import voicekit

@main
struct VoiceHarnessApp: App {
    @State private var model = HarnessSessionModel()
    @State private var running = false

    var body: some Scene {
        MenuBarExtra {
            PanelView(model: model, running: $running)
                .frame(width: 340)
        } label: {
            StatusIcon(phase: model.phase)
        }
        .menuBarExtraStyle(.window)
    }
}
```

`PanelView.swift` — the whole UI is one panel:

```swift
import SwiftUI
import voicekit

struct PanelView: View {
    let model: HarnessSessionModel
    @Binding var running: Bool

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack {
                Circle().fill(phaseColor).frame(width: 10, height: 10)
                Text(phaseLabel).font(.headline)
                Spacer()
                Button(running ? "Stop" : "Start") { Task { await toggle() } }
                    .buttonStyle(.borderedProminent)
            }
            if let err = model.errorMessage {
                Text(err).foregroundStyle(.red).font(.caption)
            }
            if !model.transcript.isEmpty {
                VStack(alignment: .leading) {
                    Text("You said").font(.caption).foregroundStyle(.secondary)
                    Text(model.transcript).font(.body)
                }
            }
            HStack {
                Text("Assistant").font(.caption).foregroundStyle(.secondary)
                if model.phase == .thinking { ThinkingDots() }
            }
            Text(model.responseText.isEmpty && model.phase == .thinking
                  ? "…" : model.responseText)
                .font(.body).frame(minHeight: 40, alignment: .topLeading)
        }
        .padding(14)
    }

    private func toggle() async {
        if running {
            await AppRuntime.shared.stop()
            running = false
        } else {
            try? await AppRuntime.shared.start(model: model, url: AppSettings.serverURL)
            running = true
        }
    }

    private var phaseColor: Color {
        switch model.phase {
        case .idle: .gray; case .listening: .green; case .speech: .orange
        case .thinking: .yellow; case .speaking: .blue
        }
    }
    private var phaseLabel: String {
        switch model.phase {
        case .idle: "Idle"; case .listening: "Listening…"; case .speech: "Hearing you…"
        case .thinking: "Thinking…"; case .speaking: "Speaking"
        }
    }
}

struct ThinkingDots: View {
    @State private var phase = 0
    var body: some View {
        HStack(spacing: 3) {
            ForEach(0..<3) { i in
                Circle().frame(width: 5, height: 5)
                    .opacity(phase == i ? 1 : 0.3)
            }
        }
        .onAppear { withAnimation(.easeInOut(duration: 0.5).repeatForever()) { phase = (phase + 1) % 3 } }
    }
}
```

`StatusIcon.swift`:

```swift
import SwiftUI
import voicekit

struct StatusIcon: View {
    let phase: SessionPhase
    var body: some View {
        switch phase {
        case .idle: Image(systemName: "waveform")
        case .listening: Image(systemName: "mic")
        case .speech: Image(systemName: "waveform.circle.fill")
        case .thinking: Image(systemName: "brain") // fallback "ellipsis.bubble"
        case .speaking: Image(systemName: "speaker.wave.2.fill")
        }
    }
}
```

Also create `Sources/AppRuntime.swift` + `Sources/AppSettings.swift` (small singletons: start/stop owns MicCapture + HarnessClient + AudioPlayer; settings persist server URL + device name in `UserDefaults`):

```swift
import Foundation
import AVFoundation
import voicekit

@MainActor
final class AppRuntime: ObservableObject {
    static let shared = AppRuntime()
    private var mic: MicCapture?
    private var client: HarnessClient?
    private var player: AudioPlayer?

    func start(model: HarnessSessionModel, url: URL) async throws {
        let player = AudioPlayer(); self.player = player
        try player.start()
        let transport = URLSessionTransport(url: url)
        let client = HarnessClient(transport: transport, model: model)
        client.onChunk = { base64, _ in player.scheduleChunk(base64: base64) }
        self.client = client
        await client.start(deviceId: Host.current().localizedName ?? "mac")
        let mic = MicCapture()
        try mic.start { event in
            if case .chunk16k(let b64) = event {
                Task { await client.sendAudio(base64: b64) }
            }
        }
        self.mic = mic
        // drain → listening transition
        Task { await drainWatcher(model: model, player: player) }
    }

    private func drainWatcher(model: HarnessSessionModel, player: AudioPlayer) async {
        while !Task.isCancelled {
            if !player.isDrained { await player.waitUntilDrained(); model.audioDidFinish() }
            try? await Task.sleep(for: .milliseconds(100))
        }
    }

    func stop() async {
        mic?.stop(); await client?.stop()
        mic = nil; client = nil; player = nil
    }
}

enum AppSettings {
    static var serverURL: URL {
        let s = UserDefaults.standard.string(forKey: "server_url") ?? "ws://127.0.0.1:8090/v1/realtime"
        return URL(string: s)!
    }
}
```

Verify: `swift build` → `Build complete!`; `swift test` still green (UI code not covered). Commit: `feat(client): menu bar app wiring — mic, WS, playback, panel UI`.

### Task T9: Live integration verification (manual, server required)

**Precondition:** `cargo run -p harness-server -- config/voice-harness.toml` running (or `target/release/harness-server config/voice-harness.toml` from repo root).

Steps (all in `clients/macos/voice-harness-client`):
1. `swift build && swift run VoiceHarnessClient` — menu bar icon appears.
2. Click icon → panel → **Start**. macOS prompts for mic permission → Allow.
3. Speak: "What time is it right now?" → **Expected:** phase shows *Hearing you…* while speaking, transcript appears within ~1 s of stopping, then *Thinking…* with animated dots, then *Speaking* while audio plays "It's 2:37…"-style answer; phase returns to *Listening…*.
4. Verify live captioning: response text builds word-by-word while audio plays (deltas + chunks interleave — this is the harness's streaming behavior).
5. Click **Stop** — mic indicator in menu bar disappears.
6. `git add -A && git commit -m "chore(client): verified live against harness server"` (or no commit if nothing changed).

## Tests / validation summary

- Unit (SwiftPM): protocol round-trips (T2), model transitions incl. drain semantics (T3), PCM/base64 + decimation (T4), player drain accounting (T6), client pump mapping (T7). `swift test` must be fully green before every commit.
- Manual: mic capture conversion (T5), live end-to-end (T9) — the harness's own verified WS behavior is the oracle.
- Not tested: SwiftUI rendering (visual), URLSessionTransport against a real socket in unit tests (covered live in T9).

## Risks / tradeoffs / open questions

- **AVAudioConverter output pacing:** converter may return zero-frame buffers early in a stream (its internal buffer fills); the tap must tolerate `frameLength == 0` (skip sending empty chunks — filter in `convert`). If choppy, switch native-rate taps to average-decimation for the 48000→16000 case (factor 3, exact) and keep the converter only for 44100.
- **Swift 6 strict concurrency:** `MicCapture`'s tap callback runs on a nonisolated dispatch queue; the plan routes through an explicit `queue` + `Task { }` hop into actor land. If the compiler fights the `@MainActor` `HarnessClient` receiving from an actor, prefer making `HarnessClient` a plain class with all entry points `async` (model already isolates UI state).
- **Mic permission prompt on first Start** — expected UX; nothing to code (macOS handles it), but the Info.plist-less SwiftPM executable gets mic access only because it's a locally-run debug binary; if it's silently denied, add a proper app bundle + `NSMicrophoneUsageDescription` (follow-up, not v1).
- **`MenuBarExtra` requires macOS 13+** — fine (targeting 13+; user runs 26).
- **No barge-in:** while `speaking`, mic stays hot and server VAD may pick up the harness's own voice (echo). The Rust server does not cancel; if this annoys, v1.1 adds a client-side "pause mic while `audioPending`" toggle — deliberately deferred (YAGNI).
- **Open question for the user:** should the panel show session history across turns (last N transcripts)? v1 shows only the current turn; history is cheap to add later if wanted.

## Execution handoff

Plan complete and saved. Ready to execute with subagent-driven-development — one subagent per task with spec-compliance then code-quality review, T1→T9 sequential (each builds on the last), `swift test` as the per-task gate.
