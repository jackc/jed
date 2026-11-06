/// Shared template vocabulary with handwritten byte-oriented parsing. A literal
/// node holds one complete UTF-8 scalar; field widths always count input bytes.
internal enum FormatTemplate {
    struct Keyword: Sendable {
        let name: [UInt8]
        let id: String
        let digit: Bool
        let mode: String
    }
    struct Node: Sendable {
        enum Kind: Sendable { case field, literal, separator, space }
        let kind: Kind
        var bytes: [UInt8] = []
        var keyword: Keyword? = nil
        var fill = false
        var translation = false
        var ordinal: String? = nil
    }
    static let datetime = keywords("dch")
    static let numeric = keywords("num")
    private static func keywords(_ key: String) -> [Keyword] {
        guard case .array(let rows) = GeneratedSpec.formatting[key] else { preconditionFailure("missing formatting metadata") }
        return rows.map { row in
            guard case .table(let fields) = row, case .text(let name) = fields["name"], case .text(let id) = fields["id"] else { preconditionFailure("invalid formatting metadata") }
            var digit = false, mode = "none"
            if case .boolean(let flag) = fields["is_digit"] { digit = flag }
            if case .text(let text) = fields["date_mode"] { mode = text }
            return Keyword(name: Array(name.utf8), id: id, digit: digit, mode: mode)
        }
    }
    static func words(_ name: String) -> [String] {
        guard case .table(let fields) = GeneratedSpec.formatting["words"], case .array(let values) = fields[name] else { preconditionFailure("missing formatting words") }
        return values.map { guard case .text(let text) = $0 else { preconditionFailure("invalid formatting word") }; return text }
    }
    static func space(_ c: UInt8) -> Bool { c == 32 || (9...13).contains(c) }
    static func digit(_ c: UInt8) -> Bool { (48...57).contains(c) }
    static func alpha(_ c: UInt8) -> Bool { (65...90).contains(c) || (97...122).contains(c) }
    static func separator(_ c: UInt8) -> Bool { c > 32 && c < 127 && !alpha(c) && !digit(c) }
    static func length(_ bytes: [UInt8], _ index: Int) -> Int {
        guard index < bytes.count else { return 0 }
        let c = bytes[index]
        let n = c < 128 ? 1 : c < 224 ? 2 : c < 240 ? 3 : 4
        return min(n, bytes.count - index)
    }
    static func prefix(_ pattern: [UInt8], _ bytes: [UInt8], _ position: Int) -> Bool {
        position + pattern.count <= bytes.count && bytes[position..<(position + pattern.count)].elementsEqual(pattern)
    }
    static func parse(_ text: SQLText, datetime: Bool = true) -> [Node] {
        let bytes = text.utf8, vocabulary = datetime ? self.datetime : numeric
        var result: [Node] = [], position = 0
        while position < bytes.count {
            var fill = false, translation = false
            if datetime {
                for (spelling, isFill) in [("FM", true), ("fm", true), ("TM", false), ("tm", false)] where prefix(Array(spelling.utf8), bytes, position) {
                    fill = isFill; translation = !isFill; position += 2; break
                }
                if position == bytes.count { break }
            }
            if let keyword = vocabulary.first(where: { prefix($0.name, bytes, position) }) {
                position += keyword.name.count
                var ordinal: String? = nil
                if datetime {
                    for suffix in ["TH", "th", "SP"] where prefix(Array(suffix.utf8), bytes, position) {
                        position += 2; if suffix != "SP" { ordinal = suffix }; break
                    }
                }
                result.append(Node(kind: .field, keyword: keyword, fill: fill, translation: translation, ordinal: ordinal))
                continue
            }
            if bytes[position] == 34 {
                position += 1
                while position < bytes.count, bytes[position] != 34 {
                    if bytes[position] == 92, position + 1 < bytes.count { position += 1 }
                    let end = position + length(bytes, position)
                    result.append(Node(kind: .literal, bytes: Array(bytes[position..<end])))
                    position = end
                }
                if position < bytes.count { position += 1 }
            } else {
                if bytes[position] == 92, position + 1 < bytes.count, bytes[position + 1] == 34 { position += 1 }
                let end = position + length(bytes, position)
                let kind: Node.Kind = datetime && space(bytes[position]) ? .space : datetime && separator(bytes[position]) ? .separator : .literal
                result.append(Node(kind: kind, bytes: Array(bytes[position..<end])))
                position = end
            }
        }
        return result
    }
    static func ordinal(_ number: Int64, upper: Bool) -> String {
        let n = number.magnitude % 100
        let index = n >= 11 && n <= 13 ? 3 : n % 10 == 1 ? 0 : n % 10 == 2 ? 1 : n % 10 == 3 ? 2 : 3
        return words(upper ? "ordinal_upper" : "ordinal_lower")[index]
    }
    static func padded(_ number: Int64, width: Int) -> String {
        let digits = String(number.magnitude), zeros = String(repeating: "0", count: max(0, width - digits.count - (number < 0 ? 1 : 0)))
        return (number < 0 ? "-" : "") + zeros + digits
    }
}

