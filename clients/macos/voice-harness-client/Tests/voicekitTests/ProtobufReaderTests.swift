import XCTest
import zlib
@testable import voicekit

/// Exercises the hand-rolled protobuf wire-format walker and the gzip body
/// decompressor against hand-built byte fixtures — no real NoteStore needed.
final class ProtobufReaderTests: XCTestCase {
    // MARK: - Fixture builders

    /// Base-128 varint encoding of a non-negative integer.
    private func varint(_ value: Int) -> [UInt8] {
        var v = UInt64(value)
        var out: [UInt8] = []
        repeat {
            var byte = UInt8(v & 0x7F)
            v >>= 7
            if v != 0 { byte |= 0x80 }
            out.append(byte)
        } while v != 0
        return out
    }

    /// Length-delimited field: tag byte (field << 3 | 2) + varint length + bytes.
    private func lengthDelimited(_ field: Int, _ payload: [UInt8]) -> [UInt8] {
        [UInt8(field << 3 | 2)] + varint(payload.count) + payload
    }

    /// Varint field: tag byte (field << 3 | 0) + varint value.
    private func varintField(_ field: Int, _ value: Int) -> [UInt8] {
        [UInt8(field << 3)] + varint(value)
    }

    /// libz deflate with a gzip header (windowBits 15+16) — the encoder side
    /// of the round trip the reader must handle.
    private static func gzipDeflate(_ data: Data) -> Data {
        var stream = z_stream()
        let status = deflateInit2_(&stream, 9, Z_DEFLATED, 15 + 16, 8,
                                   Z_DEFAULT_STRATEGY, "1.2.11",
                                   Int32(MemoryLayout<z_stream>.size))
        XCTAssertEqual(status, Z_OK, "deflateInit2_ failed")
        defer { _ = deflateEnd(&stream) }
        let bound = Int(compressBound(uLong(max(data.count, 1)))) + 64
        let output = UnsafeMutablePointer<UInt8>.allocate(capacity: bound)
        defer { output.deallocate() }
        var input = [UInt8](data)
        return input.withUnsafeMutableBufferPointer { input -> Data in
            stream.next_in = input.baseAddress
            stream.avail_in = UInt32(input.count)
            stream.next_out = output
            stream.avail_out = UInt32(bound)
            let result = deflate(&stream, Z_FINISH)
            XCTAssertEqual(result, Z_STREAM_END, "deflate Z_FINISH failed")
            return Data(bytes: output, count: bound - Int(stream.avail_out))
        }
    }

    // MARK: - Wire-format fields

    func testVarintField() {
        // Field 1, wire type 0, value 150: 0x08 0x96 0x01 (the classic example).
        let fields = ProtobufReader.parse(Data([0x08, 0x96, 0x01]))
        XCTAssertEqual(fields, [ProtobufField(fieldNumber: 1, value: .varint(150))])
    }

    func testMultiByteVarint() {
        // 300 encodes as 0xAC 0x02.
        let fields = ProtobufReader.parse(Data([0x08, 0xAC, 0x02]))
        XCTAssertEqual(fields, [ProtobufField(fieldNumber: 1, value: .varint(300))])
    }

    func testLengthDelimitedString() {
        // Field 2, wire type 2, "testing".
        var bytes: [UInt8] = [0x12, 0x07]
        bytes += Array("testing".utf8)
        let fields = ProtobufReader.parse(Data(bytes))
        XCTAssertEqual(fields, [ProtobufField(fieldNumber: 2,
                                              value: .lengthDelimited(Data("testing".utf8)))])
    }

    func testLargeFieldNumberUsesMultiByteTag() {
        // Field 16, wire type 0, value 5: tag varint 128 → 0x80 0x01.
        let fields = ProtobufReader.parse(Data([0x80, 0x01, 0x05]))
        XCTAssertEqual(fields, [ProtobufField(fieldNumber: 16, value: .varint(5))])
    }

    func testFixed64AndFixed32() {
        // Field 1 wire type 1 (fixed64): 8 little-endian bytes. 01..08 little-
        // endian re-interprets to 0x0102030405060708 (the low byte 0x01 is
        // FIRST on the wire); big-endian would be 0x0807060504030201.
        let f64 = ProtobufReader.parse(Data([0x09, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08]))
        XCTAssertEqual(f64, [ProtobufField(fieldNumber: 1, value: .fixed64(0x0102030405060708))])
        // Field 1 wire type 5 (fixed32): 4 little-endian bytes → 0x01020304.
        let f32 = ProtobufReader.parse(Data([0x0D, 0x01, 0x02, 0x03, 0x04]))
        XCTAssertEqual(f32, [ProtobufField(fieldNumber: 1, value: .fixed32(0x01020304))])
    }

    func testMultipleFieldsParseInOrder() {
        // varint field 1 = 1, then string field 2 = "a".
        var bytes: [UInt8] = [0x08, 0x01, 0x12, 0x01]
        bytes += Array("a".utf8)
        guard let fields = ProtobufReader.parse(Data(bytes)), fields.count == 2 else {
            return XCTFail("expected two well-formed fields")
        }
        XCTAssertEqual(fields[0], ProtobufField(fieldNumber: 1, value: .varint(1)))
        XCTAssertEqual(fields[1], ProtobufField(fieldNumber: 2, value: .lengthDelimited(Data("a".utf8))))
    }

