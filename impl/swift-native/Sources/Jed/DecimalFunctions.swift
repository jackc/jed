/// Decimal kernels receive already resolved, non-NULL operands. The expression
/// evaluator owns the function/operator's base charge; these helpers charge only
/// the additional work required by spec/design/cost.md.
internal enum DecimalFunctions {
    static func work(_ operation: String, _ left: SQLDecimal, _ right: SQLDecimal) -> Int64 {
        func groups(_ digits: Int) -> Int64 { Int64(max(1, (digits + 3) / 4)) }
        let scale = max(left.scale, right.scale)
        let a = groups(left.precision + scale - left.scale)
        let b = groups(right.precision + scale - right.scale)
        switch operation {
        case "*": return groups(left.precision) * groups(right.precision)
        case "/":
            let shift = left.divisionScale(right) + right.scale - left.scale
            return shift >= 0 ? groups(left.precision + shift) * groups(right.precision)
                : groups(left.precision) * groups(right.precision - shift)
        case "%": return a * b
        default: return max(a, b)
        }
    }

    static func chargeWork(_ operation: String, _ left: SQLDecimal, _ right: SQLDecimal,
                           meter: CostMeter) throws {
        if (operation == "/" || operation == "%") && right.isZero {
            throw JedError("22012", "division by zero")
        }
        meter.charge(work(operation, left, right) - 1)
        try meter.guardCost()
    }

    static func binary(_ operation: String, _ left: SQLDecimal, _ right: SQLDecimal,
                       meter: CostMeter) throws -> SQLDecimal {
        try chargeWork(operation, left, right, meter: meter)
        switch operation {
        case "+": return try left.adding(right)
        case "-": return try left.subtracting(right)
        case "*": return try left.multiplying(right)
        case "/": return try left.dividing(right)
        case "%": return try left.modulo(right)
        default: throw JedError("XX000", "unknown decimal arithmetic operation")
        }
    }

    static func minimumScale(_ value: SQLDecimal) -> Int {
        if value.isZero { return 0 }
        var trailing = 0
        for digit in value.magnitude.description.utf8.reversed() {
            if digit != 48 || trailing == value.scale { break }
            trailing += 1
        }
        return value.scale - trailing
    }

    static func call(name: String, args: [SQLDecimal], meter: CostMeter) throws -> SQLDecimal {
        guard let first = args.first else { throw JedError("42883", "function does not exist") }
        switch name {
        case "abs": return first.absolute()
        case "sign": return SQLDecimal(first.isZero ? 0 : first.negative ? -1 : 1)
        case "round", "trunc":
            let places = args.count > 1 ? try integerArgument(args[1]) : 0
            return name == "round" ? try first.rounded(to: Int(places)) : try first.truncated(to: Int(places))
        case "ceil", "ceiling", "floor":
            let truncated = try first.truncated()
            if truncated == first { return truncated }
            if name == "floor" && first.negative { return try truncated.subtracting(SQLDecimal(1)) }
            if name != "floor" && !first.negative { return try truncated.adding(SQLDecimal(1)) }
            return truncated
        case "trim_scale": return first.rescaledUncapped(to: minimumScale(first), round: false)
        case "mod":
            guard args.count == 2 else { throw JedError("42883", "function does not exist") }
            return try binary("%", first, args[1], meter: meter)
        case "div":
            guard args.count == 2 else { throw JedError("42883", "function does not exist") }
            return try first.divided(args[1], scale: 0, round: false).checked()
        case "gcd", "lcm":
            guard args.count == 2 else { throw JedError("42883", "function does not exist") }
            let second = args[1]
            let scale = max(first.scale, second.scale)
            var a = first.magnitude.shiftedDecimal(scale - first.scale)
            var b = second.magnitude.shiftedDecimal(scale - second.scale)
            while !b.isZero {
                let remainder = a.divided(by: b).remainder
                a = b
                b = remainder
            }
            if name == "gcd" { return SQLDecimal(negative: false, magnitude: a, scale: scale) }
            if first.isZero || second.isZero { return SQLDecimal(negative: false, magnitude: BigMagnitude(0), scale: scale) }
            let left = first.magnitude.shiftedDecimal(scale - first.scale)
            let right = second.magnitude.shiftedDecimal(scale - second.scale)
            return try SQLDecimal(negative: false, magnitude: left.divided(by: a).quotient.multiplying(right), scale: scale).checked()
        case "factorial":
            let count = try integerArgument(first)
            guard count >= 0 else { throw JedError("22003", "factorial of a negative number is undefined") }
            var result = SQLDecimal(1)
            var factor: Int64 = 2
            while factor <= count {
                let next = SQLDecimal(factor)
                try chargeWork("*", result, next, meter: meter)
                result = try result.multiplying(next)
                factor += 1 // Format overflow occurs long before Int64.max.
            }
            return result
        case "sqrt", "exp", "ln", "log", "log10", "power", "pow":
            return try DecimalTranscendental.call(name: name, args: args, meter: meter)
        default: throw JedError("42883", "function does not exist")
        }
    }

    static func callInteger(name: String, args: [SQLDecimal], meter: CostMeter) throws -> Int64 {
        guard let first = args.first else { throw JedError("42883", "function does not exist") }
        switch name {
        case "scale": return Int64(first.scale)
        case "min_scale": return Int64(minimumScale(first))
        case "width_bucket":
            guard args.count == 4 else { throw JedError("42883", "function does not exist") }
            return try widthBucket(first, low: args[1], high: args[2], count: integerArgument(args[3]))
        default: throw JedError("42883", "function does not exist")
        }
    }

    private static func integerArgument(_ value: SQLDecimal) throws -> Int64 {
        let integer = value.rescaledUncapped(to: 0, round: false)
        guard integer == value else { throw JedError("42883", "function does not exist") }
        return try integer.integerValue()
    }

    static func widthBucket(_ operand: SQLDecimal, low: SQLDecimal, high: SQLDecimal, count: Int64) throws -> Int64 {
        guard count > 0 else { throw JedError("2201G", "count must be greater than zero") }
        let direction = low.compare(to: high)
        guard direction != 0 else { throw JedError("2201G", "lower bound cannot equal upper bound") }
        if direction * operand.compare(to: low) > 0 { return 0 }
        if direction * operand.compare(to: high) <= 0 {
            guard count < Int32.max else { throw JedError("22003", "integer out of range") }
            return count + 1
        }
        let distance = direction < 0 ? operand.addingUncapped(low.negated()) : low.addingUncapped(operand.negated())
        let width = direction < 0 ? high.addingUncapped(low.negated()) : low.addingUncapped(high.negated())
        let numerator = distance.multiplyingUncapped(SQLDecimal(count))
        var lower: Int64 = 0
        var upper = count
        while upper - lower > 1 {
            let middle = lower + (upper - lower) / 2
            if width.multiplyingUncapped(SQLDecimal(middle)).compare(to: numerator) <= 0 { lower = middle }
            else { upper = middle }
        }
        guard lower < Int32.max else { throw JedError("22003", "integer out of range") }
        return lower + 1
    }
}