/// PostgreSQL formatting helpers intentionally use wrapping 32-bit calendar
/// arithmetic. Interval years can exceed the exact calendar's useful range.
internal enum FormatCalendar {
    static func julian(_ year: Int64, _ month: Int64, _ day: Int64) -> Int32 {
        var y = Int32(truncatingIfNeeded: year), m = Int32(truncatingIfNeeded: month)
        if m > 2 { m = m &+ 1; y = y &+ 4800 } else { m = m &+ 13; y = y &+ 4799 }
        let century = y / 100
        var result = y &* 365 &- 32167
        result = result &+ y / 4 &- century &+ century / 4
        return result &+ (7834 &* m) / 256 &+ Int32(truncatingIfNeeded: day)
    }
    static func civil(_ julian: Int32) -> Temporal.Civil {
        var j = UInt32(bitPattern: julian) &+ 32044
        var q = j / 146097
        let extra = (j &- q &* 146097) &* 4 &+ 3
        j = j &+ 60 &+ q &* 3 &+ extra / 146097
        q = j / 1461
        j = j &- q &* 1461
        var year = j &* 4 / 1461
        j = (year != 0 ? (j &+ 305) % 365 : (j &+ 306) % 366) &+ 123
        year = year &+ q &* 4
        q = j &* 2141 / 65536
        return Temporal.Civil(year: Int64(Int32(bitPattern: year &- 4800)), month: Int64((q &+ 10) % 12 &+ 1), day: Int64(j &- (7834 &* q) / 256))
    }
    static func weekday(_ julian: Int32) -> Int32 { Int32(Temporal.floorMod(Int64(julian &+ 1), 7)) }
    static func isoStart(_ year: Int32) -> Int32 {
        let fourth = julian(Int64(year), 1, 4)
        return fourth &- weekday(fourth &- 1)
    }
    static func iso(_ c: Temporal.Civil) -> (year: Int32, week: Int32, day: Int32) {
        let day = julian(c.year, c.month, c.day)
        var year = Int32(truncatingIfNeeded: c.year)
        var start = isoStart(year)
        if day < start { year = year &- 1; start = isoStart(year) }
        var week = (day &- start) / 7 &+ 1
        if week >= 52 {
            let next = isoStart(year &+ 1)
            if day >= next { year = year &+ 1; start = next; week = (day &- start) / 7 &+ 1 }
        }
        return (year, week, day &- start &+ 1)
    }
}

