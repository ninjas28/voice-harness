import XCTest
@testable import voicekit

/// Exercises the bounded osascript runner with the REAL osascript binary.
/// These scripts need no automation permission ("return "ok"" is pure).
final class OsascriptTests: XCTestCase {
    func testRealOsascriptReturnsOutput() async throws {
        let output = try await Osascript.run("return \"ok\"", timeout: .seconds(10))
        XCTAssertEqual(output, "ok")
    }

    func testDelayTimesOutBounded() async throws {
        // A 30 s script under a 2 s bound must fail with a timeout error and
        // must come back quickly — never hang the session.
        let clock = ContinuousClock()
        let start = clock.now
        do {
            _ = try await Osascript.run("delay 30", timeout: .seconds(2))
            XCTFail("expected timeout error")
        } catch Osascript.Error.timeout {
            let elapsed = clock.now - start
            XCTAssertLessThan(elapsed, .seconds(10), "timeout must be enforced promptly")
        } catch {
            XCTFail("unexpected error: \(error)")
        }
    }

    func testScriptErrorSurfacesAsScriptError() async throws {
        do {
            _ = try await Osascript.run(#"error "boom" number -2700"#, timeout: .seconds(10))
            XCTFail("expected script error")
        } catch let Osascript.Error.script(message) {
            XCTAssertTrue(message.contains("boom"), "stderr text should surface: \(message)")
        } catch {
            XCTFail("unexpected error: \(error)")
        }
    }
}
