/// Resolved expressions carry static types independently of their current value.
/// A nil descriptor is an untyped SQL NULL/parameter, never an inferred row type.
internal struct ResolvedExpression {
    var type: TypeDescriptor?
    var node: Node
    var position: Int
    /// Start of the authored expression, distinct from its operator token.
    var location: Int = 0
    /// Cached query provenance; nil on a synthesized node derives from children.
    var collation: CollationDerivation?
    /// Input folding can consume session state without retaining an operation
    /// node (notably offsetless timestamptz inside a container text literal).
    var nonimmutable = false
    var isConstant: Bool { if case .constant = node { return true }; return false }
    indirect enum Node {
        case constant(SQLValue), column(Int), parameter(Int)
        case outerColumn(OuterColumnReference), subquery(ResolvedSubquery)
        case currentTimestamp, currentDate, dateInput(SQLText)
        case cast(ResolvedExpression, TypeDescriptor, TypeRules.CastMode)
        case convert(ResolvedExpression, TypeDescriptor)
        case unary(String, ResolvedExpression)
        case binary(String, ResolvedExpression, ResolvedExpression, TypeDescriptor)
        case isNull(ResolvedExpression, Bool)
        case distinct(ResolvedExpression, ResolvedExpression, TypeDescriptor, Bool)
        case conditional([(ResolvedExpression, ResolvedExpression)], ResolvedExpression?)
        case coalesce([ResolvedExpression])
        case extreme([ResolvedExpression], greatest: Bool)
        case nullif(ResolvedExpression, ResolvedExpression, TypeDescriptor)
        case row([ResolvedExpression])
        case field(ResolvedExpression, Int)
        case call(ResolvedFunction)
        case array([ResolvedExpression], nested: Bool)
        case subscriptValue(ResolvedExpression, [ResolvedSubscript])
        case rangeOperation(String, ResolvedExpression, ResolvedExpression, SQLType)
        case quantified(String, all: Bool, ResolvedExpression, ResolvedExpression, TypeDescriptor)
        case json(ResolvedJSONExpression)
        case pattern(ResolvedPattern)
    }
}

internal struct ResolvedSubscript {
    var lower: ResolvedExpression?
    var upper: ResolvedExpression?
    var isSlice: Bool
}

internal struct ExpressionContext {
    var scalar: ScalarCasts.Context
    var column: (Int) throws -> SQLValue { didSet { hostBatchColumn = nil } }
    /// Only resident row owners install these peeks. Arbitrary column getters
    /// (including view expressions) may perform work and are never prefetched.
    var hostBatchColumn: ((Int) -> SQLValue?)?
    var parameters: [SQLValue]
    var meter: CostMeter
    var collatedKey: KeyEncoding.CollatedKey?
    var functions = ScalarFunctions.Environment()
    var sequences: SequenceExecution?
    var patterns = PatternExecution()
    var outerRows: [(Int) throws -> SQLValue] = [] { didSet { hostBatchOuterRows = [] } }
    var hostBatchOuterRows: [((Int) -> SQLValue?)?] = []
    var subqueries = SubqueryExecution()
    var ctes: CTEExecution?
    var writeScope: StatementWriteScope?
    var executionProfile: ExecutionProfile?
    var spill: SpillConfig?
    var errorDetail: ErrorDetail?
    var planningProbe: PlanningProbe?
    var hostBatch: HostBatch?
    // Derived/CTE bodies execute surviving subqueries at expression evaluation.
    var eagerSubqueries = true

    init(meter: CostMeter, timezone: SQLTimeZone = .utc, statementMicros: Int64? = nil,
         parameters: [SQLValue] = [], column: @escaping (Int) throws -> SQLValue = { _ in throw JedError("XX000", "missing expression row") }) {
        self.meter = meter
        scalar = ScalarCasts.Context(timezone: timezone, statementMicros: statementMicros, meter: meter)
        self.parameters = parameters; self.column = column
    }

    func now() throws -> Int64 {
        if let clock = scalar.statementMicros { return clock }
        guard let clock = scalar.statementClock else { throw JedError("55000", "statement clock is unavailable") }
        return try clock()
    }

    mutating func encloseHostBatchRow(from parent: ExpressionContext) {
        hostBatchOuterRows = [parent.hostBatchColumn] + parent.outerRows.indices.map {
            parent.hostBatchOuterRows.indices.contains($0) ? parent.hostBatchOuterRows[$0] : nil
        }
        hostBatch = nil
    }

