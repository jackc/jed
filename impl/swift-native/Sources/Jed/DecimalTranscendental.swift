/// Exact decimal transcendentals with the step and allocation contract from
/// decimal.md §8 / cost.md §8. Kernel values are always limb decimals. The only
/// binary-float seam is the spec's correctly-rounded decimal conversion and
/// pinned multiply used to choose a display scale; no libm logarithm or power is
/// used, and no approximate value is returned as a decimal result.
internal enum DecimalTranscendental {
    private static let log10e = 0.434294481903252
    private static let log10two = 0.301029995663981
    private static let one = SQLDecimal(1)
    private static let two = SQLDecimal(2)
    private static let ten = SQLDecimal(10)
    private static let lowerReductionBound = SQLDecimal(negative: false, magnitude: BigMagnitude(9), scale: 1)
    private static let upperReductionBound = SQLDecimal(negative: false, magnitude: BigMagnitude(11), scale: 1)

    static func call(name: String, args: [SQLDecimal], meter: CostMeter) throws -> SQLDecimal {
        guard let first = args.first else { throw JedError("42883", "function does not exist") }
        switch (name, args.count) {
        case ("sqrt", 1):
            try admit(meter, scale: 0, values: args)
            let weight = first.base10000Weight * 4 / 2 + 1
            let scale = displayScale(16 - weight, first.scale)
            return try root(first, scale: scale, meter: meter).checked()
        case ("exp", 1):
            try admit(meter, scale: 0, values: args)
            let estimate = min(2000, max(-2000, approximate(first) * log10e))
            return try exponential(first, scale: displayScale(16 - Int(estimate), first.scale), meter: meter).checked()
        case ("ln", 1):
            try admit(meter, scale: 0, values: args)
            try positiveLogArgument(first)
            let weight = try logarithmWeight(first, meter: meter)
            return try logarithm(first, scale: displayScale(16 - weight, first.scale), meter: meter).checked()
        case ("log", 1), ("log10", 1):
            try admit(meter, scale: 0, values: args)
            return try logarithm(base: ten, value: first, meter: meter)
        case ("log", 2):
            return try logarithm(base: first, value: args[1], meter: meter)
        case ("pow", 2), ("power", 2):
            try admit(meter, scale: 0, values: args)
            if first.isZero && args[1].negative {
                throw JedError("2201F", "zero raised to a negative power is undefined")
            }
            return try power(base: first, exponent: args[1], meter: meter)
        default: throw JedError("42883", "function does not exist")
        }
    }

    /// Admission precedes every potentially amplifying step. It always shares
    /// the caller's statement account, including estimates and recursive roots.
    internal static func admit(_ meter: CostMeter, scale: Int, squareRoot: Bool = false,
                               values: [SQLDecimal]) throws {
        var digits = 32 + abs(scale)
        for value in values { digits += value.precision + value.scale }
        let groups = Int64((digits + 3) / 4)
        var work = groups * groups
        if squareRoot {
            var bits: Int64 = 0
            var remaining = digits
            while remaining > 0 { bits += 1; remaining /= 2 }
            work *= bits
        }
        meter.charge(work)
        try meter.guardCost()
        try meter.reserveScalar(Int64(digits) * 8)
    }

    private static func displayScale(_ requested: Int, _ inputScales: Int...) -> Int {
        min(1000, max(0, inputScales.reduce(requested, max)))
    }

    private static func approximate(_ value: SQLDecimal) -> Double {
        // Decimal.description is ASCII canonical text; Swift's conversion is
        // correctly rounded. This estimate never supplies a kernel value.
        Double(value.description) ?? (value.negative ? -.infinity : .infinity)
    }

    private static func positiveLogArgument(_ value: SQLDecimal) throws {
        if value.isZero { throw JedError("2201E", "cannot take logarithm of zero") }
        if value.negative { throw JedError("2201E", "cannot take logarithm of a negative number") }
    }

    private static func overflow() -> JedError { JedError("22003", "numeric value out of range") }

    private static func root(_ value: SQLDecimal, scale: Int, meter: CostMeter) throws -> SQLDecimal {
        if value.isZero { return SQLDecimal(negative: false, magnitude: BigMagnitude(0), scale: max(0, scale)) }
        if value.negative { throw JedError("2201F", "cannot take square root of a negative number") }
        try admit(meter, scale: scale, squareRoot: true, values: [value])
        // Compute one extra decimal place, or enough to make the scaled
        // radicand integral. A floor root then carries every final rounding bit.
        var rootScale = max(scale, 0) + 1
        if 2 * rootScale < value.scale { rootScale = (value.scale + 1) / 2 + 1 }
        let radicand = value.magnitude.shiftedDecimal(2 * rootScale - value.scale)
        let exact = SQLDecimal(negative: false, magnitude: radicand.squareRoot(), scale: rootScale)
        return exact.rescaledUncapped(to: scale)
    }

