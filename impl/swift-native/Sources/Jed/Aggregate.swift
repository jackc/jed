/// Aggregate input types have already passed literal/parameter adoption. The
/// catalog chooses overload and result type; the transitions remain handwritten.
internal struct AggregatePlan {
    let metadata: FunctionSpecFact
    let name: String
    let inputTypes: [TypeDescriptor]
    let directTypes: [TypeDescriptor]
    let withinGroup: [AggregateOrder]
    let resultType: TypeDescriptor
    let star: Bool
    let kernel: AggregateKernel

    private init(metadata: FunctionSpecFact, name: String, inputTypes: [TypeDescriptor],
                 directTypes: [TypeDescriptor], withinGroup: [AggregateOrder],
                 resultType: TypeDescriptor, star: Bool) {
        self.metadata = metadata; self.name = name; self.inputTypes = inputTypes
        self.directTypes = directTypes; self.withinGroup = withinGroup
        self.resultType = resultType; self.star = star
        kernel = AggregateKernel.resolve(name: name, result: resultType.type, ordered: !withinGroup.isEmpty)
    }

    static func resolve(name: String, inputTypes: [TypeDescriptor], star: Bool = false,
                        withinGroup: [AggregateOrder] = [], directTypes: [TypeDescriptor] = []) throws -> Self {
        let name = name.lowercased(), lookup = star && name == "count" ? "count_star" : name
        if !withinGroup.isEmpty {
            guard !star else { throw JedError("42883", "aggregate does not exist: \(name)") }
            let result: TypeDescriptor
            if AggregateOrderedSet.hypothetical.contains(name) {
                guard inputTypes.count == withinGroup.count, directTypes.count == inputTypes.count,
                      zip(inputTypes, directTypes).allSatisfy({ hypotheticalCompatible($1.type, $0.type) }) else {
                    throw JedError("42883", "aggregate does not exist: \(name)")
                }
                for type in inputTypes { try TypeRules.comparable(type.type, type.type) }
                result = TypeDescriptor(.scalar(name == "rank" || name == "dense_rank" ? .i64 : .f64))
            } else {
                guard AggregateOrderedSet.ordered.contains(name), inputTypes.count == 1, withinGroup.count == 1,
                      directTypes.count == (name == "mode" ? 0 : 1) else { throw JedError("42883", "aggregate does not exist: \(name)") }
                try TypeRules.comparable(inputTypes[0].type, inputTypes[0].type)
                let base: TypeDescriptor
                if name == "percentile_cont" {
                    guard accepts("numeric", inputTypes[0].type) || inputTypes[0].type == .scalar(.interval) else {
                        throw JedError("42883", "aggregate does not exist: \(name)")
                    }
                    base = TypeDescriptor(inputTypes[0].type == .scalar(.interval) ? .scalar(.interval) : .scalar(.f64))
                } else { base = TypeDescriptor(inputTypes[0].type) }
                if let direct = directTypes.first {
                    let numeric: SQLType
                    if case .array(let element) = direct.type { numeric = element }
                    else { numeric = direct.type }
                    guard accepts("numeric", numeric) else { throw JedError("42883", "aggregate does not exist: \(name)") }
                    if case .array = direct.type { result = TypeDescriptor(.array(base.type)) }
                    else { result = base }
                } else { result = base }
            }
            return Self(metadata: FunctionSpecFact(category: "aggregate", name: name, kind: "aggregate", metadata: [:]),
                        name: name, inputTypes: inputTypes, directTypes: directTypes,
                        withinGroup: withinGroup, resultType: result, star: false)
        }
        guard directTypes.isEmpty else { throw JedError("42883", "aggregate does not exist: \(name)") }
        if ["json_object_agg", "jsonb_object_agg", "json_object_agg_unique", "jsonb_object_agg_unique"].contains(name) {
            guard !star, inputTypes.count == 2 else { throw JedError("42883", "aggregate does not exist: \(name)") }
            let result = TypeDescriptor(.scalar(name.hasPrefix("jsonb") ? .jsonb : .json))
            let metadata = FunctionSpecFact(category: "aggregate", name: name, kind: "aggregate", metadata: [:])
            return Self(metadata: metadata, name: name, inputTypes: inputTypes, directTypes: [], withinGroup: [], resultType: result, star: false)
        }
        let rows = GeneratedSpec.functions.filter { $0.kind == "aggregate" && $0.name == lookup }
        guard let metadata = rows.first(where: { row in
            if FunctionResolver.text(row.metadata, "arg") == "star" { return star && inputTypes.isEmpty }
            let families = FunctionResolver.strings(row.metadata, "arg_families")
            return !star && families.count == inputTypes.count && zip(families, inputTypes).allSatisfy { accepts($0, $1.type) }
        }) else { throw JedError("42883", "aggregate does not exist: \(name)") }
        if name == "min" || name == "max" { try TypeRules.comparable(inputTypes[0].type, inputTypes[0].type) }
        let result: TypeDescriptor
        switch FunctionResolver.text(metadata.metadata, "result") {
        case "same_as_input": result = TypeDescriptor(inputTypes[0].type)
        case "sum_widen": result = TypeDescriptor(inputTypes[0].type == .scalar(.i64) ? .scalar(.decimal) : .scalar(.i64))
        case "array_of_input":
            if case .array = inputTypes[0].type { result = TypeDescriptor(inputTypes[0].type) }
            else { result = TypeDescriptor(.array(inputTypes[0].type)) }
        case .some(let name):
            guard let scalar = ScalarType.allCases.first(where: { $0.name == name }) else { throw JedError("XX000", "invalid aggregate result catalog") }
            result = TypeDescriptor(.scalar(scalar))
        case .none: throw JedError("XX000", "missing aggregate result catalog")
        }
        return Self(metadata: metadata, name: lookup, inputTypes: inputTypes, directTypes: directTypes,
                    withinGroup: withinGroup, resultType: result, star: star)
    }
    static func accepts(_ family: String, _ type: SQLType) -> Bool {
        if family == "any" { return true }
        guard case .scalar(let scalar) = type else { return false }
        switch family {
        case "integer": return scalar.integerWidth != nil
        case "float": return scalar == .f32 || scalar == .f64
        case "numeric": return scalar.integerWidth != nil || scalar == .decimal || scalar == .f32 || scalar == .f64
        default: return scalar.name == family
        }
    }
    static func hypotheticalCompatible(_ argument: SQLType, _ key: SQLType) -> Bool {
        guard case .scalar(let a) = argument, case .scalar(let b) = key else { return false }
        if a.integerWidth != nil && b.integerWidth != nil { return true }
        if [ScalarType.f32, .f64].contains(a) && [ScalarType.f32, .f64].contains(b) { return true }
        return a == b && [.decimal, .text, .boolean, .bytea, .uuid, .timestamp, .timestamptz, .date, .interval].contains(a)
    }
    func makeState(context: ExpressionContext) -> AggregateState { AggregateState(plan: self, context: context) }
}

