import XCTest
@testable import voicekit

/// Exercises the hand-rolled typedstream subset decoder against byte-for-byte
/// fixtures of real Messages `attributedBody` blobs.
final class TypedStreamDecoderTests: XCTestCase {
    // MARK: - Fixture builders

    /// Every Messages typedstream payload starts with the literal "streamtyped"
    /// marker, a \u{13} separator, and the NSKeyedArchiver archive header.
    private var archiveHeader: [UInt8] {
        var bytes = Array("streamtyped".utf8)
        bytes.append(0x13)
        bytes += Array("bplist00".utf8)
        bytes += Array("_\u{0010}NSKeyedArchiver".utf8)
        bytes += [0x01, 0x74, 0x00]
        return bytes
    }

    private func stream(_ objectBytes: [UInt8]) -> Data {
        Data(archiveHeader + objectBytes)
    }

    private func utf8(_ s: String) -> [UInt8] { Array(s.utf8) }

    /// The standard object preamble: objectStart + class name "NSMutableString"
    /// + aRoot shift bytes, before the text selector.
    private func stringObjectPreamble() -> [UInt8] {
        var bytes: [UInt8] = [0x81, 0xC0]
        bytes += [0x10, 0x0F]
        bytes += utf8("NSMutableString")
        bytes += [0x81, 0xC0]
        bytes += [0x81, 0x81, 0x86]
        return bytes
    }

    /// A minimal text-body payload: preamble + text selector + tagged length +
    /// body bytes + objectEnd. Mirrors real "Noter test"-style blobs.
    private func textPayload(_ body: String) -> Data {
        var bytes = stringObjectPreamble()
        bytes.append(0x92)
        let bodyBytes = utf8(body)
        bytes += [0x10, UInt8(bodyBytes.count)]
        bytes += bodyBytes
        bytes.append(0x81)
        bytes.append(0xC0)
        return stream(bytes)
    }

    // MARK: Plain text payloads

    func testPlainTextMessageRoundTrips() {
        XCTAssertEqual(TypedStreamDecoder.decode(textPayload("Noter test")), "Noter test")
    }

    func testEmptyStringPayload() {
        XCTAssertEqual(TypedStreamDecoder.decode(textPayload("")), "")
    }

    func testMultiByteUTF8Body() {
        let body = "Réunion café ✓"
        let bodyBytes = utf8(body)
        XCTAssertTrue(bodyBytes.count > body.count)
        XCTAssertEqual(TypedStreamDecoder.decode(textPayload(body)), body)
    }

    // MARK: Attribute containers (mention links, attachments)

    func testTextInsideAttributeContainerIsExtracted() {
        // The body can be wrapped in nested attribute containers (mentions,
        // link ranges, attachment runs); the decoder keeps only the text runs.
        var bytes = stringObjectPreamble()
        bytes.append(0x92)
        bytes += [0x10, 0x04]
        bytes += utf8("Ping")
        bytes += [0x81, 0xC0] // inner objectEnd
        bytes += [0x81, 0xC0] // outer objectEnd
        XCTAssertEqual(TypedStreamDecoder.decode(stream(bytes)), "Ping")
    }

    // MARK: Legacy single-object payloads

    func testLegacySingleObjectPayload() {
        // Older rows lack the "streamtyped" wrapper entirely; the archive
        // header is followed directly by the root object.
        var bytes = Array("bplist00".utf8)
        bytes += Array("_\u{0010}NSKeyedArchiver".utf8)
        bytes += [0x01, 0x74, 0x00]
        bytes += stringObjectPreamble()
        bytes.append(0x92)
        bytes += [0x10, 0x04]
        bytes += utf8("Ping")
        bytes += [0x81, 0xC0]
        XCTAssertEqual(TypedStreamDecoder.decode(Data(bytes)), "Ping")
    }

    // MARK: Garbage in -> nil out (never throw)

    func testEmptyAndShortPayloadsYieldNil() {
        XCTAssertNil(TypedStreamDecoder.decode(Data()))
        XCTAssertNil(TypedStreamDecoder.decode(Data([0x01])))
        XCTAssertNil(TypedStreamDecoder.decode(stream([]))) // header only, no object
    }

    func testTruncatedLengthPrefixYieldNil() {
        // Selector and string tag present but the file ends mid-length.
        var bytes = stringObjectPreamble()
        bytes.append(0x92)
        bytes.append(0x10) // length cut off here
        XCTAssertNil(TypedStreamDecoder.decode(stream(bytes)))
    }

    func testTruncatedUTF8YieldNil() {
        // Declares a 5-byte body but only 4 bytes remain.
        var bytes = stringObjectPreamble()
        bytes.append(0x92)
        bytes += [0x10, 0x05]
        bytes += utf8("Hell")
        XCTAssertNil(TypedStreamDecoder.decode(stream(bytes)))
    }

    func testRandomGarbageYieldNil() {
        var seed: UInt64 = 0x9E3779B97F4A7C15
        func nextRandom() -> UInt8 {
            seed ^= seed << 13; seed ^= seed >> 7; seed ^= seed << 17
            return UInt8(truncatingIfNeeded: seed)
        }
        for _ in 0..<50 {
            var garbage = [UInt8](repeating: 0, count: 64)
            for i in garbage.indices { garbage[i] = nextRandom() }
            XCTAssertNil(TypedStreamDecoder.decode(Data(garbage)))
        }
        // And the streamtyped header with garbage after it.
        XCTAssertNil(TypedStreamDecoder.decode(Data(archiveHeader + [0xFF, 0x00, 0x81, 0x42])))
    }
}