internal enum TemporalFormat {
    static func toChar(_ value: SQLValue, template: SQLText, timezone: SQLTimeZone = .utc, meter: CostMeter? = nil) throws -> SQLValue {
        if value.isNull || template.utf8.isEmpty { return .null }
        var civil: Temporal.Civil, isInterval = false
        var offset: Int32 = 0, abbreviation = ""
        switch value {
        case .timestamp(let time):
            if Temporal.isInfinite(time) { return .null }
            civil = Temporal.civilFromMicros(time)
        case .timestamptz(let time):
            if Temporal.isInfinite(time) { return .null }
            try Temporal.chargeZone(meter)
            civil = Temporal.civilFromMicros(try timezone.instantToLocal(time))
            let zone = timezone.offset(at: time); offset = zone.seconds
            abbreviation = timezone.contentHash == nil && offset != 0 ? "" : zone.abbreviation
        case .interval(let interval):
            isInterval = true
            civil = Temporal.Civil(year: Int64(interval.months / 12), month: Int64(interval.months % 12), day: Int64(interval.days), hour: interval.microseconds / 3_600_000_000, minute: interval.microseconds / 60_000_000 % 60, second: interval.microseconds / 1_000_000 % 60, microsecond: interval.microseconds % 1_000_000)
        default: throw JedError("42883", "function does not exist")
        }
        let julian = FormatCalendar.julian(civil.year, civil.month, civil.day)
        let dow = Int(FormatCalendar.weekday(julian))
        let doy = isInterval ? Int64(Int32(truncatingIfNeeded: (civil.year * 12 + civil.month) * 30 + civil.day)) : Int64(julian &- FormatCalendar.julian(civil.year, 1, 1) &+ 1)
        let iso = FormatCalendar.iso(civil)
        func displayYear(_ y: Int64) -> Int64 { !isInterval && y <= 0 ? 1 - y : y }
        var output = ""
        for node in FormatTemplate.parse(template) {
            guard let key = node.keyword else { output += String(decoding: node.bytes, as: UTF8.self); continue }
            let id = key.id
            if isInterval && ["MONTH", "Month", "month", "MON", "Mon", "mon", "DAY", "Day", "day", "DY", "Dy", "dy", "D", "ID", "A_D", "AD", "B_C", "BC", "a_d", "ad", "b_c", "bc", "TZ", "tz", "OF", "TZH", "TZM"].contains(id) {
                throw JedError("22007", "invalid format specification for an interval value")
            }
            var number: Int64? = nil, width = 0, text = ""
            switch id {
            case "FX": break
            case "HH", "HH12": number = civil.hour % 12 == 0 ? 12 : civil.hour % 12; width = 2 + (civil.hour < 0 ? 1 : 0)
            case "HH24": number = civil.hour; width = 2 + (civil.hour < 0 ? 1 : 0)
            case "MI": number = civil.minute; width = 2 + (civil.minute < 0 ? 1 : 0)
            case "SS": number = civil.second; width = 2 + (civil.second < 0 ? 1 : 0)
            case "SSSS": number = civil.hour * 3600 + civil.minute * 60 + civil.second
            case "MS": number = civil.microsecond / 1000; width = 3
            case "US": number = civil.microsecond; width = 6
            case "FF1", "FF2", "FF3", "FF4", "FF5", "FF6":
                width = Int(id.last!.asciiValue! - 48); number = civil.microsecond / Temporal.pow10(6 - width)
            case "AM", "PM", "am", "pm", "A_M", "P_M", "a_m", "p_m":
                let lower = id.first!.isLowercase, dotted = id.contains("_")
                let list = FormatTemplate.words(dotted ? "ampm_long" : "ampm")
                text = list[(civil.hour % 24 >= 12 ? 1 : 0) + (lower ? 0 : 2)]
            case "A_D", "AD", "B_C", "BC", "a_d", "ad", "b_c", "bc":
                let list = FormatTemplate.words(id.contains("_") ? "adbc_long" : "adbc")
                text = list[(civil.year <= 0 ? 1 : 0) + (id.first!.isLowercase ? 0 : 2)]
            case "MONTH", "Month", "month", "MON", "Mon", "mon":
                text = FormatTemplate.words(id.count == 5 ? "months_full" : "months_abbrev")[Int(civil.month - 1)]
                if id == "MONTH" || id == "MON" { text = text.uppercased() }
                if id == "month" || id == "mon" { text = text.lowercased() }
                if id.count == 5 && !node.fill && !node.translation { text += String(repeating: " ", count: 9 - text.count) }
            case "DAY", "Day", "day", "DY", "Dy", "dy":
                text = FormatTemplate.words(id.count == 3 ? "days_full" : "days_abbrev")[dow]
                if id == "DAY" || id == "DY" { text = text.uppercased() }
                if id == "day" || id == "dy" { text = text.lowercased() }
                if id.count == 3 && !node.fill && !node.translation { text += String(repeating: " ", count: 9 - text.count) }
            case "D": number = Int64(dow + 1)
            case "ID": number = Int64(dow == 0 ? 7 : dow)
            case "DD": number = civil.day; width = 2
            case "DDD": number = doy; width = 3
            case "IDDD": number = Int64(iso.day); width = 3
            case "MM": number = civil.month; width = 2 + (civil.month < 0 ? 1 : 0)
            case "Q": if civil.month != 0 { number = (civil.month - 1) / 3 + 1 }
            case "WW": number = Int64((Int32(truncatingIfNeeded: doy) &- 1) / 7 &+ 1); width = 2
            case "IW": number = Int64(iso.week); width = 2
            case "W": number = Int64((Int32(truncatingIfNeeded: civil.day) &- 1) / 7 &+ 1)
            case "J": number = Int64(julian)
            case "CC": number = isInterval ? civil.year / 100 : civil.year > 0 ? (civil.year - 1) / 100 + 1 : civil.year / 100 - 1; width = 2 + (number! < 0 ? 1 : 0)
            case "YYYY", "YYY", "YY", "Y", "IYYY", "IYY", "IY", "I":
                width = id.count; let year = displayYear(id.first == "I" ? Int64(iso.year) : civil.year)
                number = width == 4 ? year : year % Temporal.pow10(width)
                if number! < 0 { width += 1 }
            case "Y_YYY":
                let y = displayYear(civil.year)
                text = String(y / 1000) + "," + FormatTemplate.padded(y % 1000, width: 3)
            case "RM", "rm":
                var month = civil.month
                if month == 0, civil.year != 0 { month = civil.year > 0 ? 12 : 1 }
                if month != 0 {
                    let index = month < 0 ? Int(-month - 1) : Int(12 - month)
                    text = FormatTemplate.words(id == "RM" ? "rm_months_upper" : "rm_months_lower")[index]
                    if !node.fill { text += String(repeating: " ", count: 4 - text.count) }
                }
            case "TZ", "tz":
                guard abbreviation.utf8.count <= key.name.count * 12 else { throw JedError("22008", "time zone format value too long") }
                text = id == "tz" ? abbreviation.lowercased() : abbreviation
            case "TZH": text = (offset < 0 ? "-" : "+") + FormatTemplate.padded(Int64(offset.magnitude / 3600), width: 2)
            case "TZM": number = Int64(offset.magnitude / 60 % 60); width = 2
            case "OF":
                text = (offset < 0 ? "-" : "+") + FormatTemplate.padded(Int64(offset.magnitude / 3600), width: node.fill ? 0 : 2)
                if offset.magnitude % 3600 != 0 { text += ":" + FormatTemplate.padded(Int64(offset.magnitude / 60 % 60), width: 2) }
            default: preconditionFailure("unhandled datetime format keyword \(id)")
            }
            if let number {
                let fill = node.fill && !["MS", "US", "FF1", "FF2", "FF3", "FF4", "FF5", "FF6", "TZM"].contains(id)
                text = FormatTemplate.padded(number, width: fill ? 0 : width)
                if let ordinal = node.ordinal, id != "TZM" { text += FormatTemplate.ordinal(number, upper: ordinal == "TH") }
            }
            if id == "Y_YYY", let ordinal = node.ordinal { text += FormatTemplate.ordinal(displayYear(civil.year), upper: ordinal == "TH") }
            output += text
        }
        meter?.charge(Int64(output.utf8.count)); try meter?.guardCost()
        return .text(SQLText(validatedUTF8: Array(output.utf8)))
    }

