/// Resolved timezone data is immutable and comes only from the environment's
/// explicitly loaded JTZ bundles. Conversion never consults the host timezone.
internal struct SQLTimeZone: Sendable {
    let name: SQLText
    let dataVersion: SQLText?
    let contentHash: UInt32?
    private let fixedOffset: Int32?
    fileprivate let data: TZifData?

    static let utc = SQLTimeZone(fixed: 0, name: SQLText(validatedUTF8: Array("UTC".utf8)))
    init(fixed seconds: Int32, name: SQLText? = nil) {
        self.name = name ?? SQLText(validatedUTF8: Array(Temporal.offsetText(seconds).utf8))
        dataVersion = nil; contentHash = nil; fixedOffset = seconds; data = nil
    }
    fileprivate init(name: SQLText, version: SQLText, hash: UInt32, data: TZifData) {
        self.name = name; dataVersion = version; contentHash = hash; self.data = data; fixedOffset = nil
    }
    func offset(at micros: Int64) -> TimeZoneOffset { offsetAtSeconds(Temporal.floorDiv(micros, 1_000_000)) }
    private func offsetAtSeconds(_ seconds: Int64) -> TimeZoneOffset {
        if let fixedOffset {
            return TimeZoneOffset(seconds: fixedOffset, abbreviation: fixedOffset == 0 ? "UTC" : Temporal.offsetText(fixedOffset, iso: true), isDST: false)
        }
        return data!.offset(at: seconds)
    }
    func instantToLocal(_ micros: Int64) throws -> Int64 {
        if Temporal.isInfinite(micros) { return micros }
        return try Temporal.finiteTimestamp(Int128(micros) + Int128(offset(at: micros).seconds) * 1_000_000)
    }
    func localToInstant(_ micros: Int64) throws -> Int64 {
        if Temporal.isInfinite(micros) { return micros }
        let wall = Temporal.floorDiv(micros, 1_000_000)
        let selected: Int32
        if let fixedOffset { selected = fixedOffset }
        else {
            // A transition's two offsets each propose an instant. The branch
            // with the later proposed instant is PG's gap/overlap choice; away
            // from a discontinuity, the actual side of the boundary decides.
            let before = offsetAtSeconds(wall - 86_400).seconds
            let after = offsetAtSeconds(wall + 86_400).seconds
            if before == after { selected = before }
            else {
                var lower = wall - 86_400, upper = wall + 86_400
                while lower < upper {
                    let midpoint = lower + (upper - lower) / 2
                    if offsetAtSeconds(midpoint).seconds == before { lower = midpoint + 1 }
                    else { upper = midpoint }
                }
                let first = wall - Int64(before), second = wall - Int64(after)
                if (first < lower) == (second < lower) { selected = first < lower ? before : after }
                else { selected = first > second ? before : after }
            }
        }
        return try Temporal.finiteTimestamp(Int128(micros) - Int128(selected) * 1_000_000)
    }
}

internal struct TimeZoneOffset: Sendable, Equatable {
    let seconds: Int32
    let abbreviation: String
    let isDST: Bool
}

internal struct TimeZoneDatabase: Sendable {
    private(set) var zones: [SQLTimeZone] = []
    mutating func load(_ bytes: [UInt8]) throws {
        let bundle = try TimeZoneBundle(bytes)
        for zone in bundle.zones where !zones.contains(where: { $0.name == zone.name }) { zones.append(zone) }
    }
    func resolve(_ name: SQLText) throws -> SQLTimeZone {
        if name.asciiLowercased.utf8 == Array("utc".utf8) { return .utc }
        if let offset = POSIXClock.fixedOffset(name.utf8) { return SQLTimeZone(fixed: -offset, name: name) }
        if let exact = zones.first(where: { $0.name == name }) { return exact }
        if let folded = zones.first(where: { $0.name.asciiLowercased == name.asciiLowercased }) { return folded }
        throw JedError("22023", "time zone \"\(name)\" not recognized")
    }

    /// make_timestamptz's numeric argument follows ISO east-positive offsets;
    /// AT TIME ZONE and the session setting follow the POSIX west-positive sign.
    func resolveConstructor(_ name: SQLText) throws -> SQLTimeZone {
        let bytes = name.utf8
        if let first = bytes.first, (48...57).contains(first) {
            throw JedError("22023", "invalid input syntax for type numeric time zone: \"\(name)\"")
        }
        if bytes.first == 43 || bytes.first == 45 {
            var parser = NumericZoneInput(bytes)
            if let offset = try parser.parse() { return SQLTimeZone(fixed: offset, name: name) }
        }
        return try resolve(name)
    }
}

