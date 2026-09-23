import Foundation
import zlib

/// One parsed protobuf wire-format field. The wire types cover everything the
/// raw-store blobs need: varint, 64-bit, 32-bit, and length-delimited (whose
/// payload is itself a message — re-parse it with `ProtobufReader.parse` for
/// the recursive descent).
public enum ProtobufValue: Equatable, Sendable {
    case varint(Int)
    /// 8 little-endian bytes re-interpreted as an integer (diagnostics only).
    case fixed64(Int)
    /// Length-delimited payload: a nested message, string, or bytes.
    case lengthDelimited(Data)
    /// 4 little-endian bytes re-interpreted as an integer (diagnostics only).
    case fixed32(Int)
}

/// One field on the protobuf wire: a field number plus its typed value.
public struct ProtobufField: Equatable, Sendable {
    public let fieldNumber: Int
    public let value: ProtobufValue
}

/// Hand-rolled minimal protobuf wire-format walker — zero SPM dependencies.
/// Notes stores note bodies as gzipped protobuf; the reader walks
/// Document{version(2)} → Version{data(3)} → String{string(2)} by re-parsing
/// length-delimited payloads.
///
/// Strict by design: any malformed input (truncated varint, cut length-
/// delimited payload, unknown wire type, field number 0, overlong tag varint)
/// yields `nil` for the whole parse — garbage in, nil out, it never throws.
public enum ProtobufReader {
    /// Parses all top-level fields of one message, in order, or nil when the
    /// bytes are not a fully well-formed protobuf message.
    public static func parse(_ data: Data) -> [ProtobufField]? {
        parse(bytes: [UInt8](data))
    }

    private static func parse(bytes: [UInt8]) -> [ProtobufField]? {
        var fields: [ProtobufField] = []
        var i = 0
        while i < bytes.count {
            // Tag: varint carrying (fieldNumber << 3) | wireType.
            guard let tag = varint(at: &i, bytes: bytes), i <= bytes.count else { return nil }
            let fieldNumber = tag >> 3
            let wireType = tag & 0x07
            guard fieldNumber != 0 else { return nil }
            switch wireType {
            case 0: // varint
                guard let value = varint(at: &i, bytes: bytes) else { return nil }
                fields.append(ProtobufField(fieldNumber: fieldNumber, value: .varint(value)))
            case 1: // 64-bit
                guard i + 8 <= bytes.count else { return nil }
                var value = 0
                for shift in stride(from: 56, through: 0, by: -8) {
                    value |= Int(bytes[i]) << shift
                    i += 1
                }
                fields.append(ProtobufField(fieldNumber: fieldNumber, value: .fixed64(value)))
            case 2: // length-delimited
                guard let length = varint(at: &i, bytes: bytes),
                      length >= 0, i + length <= bytes.count else { return nil }
                fields.append(ProtobufField(fieldNumber: fieldNumber,
                                            value: .lengthDelimited(Data(bytes[i..<(i + length)]))))
                i += length
            case 5: // 32-bit
                guard i + 4 <= bytes.count else { return nil }
                var value = 0
                for shift in stride(from: 24, through: 0, by: -8) {
                    value |= Int(bytes[i]) << shift
                    i += 1
                }
                fields.append(ProtobufField(fieldNumber: fieldNumber, value: .fixed32(value)))
            default:
                return nil // wire types 3/4 (groups) and 6/7 are not spoken here
            }
        }
        return fields
    }

    /// Reads one base-128 varint starting at `i` (advanced past it). More than
    /// 10 bytes (a 64-bit varint must fit) or a truncated buffer → nil.
    private static func varint(at i: inout Int, bytes: [UInt8]) -> Int? {
        var result = 0
        var shift = 0
        var count = 0
        while i < bytes.count && count < 10 {
            let byte = bytes[i]
            i += 1
            count += 1
            // 7 bits of payload; on the 10th byte only bit 0 may carry data
            // (shift 63), anything larger overflows the Int invariant.
            if shift < 64 {
                result |= Int(byte & 0x7F) << shift
            }
            shift += 7
            if byte & 0x80 == 0 {
                if count == 10 && byte > 0x01 { return nil }
                return result >= 0 ? result : nil // 1 << 63 would wrap negative
            }
        }
        return nil // unterminated (or > 10 bytes)
    }
}

/// Minimal gzip decompressor over libz for the Notes body blobs (0x1f8b
/// magic, 1 MiB decompressed cap). Failure of any kind → nil, never throw.
public enum Gzip {
    /// Cap on decompressed output — a hostile/corrupt blob must not balloon.
    public static let maxDecompressedBytes = 1_048_576

    /// Decompresses a gzip payload (0x1f 0x8b magic) with libz `inflate`
    /// (windowBits 15 + 16: gzip header parsing + trailer CRC validation),
    /// or nil when the input is not gzip / is corrupt / inflates past
    /// `maxBytes`. Decompresses WHOLE or not at all — never a prefix.
    public static func decompress(_ data: Data, maxBytes: Int = maxDecompressedBytes) -> Data? {
        var bytes = [UInt8](data)
        guard bytes.count >= 2, bytes[0] == 0x1f, bytes[1] == 0x8b else { return nil }
        guard maxBytes >= 0, bytes.count <= Int(UInt32.max) else { return nil }
        var stream = z_stream()
        // 15 + 16: raw window size + gzip header/CRC handling.
        guard inflateInit2_(&stream, 15 + 16, ZLIB_VERSION,
                            Int32(MemoryLayout<z_stream>.size)) == Z_OK else {
            _ = inflateEnd(&stream)
            return nil
        }
        defer { _ = inflateEnd(&stream) }
        return bytes.withUnsafeMutableBufferPointer { input -> Data? in
            guard let inputBase = input.baseAddress else { return nil }
            stream.next_in = inputBase
            stream.avail_in = UInt32(input.count)
            var output = Data()
            var chunk = [UInt8](repeating: 0, count: 65_536)
            while true {
                // Exclusive access: the output pointer borrows `chunk` for the
                // inflate call only; produced count read after it returns.
                var produced = 0
                let status: Int32 = chunk.withUnsafeMutableBufferPointer { out -> Int32 in
                    stream.next_out = out.baseAddress
                    stream.avail_out = UInt32(out.count)
                    return inflate(&stream, Z_NO_FLUSH)
                }
                produced = chunk.count - Int(stream.avail_out)
                if produced > 0 {
                    output.append(contentsOf: chunk[0..<produced])
                    if output.count > maxBytes { return nil }
                }
                if status == Z_STREAM_END { return output }
                // Z_OK: progress made, keep inflating. Z_BUF_ERROR means no
                // progress is possible (input exhausted without a stream end,
                // or no room) — corrupt/truncated → nil.
                if status != Z_OK { return nil }
            }
        }
    }
}
