/// Private, self-describing scratch rows. These bytes are not a database format
/// and never establish SQL equality. Every value's representation is retained,
/// including floating payload bits, decimal scale and array lower bounds.
internal enum SpillRowCodec {
    private enum Task { case value(SQLValue), json(SQLJSONValue) }
    static func encode(_ row: [SQLValue]) throws -> [UInt8] {
        var writer = ByteWriter(); writer.u64(UInt64(row.count))
        var tasks = row.reversed().map(Task.value)
        func bytes(_ value: [UInt8], _ writer: inout ByteWriter) {
            writer.u64(UInt64(value.count)); writer.append(value)
        }
        func decimal(_ value: SQLDecimal, _ writer: inout ByteWriter) {
            writer.u8(value.negative ? 1 : 0); writer.u64(UInt64(value.scale)); writer.u64(UInt64(value.limbs.count))
            for limb in value.limbs { writer.u32(limb) }
        }
        while let task = tasks.popLast() {
            switch task {
            case .value(let value):
                switch value {
                case .null: writer.u8(0)
                case .integer(let value): writer.u8(1); writer.i64(value)
                case .boolean(let value): writer.u8(2); writer.u8(value ? 1 : 0)
                case .text(let value): writer.u8(3); bytes(value.utf8, &writer)
                case .decimal(let value): writer.u8(4); decimal(value, &writer)
                case .bytea(let value): writer.u8(5); bytes(value, &writer)
                case .uuid(let value): writer.u8(6); writer.append(value.bytes)
                case .timestamp(let value): writer.u8(7); writer.i64(value)
                case .timestamptz(let value): writer.u8(8); writer.i64(value)
                case .date(let value): writer.u8(9); writer.i32(value)
                case .interval(let value):
                    writer.u8(10); writer.i32(value.months); writer.i32(value.days); writer.i64(value.microseconds)
                case .f32(let value): writer.u8(11); writer.u32(value)
                case .f64(let value): writer.u8(12); writer.u64(value)
                case .composite(let values):
                    writer.u8(13); writer.u64(UInt64(values.count)); tasks += values.reversed().map(Task.value)
                case .array(let value):
                    writer.u8(14); writer.u8(UInt8(value.dimensions.count))
                    for (length, lower) in zip(value.dimensions, value.lowerBounds) { writer.u32(UInt32(length)); writer.i32(lower) }
                    writer.u64(UInt64(value.elements.count)); tasks += value.elements.reversed().map(Task.value)
                case .range(let value):
                    writer.u8(15)
                    let flags: UInt8 = (value.isEmpty ? 1 : 0) | (value.lower == nil ? 0 : 2) | (value.upper == nil ? 0 : 4)
                        | (value.lowerInclusive ? 8 : 0) | (value.upperInclusive ? 16 : 0)
                    writer.u8(flags)
                    if let upper = value.upper { tasks.append(.value(upper)) }
                    if let lower = value.lower { tasks.append(.value(lower)) }
                case .json(let value): writer.u8(16); bytes(value.utf8, &writer)
                case .jsonpath(let value): writer.u8(17); bytes(value.utf8, &writer)
                case .jsonb(let value): tasks.append(.json(value))
                }
            case .json(let value):
                switch value {
                case .null: writer.u8(32)
                case .boolean(let value): writer.u8(33); writer.u8(value ? 1 : 0)
                case .string(let value): writer.u8(34); bytes(value.utf8, &writer)
                case .number(let value): writer.u8(35); decimal(value, &writer)
                case .array(let values):
                    writer.u8(36); writer.u64(UInt64(values.count)); tasks += values.reversed().map(Task.json)
                case .object(let members):
                    writer.u8(37); writer.u64(UInt64(members.count))
                    for member in members { bytes(member.key.utf8, &writer) }
                    tasks += members.reversed().map { .json($0.value) }
                }
            }
        }
        return writer.bytes
    }

