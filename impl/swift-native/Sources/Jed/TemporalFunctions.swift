/// Resolved temporal operations. The evaluator owns operator_eval and strict
/// NULL handling; timezone consultations are charged here at their semantic site.
internal enum TemporalFunctions {
    static func call(name: String, args: [SQLValue], timezone: SQLTimeZone = .utc,
                     database: TimeZoneDatabase = .init(), statementMicros: Int64? = nil,
                     meter: CostMeter) throws -> SQLValue {
        if args.contains(where: \.isNull) { return .null }
        switch name {
        case "now", "current_timestamp":
            guard let statementMicros else { throw JedError("55000", "statement clock is unavailable") }
            return .timestamptz(statementMicros)
        case "current_date", "localtimestamp":
            guard let statementMicros else { throw JedError("55000", "statement clock is unavailable") }
            try Temporal.chargeZone(meter)
            let local = try timezone.instantToLocal(statementMicros)
            return name == "current_date" ? .date(try Temporal.finiteDate(Temporal.floorDiv(local, Temporal.microsPerDay))) : .timestamp(local)
        case "make_date":
            guard args.count == 3 else { throw missing() }
            let c = try constructorDate(args)
            return .date(try Temporal.finiteDate(Temporal.daysFromCivil(year: c.year, month: c.month, day: c.day)))
        case "make_timestamp", "make_timestamptz":
            guard args.count == 6 || name == "make_timestamptz" && args.count == 7 else { throw missing() }
            var c = try constructorDate(args, maximumYear: 999_999)
            c.hour = try integer(args[3]); c.minute = try integer(args[4])
            let seconds = try floating(args[5])
            guard c.hour >= 0, c.hour <= 24, c.minute >= 0, c.minute <= 59,
                  seconds.isFinite, seconds >= 0, seconds <= 60 else { throw Temporal.overflow() }
            let time = Int128(c.hour) * 3_600_000_000 + Int128(c.minute) * 60_000_000 + Int128((seconds * 1_000_000).rounded(.toNearestOrAwayFromZero))
            guard time <= Temporal.microsPerDay else { throw Temporal.overflow() }
            c.hour = 0; c.minute = 0
            let wall = try Temporal.finiteTimestamp(Temporal.wideMicros(c) + time)
            if name == "make_timestamp" { return .timestamp(wall) }
            let zone = args.count == 7 ? try database.resolveConstructor(text(args[6])) : timezone
            try Temporal.chargeZone(meter)
            return .timestamptz(try zone.localToInstant(wall))
        case "make_interval":
            guard args.count == 7 else { throw missing() }
            let months = try Int128(integer(args[0])) * 12 + Int128(integer(args[1]))
            let days = try Int128(integer(args[2])) * 7 + Int128(integer(args[3]))
            let seconds = try floating(args[6])
            guard seconds.isFinite, abs(seconds * 1_000_000) < Double(Int64.max) else { throw Temporal.overflow("interval") }
            let time = try (Int128(integer(args[4])) * 60 + Int128(integer(args[5]))) * 60 * 1_000_000 + Int128((seconds * 1_000_000).rounded(.toNearestOrAwayFromZero))
            return .interval(try interval(months, days, time))
        case "justify_hours", "justify_days", "justify_interval":
            guard args.count == 1, case .interval(let value) = args[0] else { throw missing() }
            return .interval(try justify(value, mode: name))
        case "age":
            guard args.count == 1 || args.count == 2 else { throw missing() }
            var first = args[0], second = args.count == 2 ? args[1] : args[0]
            if args.count == 1 {
                guard let statementMicros else { throw JedError("55000", "statement clock is unavailable") }
                try Temporal.chargeZone(meter)
                let local = try timezone.instantToLocal(statementMicros)
                let midnight = try Temporal.finiteTimestamp(Int128(Temporal.floorDiv(local, Temporal.microsPerDay)) * Int128(Temporal.microsPerDay))
                if case .timestamptz = args[0] {
                    try Temporal.chargeZone(meter); first = .timestamptz(try timezone.localToInstant(midnight))
                } else { first = .timestamp(midnight) }
                second = args[0]
            }
            return .interval(try age(first, second, timezone: timezone, meter: meter))
        case "date_trunc":
            guard args.count == 2 || args.count == 3 else { throw missing() }
            let zone = args.count == 3 ? try database.resolve(text(args[2])) : timezone
            return try truncate(unit: text(args[0]), value: args[1], timezone: zone, meter: meter)
        case "date_part", "extract":
            guard args.count == 2 else { throw missing() }
            return try extract(unit: text(args[0]), value: args[1], asFloat: name == "date_part", timezone: timezone, meter: meter)
        case "timezone":
            guard args.count == 2 else { throw missing() }
            let zone = try database.resolve(text(args[0]))
            try Temporal.chargeZone(meter)
            switch args[1] {
            case .timestamp(let value): return .timestamptz(try zone.localToInstant(value))
            case .timestamptz(let value): return .timestamp(try zone.instantToLocal(value))
            default: throw missing()
            }
        case "to_timestamp" where args.count == 1:
            let seconds = try floating(args[0])
            if seconds == .infinity { return .timestamptz(.max) }
            if seconds == -.infinity { return .timestamptz(.min) }
            let shifted = ((seconds - 946_684_800) * 1_000_000).rounded(.toNearestOrEven)
            guard shifted.isFinite, shifted > -Double(Int64.max) - 946_684_800_000_000,
                  shifted < Double(Int64.max) - 946_684_800_000_000 else { throw Temporal.overflow() }
            return .timestamptz(try Temporal.finiteTimestamp(Int128(shifted) + Temporal.postgresEpoch))
        default: throw missing()
        }
    }