internal struct TimeZoneBundle: Sendable {
    let version: SQLText
    let description: SQLText
    let zones: [SQLTimeZone]
    let zoneNames: [SQLText]
    let links: [(SQLText, SQLText)]
    private let original: [UInt8]
    func encoded() -> [UInt8] { original }

    init(_ bytes: [UInt8]) throws {
        guard bytes.count >= 18 else { throw JedError.corruption("truncated timezone bundle") }
        var trailer = ByteReader(bytes.suffix(4))
        guard try trailer.u32() == Checksums.crc32(bytes.dropLast(4)) else { throw JedError.corruption("invalid timezone bundle checksum") }
        var reader = ByteReader(bytes.dropLast(4))
        guard Array(try reader.take(6)) == [74, 84, 90, 0, 0, 0], try reader.u16() == 1 else {
            throw JedError.corruption("invalid timezone bundle header")
        }
        func string(_ reader: inout ByteReader) throws -> SQLText { let count = Int(try reader.u16()); return try reader.text(count) }
        version = try string(&reader); description = try string(&reader)
        let zoneCount = Int(try reader.u16()), linkCount = Int(try reader.u16())
        var manifest: [(SQLText, UInt32, Int, Int, Int)] = []
        for _ in 0..<zoneCount {
            let name = try string(&reader)
            guard !name.utf8.isEmpty, manifest.last.map({ $0.0 < name }) ?? true else {
                throw JedError.corruption("invalid timezone manifest order")
            }
            manifest.append((name, try reader.u32(), Int(try reader.u32()), Int(try reader.u32()), Int(try reader.u32())))
        }
        var links: [(SQLText, SQLText)] = []
        for _ in 0..<linkCount {
            let alias = try string(&reader), target = try string(&reader)
            guard !alias.utf8.isEmpty, links.last.map({ $0.0 < alias }) ?? true,
                  !manifest.contains(where: { $0.0 == alias }), manifest.contains(where: { $0.0 == target }) else {
                throw JedError.corruption("invalid timezone alias")
            }
            links.append((alias, target))
        }
        var zones: [SQLTimeZone] = []
        for (name, checksum, rawLength, compressedLength, offset) in manifest {
            guard offset == reader.offset else { throw JedError.corruption("invalid timezone body offset") }
            let compressed = Array(try reader.take(compressedLength))
            let body = try LZ4.decompress(compressed, rawLength: rawLength)
            guard Checksums.crc32(body) == checksum else { throw JedError.corruption("invalid timezone section checksum") }
            zones.append(SQLTimeZone(name: name, version: version, hash: checksum, data: try TZifData(body)))
        }
        try reader.requireEnd()
        for (alias, target) in links {
            let zone = zones.first(where: { $0.name == target })!
            zones.append(SQLTimeZone(name: alias, version: version, hash: zone.contentHash!, data: zone.data!))
        }
        self.zones = zones; self.links = links; zoneNames = manifest.map { $0.0 }; original = bytes
    }
}

internal struct TZifData: Sendable {
    private let transitions: [Int64]
    private let transitionTypes: [UInt8]
    private let types: [TimeZoneOffset]
    private let footer: POSIXClock?

