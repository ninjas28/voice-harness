import XCTest
@testable import voicekit

/// Tests for the websocket handshake request construction: the API key must
/// travel in the `Authorization: Bearer <key>` header — never in the URL or
/// its query string (compat: deployed clients store `?token=…` in the URL).
final class HandshakeRequestTests: XCTestCase {
    func testSetsBearerHeaderWhenAPIKeyPresent() {
        let url = URL(string: "ws://192.168.10.93:8090/v1/realtime?token=abcd1234")!
        let request = URLSessionTransport.makeHandshakeRequest(url: url, apiKey: "k3y-abc")
        XCTAssertEqual(request.value(forHTTPHeaderField: "Authorization"), "Bearer k3y-abc")
    }

    func testNoAuthorizationHeaderWhenAPIKeyEmpty() {
        let url = URL(string: "ws://127.0.0.1:8090/v1/realtime")!
        for empty in ["", "   "] {
            let request = URLSessionTransport.makeHandshakeRequest(url: url, apiKey: empty)
            XCTAssertNil(request.value(forHTTPHeaderField: "Authorization"),
                         "empty/whitespace key must omit the header")
        }
    }

    func testURLAndQueryUntouched() {
        let raw = "ws://192.168.10.93:8090/v1/realtime?token=abcd1234"
        let url = URL(string: raw)!
        let request = URLSessionTransport.makeHandshakeRequest(url: url, apiKey: "k3y-abc")
        XCTAssertEqual(request.url?.absoluteString, raw)
        XCTAssertEqual(request.url?.query, "token=abcd1234")
    }

    func testKeyWithSurroundingWhitespaceIsTrimmedIntoHeader() {
        let url = URL(string: "ws://127.0.0.1:8090/v1/realtime")!
        let request = URLSessionTransport.makeHandshakeRequest(url: url, apiKey: "  k3y  ")
        XCTAssertEqual(request.value(forHTTPHeaderField: "Authorization"), "Bearer k3y")
    }
}
