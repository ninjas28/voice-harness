import Foundation

/// Minimal decoder for the typedstream payloads Messages stores in
/// `message.attributedBody`.
///
/// These blobs are NSKeyedArchiver output in Apple's "bridge" typedstream
/// encoding (the format `.streamtyped` files use): a "streamtyped" marker, a
/// 0x13 separator, a legacy `bplist00_NSKeyedArchiver` prefix, then a tree of
/// objects. Each object starts with `0x81 <index>` and may carry a 0x10-tagged
/// class-name string; a string payload is a `0x90`/`0x92` selector followed by
/// an optionally 0x10-tagged, varint-length-prefixed UTF-8 body.
///
/// The subset here only extracts the concatenated plain-text runs — enough for
/// message bodies — and is deliberately strict: anything it cannot fully parse
/// (truncated length, invalid UTF-8, no recognizable structure) yields `nil`.
/// Garbage in, nil out; it never throws.
enum TypedStreamDecoder {
    /// Decodes the plain text of a typedstream payload, or nil if the payload
    /// is not a parseable typedstream archive containing text.
    static func decode(_ payload: Data) -> String? {
        var bytes = [UInt8](payload)
        guard !bytes.isEmpty else { return nil }

        // Drop the "streamtyped" wrapper: everything up to and including the
        // first 0x13 separator (marker + legacy archive header).
        if let marker = range(of: Array("streamtyped".utf8), in: bytes), marker.lowerBound == 0 {
            guard let sep = bytes.firstIndex(of: 0x13), sep > marker.lowerBound else { return nil }
            bytes.removeSubrange(0...sep)
        }

        var reader = Reader(bytes: bytes)
        var text = ""
        var sawClassName = false
        var sawText = false

        while let byte = reader.next() {
            switch byte {
            case 0x81:
                // Object start or object end — both are 0x81 followed by the
                // object index varint. Consume the index; if a class-name
                // string follows this was an object start.
                guard reader.next() != nil else { return nil }
                if reader.peek() == 0x10 {
                    reader.next()
                    guard reader.skipTaggedString() else { return nil }
                    sawClassName = true
                }
            case 0x90, 0x92:
                // String payload selector: an optionally 0x10-tagged,
                // varint-length-prefixed UTF-8 body.
                guard sawClassName else { return nil }
                guard let string = reader.readStringPayload() else { return nil }
                text += string
                sawText = true
            default:
                break // control bytes, small ints, attribute payloads: ignored
            }
        }

        return sawText ? text : nil
    }

    private static func range(of needle: [UInt8], in haystack: [UInt8]) -> Range<Array<UInt8>.Index>? {
        haystack.firstRange(of: needle)
    }

    private struct Reader {
        let bytes: [UInt8]
        var index = 0

        init(bytes: [UInt8]) { self.bytes = bytes }

        mutating func next() -> UInt8? {
            guard index < bytes.count else { return nil }
            defer { index += 1 }
            return bytes[index]
        }

        func peek() -> UInt8? {
            index < bytes.count ? bytes[index] : nil
        }

        /// Bridge-style varint: 7-bit chunks, little-endian order, high bit
        /// set on every chunk except the last.
        mutating func readVarint() -> UInt64? {
            var value: UInt64 = 0
            var shift: UInt64 = 0
            while true {
                guard let byte = next() else { return nil }
                value |= UInt64(byte & 0x7F) << shift
                if byte & 0x80 == 0 { return value }
                shift += 7
                if shift > 63 { return nil }
            }
        }

        /// Skips a 0x10-tagged length-prefixed byte string (e.g. a class name).
        mutating func skipTaggedString() -> Bool {
            guard let length = readVarint(), length <= UInt64(bytes.count - index) else { return false }
            index += Int(length)
            return true
        }

        /// Reads a string payload: optional 0x10 tag, varint length, UTF-8
        /// bytes. Strict — truncated data or invalid UTF-8 fails.
        mutating func readStringPayload() -> String? {
            if peek() == 0x10 { next() }
            guard let length = readVarint(), length <= UInt64(bytes.count - index) else { return nil }
            let start = index
            index += Int(length)
            return String(bytes: bytes[start..<index], encoding: .utf8)
        }
    }
}
