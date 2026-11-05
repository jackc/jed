#if canImport(Glibc)
import Glibc
#elseif canImport(Darwin)
import Darwin
#endif

/// IEEE arithmetic remains one operation at the resolved width. Only the
/// explicitly exempted transcendental values use the host's libm (float.md §8).
/// The evaluator charges the ordinary operator_eval; these kernels add no cost.
internal enum FloatFunctions {
    static func overflow() -> JedError { JedError("22003", "value out of range: overflow") }
    static func underflow() -> JedError { JedError("22003", "value out of range: underflow") }
    private static func domain() -> JedError { JedError("22003", "input is out of range") }
    private static let radiansPerDegree: Double = 0.0174532925199432957692

    static func number(_ value: SQLValue) throws -> Double {
        switch value {
        case .f64(let bits): return Double(bitPattern: bits)
        case .f32(let bits): return Double(Float(bitPattern: bits))
        default: throw JedError("42804", "expected a float value")
        }
    }

    static func unary(op: String, value: SQLValue) throws -> SQLValue {
        if value.isNull { return .null }
        guard op == "+" || op == "-" else { throw JedError("42883", "unknown float unary operator") }
        if op == "+" { return value }
        switch value {
        case .f32(let bits): return .f32(bits ^ 0x8000_0000)
        case .f64(let bits): return .f64(bits ^ 0x8000_0000_0000_0000)
        default: throw JedError("42804", "expected a float value")
        }
    }

    static func binary(op: String, left: SQLValue, right: SQLValue, width: ScalarType) throws -> SQLValue {
        if left.isNull || right.isNull { return .null }
        if op == "^" { return .f64(try power(number(left), number(right)).bitPattern) }
        if width == .f32 {
            guard case .f32(let a) = left, case .f32(let b) = right else { throw JedError("42804", "binary32 operation requires binary32 operands") }
            return .f32(try arithmetic(op, Float(bitPattern: a), Float(bitPattern: b)).bitPattern)
        }
        guard width == .f64 else { throw JedError("42804", "float operation requires a float result type") }
        return .f64(try arithmetic(op, number(left), number(right)).bitPattern)
    }

    private static func arithmetic<T: BinaryFloatingPoint>(_ operation: String, _ left: T, _ right: T) throws -> T {
        if (operation == "/" || operation == "%" || operation == "mod") && right == 0 && !left.isNaN {
            throw JedError("22012", "division by zero")
        }
        let result: T
        switch operation {
        case "+": result = left + right
        case "-": result = left - right
        case "*": result = left * right
        case "/": result = left / right
        case "%", "mod": result = left.truncatingRemainder(dividingBy: right)
        default: throw JedError("42883", "unknown float binary operator")
        }
        if left.isFinite && right.isFinite && !result.isFinite { throw overflow() }
        if result == 0 {
            if operation == "*" && left != 0 && right != 0 { throw underflow() }
            if operation == "/" && left != 0 && right.isFinite { throw underflow() }
        }
        return result
    }

