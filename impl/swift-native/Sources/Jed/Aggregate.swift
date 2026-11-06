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

/// One group/window owner creates and closes each state. The caller charges
/// aggregate_accumulate before DISTINCT admission and fold; it also owns FILTER,
/// argument evaluation, and in-aggregate ORDER BY. Fixed state rides group ENTRY.
internal final class AggregateState {
    let plan: AggregatePlan
    let context: ExpressionContext
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

    init(plan: AggregatePlan, context: ExpressionContext) {
        self.plan = plan; self.context = context
        if AggregateCollection.names.contains(plan.name) { collection = AggregateCollection(plan: plan, context: context) }
        if !plan.withinGroup.isEmpty { orderedSet = AggregateOrderedSet(plan: plan, context: context) }
    }
    func close() { closed = true; current = .null; collection?.close(); collection = nil; orderedSet?.close(); orderedSet = nil }
    deinit { close() }
    private func increment() throws {
        guard count < Int64.max else { throw JedError("22003", "aggregate count out of range") }; count += 1
    }
    func fold(_ values: [SQLValue]) throws {
        guard !closed else { throw JedError("55000", "aggregate state is closed") }
        guard values.count == plan.inputTypes.count else { throw JedError("XX000", "aggregate argument width mismatch") }
        if let orderedSet { try orderedSet.fold(values); return }
        if let collection { try collection.fold(values); return }
        let name = plan.name
        if name == "count_star" { try increment(); return }
        if name == "regr_count" {
            if !values.contains(where: \.isNull) { try increment() }; return
        }
        if AggregateStatistics.pairs.contains(name) {
            if !values.contains(where: \.isNull) {
                try moments.addPair(y: AggregateStatistics.number(values[0]), x: AggregateStatistics.number(values[1]))
            }
            return
        }
        guard let value = values.first else { throw JedError("XX000", "aggregate is missing its argument") }
        if value.isNull { return }
        if AggregateStatistics.moments.contains(name) {
            if plan.resultType.type == .scalar(.f64) { try moments.add(try FloatFunctions.number(value)) }
            else {
                let x = try AggregateStatistics.decimal(value)
                // Frozen cross-core transition schedule: the exact square's
                // precision determines the combined addition charge. See the
                // tracked pre-charge multiplication issue before changing this.
                let square = x.multiplyingUncapped(x)
                let work = DecimalFunctions.work("*", x, x) - 1 + DecimalFunctions.work("+", sum, x) - 1
                    + DecimalFunctions.work("+", squares, square) - 1
                context.meter.charge(work); try context.meter.guardCost()
                sum = sum.addingUncapped(x); squares = squares.addingUncapped(square); try increment()
            }
            return
        }
        switch name {
        case "count": try increment()
        case "sum", "avg":
            if plan.resultType.type == .scalar(.f32) || plan.resultType.type == .scalar(.f64) {
                try float.add(value, binary32: plan.resultType.type == .scalar(.f32)); return
            }
            if plan.resultType.type == .scalar(.i64) {
                guard case .integer(let value) = value else { throw JedError("XX000", "integer aggregate operand mismatch") }
                let next = integer.addingReportingOverflow(value)
                guard !next.overflow else { throw JedError("22003", "bigint out of range") }; integer = next.partialValue
            } else {
                let value = try AggregateStatistics.decimal(value)
                try DecimalFunctions.chargeWork("+", sum, value, meter: context.meter)
                sum = sum.addingUncapped(value)
            }
            try increment()
        case "min", "max":
            if current.isNull { current = value }
            else {
                let comparison = try ValueComparison.total(value, current, type: plan.inputTypes[0], collatedKey: context.collatedKey)
                if name == "min" ? comparison < 0 : comparison > 0 { current = value }
            }
        case "bool_and", "every", "bool_or":
            guard case .boolean(let incoming) = value else { throw JedError("XX000", "boolean aggregate operand mismatch") }
            if case .boolean(let previous) = current { current = .boolean(name == "bool_or" ? previous || incoming : previous && incoming) }
            else { current = value }
        case "bit_and", "bit_or", "bit_xor":
            guard case .integer(let incoming) = value else { throw JedError("XX000", "bit aggregate operand mismatch") }
            if case .integer(let previous) = current {
                current = .integer(name == "bit_and" ? previous & incoming : name == "bit_or" ? previous | incoming : previous ^ incoming)
            } else { current = value }
        default: throw JedError("0A000", "aggregate kernel is not yet connected: \(name)")
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