    private static func missing() -> JedError { JedError("42883", "function does not exist") }
    private static func integer(_ value: SQLValue) throws -> Int64 { guard case .integer(let result) = value else { throw missing() }; return result }
    private static func text(_ value: SQLValue) throws -> SQLText { guard case .text(let result) = value else { throw missing() }; return result }
    private static func floating(_ value: SQLValue) throws -> Double {
        switch value { case .f64(let bits): return Double(bitPattern: bits); case .f32(let bits): return Double(Float(bitPattern: bits)); default: throw missing() }
    }
    private static func constructorDate(_ args: [SQLValue], maximumYear: Int64 = 9_999_999) throws -> Temporal.Civil {
        let year = try integer(args[0]), month = try integer(args[1]), day = try integer(args[2])
        guard year != 0, year >= -maximumYear, year <= maximumYear else { throw Temporal.overflow("date") }
        let c = Temporal.Civil(year: year < 0 ? year + 1 : year, month: month, day: day)
        try Temporal.validateCivil(c, maximumYear: maximumYear)
        return c
    }
    static func interval(_ months: Int128, _ days: Int128, _ micros: Int128) throws -> SQLInterval {
        guard months >= Int32.min, months <= Int32.max, days >= Int32.min, days <= Int32.max,
              micros >= Int64.min, micros <= Int64.max else { throw Temporal.overflow("interval") }
        return SQLInterval(months: Int32(months), days: Int32(days), microseconds: Int64(micros))
    }
    static func negate(_ value: SQLInterval) throws -> SQLInterval {
        try interval(-Int128(value.months), -Int128(value.days), -Int128(value.microseconds))
    }
    static func add(_ left: SQLInterval, _ right: SQLInterval, subtract: Bool = false) throws -> SQLInterval {
        let sign: Int128 = subtract ? -1 : 1
        return try interval(Int128(left.months) + sign * Int128(right.months), Int128(left.days) + sign * Int128(right.days), Int128(left.microseconds) + sign * Int128(right.microseconds))
    }
    static func shiftDate(_ days: Int32, by delta: Int64, subtract: Bool = false) throws -> Int32 {
        if Temporal.isInfinite(days) { return days }
        let result = Int128(days) + (subtract ? -Int128(delta) : Int128(delta))
        guard result > Int32.min, result < Int32.max else { throw Temporal.overflow("date") }
        return Int32(result)
    }
    static func dateDifference(_ left: Int32, _ right: Int32) throws -> Int32 {
        guard !Temporal.isInfinite(left), !Temporal.isInfinite(right) else { throw JedError("22008", "cannot subtract infinite dates") }
        let difference = Int64(left) - Int64(right)
        guard difference >= Int32.min, difference <= Int32.max else { throw Temporal.overflow("date") }
        return Int32(difference)
    }
    static func timestampDifference(_ left: Int64, _ right: Int64) throws -> SQLInterval {
        guard !Temporal.isInfinite(left), !Temporal.isInfinite(right) else { throw Temporal.overflow("interval") }
        return try justify(interval(0, 0, Int128(left) - Int128(right)), mode: "justify_hours")
    }
    static func shift(_ value: Int64, by amount: SQLInterval, subtract: Bool = false,
                      timezone: SQLTimeZone? = nil, meter: CostMeter? = nil) throws -> Int64 {
        if Temporal.isInfinite(value) { return value }
        let sign: Int128 = subtract ? -1 : 1
        var result = value
        if amount.months != 0 {
            if timezone != nil { try Temporal.chargeZone(meter) }
            let wall = try timezone?.instantToLocal(result) ?? result
            var c = Temporal.civilFromMicros(wall)
            let month = c.year * 12 + c.month - 1 + Int64(sign) * Int64(amount.months)
            c.year = Temporal.floorDiv(month, 12); c.month = Temporal.floorMod(month, 12) + 1
            c.day = min(c.day, Temporal.daysInMonth(year: c.year, month: c.month))
            let shifted = try Temporal.microsFromCivil(c)
            result = try timezone?.localToInstant(shifted) ?? shifted
        }
        if amount.days != 0 {
            if timezone != nil { try Temporal.chargeZone(meter) }
            let wall = try timezone?.instantToLocal(result) ?? result
            let shifted = try Temporal.finiteTimestamp(Int128(wall) + sign * Int128(amount.days) * Int128(Temporal.microsPerDay))
            result = try timezone?.localToInstant(shifted) ?? shifted
        }
        return try Temporal.finiteTimestamp(Int128(result) + sign * Int128(amount.microseconds))
    }
    static func justify(_ value: SQLInterval, mode: String) throws -> SQLInterval {
        var months = Int128(value.months), days = Int128(value.days), micros = Int128(value.microseconds)
        let day = Int128(Temporal.microsPerDay)
        if mode == "justify_interval" && (days > 0 && micros > 0 || days < 0 && micros < 0) {
            months += days / 30; days %= 30
        }
        if mode != "justify_days" { days += micros / day; micros %= day }
        if mode != "justify_hours" {
            months += days / 30; days %= 30
            if months > 0 && (days < 0 || days == 0 && micros < 0 && mode == "justify_interval") { months -= 1; days += 30 }
            else if months < 0 && (days > 0 || days == 0 && micros > 0 && mode == "justify_interval") { months += 1; days -= 30 }
        }
        if mode != "justify_days" {
            if days > 0 && micros < 0 { days -= 1; micros += day }
            else if days < 0 && micros > 0 { days += 1; micros -= day }
        }
        return try interval(months, days, micros)
    }
    static func cast(_ value: SQLValue, to type: ScalarType, timezone: SQLTimeZone = .utc, meter: CostMeter? = nil) throws -> SQLValue {
        if value.isNull { return .null }
        let input: Int64
        let zoned: Bool
        switch value {
        case .date(let days):
            if type == .date { return value }
            input = Temporal.isInfinite(days) ? (days < 0 ? .min : .max) : try Temporal.finiteTimestamp(Int128(days) * Int128(Temporal.microsPerDay))
            zoned = false
        case .timestamp(let micros): input = micros; zoned = false
        case .timestamptz(let micros): input = micros; zoned = true
        default: throw JedError("42804", "cannot cast temporal value")
        }
        var result = input
        if zoned && type != .timestamptz {
            if !Temporal.isInfinite(input) { try Temporal.chargeZone(meter) }; result = try timezone.instantToLocal(input)
        } else if !zoned && type == .timestamptz {
            if !Temporal.isInfinite(input) { try Temporal.chargeZone(meter) }; result = try timezone.localToInstant(input)
        }
        switch type {
        case .date: return .date(Temporal.isInfinite(result) ? (result < 0 ? .min : .max) : try Temporal.finiteDate(Temporal.floorDiv(result, Temporal.microsPerDay)))
        case .timestamp: return .timestamp(result)
        case .timestamptz: return .timestamptz(result)
        default: throw JedError("42804", "cannot cast temporal value")
        }
    }
    static func age(_ first: SQLValue, _ second: SQLValue, timezone: SQLTimeZone, meter: CostMeter?) throws -> SQLInterval {
        let a: Int64, b: Int64, zoned: Bool
        switch (first, second) {
        case (.timestamp(let x), .timestamp(let y)): a = x; b = y; zoned = false
        case (.timestamptz(let x), .timestamptz(let y)): a = x; b = y; zoned = true
        default: throw missing()
        }
        guard !Temporal.isInfinite(a), !Temporal.isInfinite(b) else { throw Temporal.overflow("interval") }
        if zoned { try Temporal.chargeZone(meter); try Temporal.chargeZone(meter) }
        let x = Temporal.civilFromMicros(zoned ? try timezone.instantToLocal(a) : a)
        let y = Temporal.civilFromMicros(zoned ? try timezone.instantToLocal(b) : b)
        let negative = a < b, sign: Int64 = negative ? -1 : 1
        var year = sign * (x.year - y.year), month = sign * (x.month - y.month), day = sign * (x.day - y.day)
        var hour = sign * (x.hour - y.hour), minute = sign * (x.minute - y.minute), second = sign * (x.second - y.second)
        var micros = sign * (x.microsecond - y.microsecond)
        if micros < 0 { micros += 1_000_000; second -= 1 }
        if second < 0 { second += 60; minute -= 1 }
        if minute < 0 { minute += 60; hour -= 1 }
        if hour < 0 { hour += 24; day -= 1 }
        let earlier = negative ? x : y
        while day < 0 { day += Temporal.daysInMonth(year: earlier.year, month: earlier.month); month -= 1 }
        while month < 0 { month += 12; year -= 1 }
        return try interval(Int128(sign * (year * 12 + month)), Int128(sign * day), Int128(sign * (((hour * 60 + minute) * 60 + second) * 1_000_000 + micros)))
    }