    init(_ bytes: [UInt8]) throws {
        var reader = ByteReader(bytes)
        struct Header {
            let version: UInt8, utc: Int, standard: Int, leaps: Int, transitions: Int, types: Int, characters: Int
            func length(_ width: Int) -> Int { transitions * (width + 1) + types * 6 + characters + leaps * (width + 4) + standard + utc }
        }
        func header(_ r: inout ByteReader) throws -> Header {
            guard Array(try r.take(4)) == Array("TZif".utf8) else { throw JedError.corruption("invalid TZif magic") }
            let version = try r.u8()
            guard [0, 50, 51].contains(version), try r.take(15).allSatisfy({ $0 == 0 }) else { throw JedError.corruption("invalid TZif header") }
            let h = Header(version: version, utc: Int(try r.u32()), standard: Int(try r.u32()), leaps: Int(try r.u32()),
                           transitions: Int(try r.u32()), types: Int(try r.u32()), characters: Int(try r.u32()))
            guard h.types >= 1 && h.types <= 256, h.length(version == 0 ? 4 : 8) >= 0 else { throw JedError.corruption("invalid TZif counts") }
            return h
        }
        var h = try header(&reader)
        var width = 4
        if h.version != 0 {
            _ = try reader.take(h.length(4))
            let second = try header(&reader)
            guard second.version == h.version else { throw JedError.corruption("inconsistent TZif versions") }
            h = second; width = 8
        }
        guard h.length(width) <= reader.remaining else { throw JedError.corruption("truncated TZif data") }
        var times: [Int64] = []
        for _ in 0..<h.transitions {
            let time = width == 8 ? try reader.i64() : Int64(try reader.i32())
            guard times.last.map({ $0 < time }) ?? true else { throw JedError.corruption("invalid TZif transition order") }
            times.append(time)
        }
        let indices = Array(try reader.take(h.transitions))
        guard indices.allSatisfy({ Int($0) < h.types }) else { throw JedError.corruption("invalid TZif transition type") }
        var records: [(Int32, UInt8, UInt8)] = []
        for _ in 0..<h.types { records.append((try reader.i32(), try reader.u8(), try reader.u8())) }
        let abbreviations = Array(try reader.take(h.characters))
        var types: [TimeZoneOffset] = []
        for (offset, daylight, index) in records {
            guard daylight <= 1, Int(index) < abbreviations.count,
                  let end = abbreviations[Int(index)...].firstIndex(of: 0) else { throw JedError.corruption("invalid TZif local-time type") }
            let text = try SQLText(utf8: Array(abbreviations[Int(index)..<end]))
            types.append(TimeZoneOffset(seconds: offset, abbreviation: text.description, isDST: daylight == 1))
        }
        _ = try reader.take(h.leaps * (width + 4) + h.standard + h.utc)
        var rule: POSIXClock?
        if width == 8 && reader.remaining > 0 {
            let remaining = Array(try reader.take(reader.remaining))
            guard remaining.count >= 2, remaining.first == 10, remaining.last == 10,
                  !remaining.dropFirst().dropLast().contains(10) else { throw JedError.corruption("invalid TZif footer framing") }
            let body = Array(remaining.dropFirst().dropLast())
            if !body.isEmpty { rule = try POSIXClock(body) }
        }
        try reader.requireEnd()
        transitions = times; transitionTypes = indices; self.types = types; footer = rule
    }

    func offset(at seconds: Int64) -> TimeZoneOffset {
        if let footer, transitions.isEmpty || seconds >= transitions.last! { return footer.offset(at: seconds) }
        if transitions.isEmpty || seconds < transitions[0] { return types.first(where: { !$0.isDST }) ?? types[0] }
        var lower = 0, upper = transitions.count
        while lower < upper { let middle = lower + (upper - lower) / 2; if transitions[middle] <= seconds { lower = middle + 1 } else { upper = middle } }
        return types[Int(transitionTypes[lower - 1])]
    }
}

private struct POSIXClock: Sendable {
    struct Transition: Sendable {
        let month: Int64, week: Int64, weekday: Int64, seconds: Int32
        func instant(year: Int64, previousOffset: Int32) -> Int64 {
            let first = Temporal.daysFromCivil(year: year, month: month, day: 1)
            var day = 1 + Temporal.floorMod(weekday - Temporal.floorMod(first + 4, 7), 7) + (week - 1) * 7
            if day > Temporal.daysInMonth(year: year, month: month) { day -= 7 }
            return (first + day - 1) * 86_400 + Int64(seconds) - Int64(previousOffset)
        }
    }
    let standard: TimeZoneOffset
    let daylight: TimeZoneOffset?
    let start: Transition?
    let end: Transition?
    init(_ bytes: [UInt8]) throws {
        var parser = POSIXInput(bytes: bytes)
        let standardName = try parser.abbreviation(), offset = try parser.offset()
        standard = TimeZoneOffset(seconds: -offset, abbreviation: standardName, isDST: false)
        if parser.finished { daylight = nil; start = nil; end = nil; return }
        let daylightName = try parser.abbreviation()
        let daylightOffset = parser.peek == 44 || parser.finished ? offset - 3600 : try parser.offset()
        daylight = TimeZoneOffset(seconds: -daylightOffset, abbreviation: daylightName, isDST: true)
        if parser.finished { start = nil; end = nil; return }
        try parser.require(44); start = try parser.transition()
        try parser.require(44); end = try parser.transition()
        guard parser.finished else { throw JedError.corruption("invalid POSIX timezone rule") }
    }
    func offset(at seconds: Int64) -> TimeZoneOffset {
        guard let daylight, let start, let end else { return standard }
        let year = Temporal.civilFromDays(Temporal.floorDiv(seconds, 86_400)).year
        let begin = start.instant(year: year, previousOffset: standard.seconds)
        let finish = end.instant(year: year, previousOffset: daylight.seconds)
        let inside = begin < finish ? seconds >= begin && seconds < finish : seconds >= begin || seconds < finish
        return inside ? daylight : standard
    }
    static func fixedOffset(_ bytes: [UInt8]) -> Int32? {
        guard bytes.first == 43 || bytes.first == 45 else { return nil }
        var parser = POSIXInput(bytes: bytes)
        guard let value = try? parser.offset(), parser.finished else { return nil }
        return value
    }
}

