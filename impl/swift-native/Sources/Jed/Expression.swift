/// Resolved expressions carry static types independently of their current value.
/// A nil descriptor is an untyped SQL NULL/parameter, never an inferred row type.
/// The fields live in one copy-on-write box: the evaluator, planner and plan
/// caches copy expressions constantly, and a copy is then one retain rather
/// than a field-by-field copy of descriptors and child nodes.
internal struct ResolvedExpression {
    private final class Storage {
        var type: TypeDescriptor?
        var node: Node
        var position: Int
        var location: Int
        var collation: CollationDerivation?
        var rowPresence: Int?
        var nonimmutable: Bool
        init(type: TypeDescriptor?, node: Node, position: Int, location: Int,
             collation: CollationDerivation?, nonimmutable: Bool, rowPresence: Int?) {
            self.type = type; self.node = node; self.position = position; self.location = location
            self.collation = collation; self.nonimmutable = nonimmutable; self.rowPresence = rowPresence
        }
    }
    private var storage: Storage
    init(type: TypeDescriptor?, node: Node, position: Int, location: Int = 0,
         collation: CollationDerivation? = nil, nonimmutable: Bool = false, rowPresence: Int? = nil) {
        storage = Storage(type: type, node: node, position: position, location: location,
                          collation: collation, nonimmutable: nonimmutable, rowPresence: rowPresence)
    }
    private mutating func unique() -> Storage {
        if !isKnownUniquelyReferenced(&storage) {
            storage = Storage(type: storage.type, node: storage.node, position: storage.position,
                location: storage.location, collation: storage.collation, nonimmutable: storage.nonimmutable, rowPresence: storage.rowPresence)
        }
        return storage
    }
    var type: TypeDescriptor? {
        get { storage.type }
        set { unique().type = newValue }
    }
    var node: Node {
        get { storage.node }
        set { unique().node = newValue }
    }
    var position: Int {
        get { storage.position }
        set { unique().position = newValue }
    }
    /// Start of the authored expression, distinct from its operator token.
    var location: Int {
        get { storage.location }
        set { unique().location = newValue }
    }
    /// Cached query provenance; nil on a synthesized node derives from children.
    var collation: CollationDerivation? {
        get { storage.collation }
        set { unique().collation = newValue }
    }
    /// Input folding can consume session state without retaining an operation
    /// node (notably offsetless timestamptz inside a container text literal).
    var nonimmutable: Bool {
        get { storage.nonimmutable }
        set { unique().nonimmutable = newValue }
    }
    var rowPresence: Int? { storage.rowPresence }
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
    // Row-varying and per-evaluation fields stay inline. Row loops copy a
    // context and rebind `column` for every row, so everything else lives in
    // one copy-on-write box below: such a copy retains a handful of
    // references instead of every statement service.
    var column: (Int) throws -> SQLValue { didSet { hostBatchColumn = nil } }
    /// Only resident row owners install these peeks. Arbitrary column getters
    /// (including view expressions) may perform work and are never prefetched.
    var hostBatchColumn: ((Int) -> SQLValue?)?
    /// Internal differential-test switch; never exposed as a host setting.
    var columnarDisabled = false
    var parameterInference: ParameterInference? = nil
    var parameters: [SQLValue]
    var meter: CostMeter
    var collatedKey: KeyEncoding.CollatedKey?
    var outerRows: [(Int) throws -> SQLValue] = [] { didSet { hostBatchOuterRows = [] } }
    var hostBatchOuterRows: [((Int) -> SQLValue?)?] = []
    var hostBatch: HostBatch?
    /// Stored fields live in one copy-on-write box, so passing this value
    /// around costs one retain instead of a field-by-field copy.
    private final class Storage {
        var scalar: ScalarCasts.Context
        var functions: ScalarFunctions.Environment
        var sequences: SequenceExecution?
        var patterns: PatternExecution
        var subqueries: SubqueryExecution
        var ctes: CTEExecution?
        var writeScope: StatementWriteScope?
        var executionProfile: ExecutionProfile?
        var spill: SpillConfig?
        var errorDetail: ErrorDetail?
        var planningProbe: PlanningProbe?
        var eagerSubqueries: Bool
        init(scalar: ScalarCasts.Context, functions: ScalarFunctions.Environment, sequences: SequenceExecution?, patterns: PatternExecution, subqueries: SubqueryExecution, ctes: CTEExecution?, writeScope: StatementWriteScope?, executionProfile: ExecutionProfile?, spill: SpillConfig?, errorDetail: ErrorDetail?, planningProbe: PlanningProbe?, eagerSubqueries: Bool) {
            self.scalar = scalar
            self.functions = functions
            self.sequences = sequences
            self.patterns = patterns
            self.subqueries = subqueries
            self.ctes = ctes
            self.writeScope = writeScope
            self.executionProfile = executionProfile
            self.spill = spill
            self.errorDetail = errorDetail
            self.planningProbe = planningProbe
            self.eagerSubqueries = eagerSubqueries
        }
        func copy() -> Storage {
            Storage(scalar: scalar, functions: functions, sequences: sequences, patterns: patterns, subqueries: subqueries, ctes: ctes, writeScope: writeScope, executionProfile: executionProfile, spill: spill, errorDetail: errorDetail, planningProbe: planningProbe, eagerSubqueries: eagerSubqueries)
        }
    }
    private var storage: Storage
    @inline(__always) private mutating func unique() {
        if !isKnownUniquelyReferenced(&storage) { storage = storage.copy() }
    }
    var scalar: ScalarCasts.Context {
        get { storage.scalar }
        _modify { unique(); yield &storage.scalar }
    }
    var functions: ScalarFunctions.Environment {
        get { storage.functions }
        _modify { unique(); yield &storage.functions }
    }
    var sequences: SequenceExecution? {
        get { storage.sequences }
        _modify { unique(); yield &storage.sequences }
    }
    var patterns: PatternExecution {
        get { storage.patterns }
        _modify { unique(); yield &storage.patterns }
    }
    var subqueries: SubqueryExecution {
        get { storage.subqueries }
        _modify { unique(); yield &storage.subqueries }
    }
    var ctes: CTEExecution? {
        get { storage.ctes }
        _modify { unique(); yield &storage.ctes }
    }
    var writeScope: StatementWriteScope? {
        get { storage.writeScope }
        _modify { unique(); yield &storage.writeScope }
    }
    var executionProfile: ExecutionProfile? {
        get { storage.executionProfile }
        _modify { unique(); yield &storage.executionProfile }
    }
    var spill: SpillConfig? {
        get { storage.spill }
        _modify { unique(); yield &storage.spill }
    }
    var errorDetail: ErrorDetail? {
        get { storage.errorDetail }
        _modify { unique(); yield &storage.errorDetail }
    }
    var planningProbe: PlanningProbe? {
        get { storage.planningProbe }
        _modify { unique(); yield &storage.planningProbe }
    }
    // Derived/CTE bodies execute surviving subqueries at expression evaluation.
    var eagerSubqueries: Bool {
        get { storage.eagerSubqueries }
        _modify { unique(); yield &storage.eagerSubqueries }
    }
    init(meter: CostMeter, timezone: SQLTimeZone = .utc, statementMicros: Int64? = nil,
         parameters: [SQLValue] = [], column: @escaping (Int) throws -> SQLValue = { _ in throw JedError("XX000", "missing expression row") }) {
        self.meter = meter
        storage = Storage(scalar: ScalarCasts.Context(timezone: timezone, statementMicros: statementMicros, meter: meter),
            functions: ScalarFunctions.Environment(), sequences: nil, patterns: PatternExecution(),
            subqueries: SubqueryExecution(), ctes: nil, writeScope: nil, executionProfile: nil, spill: nil,
            errorDetail: nil, planningProbe: nil, eagerSubqueries: true)
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
            if Self.operatorCode(operation) == 1 {
                meter.charge(1); try meter.step()
                guard try matches(left) else { return false }
                return try matches(right)
            }
        }
        if case .boolean(true) = try evaluate(expression) { return true }
        return false
    }

    func evaluate(_ expression: ResolvedExpression) throws -> SQLValue {
        let node = expression.node
        switch node {
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
        case .binary(let op, let left, let right, _):
            meter.charge(1); try meter.step()
            return try evaluateBinary(op, left, right, expression)
        case .call(let function): meter.charge(function.cost); try meter.step()
        default: meter.charge(1); try meter.step()
        }
        switch node {
        case .currentTimestamp, .currentDate, .dateInput:
            return try evaluateState(expression)
        case .cast, .unary:
            return try evaluateUnary(expression)
        case .isNull, .distinct:
            return try evaluatePredicate(expression)
        case .conditional, .coalesce, .extreme, .nullif:
            return try evaluateConditional(expression)
        case .row, .array, .subscriptValue, .rangeOperation, .quantified, .field:
            return try evaluateContainer(expression)
        case .json, .pattern, .call:
            return try evaluateDelegated(expression)
        case .constant, .column, .parameter, .convert, .outerColumn, .subquery, .binary: throw JedError("XX000", "invalid expression dispatch")
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

    /// Operators are resolved strings. Comparing Swift strings that differ
    /// takes the slow Unicode path, so classify the UTF-8 bytes once instead.
    /// 0 other, 1 and, 2 or, 3 =, 4 <>, 5 <, 6 <=, 7 >, 8 >=,
    /// 9 +, 10 -, 11 *, 12 /, 13 %, 14 &, 15 |, 16 #.
    @inline(__always)
    private static func operatorCode(_ op: String) -> Int {
        let utf8 = op.utf8, count = utf8.count
        guard count >= 1, count <= 3 else { return 0 }
        let first = utf8[utf8.startIndex]
        if count == 1 {
            switch first {
            case 61: return 3
            case 60: return 5
            case 62: return 7
            case 43: return 9
            case 45: return 10
            case 42: return 11
            case 47: return 12
            case 37: return 13
            case 38: return 14
            case 124: return 15
            case 35: return 16
            default: return 0
            }
        }
        let second = utf8[utf8.index(after: utf8.startIndex)]
        if count == 2 {
            if first == 60 { return second == 62 ? 4 : (second == 61 ? 6 : 0) }
            if first == 62 { return second == 61 ? 8 : 0 }
            return first == 111 && second == 114 ? 2 : 0
        }
        return first == 97 && second == 110 && utf8[utf8.index(utf8.startIndex, offsetBy: 2)] == 100 ? 1 : 0
    }

    /// The operand descriptor is copied out of the node only on the paths
    /// that consult it; integer comparisons never do.
    private func evaluateBinary(_ op: String, _ left: ResolvedExpression, _ right: ResolvedExpression,
                                _ expression: ResolvedExpression) throws -> SQLValue {
        let code = Self.operatorCode(op)
        let a = try evaluate(left)
        if code == 1, case .boolean(false) = a { return .boolean(false) }
        if code == 2, case .boolean(true) = a { return .boolean(true) }
        let b = try evaluate(right)
        if code == 1 {
            if case .boolean(false) = b { return .boolean(false) }
            return a.isNull || b.isNull ? .null : .boolean(true)
        }
        if code == 2 {
            if case .boolean(true) = b { return .boolean(true) }
            return a.isNull || b.isNull ? .null : .boolean(false)
        }
        if code >= 3 && code <= 8 {
            // Integer pairs charge no comparison work and order exactly
            // as ValueComparison.total orders them.
            if case .integer(let x) = a, case .integer(let y) = b {
                switch code {
                case 3: return .boolean(x == y)
                case 4: return .boolean(x != y)
                case 5: return .boolean(x < y)
                case 6: return .boolean(x <= y)
                case 7: return .boolean(x > y)
                default: return .boolean(x >= y)
                }
            }
            guard case .binary(_, _, _, let operandType) = expression.node else {
                throw JedError("XX000", "invalid expression evaluator dispatch")
            }
            try comparisonWork(op, a, b, type: operandType)
            return try ValueComparison.sql(op, a, b, type: operandType, collatedKey: collatedKey)
        }
        if a.isNull || b.isNull { return .null }
        guard case .binary(_, _, _, let operandType) = expression.node else {
            throw JedError("XX000", "invalid expression evaluator dispatch")
        }
        if code >= 9, case .integer(let x) = a, case .integer(let y) = b, case .scalar(let width) = operandType.type,
           width == .i16 || width == .i32 || width == .i64 {
            return try Self.integerArithmetic(code, x, y, width: width)
        }
        return try arithmetic(op, a, b, type: operandType,
            constant: FunctionResolver.isConstant(left) && FunctionResolver.isConstant(right))
    }

    /// The integer arm of `arithmetic`, dispatched by operator code. Integer
    /// operands of an integer-typed operator reach no earlier arm there.
    private static func integerArithmetic(_ code: Int, _ x: Int64, _ y: Int64, width: ScalarType) throws -> SQLValue {
        let value: Int64
        switch code {
        case 9, 10, 11:
            let result = code == 9 ? x.addingReportingOverflow(y) : code == 10 ? x.subtractingReportingOverflow(y) : x.multipliedReportingOverflow(by: y)
            guard !result.overflow else { throw JedError("22003", "\(width.name) out of range").withDataType(width.name) }
            value = result.partialValue
        case 12, 13:
            guard y != 0 else { throw JedError("22012", "division by zero") }
            if code == 13 && y == -1 { value = 0 }
            else {
                guard x != .min || y != -1 else { throw JedError("22003", "\(width.name) out of range").withDataType(width.name) }
                value = code == 12 ? x / y : x % y
            }
        case 14: value = x & y
        case 15: value = x | y
        default: value = x ^ y
        }
        return .integer(try ScalarCasts.checkedInteger(value, type: width))
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
        case .row(let fields):
            if let presence = expression.rowPresence, try evaluate(fields[presence]).isNull { return .null }
            return .composite(try fields.map(evaluate))
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