    /// Filter contexts need only TRUE. In an AND, both FALSE and UNKNOWN reject
    /// the row before visiting the right child; value expressions retain 3VL.
    func matches(_ expression: ResolvedExpression) throws -> Bool {
        if case .binary(let operation, let left, let right, _) = expression.node {
            if operation == "and" {
                meter.charge(1); try meter.step()
                guard try matches(left) else { return false }
                return try matches(right)
            }
        }
        if case .boolean(true) = try evaluate(expression) { return true }
        return false
    }

    func evaluate(_ expression: ResolvedExpression) throws -> SQLValue {
        switch expression.node {
        case .constant(let value): return value
        case .column(let index): return try column(index)
        case .outerColumn(let reference):
            guard reference.depth > 0, reference.depth <= outerRows.count else { throw JedError("XX000", "missing enclosing expression row") }
            return try outerRows[reference.depth - 1](reference.index)
        case .subquery(let query): return try subqueries.evaluate(query, context: self)
        case .parameter(let index):
            guard index > 0 && index <= parameters.count else { throw JedError("42P02", "there is no parameter $\(index)") }
            return parameters[index - 1]
        case .convert(let source, let target):
            return try convert(evaluate(source), from: source.type?.type, to: target)
        case .call(let function): meter.charge(function.cost); try meter.step()
        default: meter.charge(1); try meter.step()
        }
        switch expression.node {
        case .currentTimestamp, .currentDate, .dateInput:
            return try evaluateState(expression)
        case .cast, .unary:
            return try evaluateUnary(expression)
        case .binary:
            return try evaluateBinary(expression)
        case .isNull, .distinct:
            return try evaluatePredicate(expression)
        case .conditional, .coalesce, .extreme, .nullif:
            return try evaluateConditional(expression)
        case .row, .array, .subscriptValue, .rangeOperation, .quantified, .field:
            return try evaluateContainer(expression)
        case .json, .pattern, .call:
            return try evaluateDelegated(expression)
        case .constant, .column, .parameter, .convert, .outerColumn, .subquery: throw JedError("XX000", "invalid expression dispatch")
        }
    }

    private func evaluateState(_ expression: ResolvedExpression) throws -> SQLValue {
        switch expression.node {
        case .currentTimestamp:
            return .timestamptz(try now())
        case .currentDate:
            let clock = try now()
            try Temporal.chargeZone(meter)
            return .date(try Temporal.finiteDate(Temporal.floorDiv(scalar.timezone.instantToLocal(clock), Temporal.microsPerDay)))
        case .dateInput(let text):
            return try ScalarCasts.input(text, to: TypeDescriptor(.scalar(.date)), context: scalar)
        default: throw JedError("XX000", "invalid expression evaluator dispatch")
        }
    }

    private func evaluateUnary(_ expression: ResolvedExpression) throws -> SQLValue {
        switch expression.node {
        case .cast(let source, let target, let mode):
            let value = try evaluate(source)
            guard let sourceType = source.type else { return .null }
            return try ContainerCasts.cast(value, from: sourceType, to: target, mode: mode, context: scalar, growth: !FunctionResolver.isConstant(source))
        case .unary(let op, let source):
            let value = try evaluate(source)
            if value.isNull { return .null }
            if op == "not", case .boolean(let b) = value { return .boolean(!b) }
            if op == "+" { return value }
            if op == "-" {
                switch value {
                case .integer(let n):
                    guard case .scalar(let type) = expression.type?.type else { throw JedError("22003", "integer out of range") }
                    guard n != .min else { throw JedError("22003", "integer out of range").withDataType(type.name) }
                    return .integer(try ScalarCasts.checkedInteger(-n, type: type))
                case .decimal(let n): return .decimal(n.negated())
                case .f32, .f64: return try FloatFunctions.unary(op: op, value: value)
                case .interval(let n): return .interval(try TemporalFunctions.negate(n))
                default: break
                }
            }
            if op == "~", case .integer(let n) = value { return .integer(~n) }
            throw JedError("42804", "invalid resolved unary operand")
        default: throw JedError("XX000", "invalid expression evaluator dispatch")
        }
    }

