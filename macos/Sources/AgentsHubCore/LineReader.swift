import Foundation

/// Bytes in, complete NDJSON lines out.
///
/// The cursor is the whole point. An attach replay arrives as a single ~175 KB base64
/// line (128 KB of PTY log), so rescanning from index 0 on every chunk would be
/// quadratic on exactly the frame that matters. `[UInt8]` rather than `Data` on purpose:
/// `Data` slices keep parent-relative indices, which makes `buf[0]` a lurking crash.
public struct LineReader {
    /// A peer that never sends a newline must not be able to grow this without bound.
    public static let maxLine = 8 << 20

    private var buf: [UInt8] = []
    private var scanned = 0

    public init() {}

    /// Returns false once the buffer has passed `maxLine` with no newline in it, which
    /// the caller should treat as a wedged peer and drop.
    @discardableResult
    public mutating func push(_ chunk: [UInt8], _ onLine: ([UInt8]) -> Void) -> Bool {
        buf.append(contentsOf: chunk)
        var start = 0
        var i = scanned
        while i < buf.count {
            if buf[i] == 0x0A {
                if i > start { onLine(Array(buf[start..<i])) }
                start = i + 1
            }
            i += 1
        }
        if start > 0 { buf.removeFirst(start) }  // one memmove per chunk, not per line
        scanned = buf.count
        return buf.count <= Self.maxLine
    }

    public mutating func reset() {
        buf.removeAll(keepingCapacity: false)
        scanned = 0
    }
}
