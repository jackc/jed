internal struct OuterColumnReference: Hashable {
    let depth: Int
    let index: Int
}

internal struct SubqueryScope {
    let snapshot: DomainSnapshot
    let settings: SessionSettings
}

internal struct SubqueryPlanKey: Hashable {
    let start: Int
    let end: Int
    let kind: String
    let arity: Int
}

/// Query bodies keep their own lexical scopes, but share parameter inference
/// with the containing statement. No row values participate in resolution.
internal final class ResolvedSubquery {
    enum Kind { case scalar, exists, membership(String, Bool, Bool, Bool, ResolvedExpression) }
    let plan: SelectPlan
    let settings: SessionSettings
    let kind: Kind
    let type: TypeDescriptor?
    let rowArity: Int?
    let outerReferences: Set<OuterColumnReference>
    var correlated: Bool { !outerReferences.isEmpty }
    // Frozen cores leave ORDER-only subqueries outside the initplan fold.
    // A shared projection occurrence can still fold this identity first.
    var runtimeOrderKey = false
    var operand: ResolvedExpression? {
        if case .membership(_, _, _, _, let value) = kind { return value }
        return nil
    }
    init(plan: SelectPlan, settings: SessionSettings, kind: Kind, type: TypeDescriptor?,
         rowArity: Int?, outerReferences: Set<OuterColumnReference>) {
        self.plan = plan; self.settings = settings; self.kind = kind; self.type = type
        self.rowArity = rowArity; self.outerReferences = outerReferences
    }
}

extension ExpressionResolver {
    func resolveOuterColumn(qualifier: SQLText?, name: SQLText, position: Int) throws -> ResolvedExpression {
        // A local relation label shadows the same outer label, even when the
        // requested column is absent from that local relation.
        if qualifier == nil || !columns.contains(where: { $0.qualifier == qualifier }) {
            for (level, scope) in outerScopes.enumerated() {
                if let qualifier, scope.contains(where: { $0.qualifier == qualifier && $0.ambiguousRelation }) {
                    throw JedError("42P09", "table reference is ambiguous: \(qualifier)")
                }
                if let qualifier, scope.contains(where: { $0.qualifier == qualifier && $0.lateralDenied }) {
                    throw JedError("42P10", "invalid reference to FROM-clause entry in a lateral outer join").detailed(ErrorHints.lateralDetail)
                }
                let matches = scope.filter { $0.name == name && (qualifier == nil ? !$0.qualifiedOnly : $0.qualifier == qualifier) }
                if let column = matches.first {
                    if column.lateralDenied { throw JedError("42P10", "invalid reference to FROM-clause entry in a lateral outer join").detailed(ErrorHints.lateralDetail) }
                    guard matches.count == 1 else { throw JedError("42702", "column reference is ambiguous: \(name)") }
                    if column.ungroupedAncestor { throw JedError("42803", "subquery uses ungrouped column \(name) from outer query") }
                    let reference = OuterColumnReference(depth: level + 1, index: column.index)
                    outerReferences.insert(reference)
                    return ResolvedExpression(type: column.unresolved ? nil : column.type, node: .outerColumn(reference), position: position,
                        collation: column.collation ?? (ExpressionCollation.collatable(column.type.type) ? .implicit(column.type.collation ?? SQLText(validatedUTF8: [67])) : nil))
                }
                if let qualifier, scope.contains(where: { $0.qualifier == qualifier }) { break }
            }
        }
        // Columns at every enclosing level win before relation labels.
        if qualifier == nil {
            for (level, scope) in ([columns] + outerScopes).enumerated() {
                let matches = scope.filter { $0.qualifier == name && !$0.hiddenFromStar }
                guard !matches.isEmpty else { continue }
                if matches.contains(where: { $0.ambiguousRelation }) { throw JedError("42P09", "table reference is ambiguous: \(name)") }
                if matches.contains(where: { $0.lateralDenied }) { throw JedError("42P10", "invalid lateral reference").detailed(ErrorHints.lateralDetail) }
                if matches.contains(where: { $0.ungroupedAncestor }) { throw JedError("42803", "subquery uses ungrouped column from outer query") }
                let scalar = matches.count == 1 && matches[0].scalarFunction
                if !scalar && storageCollation { throw JedError("0A000", "whole-row references are not supported in index expressions or predicates") }
                let presence = matches.contains(where: { $0.rowNullable }) ? matches.firstIndex(where: { $0.rowNotNull }) : nil
                if !scalar && matches.contains(where: { $0.rowNullable }) && presence == nil {
                    throw JedError("0A000", "NULL-extendable whole-row reference has no NOT NULL column")
                }
                let fields = matches.map { column -> ResolvedExpression in
                    let node: ResolvedExpression.Node
                    if level == 0 { referencedColumns.insert(column.index); node = .column(column.index) }
                    else {
                        let reference = OuterColumnReference(depth: level, index: column.index)
                        outerReferences.insert(reference); node = .outerColumn(reference)
                    }
                    return ResolvedExpression(type: column.unresolved ? nil : column.type, node: node, position: position,
                        collation: column.collation ?? (ExpressionCollation.collatable(column.type.type) ? .implicit(column.type.collation ?? SQLText(validatedUTF8: [67])) : nil))
                }
                if scalar { return fields[0] }
                let types = matches.map { TypeField(name: $0.rowName ?? $0.name, descriptor: $0.type) }
                return ResolvedExpression(type: TypeDescriptor(.record(types)), node: .row(fields), position: position, rowPresence: presence)
            }
        }
        if let qualifier, !([columns] + outerScopes).contains(where: { $0.contains { $0.qualifier == qualifier } }) {
            throw JedError("42P01", "missing FROM-clause entry for table: \(qualifier)")
        }
        throw JedError("42703", "column does not exist: \(name)")
    }