    private func evaluateBinary(_ expression: ResolvedExpression) throws -> SQLValue {
        switch expression.node {
        case .binary(let op, let left, let right, let operandType):
            let a = try evaluate(left)
            if op == "and", case .boolean(false) = a { return .boolean(false) }
            if op == "or", case .boolean(true) = a { return .boolean(true) }
            let b = try evaluate(right)
            if op == "and" {
                if case .boolean(false) = b { return .boolean(false) }
                return a.isNull || b.isNull ? .null : .boolean(true)
            }
            if op == "or" {
                if case .boolean(true) = b { return .boolean(true) }
                return a.isNull || b.isNull ? .null : .boolean(false)
            }
            if ["=", "<>", "<", "<=", ">", ">="].contains(op) {
                try comparisonWork(op, a, b, type: operandType)
                return try ValueComparison.sql(op, a, b, type: operandType, collatedKey: collatedKey)
            }
            if a.isNull || b.isNull { return .null }
            return try arithmetic(op, a, b, type: operandType,
                constant: FunctionResolver.isConstant(left) && FunctionResolver.isConstant(right))
        default: throw JedError("XX000", "invalid expression evaluator dispatch")
        }
    }

    private func evaluatePredicate(_ expression: ResolvedExpression) throws -> SQLValue {
        switch expression.node {
        case .isNull(let source, let negated): return .boolean(ValueComparison.isNull(try evaluate(source), negated: negated))
        case .distinct(let left, let right, let type, let negated):
            let a = try evaluate(left), b = try evaluate(right)
            let equal: Bool
            if a.isNull || b.isNull { equal = a.isNull && b.isNull }
            else { try comparisonWork("=", a, b, type: type); equal = try ValueComparison.total(a, b, type: type, collatedKey: collatedKey, equality: true) == 0 }
            return .boolean(negated ? equal : !equal)
        default: throw JedError("XX000", "invalid expression evaluator dispatch")
        }
    }

    private func evaluateConditional(_ expression: ResolvedExpression) throws -> SQLValue {
        switch expression.node {
        case .conditional(let arms, let fallback):
            for (condition, value) in arms {
                if case .boolean(true) = try evaluate(condition) { return try evaluate(value) }
            }
            return try fallback.map(evaluate) ?? .null
        case .coalesce(let sources):
            for source in sources { let value = try evaluate(source); if !value.isNull { return value } }
            return .null
        case .extreme(let sources, let greatest):
            var result = SQLValue.null
            for source in sources {
                let value = try evaluate(source)
                if value.isNull { continue }
                if result.isNull { result = value; continue }
                let type = expression.type!
                try comparisonWork(greatest ? ">" : "<", value, result, type: type)
                let order = try ValueComparison.total(value, result, type: type, collatedKey: collatedKey)
                if greatest ? order > 0 : order < 0 { result = value }
            }
            return result
        case .nullif(let first, let second, let type):
            let a = try evaluate(first), b = try evaluate(second)
            try comparisonWork("=", a, b, type: type)
            if case .boolean(true) = try ValueComparison.sql("=", a, b, type: type, collatedKey: collatedKey) { return .null }
            return a
        default: throw JedError("XX000", "invalid expression evaluator dispatch")
        }
    }

    private func evaluateContainer(_ expression: ResolvedExpression) throws -> SQLValue {
        switch expression.node {
        case .row(let fields): return .composite(try fields.map(evaluate))
        case .array(let sources, let nested):
            return try ArrayFunctions.construct(sources.map(evaluate), nested: nested,
                                                constant: sources.allSatisfy(FunctionResolver.isConstant), meter: meter)
        case .subscriptValue(let source, let subscripts):
            let value = try evaluate(source)
            var bounds: [ArrayFunctions.Subscript] = []
            func bound(_ expression: ResolvedExpression?) throws -> ArrayFunctions.Bound {
                guard let expression else { return .omitted }
                let value = try evaluate(expression)
                if value.isNull { return .value(nil) }
                guard case .integer(let number) = value else { throw JedError("42804", "array subscript must be an integer") }
                return .value(number)
            }
            for subscriptValue in subscripts {
                if subscriptValue.isSlice { bounds.append(.slice(lower: try bound(subscriptValue.lower), upper: try bound(subscriptValue.upper))) }
                else {
                    if case .value(let number) = try bound(subscriptValue.lower) { bounds.append(.index(number)) }
                    else { bounds.append(.index(nil)) }
                }
            }
            return try ArrayFunctions.subscriptValue(value, subscripts: bounds)
        case .rangeOperation(let op, let left, let right, let element):
            var a = try evaluate(left), b = try evaluate(right)
            if left.type?.type != .range(element), let type = left.type {
                a = try ContainerCasts.cast(a, from: type, to: TypeDescriptor(element), mode: .assignment, context: scalar)
            }
            if right.type?.type != .range(element), let type = right.type {
                b = try ContainerCasts.cast(b, from: type, to: TypeDescriptor(element), mode: .assignment, context: scalar)
            }
            return try RangeFunctions.binary(op, a, b, element: element)
        case .quantified(let op, let all, let operand, let array, let element):
            return try ArrayFunctions.quantified(op, all: all, operand: evaluate(operand), source: evaluate(array),
                                                 element: element,
                                                 meter: meter, comparisonWork: comparisonWork, collatedKey: collatedKey)
        case .field(let source, let index):
            let value = try evaluate(source)
            if value.isNull { return .null }
            guard case .composite(let fields) = value, fields.indices.contains(index) else { throw JedError("42804", "invalid record value") }
            return fields[index]
        default: throw JedError("XX000", "invalid expression evaluator dispatch")
        }
    }