internal struct AggregateOrder {
    let type: TypeDescriptor
    let descending: Bool
    let nullsFirst: Bool
    init(type: TypeDescriptor, descending: Bool = false, nullsFirst: Bool? = nil) {
        self.type = type; self.descending = descending; self.nullsFirst = nullsFirst ?? descending
    }
}

/// Runtime transition selection is a resolved-plan fact. Every execution
/// owner uses this discriminator; SQL names and result types stay out of folds.
internal enum AggregateKernel {
    enum Bit { case and, or, xor }
    case countStar, count, pairCount, pairs
    case integerSum, exactSum, floatSum(binary32: Bool)
    case exactMoments, floatMoments
    case extreme(minimum: Bool), boolean(or: Bool), bit(Bit)
    case collection, orderedSet, unsupported

    static func resolve(name: String, result: SQLType, ordered: Bool) -> Self {
        if ordered { return .orderedSet }
        if AggregateCollection.names.contains(name) { return .collection }
        if name == "count_star" { return .countStar }
        if name == "count" { return .count }
        if name == "regr_count" { return .pairCount }
        if AggregateStatistics.pairs.contains(name) { return .pairs }
        if AggregateStatistics.moments.contains(name) {
            return result == .scalar(.f64) ? .floatMoments : .exactMoments
        }
        switch name {
        case "sum", "avg":
            if result == .scalar(.f32) { return .floatSum(binary32: true) }
            if result == .scalar(.f64) { return .floatSum(binary32: false) }
            return result == .scalar(.i64) ? .integerSum : .exactSum
        case "min": return .extreme(minimum: true)
        case "max": return .extreme(minimum: false)
        case "bool_and", "every": return .boolean(or: false)
        case "bool_or": return .boolean(or: true)
        case "bit_and": return .bit(.and)
        case "bit_or": return .bit(.or)
        case "bit_xor": return .bit(.xor)
        default: return .unsupported
        }
    }
}