    static func parse(_ input: SQLText, template: SQLText, asDate: Bool = false, timezone: SQLTimeZone = .utc, meter: CostMeter? = nil) throws -> SQLValue {
        var reader = DateTemplateReader(input: input.utf8, nodes: FormatTemplate.parse(template))
        let parts = try reader.read()
        if asDate { return .date(try Temporal.finiteDate(Temporal.daysFromCivil(year: parts.civil.year, month: parts.civil.month, day: parts.civil.day))) }
        let wall = try Temporal.microsFromCivil(parts.civil)
        var instant: Int64
        if let seconds = parts.offset { instant = try Temporal.finiteTimestamp(Int128(wall) - Int128(seconds) * 1_000_000) }
        else { try Temporal.chargeZone(meter); instant = try timezone.localToInstant(wall) }
        if let precision = parts.precision { instant = try Temporal.roundedTimestamp(instant, precision: precision) }
        return .timestamptz(instant)
    }
}

private struct DateTemplateReader {
    struct Parts {
        var civil: Temporal.Civil
        var offset: Int32?
        var precision: UInt8?
    }
    enum Field: Hashable { case hour, minute, second, secondsDay, day, dayYear, dayWeek, month, year, century, week, weekMonth, julian, millisecond, microsecond, pm, bc, offsetHour, offsetMinute }
    let input: [UInt8]
    let nodes: [FormatTemplate.Node]
    var position = 0
    var fields: [Field: Int64] = [:]
    var convention = "none"
    var clock12 = false
    var yearWidth = 0
    var offsetSign: Int64 = 0
    var precision: UInt8?
    private func value(_ field: Field) -> Int64 { fields[field] ?? 0 }
    private func overflow() -> JedError { JedError("22008", "date/time field value out of range: \"\(String(decoding: input, as: UTF8.self))\"") }
    private func checked(_ n: Int64) throws -> Int64 { guard n >= Int32.min, n <= Int32.max else { throw overflow() }; return n }
    private func invalid(_ bytes: ArraySlice<UInt8>, _ node: FormatTemplate.Node, _ detail: String, hint: Bool = false) -> JedError {
        var error = JedError("22007", "invalid value \"\(String(decoding: bytes, as: UTF8.self))\" for \"\(String(decoding: node.keyword!.name, as: UTF8.self))\"")
        error.detail = detail
        if hint { error.hint = "If your source string is not fixed-width, try using the \"FM\" modifier." }
        return error
    }
    private mutating func set(_ field: Field, _ n: Int64, node: FormatTemplate.Node) throws {
        if value(field) != 0 && value(field) != n {
            var error = JedError("22007", "conflicting values for \"\(String(decoding: node.keyword!.name, as: UTF8.self))\" field in formatting string")
            error.detail = "This value contradicts a previous setting for the same field type."
            throw error
        }
        fields[field] = n
    }
    private func integerScan(_ start: Int, end: Int) -> (value: Int64, end: Int, overflow: Bool) {
        var p = start
        while p < end && FormatTemplate.space(input[p]) { p += 1 }
        var sign: Int64 = 1
        if p < end && (input[p] == 43 || input[p] == 45) { if input[p] == 45 { sign = -1 }; p += 1 }
        let first = p
        var magnitude: Int64 = 0, tooLarge = false
        while p < end && FormatTemplate.digit(input[p]) {
            if magnitude > 214_748_364 { tooLarge = true }
            if !tooLarge { magnitude = magnitude * 10 + Int64(input[p] - 48) }
            p += 1
        }
        if p == first { return (0, start, false) }
        return (magnitude * sign, p, tooLarge || magnitude * sign < Int32.min || magnitude * sign > Int32.max)
    }
    private func variableWidth(_ index: Int) -> Bool {
        let node = nodes[index]
        if node.fill || node.ordinal != nil || index + 1 == nodes.count { return true }
        let next = nodes[index + 1]
        if let key = next.keyword { return !key.digit }
        return next.bytes.count != 1 || !FormatTemplate.digit(next.bytes[0])
    }
    @discardableResult private mutating func number(_ field: Field?, index: Int, width: Int? = nil, forceVariable: Bool = false) throws -> (value: Int64, consumed: Int) {
        let node = nodes[index], width = width ?? node.keyword!.name.count
        let start = position
        while position < input.count && FormatTemplate.space(input[position]) { position += 1 }
        let slice = input[position..<min(position + width, input.count)]
        let variable = forceVariable || variableWidth(index)
        if !variable && input.count - position < width {
            var error = JedError("22007", "source string too short for \"\(String(decoding: node.keyword!.name, as: UTF8.self))\" formatting field")
            error.detail = "Field requires \(width) characters, but only \(input.count - position) remain."
            error.hint = "If your source string is not fixed-width, try using the \"FM\" modifier."
            throw error
        }
        let scanStart = variable ? start : position
        let scan = integerScan(scanStart, end: variable ? input.count : position + width)
        if !variable && scan.end > scanStart && scan.end - scanStart < width {
            throw invalid(slice, node, "Field requires \(width) characters, but only \(scan.end - scanStart) could be parsed.", hint: true)
        }
        position = scan.end
        if position == start { throw invalid(slice, node, "Value must be an integer.") }
        if scan.overflow {
            var error = JedError("22008", "value for \"\(String(decoding: node.keyword!.name, as: UTF8.self))\" in source string is out of range")
            error.detail = "Value must be in the range -2147483648 to 2147483647."
            throw error
        }
        if let field { try set(field, scan.value, node: node) }
        return (scan.value, position - start)
    }
    private mutating func word(_ list: String, index: Int) throws -> Int {
        let words = FormatTemplate.words(list)
        for (number, word) in words.enumerated() {
            let bytes = Array(word.utf8)
            if position + bytes.count <= input.count && zip(bytes, input[position..<(position + bytes.count)]).allSatisfy({ Temporal.asciiLower($0.0) == Temporal.asciiLower($0.1) }) {
                position += bytes.count; return number
            }
        }
        var end = position
        while end < input.count && !FormatTemplate.space(input[end]) { end += 1 }
        throw invalid(input[position..<end], nodes[index], "The given value did not match any of the allowed values for this field.")
    }
    private mutating func timezone(index: Int, combined: Bool, skipped: Int) throws {
        let node = nodes[index]
        if ["TZ", "tz"].contains(node.keyword?.id ?? "") && position < input.count && FormatTemplate.alpha(input[position]) {
            throw JedError("0A000", "time zone abbreviations are not supported")
        }
        var sign: Int64 = 1
        if position < input.count && (input[position] == 43 || input[position] == 45) {
            sign = input[position] == 45 ? -1 : 1; position += 1
        } else if skipped > 0 && position > 0 && input[position - 1] == 45 { sign = -1 }
        offsetSign = sign
        try number(.offsetHour, index: index, width: 2, forceVariable: combined)
        if combined && position < input.count && input[position] == 58 {
            position += 1
            let minute = try number(nil, index: index, width: 2, forceVariable: true).value
            try set(.offsetMinute, minute, node: node)
        }
    }
    mutating func read() throws -> Parts {
        var fixed = false, skipped = 0
        for (index, node) in nodes.enumerated() {
            if position >= input.count { break }
            if !fixed && node.keyword?.id != "FX" && (node.kind == .field || index == 0) {
                while position < input.count && FormatTemplate.space(input[position]) { position += 1; skipped += 1 }
            }
            if node.kind == .separator || node.kind == .space {
                if fixed { position += FormatTemplate.length(input, position) }
                else {
                    skipped -= 1
                    if position < input.count && (FormatTemplate.space(input[position]) || FormatTemplate.separator(input[position])) { position += 1; skipped += 1 }
                }
                continue
            }
            guard let key = node.keyword else {
                if !fixed && skipped > 0 { skipped -= 1 } else { position += FormatTemplate.length(input, position) }
                continue
            }
            if key.mode != "none" {
                if convention != "none" && convention != key.mode {
                    var error = JedError("22007", "invalid combination of date conventions")
                    error.hint = "Do not mix Gregorian and ISO week date conventions in a formatting template."
                    throw error
                }
                convention = key.mode
            }
            let id = key.id
            switch id {
            case "FX": fixed = true
            case "HH", "HH12": try number(.hour, index: index, width: 2); clock12 = true
            case "HH24": try number(.hour, index: index, width: 2)
            case "MI": try number(.minute, index: index)
            case "SS": try number(.second, index: index)
            case "SSSS": try number(.secondsDay, index: index)
            case "MS":
                let read = try number(.millisecond, index: index, width: 3)
                if read.consumed == 1 || read.consumed == 2 { fields[.millisecond] = Int64(Int32(truncatingIfNeeded: read.value * Temporal.pow10(3 - read.consumed))) }
            case "US", "FF1", "FF2", "FF3", "FF4", "FF5", "FF6":
                let width = id == "US" ? 6 : Int(id.last!.asciiValue! - 48)
                if id != "US" { precision = UInt8(width) }
                let read = try number(.microsecond, index: index, width: width)
                if read.consumed > 0 && read.consumed < 6 { fields[.microsecond] = Int64(Int32(truncatingIfNeeded: read.value * Temporal.pow10(6 - read.consumed))) }
            case "AM", "PM", "am", "pm", "A_M", "P_M", "a_m", "p_m":
                let match = try word(id.contains("_") ? "ampm_long" : "ampm", index: index)
                try set(.pm, Int64(match % 2), node: node); clock12 = true
            case "AD", "BC", "ad", "bc", "A_D", "B_C", "a_d", "b_c":
                let match = try word(id.contains("_") ? "adbc_long" : "adbc", index: index)
                try set(.bc, Int64(match % 2), node: node)
            case "MONTH", "Month", "month", "MON", "Mon", "mon":
                let match = try word(id.count == 5 ? "months_full" : "months_abbrev", index: index)
                try set(.month, Int64(match + 1), node: node)
            case "DAY", "Day", "day", "DY", "Dy", "dy":
                let match = try word(id.count == 3 ? "days_full" : "days_abbrev", index: index)
                try set(.dayWeek, Int64(match + 1), node: node)
            case "RM", "rm": let match = try word("rm_months_lower", index: index); try set(.month, Int64(12 - match), node: node)
            case "MM": try number(.month, index: index)
            case "DD": try number(.day, index: index)
            case "DDD", "IDDD": try number(.dayYear, index: index)
            case "D": try number(.dayWeek, index: index)
            case "ID":
                let day = try number(nil, index: index).value
                try set(.dayWeek, day == 7 ? 1 : day + 1, node: node)
            case "WW", "IW": try number(.week, index: index)
            case "W": try number(.weekMonth, index: index)
            case "Q": try number(nil, index: index)
            case "CC": try number(.century, index: index)
            case "J": try number(.julian, index: index)
            case "YYYY", "IYYY", "YYY", "IYY", "YY", "IY", "Y", "I":
                let read = try number(.year, index: index)
                yearWidth = key.name.count
                if yearWidth < 4 && read.consumed < 4 {
                    fields[.year] = read.value < 70 ? read.value + 2000 : read.value < 100 ? read.value + 1900 : read.value < 520 ? read.value + 2000 : read.value < 1000 ? read.value + 1000 : read.value
                }
            case "Y_YYY":
                let high = integerScan(position, end: input.count)
                guard high.end > position, high.end < input.count, input[high.end] == 44 else { throw invalid(input[position...], node, "Value must be an integer.") }
                position = high.end + 1
                while position < input.count && FormatTemplate.space(input[position]) { position += 1 }
                let low = integerScan(position, end: min(position + 3, input.count))
                guard low.end > position else { throw invalid(input[position...], node, "Value must be an integer.") }
                position = low.end
                if high.overflow || low.overflow { throw overflow() }
                try set(.year, checked(high.value * 1000 + low.value), node: node); yearWidth = 4
            case "TZH": try timezone(index: index, combined: false, skipped: skipped)
            case "TZM": try number(.offsetMinute, index: index, width: 2); if offsetSign == 0 { offsetSign = 1 }
            case "TZ", "tz", "OF": try timezone(index: index, combined: true, skipped: skipped)
            default: preconditionFailure("unhandled datetime input keyword \(id)")
            }
            if node.ordinal != nil { for _ in 0..<2 { position += FormatTemplate.length(input, position) } }
            if !fixed {
                skipped = 0
                while position < input.count && FormatTemplate.space(input[position]) { position += 1; skipped += 1 }
            }
        }
        return try assemble()
    }
    private func assemble() throws -> Parts {
        var civil = Temporal.Civil(year: 0, month: 1, day: 1)
        var explicitYear = false, explicitMonth = false, explicitDay = false
        let secondsDay = value(.secondsDay)
        if secondsDay != 0 { civil.hour = secondsDay / 3600; civil.minute = secondsDay % 3600 / 60; civil.second = secondsDay % 60 }
        if value(.hour) != 0 { civil.hour = value(.hour) }
        if value(.minute) != 0 { civil.minute = value(.minute) }
        if value(.second) != 0 { civil.second = value(.second) }
        if clock12 {
            guard civil.hour >= 1 && civil.hour <= 12 else {
                var error = JedError("22007", "hour \"\(civil.hour)\" is invalid for the 12-hour clock")
                error.hint = "Use the 24-hour clock, or give an hour between 1 and 12."
                throw error
            }
            if value(.pm) != 0 && civil.hour < 12 { civil.hour += 12 }
            if value(.pm) == 0 && civil.hour == 12 { civil.hour = 0 }
        }
        let suppliedYear = value(.year), century = value(.century) * (value(.bc) == 0 ? 1 : -1)
        if suppliedYear != 0 {
            if century != 0 && yearWidth <= 2 {
                let low = suppliedYear % 100
                if low == 0 { civil.year = Int64(Int32(truncatingIfNeeded: century * 100 + (century < 0 ? 1 : 0))) }
                else { civil.year = try checked(century >= 0 ? checked((century - 1) * 100) + low : checked((century + 1) * 100) - low + 1) }
            } else {
                civil.year = suppliedYear * (value(.bc) == 0 ? 1 : -1)
                if civil.year < 0 { civil.year += 1 }
            }
            explicitYear = true
        } else if century != 0 {
            civil.year = try checked(checked((century >= 0 ? century - 1 : century) * 100) + 1)
            explicitYear = true
        }
        if value(.julian) != 0 {
            let c = FormatCalendar.civil(Int32(value(.julian)))
            civil.year = c.year; civil.month = c.month; civil.day = c.day
            explicitYear = true; explicitMonth = true; explicitDay = true
        }
        var ordinalDay = value(.dayYear), day = value(.day)
        if value(.week) != 0 {
            if convention == "isoweek" {
                var julian = FormatCalendar.isoStart(Int32(truncatingIfNeeded: civil.year)) &+ (Int32(value(.week)) &- 1) &* 7
                if value(.dayWeek) != 0 { julian = julian &+ Int32(value(.dayWeek) > 1 ? value(.dayWeek) - 2 : 6) }
                let c = FormatCalendar.civil(julian)
                civil.year = c.year; civil.month = c.month; civil.day = c.day
                explicitYear = true; explicitMonth = true; explicitDay = true
            } else { ordinalDay = try checked(checked(checked(value(.week) - 1) * 7) + 1) }
        }
        if value(.weekMonth) != 0 { day = try checked(checked(checked(value(.weekMonth) - 1) * 7) + 1) }
        if day != 0 { civil.day = day; explicitDay = true }
        if value(.month) != 0 { civil.month = value(.month); explicitMonth = true }
        if ordinalDay != 0 && (civil.month <= 1 || civil.day <= 1) {
            if civil.year == 0 && value(.bc) == 0 { throw JedError("22007", "cannot calculate day of year without year information") }
            if convention == "isoweek" {
                let c = FormatCalendar.civil(FormatCalendar.isoStart(Int32(truncatingIfNeeded: civil.year)) &- 1 &+ Int32(truncatingIfNeeded: ordinalDay))
                civil.year = c.year; civil.month = c.month; civil.day = c.day; explicitYear = true
            } else {
                var month: Int64 = 1, previous: Int64 = 0
                while month <= 12 {
                    let end = previous + Temporal.daysInMonth(year: civil.year, month: month)
                    if ordinalDay <= end { break }
                    previous = end; month += 1
                }
                if civil.month <= 1 { civil.month = month }
                if civil.day <= 1 { civil.day = ordinalDay - previous }
            }
            explicitMonth = true; explicitDay = true
        }
        let milli = try checked(value(.millisecond) * 1000)
        civil.microsecond = Int64(Int32(truncatingIfNeeded: milli + value(.microsecond)))
        if explicitMonth && !(1...12).contains(civil.month) || explicitDay && !(1...31).contains(civil.day) { throw overflow() }
        if explicitYear && explicitMonth && explicitDay && civil.day > Temporal.daysInMonth(year: civil.year, month: civil.month) { throw overflow() }
        guard (0..<24).contains(civil.hour), (0..<60).contains(civil.minute), (0..<60).contains(civil.second), (0..<1_000_000).contains(civil.microsecond) else { throw overflow() }
        var offset: Int32? = nil
        if offsetSign != 0 {
            guard (0...15).contains(value(.offsetHour)), (0..<60).contains(value(.offsetMinute)) else { throw JedError("22009", "time zone displacement out of range: \"\(String(decoding: input, as: UTF8.self))\"") }
            offset = Int32((value(.offsetHour) * 3600 + value(.offsetMinute) * 60) * offsetSign)
        }
        return Parts(civil: civil, offset: offset, precision: precision)
    }
}
