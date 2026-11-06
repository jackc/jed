import JedCrypto

/// Execution-owned services. Built-ins never read host clocks, entropy, session
/// state, or loaded Unicode data except through this explicit environment.
internal enum ScalarFunctions {
    struct RegexExecution {
        var program: RegexProgram?
        var chargeCompile = true
        var scratch: RegexScratch?
    }
    struct Environment {
        var unicode = UnicodeDatabase()
        var timezones = TimeZoneDatabase()
        var hostCall: ((UInt64, [SQLValue]) throws -> SQLValue)?
        var stateCall: ((String, [SQLValue]) throws -> SQLValue)?
        var regexExecution: ((ResolvedFunction) throws -> RegexExecution)?
        var render: JSONFunctions.ScalarRenderer?
        var specialized: ((ResolvedFunction, [SQLValue], ExpressionContext) throws -> SQLValue)?
    }

    /// The caller charges function.cost and eagerly evaluates every argument in
    /// evaluationOrder. Strict NULL handling happens here, after those evals.
    static func evaluate(_ function: ResolvedFunction, values: [SQLValue], context: ExpressionContext,
                         environment: Environment = Environment()) throws -> SQLValue {
        guard values.count == function.arguments.count else { throw JedError("XX000", "function argument count mismatch") }
        if function.nullBehavior == "propagates" && values.contains(where: \.isNull) { return .null }
        if let id = function.hostID {
            guard let call = environment.hostCall else { throw JedError("55000", "host function is unavailable") }
            return try call(id, values)
        }
        if function.metadata.category == "json" { return try JsonScalarFunctions.evaluate(function, values: values, context: context) }
        let name = function.name, meter = context.meter
        let types = function.arguments.map { $0.type ?? TypeDescriptor(.scalar(.text)) }
        let constant = function.constantArguments.allSatisfy { $0 }
        let renderer: JSONFunctions.ScalarRenderer = environment.render ?? { value, descriptor, meter in
            var scalar = context.scalar; scalar.meter = meter
            return try ContainerText.render(value, type: descriptor, context: scalar, growth: false)
        }
        let jsonRenderer: JSONFunctions.ScalarRenderer = { try JsonScalarFunctions.render($0, type: $1, context: context, meter: $2) }
        func text(_ index: Int) throws -> SQLText {
            guard values.indices.contains(index), case .text(let result) = values[index] else { throw JedError("42804", "text argument required") }; return result
        }
        if case .range(let element) = function.resultType.type,
           GeneratedSpec.ranges.contains(where: { $0.id == name }) {
            var bounds: [SQLValue] = []
            for index in 0..<2 {
                bounds.append(try ContainerCasts.cast(values[index], from: types[index], to: TypeDescriptor(element), mode: .assignment, context: context.scalar, growth: false))
            }
            return try RangeFunctions.construct(lower: bounds[0], upper: bounds[1], flags: values.count > 2 ? values[2] : nil, element: element)
        }
        if let range = types.first(where: { if case .range = $0.type { true } else { false } }), case .range(let element) = range.type,
           ["lower", "upper", "isempty", "lower_inc", "upper_inc", "lower_inf", "upper_inf", "range_merge"].contains(name) {
            return try RangeFunctions.call(name: name, args: values, element: element)
        }
        if name == "array_to_json" {
            let pretty: Bool
            if values.count > 1, case .boolean(let enabled) = values[1] { pretty = enabled } else { pretty = false }
            return try JSONFunctions.chargeGrowth(JSONFunctions.image(values[0], type: types[0], binary: false, meter: meter, scalarRender: jsonRenderer, pretty: pretty), constant: constant, meter: meter)
        }
        let arrayNames: Set<String> = ["@>", "<@", "&&", "array_ndims", "array_length", "array_lower", "array_upper", "cardinality", "array_dims", "array_append", "array_prepend", "array_cat", "array_remove", "array_replace", "array_position", "array_positions", "array_to_string", "string_to_array"]
        if arrayNames.contains(name) || name == "width_bucket" && values.count == 2 {
            let arrayType = types.first(where: { if case .array = $0.type { true } else { false } }) ?? function.resultType
            guard case .array(let element) = arrayType.type else { throw JedError("42804", "array descriptor required") }
            let descriptor = ContainerText.elementDescriptor(arrayType, element: element)
            if name == "width_bucket" {
                return try ArrayFunctions.widthBucket(values[0], thresholds: values[1], element: descriptor, meter: meter,
                                                     comparisonWork: { try context.comparisonWork($0, $1, $2, type: $3) }, collatedKey: context.collatedKey)
            }
            return try ArrayFunctions.call(name: name, args: values, element: descriptor, context: context.scalar, constant: constant, collatedKey: context.collatedKey)
        }
        if ["now", "current_timestamp", "current_date", "localtimestamp", "make_date", "make_timestamp", "make_timestamptz", "make_interval", "justify_hours", "justify_days", "justify_interval", "age", "date_trunc", "date_part", "extract", "timezone"].contains(name) || name == "to_timestamp" && values.count == 1 {
            let usesClock = ["now", "current_timestamp", "current_date", "localtimestamp"].contains(name) || name == "age" && values.count == 1
            let clock = usesClock ? try context.now() : context.scalar.statementMicros
            return try TemporalFunctions.call(name: name, args: values, timezone: context.scalar.timezone,
                                              database: environment.timezones, statementMicros: clock, meter: meter)
        }
        switch name {
        case "to_char":
            guard case .scalar(let type) = types[0].type else { throw JedError("42804", "scalar to_char argument required") }
            if type.integerWidth != nil || [.decimal, .f32, .f64].contains(type) {
                return try NumericFormat.toChar(values[0], type: type, template: text(1), meter: meter)
            }
            return try TemporalFormat.toChar(values[0], template: text(1), timezone: context.scalar.timezone, meter: meter)
        case "to_date", "to_timestamp":
            return try TemporalFormat.parse(text(0), template: text(1), asDate: name == "to_date", timezone: context.scalar.timezone, meter: meter)
        case "to_number": return try NumericFormat.toNumber(text(0), template: text(1), meter: meter)
        case "clock_timestamp", "uuidv4", "gen_random_uuid", "uuidv7", "nextval", "currval", "setval", "lastval", "current_setting":
            guard let call = environment.stateCall else { throw JedError("55000", "function requires an execution-owned host service: \(name)") }
            return try call(name, values)
        case "uuid_extract_version", "uuid_extract_timestamp":
            guard case .uuid(let uuid) = values[0] else { throw JedError("42804", "uuid argument required") }
            let bytes = uuid.bytes
            guard bytes[8] & 0xc0 == 0x80 else { return .null }
            let version = bytes[6] >> 4
            if name == "uuid_extract_version" { return .integer(Int64(version)) }
            if version == 7 {
                let milliseconds = bytes.prefix(6).reduce(Int64(0)) { ($0 << 8) | Int64($1) }
                return .timestamptz(milliseconds * 1000)
            }
            if version == 1 {
                let low = bytes[0..<4].reduce(Int64(0)) { ($0 << 8) | Int64($1) }
                let mid = Int64(bytes[4]) << 8 | Int64(bytes[5])
                let high = Int64(bytes[6] & 15) << 8 | Int64(bytes[7])
                return .timestamptz(((high << 48 | mid << 32 | low) - 122_192_928_000_000_000) / 10)
            }
            return .null
        case "md5", "sha224", "sha256", "sha384", "sha512":
            let bytes: [UInt8]
            switch values[0] { case .text(let text): bytes = text.utf8; case .bytea(let data): bytes = data; default: throw JedError("42804", "digest requires text or bytea") }
            let digest: [UInt8]
            switch name {
            case "md5": digest = JedCrypto.md5(bytes)
            case "sha224": digest = JedCrypto.sha224(bytes)
            case "sha256": digest = JedCrypto.sha256(bytes)
            case "sha384": digest = JedCrypto.sha384(bytes)
            default: digest = JedCrypto.sha512(bytes)
            }
            if name != "md5" { return .bytea(digest) }
            let digits = Array("0123456789abcdef".utf8)
            return .text(SQLText(validatedUTF8: digest.flatMap { [digits[Int($0 >> 4)], digits[Int($0 & 15)]] }))
        case "length", "char_length", "character_length", "octet_length", "bit_length":
            let bytes: Int, characters: Int
            switch values[0] {
            case .text(let text): bytes = text.utf8.count; characters = text.scalarCount
            case .bytea(let data): bytes = data.count; characters = bytes
            default: throw JedError("42804", "string argument required")
            }
            let count = name == "bit_length" ? Int64(bytes) * 8 : name == "octet_length" ? Int64(bytes) : Int64(characters)
            return .integer(try ScalarCasts.checkedInteger(count, type: .i32))
        case "lower": return .text(environment.unicode.lower(try text(0)))
        case "upper": return .text(environment.unicode.upper(try text(0)))
        case "num_nulls", "num_nonnulls":
            let items: [SQLValue]
            if function.variadicArray {
                if values[0].isNull { return .null }
                guard case .array(let array) = values[0] else { throw JedError("42804", "VARIADIC argument must be an array") }
                items = array.elements
            } else { items = values }
            return .integer(Int64(items.filter { name == "num_nulls" ? $0.isNull : !$0.isNull }.count))
        case "jsonb_typeof", "json_typeof": return .text(try JSONFunctions.typeOf(values[0]))
        case "jsonb_array_length", "json_array_length": return .integer(try JSONFunctions.arrayLength(values[0]))
        case "jsonb_strip_nulls", "json_strip_nulls": return try JSONFunctions.stripNulls(values[0])
        case "jsonb_pretty": return .text(try SQLJSON.render(JSONFunctions.node(values[0]), pretty: true, meter: meter))
        case "to_json", "to_jsonb", "json_scalar":
            let image = try JSONFunctions.image(values[0], type: types[0], binary: name == "to_jsonb", meter: meter, scalarRender: jsonRenderer)
            return try JSONFunctions.chargeGrowth(image, constant: constant, meter: meter)
        case "json_serialize":
            switch values[0] {
            case .json(let text): return .text(text)
            case .jsonb(let value): return .text(try SQLJSON.render(value, meter: meter, growth: !constant))
            default: throw JedError("42804", "JSON argument required")
            }
        case "json_build_array", "jsonb_build_array", "json_build_object", "jsonb_build_object":
            var items = values, descriptors = types
            if function.variadicArray {
                guard values.count == 1 else { throw JedError("XX000", "invalid builder variadic arity") }
                if values[0].isNull { return .null }
                guard case .array(let array) = values[0], case .array(let element) = types[0].type else { throw JedError("42804", "VARIADIC argument must be an array") }
                items = array.elements; descriptors = items.map { _ in TypeDescriptor(element) }
            }
            return try JSONFunctions.build(items, types: descriptors, object: name.hasSuffix("object"), binary: name.hasPrefix("jsonb"), constant: constant, meter: meter, scalarRender: jsonRenderer)
        case "similar_to_escape": return .text(try Similar.translate(text(0), escape: values.count > 1 ? text(1) : nil, meter: constant ? nil : meter).text)
        case "substring" where values.count == 3 && types[1].type == .scalar(.text):
            return try context.patterns.similarSubstring(function, values: values, meter: meter)
        default: break
        }
        if name.hasPrefix("regexp_") || name == "substring" && types.count > 1 && types[1].type == .scalar(.text) {
            let execution = try environment.regexExecution?(function) ?? context.patterns.regexExecution(function)
            let unicode = environment.unicode
            return try RegexFunctions.call(name: name, args: values, program: execution.program, chargeCompile: execution.chargeCompile,
                                           scratch: execution.scratch, fold: { unicode.simpleLower($0) }, meter: meter)
        }
        if name == "encode" || name == "decode" || ["get_byte", "set_byte", "get_bit", "set_bit"].contains(name) || TextFunctions.names.contains(name) && types.first?.type == .scalar(.bytea) {
            return try ByteaFunctions.call(name: name, args: values, constant: constant, meter: meter)
        }
        if TextFunctions.names.contains(name) {
            return try TextFunctions.call(name: name, args: values, types: types, constant: constant, variadicArray: function.variadicArray, meter: meter, render: renderer)
        }
        let numeric = Set(["abs", "round", "trunc", "ceil", "ceiling", "floor", "sqrt", "exp", "ln", "log", "log10", "pow", "power", "sin", "cos", "tan", "cbrt", "pi", "radians", "degrees", "asin", "acos", "atan", "atan2", "cot", "sinh", "cosh", "tanh", "asinh", "acosh", "atanh", "sign", "factorial", "scale", "min_scale", "trim_scale", "mod", "div", "gcd", "lcm", "width_bucket"])
        if numeric.contains(name) {
            if name == "width_bucket", types[0].type == .scalar(.f64) || name == "width_bucket" && types[0].type == .scalar(.f32) {
                guard case .integer(let count) = values[3] else { throw JedError("42804", "bucket count must be an integer") }
                return .integer(try floatBucket(FloatFunctions.number(values[0]), low: FloatFunctions.number(values[1]), high: FloatFunctions.number(values[2]), count: count))
            }
            if ["mod", "gcd", "lcm"].contains(name), case .scalar(let type) = function.resultType.type, type.integerWidth != nil {
                guard case .integer(let a) = values[0], case .integer(let b) = values[1] else { throw JedError("42804", "integer arguments required") }
                let result: Int128
                if name == "mod" {
                    guard b != 0 else { throw JedError("22012", "division by zero") }; result = Int128(a) % Int128(b)
                } else {
                    var x = abs(Int128(a)), y = abs(Int128(b))
                    while y != 0 { let remainder = x % y; x = y; y = remainder }
                    result = name == "gcd" ? x : x == 0 ? 0 : (abs(Int128(a)) / x) * abs(Int128(b))
                }
                guard result >= Int64.min, result <= Int64.max else { throw JedError("22003", "integer out of range") }
                return .integer(try ScalarCasts.checkedInteger(Int64(result), type: type))
            }
            if name == "mod", case .scalar(let width) = function.resultType.type, width == .f32 || width == .f64 {
                return try FloatFunctions.binary(op: "%", left: values[0], right: values[1], width: width)
            }
            if name == "pi" || types.first.map({ $0.type == .scalar(.f32) || $0.type == .scalar(.f64) }) == true {
                return try FloatFunctions.call(name: name, args: values, meter: meter)
            }
            if name == "abs", case .integer(let value) = values[0], case .scalar(let type) = function.resultType.type {
                guard value != Int64.min else { throw JedError("22003", "integer out of range") }
                return .integer(try ScalarCasts.checkedInteger(value < 0 ? -value : value, type: type))
            }
            let arguments = try values.map { value -> SQLDecimal in
                switch value { case .integer(let n): SQLDecimal(n); case .decimal(let n): n; default: throw JedError("42804", "numeric argument required") }
            }
            if ["scale", "min_scale", "width_bucket"].contains(name) { return .integer(try DecimalFunctions.callInteger(name: name, args: arguments, meter: meter)) }
            return .decimal(try DecimalFunctions.call(name: name, args: arguments, meter: meter))
        }
        if let specialized = environment.specialized { return try specialized(function, values, context) }
        throw JedError("0A000", "resolved function kernel is not yet connected: \(name)")
    }
    private static func floatBucket(_ operand: Double, low: Double, high: Double, count: Int64) throws -> Int64 {
        guard count > 0 else { throw JedError("2201G", "count must be greater than zero") }
        guard !operand.isNaN, !low.isNaN, !high.isNaN else { throw JedError("2201G", "operand, lower bound, and upper bound cannot be NaN") }
        guard low.isFinite, high.isFinite else { throw JedError("2201G", "lower and upper bounds must be finite") }
        guard low != high else { throw JedError("2201G", "lower bound cannot equal upper bound") }
        if low < high ? operand < low : operand > low { return 0 }
        if low < high ? operand >= high : operand <= high {
            guard count < Int32.max else { throw JedError("22003", "integer out of range") }; return count + 1
        }
        let fraction = (high - low).isInfinite ? (operand / 2 - low / 2) / (high / 2 - low / 2) : (operand - low) / (high - low)
        let result = min((fraction * Double(count)).rounded(.down) + 1, Double(count))
        guard result <= Double(Int32.max) else { throw JedError("22003", "integer out of range") }; return Int64(result)
    }

}