/// One group/window owner creates and closes each state. The caller charges
/// aggregate_accumulate before DISTINCT admission and fold; it also owns FILTER,
/// argument evaluation, and in-aggregate ORDER BY. Fixed state rides group ENTRY.
internal final class AggregateState {
    let plan: AggregatePlan
    /// Only these two services are consulted while folding. Holding the whole
    /// expression context made every state construction copy it.
    private let meter: CostMeter
    private let collatedKey: KeyEncoding.CollatedKey?
    private var closed = false
    private var count: Int64 = 0
    private var integer: Int64 = 0
    private var sum = SQLDecimal(0)
    private var squares = SQLDecimal(0)
    private var current: SQLValue = .null
    private var float = AggregateFloatSum()
    private var moments = AggregateFloatMoments()
    private var collection: AggregateCollection?
    private var orderedSet: AggregateOrderedSet?

    init(plan: AggregatePlan, context: ExpressionContext, bounded: Bool = false) {
        self.plan = plan; meter = context.meter; collatedKey = context.collatedKey
        if case .collection = plan.kernel { collection = AggregateCollection(plan: plan, context: context, measured: !bounded) }
        if case .orderedSet = plan.kernel { orderedSet = AggregateOrderedSet(plan: plan, context: context, measured: !bounded) }
    }
    /// Private finite-state tuple. Its field shape is also the bounded map's
    /// logical residency measure; retained inputs have a different owner.
    func packed() -> [SQLValue] {
        if orderedSet != nil { return [] }
        if let collection { return collection.packed() }
        let name = plan.name
        if ["count", "count_star", "regr_count"].contains(name) { return [.integer(count)] }
        if AggregateStatistics.pairs.contains(name) {
            return [moments.count, moments.sumX, moments.squareX, moments.sumY, moments.squareY, moments.cross].map { .f64($0.bitPattern) }
        }
        if AggregateStatistics.moments.contains(name) {
            if plan.resultType.type == .scalar(.f64) {
                return [moments.count, moments.sumX, moments.squareX].map { .f64($0.bitPattern) }
            }
            return [.integer(count), .decimal(sum), .decimal(squares)]
        }
        if name == "sum" || name == "avg" {
            if plan.resultType.type == .scalar(.f32) || plan.resultType.type == .scalar(.f64) {
                return [.f64(float.total.bitPattern), .integer(float.count), .boolean(float.nan),
                        .boolean(float.positiveInfinity), .boolean(float.negativeInfinity)]
            }
            if name == "avg" { return [.decimal(sum), .integer(count)] }
            return [plan.resultType.type == .scalar(.i64) ? .integer(integer) : .decimal(sum), .boolean(count > 0)]
        }
        return [current]
    }
    func restore(_ values: [SQLValue]) throws {
        let expected = packed().count
        // Array-of-array state has a shape-dependent tail checked by its owner.
        if let collection { try collection.restore(values); return }
        guard values.count == expected else { throw JedError("58030", "invalid aggregate state width") }
        func integer(_ index: Int) throws -> Int64 {
            guard case .integer(let value) = values[index] else { throw JedError("58030", "invalid aggregate integer state") }; return value
        }
        func decimal(_ index: Int) throws -> SQLDecimal {
            guard case .decimal(let value) = values[index] else { throw JedError("58030", "invalid aggregate decimal state") }; return value
        }
        func number(_ index: Int) throws -> Double {
            guard case .f64(let bits) = values[index] else { throw JedError("58030", "invalid aggregate floating state") }; return Double(bitPattern: bits)
        }
        func boolean(_ index: Int) throws -> Bool {
            guard case .boolean(let value) = values[index] else { throw JedError("58030", "invalid aggregate boolean state") }; return value
        }
        if orderedSet != nil { return }
        let name = plan.name
        if ["count", "count_star", "regr_count"].contains(name) { count = try integer(0); return }
        if AggregateStatistics.pairs.contains(name) {
            moments.count = try number(0); moments.sumX = try number(1); moments.squareX = try number(2)
            moments.sumY = try number(3); moments.squareY = try number(4); moments.cross = try number(5); return
        }
        if AggregateStatistics.moments.contains(name) {
            if plan.resultType.type == .scalar(.f64) {
                moments.count = try number(0); moments.sumX = try number(1); moments.squareX = try number(2)
            } else { count = try integer(0); sum = try decimal(1); squares = try decimal(2) }
            return
        }
        if name == "sum" || name == "avg" {
            if plan.resultType.type == .scalar(.f32) || plan.resultType.type == .scalar(.f64) {
                float.total = try number(0); float.count = try integer(1); float.nan = try boolean(2)
                float.positiveInfinity = try boolean(3); float.negativeInfinity = try boolean(4); return
            }
            if name == "avg" { sum = try decimal(0); count = try integer(1) }
            else {
                if plan.resultType.type == .scalar(.i64) { self.integer = try integer(0) } else { sum = try decimal(0) }
                count = try boolean(1) ? 1 : 0
            }
            return
        }
        current = values[0]
    }
    func drainRetained(_ visit: ([SQLValue]) throws -> Void) rethrows {
        try collection?.drainRetained(visit)
        try orderedSet?.drainRetained(visit)
    }
    func close() { closed = true; current = .null; collection?.close(); collection = nil; orderedSet?.close(); orderedSet = nil }
    deinit { close() }
    private func increment() throws {
        guard count < Int64.max else { throw JedError("22003", "aggregate count out of range") }; count += 1
    }
    private func checkFold(width: Int) throws {
        guard !closed else { throw JedError("55000", "aggregate state is closed") }
        guard width == plan.inputTypes.count else { throw JedError("XX000", "aggregate argument width mismatch") }
    }