    private static func exponential(_ value: SQLDecimal, scale: Int, meter: CostMeter) throws -> SQLDecimal {
        try admit(meter, scale: scale, values: [value])
        var estimate = approximate(value)
        if abs(estimate) >= 6000 {
            if estimate > 0 { throw overflow() }
            return SQLDecimal(negative: false, magnitude: BigMagnitude(0), scale: max(0, scale))
        }
        let weight = Int(estimate * log10e)
        var halvings = 0
        while abs(estimate) > 0.01 { estimate /= 2; halvings += 1 }
        let reduced: SQLDecimal
        if halvings > 0 {
            reduced = try value.divided(SQLDecimal(Int64(1) << halvings), scale: value.scale + halvings, round: true)
        } else { reduced = value }
        let significantDigits = max(0, 1 + weight + scale + Int(Double(halvings) * log10two)) + 8
        let localScale = significantDigits - 1
        var sum = one.addingUncapped(reduced)
        var denominator: Int64 = 2
        var term = try reduced.multiplying(reduced, scale: localScale)
            .divided(SQLDecimal(denominator), scale: localScale, round: true)
        while !term.isZero {
            try admit(meter, scale: localScale, values: [sum, term, reduced])
            sum = sum.addingUncapped(term)
            denominator += 1
            term = try term.multiplying(reduced, scale: localScale)
                .divided(SQLDecimal(denominator), scale: localScale, round: true)
        }
        for _ in 0..<halvings {
            try admit(meter, scale: scale, values: [sum])
            let squareScale = max(0, significantDigits - 8 * sum.base10000Weight)
            sum = sum.multiplying(sum, scale: squareScale)
        }
        return sum.rescaledUncapped(to: scale)
    }

    private static func logarithm(_ value: SQLDecimal, scale: Int, meter: CostMeter) throws -> SQLDecimal {
        try admit(meter, scale: scale, values: [value])
        var reduced = value
        var multiplier = two
        var roots = 0
        while reduced.compare(to: lowerReductionBound) <= 0 || reduced.compare(to: upperReductionBound) >= 0 {
            let rootScale = scale - reduced.base10000Weight * 4 / 2 + 8
            reduced = try root(reduced, scale: rootScale, meter: meter)
            multiplier = multiplier.multiplying(two, scale: 0)
            roots += 1
        }
        let localScale = scale + Int(Double(roots + 1) * log10two) + 8
        let ratio = try reduced.addingUncapped(one.negated())
            .divided(reduced.addingUncapped(one), scale: localScale, round: true)
        let squared = ratio.multiplying(ratio, scale: localScale)
        var power = ratio
        var sum = ratio
        var denominator: Int64 = 1
        while true {
            try admit(meter, scale: localScale, values: [sum, power, squared])
            denominator += 2
            power = power.multiplying(squared, scale: localScale)
            let term = try power.divided(SQLDecimal(denominator), scale: localScale, round: true)
            if term.isZero { break }
            sum = sum.addingUncapped(term)
            if term.base10000Weight < sum.base10000Weight - localScale * 2 / 4 { break }
        }
        return sum.multiplying(multiplier, scale: scale)
    }

    /// Weight estimates use exact preliminary logarithms, never platform log().
    private static func logarithmWeight(_ value: SQLDecimal, meter: CostMeter) throws -> Int {
        if value.compare(to: lowerReductionBound) >= 0 && value.compare(to: upperReductionBound) <= 0 {
            let difference = value.addingUncapped(one.negated())
            return difference.isZero ? 0 : difference.precision - 1 - difference.scale
        }
        let estimate = try logarithm(value, scale: 20, meter: meter)
        if estimate.isZero { return 0 }
        let weight = estimate.precision - 1 - estimate.scale
        return weight < 0 ? weight + 1 : weight
    }

    private static func logarithm(base: SQLDecimal, value: SQLDecimal, meter: CostMeter) throws -> SQLDecimal {
        try admit(meter, scale: 0, values: [base, value])
        try positiveLogArgument(base)
        try positiveLogArgument(value)
        let baseWeight = try logarithmWeight(base, meter: meter)
        let valueWeight = try logarithmWeight(value, meter: meter)
        let weight = valueWeight - baseWeight
        let scale = displayScale(16 - weight, base.scale, value.scale)
        let denominator = try logarithm(base, scale: max(0, scale + weight - baseWeight + 8), meter: meter)
        let numerator = try logarithm(value, scale: max(0, scale + weight - valueWeight + 8), meter: meter)
        return try numerator.divided(denominator, scale: scale, round: true).checked()
    }