    private func evaluateDelegated(_ expression: ResolvedExpression) throws -> SQLValue {
        switch expression.node {
        case .json(let expression): return try expression.evaluate(context: self)
        case .pattern(let pattern): return try pattern.evaluate(context: self)
        case .call(let function):
            if let result = try hostBatch?.take(function) { return result }
            var values = Array(repeating: SQLValue.null, count: function.arguments.count)
            let similarSubstring = function.hostID == nil && function.name == "substring" && function.arguments.count == 3
                && function.arguments[1].type?.type == .scalar(.text)
            for index in function.evaluationOrder {
                values[index] = try evaluate(function.arguments[index])
                if similarSubstring, case .constant(.null) = function.arguments[index].node { return .null }
            }
            return try ScalarFunctions.evaluate(function, values: values, context: self, environment: functions)
        default: throw JedError("XX000", "invalid expression evaluator dispatch")
        }
    }

    func convert(_ value: SQLValue, from source: SQLType?, to target: TypeDescriptor) throws -> SQLValue {
        if value.isNull { return .null }
        if source == target.type { return value }
        if case .scalar(let source) = source, case .scalar = target.type {
            return try ScalarCasts.cast(value, from: source, to: target, mode: .implicitCast, context: scalar)
        }
        if case .composite(let fields) = value, let sources = source.flatMap(TypeRules.fields), let targets = TypeRules.fields(target.type) {
            return .composite(try zip(fields.indices, targets).map { i, target in try convert(fields[i], from: sources[i].descriptor.type, to: target.descriptor) })
        }
        if case .array(let array) = value, case .array(let element) = source, case .array(let destination) = target.type {
            return .array(try SQLArray(dimensions: array.dimensions, lowerBounds: array.lowerBounds,
                                      elements: array.elements.map { try convert($0, from: element, to: TypeDescriptor(destination)) }))
        }
        throw JedError("42804", "cannot convert resolved value")
    }

    func comparisonWork(_ op: String, _ left: SQLValue, _ right: SQLValue, type: TypeDescriptor) throws {
        if left.isNull || right.isNull { return }
        switch (left, right) {
        case (.decimal(let a), .decimal(let b)): try DecimalFunctions.chargeWork(op, a, b, meter: meter)
        case (.integer(let a), .decimal(let b)): try DecimalFunctions.chargeWork(op, SQLDecimal(a), b, meter: meter)
        case (.decimal(let a), .integer(let b)): try DecimalFunctions.chargeWork(op, a, SQLDecimal(b), meter: meter)
        case (.text(let a), .text(let b)):
            if type.collation != nil && type.collation?.utf8 != [67] && op != "=" && op != "<>" {
                meter.charge(saturatingAdd(Int64(a.scalarCount), Int64(b.scalarCount)))
            } else { meter.charge(Int64(max(0, min(a.scalarCount, b.scalarCount) - 1))) }
            try meter.guardCost()
        case (.bytea(let a), .bytea(let b)):
            meter.charge(Int64(max(0, min(a.count, b.count) - 1))); try meter.guardCost()
        case (.array(let a), .array(let b)):
            if type.type == .array(.scalar(.text)), let name = type.collation, name.utf8 != [67], op != "=" && op != "<>" {
                var element = TypeDescriptor(.scalar(.text)); element.collation = name
                for (left, right) in zip(a.elements, b.elements) {
                    if left.isNull || right.isNull {
                        if left.isNull != right.isNull { break }
                        continue
                    }
                    try comparisonWork(op, left, right, type: element)
                    if try ValueComparison.total(left, right, type: element, collatedKey: collatedKey) != 0 { break }
                }
            }
        default: break
        }
    }