    /// The general tuple adapter remains the entry for grouped, window and
    /// spill owners. A scalar owner need not allocate a one-element array.
    func fold(_ values: [SQLValue]) throws {
        try checkFold(width: values.count)
        switch plan.kernel {
        case .orderedSet: try orderedSet!.fold(values)
        case .collection: try collection!.fold(values)
        case .countStar: try increment()
        case .pairCount:
            if !values.contains(where: \.isNull) { try increment() }
        case .pairs:
            if !values.contains(where: \.isNull) {
                try moments.addPair(y: AggregateStatistics.number(values[0]), x: AggregateStatistics.number(values[1]))
            }
        default:
            guard let value = values.first else { throw JedError("XX000", "aggregate is missing its argument") }
            try foldValue(value)
        }
    }

    func foldScalar(_ value: SQLValue) throws {
        try checkFold(width: 1)
        switch plan.kernel {
        case .orderedSet: try orderedSet!.fold([value])
        case .collection: try collection!.fold([value])
        default: try foldValue(value)
        }
    }

    /// COUNT's packed operand is stored nullness, not a fabricated SQL value.
    func foldCount(isNull: Bool) throws {
        try checkFold(width: 1)
        guard case .count = plan.kernel else { throw JedError("XX000", "aggregate does not accept a nullness operand") }
        if !isNull { try increment() }
    }

    func foldStar() throws {
        try checkFold(width: 0)
        guard case .countStar = plan.kernel else { throw JedError("XX000", "aggregate is missing its argument") }
        try increment()
    }