    private static func power(base: SQLDecimal, exponent: SQLDecimal, meter: CostMeter) throws -> SQLDecimal {
        let integral = exponent.rescaledUncapped(to: 0, round: false)
        if integral == exponent, let integer = Int32(integral.description) {
            return try integerPower(base: base, exponent: integer, exponentScale: exponent.scale, meter: meter)
        }
        if base.isZero { return SQLDecimal(negative: false, magnitude: BigMagnitude(0), scale: 16) }
        if base.negative && integral != exponent {
            throw JedError("2201F", "a negative number raised to a non-integer power yields a complex result")
        }
        let negativeResult = base.negative && (integral.limbs.first ?? 0) % 2 != 0
        let magnitude = base.absolute()
        let weight = try logarithmWeight(magnitude, meter: meter)
        var localScale = max(0, 8 - weight)
        var logBase = try logarithm(magnitude, scale: localScale, meter: meter)
        var logResult = logBase.multiplying(exponent, scale: localScale)
        let estimate = approximate(logResult)
        if abs(estimate) > 6020 {
            if estimate > 0 { throw overflow() }
            return SQLDecimal(negative: false, magnitude: BigMagnitude(0), scale: 1000)
        }
        let resultWeight = Int(estimate * log10e)
        let scale = displayScale(16 - resultWeight, base.scale, exponent.scale)
        localScale = max(0, max(0, scale + resultWeight) - weight + 8)
        logBase = try logarithm(magnitude, scale: localScale, meter: meter)
        logResult = logBase.multiplying(exponent, scale: localScale)
        let result = try exponential(logResult, scale: scale, meter: meter).checked()
        return negativeResult ? result.negated() : result
    }

    private static func integerPower(base: SQLDecimal, exponent: Int32, exponentScale: Int,
                                     meter: CostMeter) throws -> SQLDecimal {
        var estimate: Double = 0
        if !base.isZero {
            let logBase = try logarithm(base.absolute(), scale: 30, meter: meter)
            let logTen = try logarithm(ten, scale: 30, meter: meter)
            let decimalEstimate = try logBase.divided(logTen, scale: 30, round: true)
                .multiplyingUncapped(SQLDecimal(Int64(exponent)))
            estimate = approximate(decimalEstimate)
        }
        if estimate > Double(SQLDecimal.maximumIntegerDigits) { throw overflow() }
        if estimate + 1 < -1000 { return SQLDecimal(negative: false, magnitude: BigMagnitude(0), scale: 1000) }
        let weight = Int(estimate)
        let scale = displayScale(16 - weight, base.scale, exponentScale)
        switch exponent {
        case 0: return one.rescaledUncapped(to: scale)
        case 1: return base.rescaledUncapped(to: scale)
        case -1: return try one.divided(base, scale: scale, round: true).checked()
        case 2: return try base.multiplying(base, scale: scale).checked()
        default: break
        }
        if base.isZero { return SQLDecimal(negative: false, magnitude: BigMagnitude(0), scale: scale) }
        var mask = Int64(exponent).magnitude
        let logExponent = try logarithm(SQLDecimal(Int64(mask)), scale: 12, meter: meter)
        let exponentGuard = Int(try logExponent.rescaledUncapped(to: 0, round: false).integerValue())
        let significantDigits = 1 + scale + weight + exponentGuard + 8
        var product = base
        var result = mask & 1 == 1 ? base : one
        var reciprocalOverflow = false
        while true {
            mask >>= 1
            if mask == 0 { break }
            try admit(meter, scale: scale, values: [product, result])
            let productScale = max(0, min(significantDigits - 8 * product.base10000Weight, 2 * product.scale))
            product = product.multiplying(product, scale: productScale)
            if mask & 1 == 1 {
                let resultScale = max(0, min(significantDigits - 4 * (product.base10000Weight + result.base10000Weight),
                                            product.scale + result.scale))
                result = product.multiplying(result, scale: resultScale)
            }
            if product.base10000Weight > 32767 || result.base10000Weight > 32767 {
                if exponent > 0 { throw overflow() }
                result = SQLDecimal(0)
                reciprocalOverflow = true
                break
            }
        }
        if exponent < 0 && !reciprocalOverflow { return try one.divided(result, scale: scale, round: true).checked() }
        return try result.rescaledUncapped(to: scale).checked()
    }
}