    private func arithmetic(_ op: String, _ a: SQLValue, _ b: SQLValue, type: TypeDescriptor, constant: Bool = false) throws -> SQLValue {
        func decimal(_ value: SQLValue) throws -> SQLDecimal {
            switch value { case .integer(let n): return SQLDecimal(n); case .decimal(let n): return n; default: throw JedError("42804", "not an exact numeric operand") }
        }
        if case .interval(let interval) = a, op == "*" || op == "/" {
            return .interval(try TemporalFunctions.scale(interval, factor: decimal(b), divide: op == "/"))
        }
        if case .interval(let interval) = b, op == "*" {
            return .interval(try TemporalFunctions.scale(interval, factor: decimal(a)))
        }
        switch (a, b) {
        case (.interval(let left), .interval(let right)) where op == "+" || op == "-":
            return .interval(try TemporalFunctions.add(left, right, subtract: op == "-"))
        case (.timestamp(let left), .interval(let right)) where op == "+" || op == "-":
            return .timestamp(try TemporalFunctions.shift(left, by: right, subtract: op == "-"))
        case (.timestamptz(let left), .interval(let right)) where op == "+" || op == "-":
            return .timestamptz(try TemporalFunctions.shift(left, by: right, subtract: op == "-", timezone: scalar.timezone, meter: meter))
        case (.interval, .timestamp) where op == "+",
             (.interval, .timestamptz) where op == "+",
             (.interval, .date) where op == "+":
            return try arithmetic(op, b, a, type: type)
        case (.date(let left), .interval(let right)) where op == "+" || op == "-":
            let stamp = try TemporalFunctions.cast(.date(left), to: .timestamp)
            return try arithmetic(op, stamp, .interval(right), type: type)
        case (.date(let left), .integer(let right)) where op == "+" || op == "-":
            return .date(try TemporalFunctions.shiftDate(left, by: right, subtract: op == "-"))
        case (.integer, .date) where op == "+": return try arithmetic(op, b, a, type: type)
        case (.date(let left), .date(let right)) where op == "-": return .integer(Int64(try TemporalFunctions.dateDifference(left, right)))
        case (.timestamp(let left), .timestamp(let right)) where op == "-",
             (.timestamptz(let left), .timestamptz(let right)) where op == "-":
            return .interval(try TemporalFunctions.timestampDifference(left, right))
        default: break
        }
        if case .scalar(.decimal) = type.type {
            func decimal(_ v: SQLValue) throws -> SQLDecimal {
                switch v { case .integer(let n): return SQLDecimal(n); case .decimal(let n): return n; default: throw JedError("42804", "not a decimal operand") }
            }
            return .decimal(try DecimalFunctions.binary(op, decimal(a), decimal(b), meter: meter))
        }
        if case .scalar(let width) = type.type, width == .f32 || width == .f64 {
            return try FloatFunctions.binary(op: op, left: a, right: b, width: width)
        }
        if case .integer(let x) = a, case .integer(let y) = b, case .scalar(let width) = type.type {
            let value: Int64
            switch op {
            case "+", "-", "*":
                let result = op == "+" ? x.addingReportingOverflow(y) : op == "-" ? x.subtractingReportingOverflow(y) : x.multipliedReportingOverflow(by: y)
                guard !result.overflow else { throw JedError("22003", "\(width.name) out of range").withDataType(width.name) }; value = result.partialValue
            case "/", "%":
                guard y != 0 else { throw JedError("22012", "division by zero") }
                if op == "%" && y == -1 { value = 0 }
                else { guard x != .min || y != -1 else { throw JedError("22003", "\(width.name) out of range").withDataType(width.name) }; value = op == "/" ? x / y : x % y }
            case "&": value = x & y
            case "|": value = x | y
            case "#": value = x ^ y
            default: throw JedError("42883", "integer operator does not exist")
            }
            return .integer(try ScalarCasts.checkedInteger(value, type: width))
        }
        if op == "||", case .text(let left) = a, case .text(let right) = b {
            if !constant { meter.charge(saturatingAdd(Int64(left.utf8.count), Int64(right.utf8.count))); try meter.guardCost() }
            return .text(SQLText(validatedUTF8: left.utf8 + right.utf8))
        }
        if op == "||", case .bytea(let left) = a, case .bytea(let right) = b {
            if !constant { meter.charge(saturatingAdd(Int64(left.count), Int64(right.count))); try meter.guardCost() }
            return .bytea(left + right)
        }
        if op == "||", case .jsonb(let left) = a, case .jsonb(let right) = b {
            return try JSONFunctions.concatenate(left, right, constant: constant, meter: meter)
        }
        throw JedError("42883", "resolved operator is not connected")
    }
}