    /// The factor's input width is capped by interval.md. Rational intermediates
    /// use our own limbs so a large common numerator cannot cause false overflow.
    static func scale(_ value: SQLInterval, factor: SQLDecimal, divide: Bool = false) throws -> SQLInterval {
        let reduced = factor.rescaledUncapped(to: DecimalFunctions.minimumScale(factor), round: false)
        guard max(1, reduced.precision - reduced.scale) <= 18, reduced.scale <= 18 else {
            throw JedError("22008", "interval factor has too many digits")
        }
        if divide && reduced.isZero { throw JedError("22012", "division by zero") }
        let coefficient = SQLDecimal(negative: reduced.negative, magnitude: reduced.magnitude, scale: 0)
        let denominator = SQLDecimal(negative: false, magnitude: BigMagnitude(1).shiftedDecimal(reduced.scale), scale: 0)
        let numerator = divide ? (reduced.negative ? denominator.negated() : denominator) : coefficient
        let divisor = divide ? coefficient.absolute() : denominator
        func quotientRemainder(_ value: SQLDecimal) throws -> (SQLDecimal, SQLDecimal) {
            let q = try value.divided(divisor, scale: 0, round: false)
            return (q, value.addingUncapped(q.multiplyingUncapped(divisor).negated()))
        }
        func rounded(_ value: SQLDecimal) throws -> SQLDecimal {
            let (q, r) = try quotientRemainder(value)
            let order = r.absolute().multiplyingUncapped(SQLDecimal(2)).compare(to: divisor)
            return order > 0 || order == 0 && (q.limbs.first ?? 0) % 2 != 0 ? q.addingUncapped(SQLDecimal(value.negative ? -1 : 1)) : q
        }
        let (months, fractionMonths) = try quotientRemainder(SQLDecimal(Int64(value.months)).multiplyingUncapped(numerator))
        let monthDays = try rounded(fractionMonths.multiplyingUncapped(SQLDecimal(30_000_000)))
        let monthDaysInteger = try monthDays.divided(SQLDecimal(1_000_000), scale: 0, round: false)
        let monthDaysFraction = monthDays.addingUncapped(monthDaysInteger.multiplyingUncapped(SQLDecimal(-1_000_000)))
        var (days, fractionDays) = try quotientRemainder(SQLDecimal(Int64(value.days)).multiplyingUncapped(numerator))
        var secondsRemainder = try rounded(fractionDays.multiplyingUncapped(SQLDecimal(Temporal.microsPerDay))
            .addingUncapped(monthDaysFraction.multiplyingUncapped(SQLDecimal(86_400)).multiplyingUncapped(divisor)))
        let wholeDays = try secondsRemainder.divided(SQLDecimal(Temporal.microsPerDay), scale: 0, round: false)
        days = days.addingUncapped(wholeDays).addingUncapped(monthDaysInteger)
        secondsRemainder = secondsRemainder.addingUncapped(wholeDays.multiplyingUncapped(SQLDecimal(-Temporal.microsPerDay)))
        let micros = try rounded(SQLDecimal(value.microseconds).multiplyingUncapped(numerator).addingUncapped(secondsRemainder.multiplyingUncapped(divisor)))
        guard let m = Int128(months.description), let d = Int128(days.description), let u = Int128(micros.description) else { throw Temporal.overflow("interval") }
        return try interval(m, d, u)
    }