private struct POSIXInput {
    let bytes: [UInt8]
    var position = 0
    var finished: Bool { position == bytes.count }
    var peek: UInt8? { finished ? nil : bytes[position] }
    mutating func require(_ byte: UInt8) throws { guard peek == byte else { throw JedError.corruption("invalid POSIX timezone rule") }; position += 1 }
    mutating func number(maximum: Int64) throws -> Int64 {
        let start = position
        var value: Int64 = 0
        while let byte = peek, (48...57).contains(byte) {
            guard value <= (maximum - Int64(byte - 48)) / 10 else { throw JedError.corruption("POSIX timezone field out of range") }
            value = value * 10 + Int64(byte - 48); position += 1
            guard value <= maximum else { throw JedError.corruption("POSIX timezone field out of range") }
        }
        guard position > start else { throw JedError.corruption("missing POSIX timezone field") }
        return value
    }
    mutating func abbreviation() throws -> String {
        var result: [UInt8] = []
        if peek == 60 {
            position += 1
            while let byte = peek, byte != 62 { result.append(byte); position += 1 }
            try require(62)
        } else {
            while let byte = peek, (65...90).contains(byte) || (97...122).contains(byte) { result.append(byte); position += 1 }
        }
        guard result.count >= 3 else { throw JedError.corruption("invalid POSIX timezone abbreviation") }
        return try SQLText(utf8: result).description
    }
    mutating func offset() throws -> Int32 {
        var sign: Int64 = 1
        if peek == 43 || peek == 45 { if peek == 45 { sign = -1 }; position += 1 }
        let hours = try number(maximum: 167)
        var minutes: Int64 = 0, seconds: Int64 = 0
        if peek == 58 { position += 1; minutes = try number(maximum: 59)
            if peek == 58 { position += 1; seconds = try number(maximum: 60) }
        }
        return Int32(sign * (hours * 3600 + minutes * 60 + seconds))
    }
    mutating func transition() throws -> POSIXClock.Transition {
        try require(77)
        let month = try number(maximum: 12); try require(46)
        let week = try number(maximum: 5); try require(46)
        let weekday = try number(maximum: 6)
        guard month > 0 && week > 0 else { throw JedError.corruption("invalid POSIX transition date") }
        var seconds: Int32 = 7200
        if peek == 47 { position += 1; seconds = try offset() }
        return .init(month: month, week: week, weekday: weekday, seconds: seconds)
    }
}

private struct NumericZoneInput {
    let bytes: [UInt8]
    var position = 1
    init(_ bytes: [UInt8]) { self.bytes = bytes }
    mutating func integer() throws -> Int64 {
        while position < bytes.count && Temporal.space(bytes[position]) { position += 1 }
        var sign: Int64 = 1
        if position < bytes.count && (bytes[position] == 43 || bytes[position] == 45) { if bytes[position] == 45 { sign = -1 }; position += 1 }
        var value: Int64 = 0
        while position < bytes.count && (48...57).contains(bytes[position]) {
            value = value * 10 + Int64(bytes[position] - 48); position += 1
            if value > 2_147_483_648 { throw JedError("22023", "numeric time zone out of range") }
        }
        let result = value * sign
        guard result <= Int32.max else { throw JedError("22023", "numeric time zone out of range") }
        return result
    }
    mutating func parse() throws -> Int32? {
        var hours = try integer(), minutes: Int64 = 0, seconds: Int64 = 0
        if position < bytes.count && bytes[position] == 58 {
            position += 1; minutes = try integer()
            if position < bytes.count && bytes[position] == 58 { position += 1; seconds = try integer() }
        } else if position == bytes.count && bytes.count > 3 { minutes = hours % 100; hours /= 100 }
        guard (0...15).contains(hours), (0...59).contains(minutes), (0...59).contains(seconds) else { throw JedError("22023", "numeric time zone out of range") }
        guard position == bytes.count else { return nil }
        return Int32((bytes[0] == 45 ? -1 : 1) * (hours * 3600 + minutes * 60 + seconds))
    }
}