    /// All scalar entry forms share these transitions and their existing
    /// accumulator fields, work charges, overflow order and finalization.
    private func foldValue(_ value: SQLValue) throws {
        if value.isNull { return }
        switch plan.kernel {
        case .count: try increment()
        case .integerSum:
            guard case .integer(let value) = value else { throw JedError("XX000", "integer aggregate operand mismatch") }
            let next = integer.addingReportingOverflow(value)
            guard !next.overflow else { throw JedError("22003", "bigint out of range").withDataType("i64") }; integer = next.partialValue
            try increment()
        case .exactSum:
            let value = try AggregateStatistics.decimal(value)
            try DecimalFunctions.chargeWork("+", sum, value, meter: meter)
            sum = sum.addingUncapped(value)
            try increment()
        case .floatSum(let binary32): try float.add(value, binary32: binary32)
        case .floatMoments: try moments.add(try FloatFunctions.number(value))
        case .exactMoments:
            let x = try AggregateStatistics.decimal(value)
            // Frozen cross-core transition schedule: the exact square's
            // precision determines the combined addition charge. See the
            // tracked pre-charge multiplication issue before changing this.
            let square = x.multiplyingUncapped(x)
            let work = DecimalFunctions.work("*", x, x) - 1 + DecimalFunctions.work("+", sum, x) - 1
                + DecimalFunctions.work("+", squares, square) - 1
            meter.charge(work); try meter.guardCost()
            sum = sum.addingUncapped(x); squares = squares.addingUncapped(square); try increment()
        case .extreme(let minimum):
            if current.isNull { current = value }
            else if case .integer(let incoming) = value, case .integer(let held) = current {
                // Integer types order numerically; no descriptor is needed.
                if minimum ? incoming < held : incoming > held { current = value }
            } else {
                let comparison = try ValueComparison.total(value, current, type: plan.inputTypes[0], collatedKey: collatedKey)
                if minimum ? comparison < 0 : comparison > 0 { current = value }
            }
        case .boolean(let or):
            guard case .boolean(let incoming) = value else { throw JedError("XX000", "boolean aggregate operand mismatch") }
            if case .boolean(let previous) = current { current = .boolean(or ? previous || incoming : previous && incoming) }
            else { current = value }
        case .bit(let operation):
            guard case .integer(let incoming) = value else { throw JedError("XX000", "bit aggregate operand mismatch") }
            if case .integer(let previous) = current {
                switch operation {
                case .and: current = .integer(previous & incoming)
                case .or: current = .integer(previous | incoming)
                case .xor: current = .integer(previous ^ incoming)
                }
            } else { current = value }
        default: throw JedError("0A000", "aggregate kernel is not yet connected: \(plan.name)")
        }
    }
    /// Only COUNT has an exact inverse across a moving window boundary.
    func unfold(_ values: [SQLValue]) throws {
        guard !closed, plan.name == "count" || plan.name == "count_star",
              values.count == plan.inputTypes.count else { throw JedError("XX000", "aggregate cannot remove a frame operand") }
        if plan.name == "count_star" || values.first?.isNull == false {
            guard count > 0 else { throw JedError("XX000", "window count underflow") }
            count -= 1
        }
    }
    func finish(direct: [SQLValue] = []) throws -> SQLValue {
        guard !closed else { throw JedError("55000", "aggregate state is closed") }
        if let orderedSet { return try orderedSet.finish(direct: direct) }
        guard direct.isEmpty else { throw JedError("XX000", "ordinary aggregate has direct arguments") }
        if let collection { return try collection.finish() }
        let name = plan.name
        if AggregateStatistics.pairs.contains(name) { return moments.finishPair(name) }
        if AggregateStatistics.moments.contains(name) {
            let sample = !name.hasSuffix("_pop"), deviation = name.hasPrefix("stddev")
            if plan.resultType.type == .scalar(.f64) { return moments.finish(sample: sample, deviation: deviation) }
            return try AggregateStatistics.exact(count: count, sum: sum, squares: squares, sample: sample, deviation: deviation)
        }
        switch name {
        case "count", "count_star", "regr_count": return .integer(count)
        case "sum", "avg":
            if plan.resultType.type == .scalar(.f32) || plan.resultType.type == .scalar(.f64) {
                return try float.finish(average: name == "avg", binary32: plan.resultType.type == .scalar(.f32))
            }
            if count == 0 { return .null }
            if plan.resultType.type == .scalar(.i64) { return .integer(integer) }
            return .decimal(try name == "avg" ? sum.dividing(SQLDecimal(count)) : sum.checked())
        case "min", "max", "bool_and", "bool_or", "every", "bit_and", "bit_or", "bit_xor": return current
        default: throw JedError("0A000", "aggregate kernel is not yet connected: \(name)")
        }
    }
}
