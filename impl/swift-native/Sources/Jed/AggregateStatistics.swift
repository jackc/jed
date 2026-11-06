internal enum AggregateStatistics {
    static let moments: Set<String> = ["var_pop", "var_samp", "variance", "stddev_pop", "stddev_samp", "stddev"]
    static let pairs: Set<String> = ["covar_pop", "covar_samp", "corr", "regr_avgx", "regr_avgy", "regr_sxx", "regr_syy", "regr_sxy", "regr_slope", "regr_intercept", "regr_r2"]
    static func decimal(_ value: SQLValue) throws -> SQLDecimal {
        switch value { case .integer(let x): return SQLDecimal(x); case .decimal(let x): return x
        default: throw JedError("XX000", "exact aggregate operand mismatch") }
    }
    static func number(_ value: SQLValue) throws -> Double {
        switch value {
        case .integer(let x): return Double(x)
        case .decimal(let x): return try FloatFunctions.number(FloatText.fromDecimal(x, type: .f64))
        case .f32, .f64: return try FloatFunctions.number(value)
        default: throw JedError("XX000", "numeric aggregate operand mismatch")
        }
    }
    static func exact(count: Int64, sum: SQLDecimal, squares: SQLDecimal, sample: Bool, deviation: Bool) throws -> SQLValue {
        if count == 0 || sample && count < 2 { return .null }
        let n = SQLDecimal(count), scale = sum.scale * 2
        let first = n.multiplying(squares, scale: scale), second = sum.multiplying(sum, scale: scale)
        let numerator = first.addingUncapped(second.negated())
        if numerator.negative || numerator.isZero { return .decimal(SQLDecimal(0)) }
        let denominator = n.multiplyingUncapped(SQLDecimal(sample ? count - 1 : count))
        let resultScale = numerator.divisionScale(denominator)
        var result = try numerator.divided(denominator, scale: resultScale, round: true)
        if deviation {
            // Finalization is unmetered. One extra digit retains the complete
            // half-away rounding decision of the exact integer square root.
            let rootScale = max(resultScale + 1, (result.scale + 1) / 2 + 1)
            let radicand = result.magnitude.shiftedDecimal(2 * rootScale - result.scale)
            result = SQLDecimal(negative: false, magnitude: radicand.squareRoot(), scale: rootScale).rescaledUncapped(to: resultScale)
        }
        return .decimal(try result.checked())
    }
}

/// The float SUM/AVG contract is a streaming, width-correct scan-order fold.
/// Nonfinite input flags dominate finite overflow only at finalization.
internal struct AggregateFloatSum {
    var count: Int64 = 0
    var total: Double = 0
    var nan = false, positiveInfinity = false, negativeInfinity = false
    mutating func add(_ value: SQLValue, binary32: Bool) throws {
        let incoming = try FloatFunctions.number(value)
        guard count < Int64.max else { throw JedError("22003", "aggregate count out of range") }; count += 1
        if incoming.isNaN { nan = true }
        else if incoming == .infinity { positiveInfinity = true }
        else if incoming == -.infinity { negativeInfinity = true }
        else {
            let value = incoming == 0 ? 0 : incoming
            total = binary32 ? Double(Float(total) + Float(value)) : total + value
        }
    }
    func finish(average: Bool, binary32: Bool) throws -> SQLValue {
        if count == 0 { return .null }
        let value: Double
        if nan || positiveInfinity && negativeInfinity { value = .nan }
        else if positiveInfinity { value = .infinity }
        else if negativeInfinity { value = -.infinity }
        else {
            guard !total.isInfinite else { throw FloatFunctions.overflow(binary32 ? .f32 : .f64) }
            value = average ? (binary32 ? Double(Float(total) / Float(count)) : total / Double(count)) : total
        }
        return binary32 ? .f32(Float(value).bitPattern) : .f64(value.bitPattern)
    }
}

/// Youngs-Cramer updates, with explicit IEEE operation boundaries. Swift does
/// not contract these expressions into a fused multiply-add.
internal struct AggregateFloatMoments {
    var count: Double = 0
    var sumX: Double = 0, sumY: Double = 0
    var squareX: Double = 0, squareY: Double = 0, cross: Double = 0
    mutating func add(_ x: Double) throws {
        let n = count + 1, sum = sumX + x
        var square = squareX
        if count > 0 {
            let product = x * n, delta = product - sum, squared = delta * delta, denominator = n * count
            square += squared / denominator
            if sum.isInfinite || square.isInfinite {
                if !sumX.isInfinite && !x.isInfinite { throw FloatFunctions.overflow(.f64) }
                square = .nan
            }
        } else if !x.isFinite { square = .nan }
        count = n; sumX = sum; squareX = square
    }
    mutating func addPair(y: Double, x: Double) throws {
        let n = count + 1, sx = sumX + x, sy = sumY + y
        var xx = squareX, yy = squareY, xy = cross
        if count > 0 {
            let productX = x * n, productY = y * n
            let dx = productX - sx, dy = productY - sy, scale = 1 / (n * count)
            let dx2 = dx * dx, dy2 = dy * dy, dxy = dx * dy
            xx += dx2 * scale; yy += dy2 * scale; xy += dxy * scale
            if sx.isInfinite || sy.isInfinite || xx.isInfinite || yy.isInfinite || xy.isInfinite {
                if (sx.isInfinite || xx.isInfinite) && !sumX.isInfinite && !x.isInfinite
                    || (sy.isInfinite || yy.isInfinite) && !sumY.isInfinite && !y.isInfinite
                    || xy.isInfinite && !sumX.isInfinite && !x.isInfinite && !sumY.isInfinite && !y.isInfinite {
                    throw FloatFunctions.overflow(.f64)
                }
                if xx.isInfinite { xx = .nan }; if yy.isInfinite { yy = .nan }; if xy.isInfinite { xy = .nan }
            }
        } else {
            if !x.isFinite { xx = .nan; xy = .nan }
            if !y.isFinite { yy = .nan; xy = .nan }
        }
        count = n; sumX = sx; sumY = sy; squareX = xx; squareY = yy; cross = xy
    }
    func finish(sample: Bool, deviation: Bool) -> SQLValue {
        if count == 0 || sample && count < 2 { return .null }
        let variance = squareX / (sample ? count - 1 : count)
        return .f64((deviation ? variance.squareRoot() : variance).bitPattern)
    }
    func finishPair(_ name: String) -> SQLValue {
        if count == 0 { return .null }
        let value: Double
        switch name {
        case "covar_pop": value = cross / count
        case "covar_samp": if count < 2 { return .null }; value = cross / (count - 1)
        case "corr": if squareX == 0 || squareY == 0 { return .null }; value = cross / (squareX * squareY).squareRoot()
        case "regr_avgx": value = sumX / count
        case "regr_avgy": value = sumY / count
        case "regr_sxx": value = squareX
        case "regr_syy": value = squareY
        case "regr_sxy": value = cross
        case "regr_slope": if squareX == 0 { return .null }; value = cross / squareX
        case "regr_intercept": if squareX == 0 { return .null }; value = (sumY - (sumX * cross) / squareX) / count
        case "regr_r2":
            if squareX == 0 { return .null }; if squareY == 0 { return .f64(Double(1).bitPattern) }
            value = (cross * cross) / (squareX * squareY)
        default: preconditionFailure("invalid pair statistic")
        }
        return .f64(value.bitPattern)
    }
}