    func syntacticRowArity(_ expression: SQLExpression) -> Int? {
        switch expression.kind {
        case .row(let fields): fields.count
        case .parenthesized(let child): syntacticRowArity(child)
        default: nil
        }
    }
    func scalarSubquery(_ expression: SQLExpression) -> SQLQuery? {
        switch expression.kind {
        case .scalarQuery(let query): query
        case .parenthesized(let child): scalarSubquery(child)
        default: nil
        }
    }

    func subqueryExpression(_ query: SQLQuery, kind: ResolvedSubquery.Kind, rowArity: Int? = nil,
                            position: Int) throws -> ResolvedExpression {
        guard let scope = queryScope else { throw JedError("0A000", "subquery is not allowed in this expression") }
        let label: String
        switch kind {
        case .scalar: label = "scalar"
        case .exists: label = "exists"
        case .membership(let op, let all, let negated, let member, _): label = "\(op)/\(all)/\(negated)/\(member)"
        }
        let key = SubqueryPlanKey(start: query.span.byteStart, end: query.span.byteEnd, kind: label, arity: rowArity ?? 0)
        if let existing = subqueryPlans[key] { return subqueryNode(existing, position: position) }
        let child = ExpressionResolver(composites: composites, scalar: scalar)
        let ancestorColumns = columns.map { column in
            var column = column
            if let groupedColumns { column.ungroupedAncestor = !groupedColumns.contains(column.index) }
            return column
        }
        child.outerScopes = [ancestorColumns] + outerScopes
        child.outerNameHints = [nameHints] + outerNameHints
        child.nameHintFrames = nameHintFrames
        child.parameters = parameters; child.parameterUses = parameterUses
        child.viewExpansion = viewExpansion
        child.cteBindings = cteBindings
        child.cteExpansionEnabled = cteExpansionEnabled
        let plan = try SelectPlan.resolve(query, snapshot: scope.snapshot, settings: scope.settings,
            scalar: scalar, using: child, finalizeParameters: false)
        parameters = child.parameters; parameterUses.formUnion(child.parameterUses)
        for reference in child.outerReferences {
            if reference.depth == 1 { referencedColumns.insert(reference.index) }
            else { outerReferences.insert(OuterColumnReference(depth: reference.depth - 1, index: reference.index)) }
        }
        let outputType: TypeDescriptor?
        if case .exists = kind { outputType = TypeDescriptor(.scalar(.boolean)) }
        else {
            guard plan.columns.count == (rowArity ?? 1) else {
                throw JedError("42601", plan.columns.count > (rowArity ?? 1) ? "subquery has too many columns" : "subquery has too few columns")
            }
            if rowArity != nil {
                outputType = TypeDescriptor(.record(plan.columns.enumerated().map { ordinal, column in
                    TypeField(name: SQLText(validatedUTF8: Array("f\(ordinal + 1)".utf8)), descriptor: column.type ?? TypeDescriptor(.scalar(.text)))
                }))
            } else { outputType = plan.columns[0].type }
        }
        let value = ResolvedSubquery(plan: plan, settings: scope.settings, kind: kind, type: outputType,
            rowArity: rowArity, outerReferences: child.outerReferences)
        subqueryPlans[key] = value
        return subqueryNode(value, position: position)
    }

    private func subqueryNode(_ query: ResolvedSubquery, position: Int) -> ResolvedExpression {
        let type: TypeDescriptor?
        if case .membership = query.kind { type = TypeDescriptor(.scalar(.boolean)) } else { type = query.type }
        let collation = if case .scalar = query.kind, query.rowArity == nil { query.plan.projections.first?.collation?.asColumn } else { nil as CollationDerivation? }
        return ResolvedExpression(type: type, node: .subquery(query), position: position, collation: collation)
    }

    func membershipSubquery(_ operand: SQLExpression, query: SQLQuery, operation: String, all: Bool,
                            negated: Bool, membership: Bool, position: Int) throws -> ResolvedExpression {
        let rowArity = syntacticRowArity(operand)
        // Resolve the body's type before adopting a bare literal on the left.
        let body = try subqueryExpression(query, kind: .scalar, rowArity: rowArity, position: position)
        guard case .subquery(let source) = body.node else { throw JedError("XX000", "missing subquery plan") }
        let left = try resolve(operand, expected: body.type)
        let comparison: SQLType?
        do {
            try TypeRules.comparable(left.type?.type, body.type?.type)
            comparison = rowArity == nil ? try TypeRules.common(left.type?.type, body.type?.type) : left.type?.type
        } catch {
            if !membership { throw JedError("42883", "ANY/ALL comparison operator does not exist") }
            throw error
        }
        var type = comparison.map(TypeDescriptor.init)
        if ExpressionCollation.collatable(comparison) { type?.collation = try demandCollation([left, body]) }
        let key = SubqueryPlanKey(start: query.span.byteStart, end: query.span.byteEnd,
            kind: "\(operation)/\(all)/\(negated)/\(membership)", arity: rowArity ?? 0)
        let resolved: ResolvedSubquery
        if let existing = subqueryPlans[key] { resolved = existing }
        else {
            resolved = ResolvedSubquery(plan: source.plan, settings: source.settings,
                kind: .membership(operation, all, negated, membership, left), type: type,
                rowArity: rowArity, outerReferences: source.outerReferences)
            subqueryPlans[key] = resolved
        }
        return subqueryNode(resolved, position: position)
    }
}