    static func call(name: String, args: [SQLValue], meter: CostMeter? = nil) throws -> SQLValue {
        let name = name.lowercased()
        if args.contains(where: \.isNull) { return .null }
        if name == "pi" {
            guard args.isEmpty else { throw JedError("42883", "pi expects no arguments") }
            return .f64(0x4009_21fb_5444_2d18)
        }
        let two = ["pow", "power", "atan2", "mod"].contains(name)
        guard args.count == (two ? 2 : 1) || name == "round" && args.count == 2 else { throw JedError("42883", "invalid float function arity") }
        if name == "abs" {
            switch args[0] {
            case .f32(let bits): return .f32(bits & 0x7fff_ffff)
            case .f64(let bits): return .f64(bits & 0x7fff_ffff_ffff_ffff)
            default: throw JedError("42804", "expected a float value")
            }
        }
        let x = try number(args[0])
        let result: Double
        switch name {
        case "ceil", "ceiling": result = x.rounded(.up)
        case "floor": result = x.rounded(.down)
        case "trunc": result = x.rounded(.towardZero)
        case "round":
            var places: Int64 = 0
            if args.count == 2 {
                guard case .integer(let value) = args[1] else { throw JedError("42804", "round precision must be an integer") }
                places = value
            }
            result = try rounded(x, places: places)
        case "sign": result = x > 0 ? 1 : x < 0 ? -1 : 0
        case "sqrt":
            guard !(x < 0) else { throw JedError("2201F", "cannot take square root of a negative number") }
            result = x.squareRoot()
        case "radians": result = try arithmetic("*", x, radiansPerDegree)
        case "degrees": result = try arithmetic("/", x, radiansPerDegree)
        case "mod": result = try arithmetic("%", x, number(args[1]))
        case "exp":
            result = exp(x)
            if x.isFinite {
                if !result.isFinite { throw overflow() }
                if result == 0 { throw underflow() }
            }
        case "ln", "log10":
            if x == 0 { throw JedError("2201E", "cannot take logarithm of zero") }
            if x < 0 { throw JedError("2201E", "cannot take logarithm of a negative number") }
            result = name == "ln" ? log(x) : log10(x)
        case "pow", "power": result = try power(x, number(args[1]))
        case "sin", "cos", "tan", "cot":
            if x.isInfinite { throw domain() }
            switch name {
            case "sin": result = sin(x)
            case "cos": result = cos(x)
            case "tan": result = tan(x)
            default: result = 1 / tan(x)
            }
        case "cbrt": result = cbrt(x)
        case "asin", "acos":
            if x < -1 || x > 1 { throw domain() }
            result = name == "asin" ? asin(x) : acos(x)
        case "atan": result = atan(x)
        case "atan2": result = atan2(x, try number(args[1]))
        case "sinh": result = sinh(x)
        case "cosh": result = cosh(x)
        case "tanh": result = tanh(x)
        case "asinh": result = asinh(x)
        case "acosh":
            if x < 1 { throw domain() }
            result = acosh(x)
        case "atanh":
            if x < -1 || x > 1 { throw domain() }
            result = atanh(x)
        default: throw JedError("42883", "unknown float function: \(name)")
        }
        return .f64(result.bitPattern)
    }

    private static func rounded(_ value: Double, places: Int64) throws -> Double {
        if !value.isFinite || value == 0 || places >= 324 { return value }
        if places < -323 { return Double(sign: value.sign, exponent: 0, significand: 0) }
        if places == 0 { return value.rounded(.toNearestOrAwayFromZero) }
        if places > 308 {
            let exact = try BinaryFloatDecimal.exact(bits64: value.bitPattern)
            let rounded = exact.rescaledUncapped(to: Int(places))
            guard let result = Double(rounded.description) else { throw overflow() }
            return Double(sign: value.sign, exponent: 0, significand: result.magnitude)
        }
        // Parse an exact decimal power of ten to avoid platform-dependent libm
        // exponentiation in the otherwise correctly rounded scale/round path.
        guard let scale = Double("1e\(places)") else { throw overflow() }
        let scaled = value * scale
        if scaled.isInfinite { return value }
        return scaled.rounded(.toNearestOrAwayFromZero) / scale
    }

    private static func power(_ base: Double, _ exponent: Double) throws -> Double {
        if exponent == 0 || base == 1 { return 1 }
        if base.isNaN || exponent.isNaN { return .nan }
        if base == 0 && exponent < 0 { throw JedError("2201F", "zero raised to a negative power is undefined") }
        let integer = exponent.isFinite && exponent.rounded(.towardZero) == exponent
        if base < 0 && base.isFinite && exponent.isFinite && !integer {
            throw JedError("2201F", "a negative number raised to a non-integer power yields a complex result")
        }
        if exponent.isInfinite {
            let absolute = base.magnitude
            if absolute == 1 { return 1 }
            return (absolute > 1) == (exponent > 0) ? .infinity : 0
        }
        if base.isInfinite {
            let odd = integer && exponent.truncatingRemainder(dividingBy: 2) != 0
            let sign: FloatingPointSign = base.sign == .minus && odd ? .minus : .plus
            return Double(sign: sign, exponent: 0, significand: exponent > 0 ? .infinity : 0)
        }
        let result = pow(base, exponent)
        if !result.isFinite { throw overflow() }
        if result == 0 && base != 0 { throw underflow() }
        return result
    }
}