    static func isoWeek(_ days: Int64) -> (week: Int64, year: Int64) {
        let thursday = days + 3 - Temporal.floorMod(days + 3, 7)
        let year = Temporal.civilFromDays(thursday).year
        let january4 = Temporal.daysFromCivil(year: year, month: 1, day: 4)
        let firstMonday = january4 - Temporal.floorMod(january4 + 3, 7)
        return ((days - firstMonday) / 7 + 1, year)
    }
    private static func typeName(_ value: SQLValue) -> String {
        switch value { case .date: "date"; case .timestamp: "timestamp without time zone"; case .timestamptz: "timestamp with time zone"; case .interval: "interval"; default: "unknown" }
    }
    private static func unsupported(_ unit: SQLText, value: SQLValue) -> JedError {
        var error = JedError("0A000", "unit \"\(unit.asciiLowercased)\" not supported for type \(typeName(value))")
        if case .interval = value, unit.asciiLowercased.description == "week" { error.detail = "Months usually have fractional weeks." }
        return error
    }
    private static func unit(_ name: SQLText, value: SQLValue, truncation: Bool) throws -> String {
        let decoded = DateTimeUnits.lookup(name.description, includeDate: !truncation)
        if let decoded, decoded.kind == "units", let unit = decoded.unit { return unit }
        if !truncation, decoded?.kind == "reserv" {
            if decoded?.unit == "epoch" { return "epoch" }
            if case .interval = value {} else { throw unsupported(name, value: value) }
        }
        throw JedError("22023", "unit \"\(name.asciiLowercased)\" not recognized for type \(typeName(value))")
    }
    static func truncate(unit name: SQLText, value: SQLValue, timezone: SQLTimeZone = .utc, meter: CostMeter? = nil) throws -> SQLValue {
        let unit = try unit(name, value: value, truncation: true)
        if case .interval(let iv) = value {
            let quanta: [String: Int64] = ["microseconds": 1, "milliseconds": 1000, "second": 1_000_000, "minute": 60_000_000, "hour": 3_600_000_000]
            if let q = quanta[unit] { return .interval(SQLInterval(months: iv.months, days: iv.days, microseconds: iv.microseconds / q * q)) }
            if unit == "day" { return .interval(SQLInterval(months: iv.months, days: iv.days)) }
            let monthQuanta: [String: Int32] = ["month": 1, "quarter": 3, "year": 12, "decade": 120, "century": 1200, "millennium": 12000]
            if let q = monthQuanta[unit] { return .interval(SQLInterval(months: iv.months / q * q)) }
            throw unsupported(name, value: value)
        }
        let input: Int64, zoned: Bool
        switch value { case .timestamp(let v): input = v; zoned = false; case .timestamptz(let v): input = v; zoned = true; default: throw missing() }
        let valid = ["microseconds", "milliseconds", "second", "minute", "hour", "day", "week", "month", "quarter", "year", "decade", "century", "millennium"]
        guard valid.contains(unit) else { throw unsupported(name, value: value) }
        if Temporal.isInfinite(input) { return value }
        if zoned { try Temporal.chargeZone(meter) }
        let wall = zoned ? try timezone.instantToLocal(input) : input
        var c = Temporal.civilFromMicros(wall)
        switch unit {
        case "microseconds": break
        case "milliseconds": c.microsecond = c.microsecond / 1000 * 1000
        default:
            c.microsecond = 0
            if unit != "second" { c.second = 0 }
            if !["second", "minute"].contains(unit) { c.minute = 0 }
            if !["second", "minute", "hour"].contains(unit) { c.hour = 0 }
            if unit == "week" {
                let day = Temporal.daysFromCivil(year: c.year, month: c.month, day: c.day)
                c = Temporal.civilFromDays(day - Temporal.floorMod(day + 3, 7))
            } else if ["month", "quarter", "year", "decade", "century", "millennium"].contains(unit) {
                c.day = 1
                if unit == "quarter" { c.month = (c.month - 1) / 3 * 3 + 1 }
                else if unit != "month" {
                    c.month = 1
                    if unit == "decade" { c.year = Temporal.floorDiv(c.year, 10) * 10 }
                    if unit == "century" { c.year = Temporal.floorDiv(c.year - 1, 100) * 100 + 1 }
                    if unit == "millennium" { c.year = Temporal.floorDiv(c.year - 1, 1000) * 1000 + 1 }
                }
            }
        }
        let result = try Temporal.microsFromCivil(c)
        return zoned ? .timestamptz(try timezone.localToInstant(result)) : .timestamp(result)
    }