    // MARK: - Nested-message descent

    func testNestedMessageDescent() {
        // Inner message: field 1 varint 42 → 0x08 0x2A.
        let inner = Data([0x08, 0x2A])
        // Outer: field 3 length-delimited(inner) → 0x1A 0x02 0x08 0x2A.
        let outer = ProtobufReader.parse(Data([0x1A, 0x02, 0x08, 0x2A]))
        XCTAssertEqual(outer, [ProtobufField(fieldNumber: 3, value: .lengthDelimited(inner))])
        // Recursive descent is a re-parse of the length-delimited payload.
        XCTAssertEqual(ProtobufReader.parse(inner),
                       [ProtobufField(fieldNumber: 1, value: .varint(42))])
    }

    func testNoteDocumentShapeDescent() {
        // The Notes blob chain: Document(field 2) → Version(field 3) →
        // String(field 2) = text. Hand-built end to end.
        let text = Array("hello note".utf8)
        let stringMessage = lengthDelimited(2, text)           // String.string
        let version = lengthDelimited(3, stringMessage)        // Version.data
        let document = lengthDelimited(2, version)             // Document.version
        guard let fields = ProtobufReader.parse(Data(document)), fields.count == 1,
              case .lengthDelimited(let versionBytes) = fields[0].value else {
            return XCTFail("expected one length-delimited version field")
        }
        guard let versionFields = ProtobufReader.parse(versionBytes), !versionFields.isEmpty,
              case .lengthDelimited(let stringBytes) = versionFields[0].value else {
            return XCTFail("expected length-delimited data field")
        }
        guard let stringFields = ProtobufReader.parse(stringBytes), !stringFields.isEmpty,
              case .lengthDelimited(let textBytes) = stringFields[0].value else {
            return XCTFail("expected length-delimited string field")
        }
        XCTAssertEqual(String(decoding: textBytes, as: UTF8.self), "hello note")
    }

    // MARK: - Malformed input (garbage in, nil out)

    func testEmptyDataParsesToNoFields() {
        XCTAssertEqual(ProtobufReader.parse(Data()), [])
    }

    func testTruncatedVarintYieldsNil() {
        XCTAssertNil(ProtobufReader.parse(Data([0x08, 0x96]))) // value cut short
    }

    func testTenByteVarintWithoutTerminatorYieldsNil() {
        XCTAssertNil(ProtobufReader.parse(Data([0x08] + [UInt8](repeating: 0xFF, count: 10))))
    }

    func testTruncatedLengthDelimitedYieldsNil() {
        XCTAssertNil(ProtobufReader.parse(Data([0x12, 0x07, 0x61]))) // claims 7, has 1
    }

    func testUnknownWireTypeYieldsNil() {
        XCTAssertNil(ProtobufReader.parse(Data([0x0F]))) // field 1, wire type 7
        XCTAssertNil(ProtobufReader.parse(Data([0x1B]))) // wire type 3 (group) unsupported
    }

    func testFieldNumberZeroYieldsNil() {
        XCTAssertNil(ProtobufReader.parse(Data([0x00, 0x01]))) // field 0 is invalid
    }

    // MARK: - Gzip decompression (libz)

    func testGzipRoundTripViaLibz() {
        let payload = Data("hello harness".utf8)
        let compressed = Self.gzipDeflate(payload)
        XCTAssertTrue(compressed.starts(with: [0x1f, 0x8b]), "fixture must carry the gzip magic")
        XCTAssertEqual(Gzip.decompress(compressed), payload)
    }

    func testGzipRoundTripsBinaryPayloadWithNULs() {
        let payload = Data([0x00, 0x01, 0xFF, 0x00, 0x7F, 0x80])
        XCTAssertEqual(Gzip.decompress(Self.gzipDeflate(payload)), payload)
    }

    func testGzipRejectsNonGzipMagic() {
        XCTAssertNil(Gzip.decompress(Data([0x78, 0x9C, 0x01, 0x02])), "zlib header, not gzip")
        XCTAssertNil(Gzip.decompress(Data([0xDE, 0xAD, 0xBE, 0xEF])))
        XCTAssertNil(Gzip.decompress(Data()))
    }

    func testGzipRejectsOutputBeyondExplicitCap() {
        let payload = Data(repeating: 0, count: 64)
        XCTAssertNil(Gzip.decompress(Self.gzipDeflate(payload), maxBytes: 32))
    }

    func testGzipDefaultCapRejectsOversizedBody() {
        let payload = Data(repeating: 0x41, count: Gzip.maxDecompressedBytes + 1)
        XCTAssertNil(Gzip.decompress(Self.gzipDeflate(payload)),
                     "a body inflating past the 1 MiB cap must fail whole, not truncate")
    }

    func testGzipRejectsCorruptStream() {
        // Valid gzip magic + header, garbage payload.
        var bytes = Self.gzipDeflate(Data("intact".utf8))
        bytes[bytes.count - 2] ^= 0xFF
        bytes[bytes.count - 1] ^= 0xFF
        XCTAssertNil(Gzip.decompress(bytes))
    }
}
