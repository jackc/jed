/// One original statement source span, without surrounding whitespace/comments
/// or its semicolon terminator. Offset is a zero-based UTF-8 byte offset.
public struct StatementSpan: Sendable, Equatable {
    public let text: String
    public let offset: Int
}

/// Lazy, pure, total statement boundaries. Malformed SQL remains a source span
/// for the caller's parser; splitting alone never executes or validates SQL.
public struct StatementSpans: Sequence, Sendable {
    private let bytes: [UInt8]
    fileprivate init(_ sql: String) { bytes = Array(sql.utf8) }

    public func makeIterator() -> Iterator { Iterator(bytes: bytes) }

    public struct Iterator: IteratorProtocol {
        private let bytes: [UInt8]
        private var cursor = 0
        fileprivate init(bytes: [UInt8]) { self.bytes = bytes }

        public mutating func next() -> StatementSpan? {
            var start: Int?
            var significantEnd = cursor
            while cursor < bytes.count {
                let byte = bytes[cursor]
                if SQLLexicalBoundary.isWhitespace(byte) { cursor += 1; continue }
                if byte == 59 {
                    cursor += 1
                    if let start { return span(start, significantEnd) }
                    continue
                }
                if byte == 45 && cursor + 1 < bytes.count && bytes[cursor + 1] == 45 {
                    cursor += 2
                    while cursor < bytes.count && bytes[cursor] != 10 && bytes[cursor] != 13 { cursor += 1 }
                    continue
                }
                if byte == 47 && cursor + 1 < bytes.count && bytes[cursor + 1] == 42 {
                    cursor = SQLLexicalBoundary.blockCommentEnd(bytes, afterOpening: cursor + 2).end
                    continue
                }
                if start == nil { start = cursor }
                if byte == 39 || byte == 34 {
                    cursor = SQLLexicalBoundary.quotedEnd(bytes, afterOpening: cursor + 1, quote: byte).end
                } else if byte == 36 {
                    let length = SQLLexicalBoundary.dollarDelimiterLength(bytes, at: cursor)
                    if length > 0 {
                        let delimiter = bytes[cursor..<cursor + length]
                        cursor += length
                        while cursor < bytes.count {
                            if bytes[cursor] == 36 && bytes[cursor...].starts(with: delimiter) {
                                cursor += length
                                break
                            }
                            cursor += 1
                        }
                    } else { cursor += 1 }
                } else { cursor += 1 }
                significantEnd = cursor
            }
            return start.map { span($0, significantEnd) }
        }

        private func span(_ start: Int, _ end: Int) -> StatementSpan {
            StatementSpan(text: String(decoding: bytes[start..<end], as: UTF8.self), offset: start)
        }
    }
}

/// SQL execution and its transaction policy are deliberately outside this API.
public func splitStatements(_ sql: String) -> StatementSpans { StatementSpans(sql) }