    private enum Container {
        case row, composite, array([Int], [Int32]), range(UInt8), jsonArray, jsonObject([SQLText])
    }
    private struct Frame {
        let kind: Container
        let count: Int
        var values: [SQLValue] = []
    }
    private enum Item { case value(SQLValue), container(Frame) }
    static func decode(_ bytes: [UInt8]) throws -> [SQLValue] {
        do {
            var reader = ByteReader(bytes)
            var stack = [Frame(kind: .row, count: try count(&reader, minimumBytes: 1))]
            while !stack.isEmpty {
                let top = stack.count - 1
                if stack[top].values.count == stack[top].count {
                    let frame = stack.removeLast()
                    if case .row = frame.kind { try reader.requireEnd(); return frame.values }
                    let value = try complete(frame)
                    stack[stack.count - 1].values.append(value)
                } else {
                    switch try item(&reader) {
                    case .value(let value): stack[top].values.append(value)
                    case .container(let frame): stack.append(frame)
                    }
                }
            }
            throw JedError("58030", "scratch row has no root")
        } catch { throw JedError("58030", "invalid scratch row: \(error)") }
    }
    private static func count(_ reader: inout ByteReader, minimumBytes: Int) throws -> Int {
        let raw = try reader.u64()
        guard raw <= UInt64(Int.max), raw <= UInt64(reader.remaining / minimumBytes) else {
            throw JedError("58030", "scratch field count exceeds its frame")
        }
        return Int(raw)
    }
    private static func bytes(_ reader: inout ByteReader) throws -> [UInt8] {
        let length = try count(&reader, minimumBytes: 1)
        return Array(try reader.take(length))
    }
    private static func text(_ reader: inout ByteReader) throws -> SQLText { try SQLText(utf8: bytes(&reader)) }
    private static func bool(_ reader: inout ByteReader) throws -> Bool {
        let value = try reader.u8()
        guard value <= 1 else { throw JedError("58030", "invalid scratch boolean") }; return value == 1
    }
    private static func decimal(_ reader: inout ByteReader) throws -> SQLDecimal {
        let negative = try bool(&reader), rawScale = try reader.u64()
        guard rawScale <= UInt64(Int.max) else { throw JedError("58030", "invalid scratch decimal scale") }
        let length = try count(&reader, minimumBytes: 4)
        var limbs: [UInt32] = []; limbs.reserveCapacity(length)
        for _ in 0..<length {
            let limb = try reader.u32()
            guard limb < 1_000_000_000 else { throw JedError("58030", "invalid scratch decimal limb") }
            limbs.append(limb)
        }
        guard limbs.last != 0, !negative || !limbs.isEmpty else { throw JedError("58030", "noncanonical scratch decimal") }
        return SQLDecimal(negative: negative, magnitude: BigMagnitude(limbs), scale: Int(rawScale))
    }
    private static func item(_ reader: inout ByteReader) throws -> Item {
        switch try reader.u8() {
        case 0: return .value(.null)
        case 1: return .value(.integer(try reader.i64()))
        case 2: return .value(.boolean(try bool(&reader)))
        case 3: return .value(.text(try text(&reader)))
        case 4: return .value(.decimal(try decimal(&reader)))
        case 5: return .value(.bytea(try bytes(&reader)))
        case 6: return .value(.uuid(try SQLUUID(bytes: Array(reader.take(16)))))
        case 7: return .value(.timestamp(try reader.i64()))
        case 8: return .value(.timestamptz(try reader.i64()))
        case 9: return .value(.date(try reader.i32()))
        case 10:
            return .value(.interval(SQLInterval(months: try reader.i32(), days: try reader.i32(), microseconds: try reader.i64())))
        case 11: return .value(.f32(try reader.u32()))
        case 12: return .value(.f64(try reader.u64()))
        case 13: return .container(Frame(kind: .composite, count: try count(&reader, minimumBytes: 1)))
        case 14:
            let dimensions = Int(try reader.u8()); var lengths: [Int] = [], lower: [Int32] = []
            guard dimensions <= reader.remaining / 8 else { throw JedError("58030", "truncated scratch array dimensions") }
            for _ in 0..<dimensions { lengths.append(Int(try reader.u32())); lower.append(try reader.i32()) }
            return .container(Frame(kind: .array(lengths, lower), count: try count(&reader, minimumBytes: 1)))
        case 15:
            let flags = try reader.u8()
            guard flags & ~31 == 0, flags & 1 == 0 || flags == 1,
                  flags & 8 == 0 || flags & 2 != 0, flags & 16 == 0 || flags & 4 != 0 else {
                throw JedError("58030", "invalid scratch range flags")
            }
            return .container(Frame(kind: .range(flags), count: (flags & 2 == 0 ? 0 : 1) + (flags & 4 == 0 ? 0 : 1)))
        case 16: return .value(.json(try text(&reader)))
        case 17: return .value(.jsonpath(try text(&reader)))
        case 32: return .value(.jsonb(.null))
        case 33: return .value(.jsonb(.boolean(try bool(&reader))))
        case 34: return .value(.jsonb(.string(try text(&reader))))
        case 35: return .value(.jsonb(.number(try decimal(&reader))))
        case 36: return .container(Frame(kind: .jsonArray, count: try count(&reader, minimumBytes: 1)))
        case 37:
            let length = try count(&reader, minimumBytes: 9); var keys: [SQLText] = []
            for _ in 0..<length { keys.append(try text(&reader)) }
            return .container(Frame(kind: .jsonObject(keys), count: length))
        default: throw JedError("58030", "invalid scratch value tag")
        }
    }
    private static func complete(_ frame: Frame) throws -> SQLValue {
        switch frame.kind {
        case .row: throw JedError("58030", "invalid nested scratch row")
        case .composite: return .composite(frame.values)
        case .array(let dimensions, let lower):
            return .array(try SQLArray(dimensions: dimensions, lowerBounds: lower, elements: frame.values))
        case .range(let flags):
            return .range(SQLRange(empty: flags & 1 != 0,
                lower: flags & 2 == 0 ? nil : frame.values[0],
                upper: flags & 4 == 0 ? nil : frame.values[flags & 2 == 0 ? 0 : 1],
                lowerInclusive: flags & 8 != 0, upperInclusive: flags & 16 != 0))
        case .jsonArray, .jsonObject:
            let values = try frame.values.map { value -> SQLJSONValue in
                guard case .jsonb(let node) = value else { throw JedError("58030", "non-JSON value in scratch JSON") }
                return node
            }
            if case .jsonObject(let keys) = frame.kind {
                return .jsonb(.object(zip(keys, values).map { SQLJSONMember(key: $0.0, value: $0.1) }))
            }
            return .jsonb(.array(values))
        }
    }
}
