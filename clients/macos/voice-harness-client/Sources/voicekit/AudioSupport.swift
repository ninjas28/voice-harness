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