    static func extract(unit name: SQLText, value source: SQLValue, asFloat: Bool = false,
                        timezone: SQLTimeZone = .utc, meter: CostMeter? = nil) throws -> SQLValue {
        var value = source
        if asFloat, case .date = value { value = try cast(value, to: .timestamp) }
        let unit = try unit(name, value: value, truncation: false)
        let dateOnly: Bool
        if case .date = value { dateOnly = true } else { dateOnly = false }
        let timeUnits = ["microseconds", "milliseconds", "second", "minute", "hour"]
        let zoneUnits = ["timezone", "timezone_hour", "timezone_minute"]
        if dateOnly && (timeUnits.contains(unit) || zoneUnits.contains(unit)) { throw unsupported(name, value: value) }
        if !asFloat, case .timestamp = value, zoneUnits.contains(unit) { throw unsupported(name, value: value) }
        let infinite: Int64?
        switch value {
        case .timestamp(let v), .timestamptz(let v): infinite = Temporal.isInfinite(v) ? v : nil
        case .date(let v): infinite = Temporal.isInfinite(v) ? (v < 0 ? Int64.min : Int64.max) : nil
        default: infinite = nil
        }
        if let infinite {
            if ["year", "decade", "century", "millennium", "julian", "isoyear", "epoch"].contains(unit) {
                if asFloat { return .f64((infinite < 0 ? -Double.infinity : Double.infinity).bitPattern) }
                throw JedError("22003", "cannot extract field from an infinite \(dateOnly ? "date" : "timestamp")")
            }
            return .null
        }
        let decimal: SQLDecimal
        if case .interval(let iv) = value {
            decimal = try intervalPart(unit, iv, name: name)
        } else {
            let day: Int64, wall: Int64?, instant: Int64, offset: Int64
            let c: Temporal.Civil
            switch value {
            case .date(let v): day = Int64(v); c = Temporal.civilFromDays(day); wall = nil; instant = 0; offset = 0
            case .timestamp(let v): day = Temporal.floorDiv(v, Temporal.microsPerDay); c = Temporal.civilFromMicros(v); wall = v; instant = v; offset = 0
            case .timestamptz(let v):
                instant = v
                if unit == "epoch" { wall = v; offset = 0 }
                else { try Temporal.chargeZone(meter); wall = try timezone.instantToLocal(v); offset = Int64(timezone.offset(at: v).seconds) }
                day = Temporal.floorDiv(wall!, Temporal.microsPerDay); c = Temporal.civilFromMicros(wall!)
            default: throw missing()
            }
            let pgYear = c.year <= 0 ? c.year - 1 : c.year
            switch unit {
            case "microseconds": decimal = SQLDecimal(c.second * 1_000_000 + c.microsecond)
            case "milliseconds": decimal = scaled(Int128(c.second * 1_000_000 + c.microsecond), 3)
            case "second": decimal = scaled(Int128(c.second * 1_000_000 + c.microsecond), 6)
            case "minute": decimal = SQLDecimal(c.minute)
            case "hour": decimal = SQLDecimal(c.hour)
            case "day": decimal = SQLDecimal(c.day)
            case "month": decimal = SQLDecimal(c.month)
            case "quarter": decimal = SQLDecimal((c.month - 1) / 3 + 1)
            case "year": decimal = SQLDecimal(pgYear)
            case "decade": decimal = SQLDecimal(Temporal.floorDiv(c.year, 10))
            case "century": decimal = SQLDecimal(pgYear > 0 ? (pgYear + 99) / 100 : -((99 - pgYear) / 100))
            case "millennium": decimal = SQLDecimal(pgYear > 0 ? (pgYear + 999) / 1000 : -((999 - pgYear) / 1000))
            case "week": decimal = SQLDecimal(isoWeek(day).week)
            case "dow": decimal = SQLDecimal(Temporal.floorMod(day + 4, 7))
            case "isodow": decimal = SQLDecimal(Temporal.floorMod(day + 3, 7) + 1)
            case "doy": decimal = SQLDecimal(day - Temporal.daysFromCivil(year: c.year, month: 1, day: 1) + 1)
            case "isoyear": let year = isoWeek(day).year; decimal = SQLDecimal(year <= 0 ? year - 1 : year)
            case "epoch": decimal = dateOnly ? SQLDecimal(day * 86_400) : scaled(Int128(instant), 6)
            case "timezone", "timezone_hour", "timezone_minute":
                guard case .timestamptz = value else { throw unsupported(name, value: value) }
                decimal = SQLDecimal(unit == "timezone" ? offset : unit == "timezone_hour" ? offset / 3600 : offset % 3600 / 60)
            case "julian":
                if let wall {
                    let time = Temporal.floorMod(wall, Temporal.microsPerDay)
                    if asFloat {
                        let seconds = Double(time / 1_000_000) + Double(time % 1_000_000) / 1_000_000
                        return .f64((Double(day + 2_440_588) + seconds / 86_400).bitPattern)
                    }
                    decimal = try SQLDecimal(day + 2_440_588).adding(SQLDecimal(time).dividing(SQLDecimal(Temporal.microsPerDay)))
                } else { decimal = SQLDecimal(day + 2_440_588) }
            default: throw unsupported(name, value: value)
            }
        }
        return asFloat ? .f64((Double(decimal.description) ?? 0).bitPattern) : .decimal(decimal)
    }
    static func scaled(_ value: Int128, _ scale: Int) -> SQLDecimal {
        SQLDecimal(negative: value < 0, magnitude: BigMagnitude(decimalDigits: Array(String(value.magnitude).utf8)[...]), scale: scale)
    }
    private static func intervalPart(_ unit: String, _ value: SQLInterval, name: SQLText) throws -> SQLDecimal {
        let months = Int64(value.months), days = Int64(value.days), time = value.microseconds
        switch unit {
        case "microseconds": return SQLDecimal(time % 60_000_000)
        case "milliseconds": return scaled(Int128(time % 60_000_000), 3)
        case "second": return scaled(Int128(time % 60_000_000), 6)
        case "minute": return SQLDecimal(time / 60_000_000 % 60)
        case "hour": return SQLDecimal(time / 3_600_000_000)
        case "day": return SQLDecimal(days)
        case "week": return SQLDecimal(days / 7)
        case "month": return SQLDecimal(months % 12)
        case "quarter": return SQLDecimal(months >= 0 ? months % 12 / 3 + 1 : -((-months % 12) / 3 + 1))
        case "year": return SQLDecimal(months / 12)
        case "decade": return SQLDecimal(months / 120)
        case "century": return SQLDecimal(months / 1200)
        case "millennium": return SQLDecimal(months / 12000)
        case "epoch": return scaled(Int128(months / 12 * 31_557_600 + months % 12 * 2_592_000 + days * 86_400) * 1_000_000 + Int128(time), 6)
        default: throw unsupported(name, value: .interval(value))
        }
    }
}
