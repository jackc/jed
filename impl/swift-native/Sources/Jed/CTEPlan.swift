/// WITH owns a lexical binding layer, independent of correlated row scopes.
/// Binding identity, rather than a runtime name lookup, preserves shadowing.
internal struct CTEReference {
    unowned let binding: CTEBinding
    let selfReference: Bool
}

internal final class CTEBinding {
    let name: SQLText
    var columns: [ResultColumn]
    var collations: [CollationDerivation?]
    let plan: SelectPlan?
    let mutation: SQLStatement?
    let mutationSchema: MutationSchema?
    let settings: SessionSettings
    let materialized: Bool?
    var references = 0
    var resolvingSelf = false
    var hiddenColumns: Int?
    func reference() throws -> CTEReference {
        guard mutationSchema?.noReturning != true else {
            throw JedError("0A000", "WITH query \(name) does not have a RETURNING clause")
        }
        if !resolvingSelf { references += 1 }
        return CTEReference(binding: self, selfReference: resolvingSelf)
    }
    var recursive: RecursiveCTEPlan?
    var executionBuffers: Bool?
    var buffers: Bool { executionBuffers ?? (mutation != nil || recursive != nil || (materialized ?? (references > 1))) }
    var expressions: [ResolvedExpression] {
        (plan?.expressions ?? mutationSchema?.expressions ?? []) + (recursive?.expressions ?? [])
    }
    init(name: SQLText, columns: [ResultColumn], plan: SelectPlan, settings: SessionSettings, materialized: Bool?) {
        self.name = name; self.columns = columns; self.plan = plan; self.settings = settings; self.materialized = materialized
        mutation = nil; mutationSchema = nil
        collations = plan.projections.map(\.collation)
    }
    init(name: SQLText, columns: [ResultColumn], statement: SQLStatement, schema: MutationSchema,
         settings: SessionSettings) {
        self.name = name; self.columns = columns; self.settings = settings
        mutation = statement; mutationSchema = schema; materialized = true; plan = nil
        collations = schema.expressions.map(\.collation)
    }
}

internal final class WithQueryPlan {
    let bindings: [CTEBinding]
    let body: SelectPlan
    var expressions: [ResolvedExpression] {
        bindings.filter { $0.references > 0 }.flatMap { $0.expressions } + body.expressions
    }
    init(bindings: [CTEBinding], body: SelectPlan) { self.bindings = bindings; self.body = body }

    static func resolve(_ clause: SQLWith, body: SQLQuery, snapshot: DomainSnapshot, settings: SessionSettings,
                        scalar: ScalarCasts.Context, using shared: ExpressionResolver?, finalizeParameters: Bool) throws -> SelectPlan {
        let resolver = shared ?? ExpressionResolver(composites: snapshot.types, scalar: scalar)
        let inherited = resolver.cteBindings
        defer { resolver.cteBindings = inherited }
        // Writable bindings belong only to the statement-level orchestrator.
        guard clause.bindings.allSatisfy({ $0.body.queryBody != nil }) else {
            throw JedError("0A000", "WITH containing a data-modifying statement is only supported at the top level")
        }
        let bindings = try resolveBindings(clause, snapshot: snapshot, settings: settings, scalar: scalar, using: resolver)
        let plan = try SelectPlan.resolve(body, snapshot: snapshot, settings: settings, scalar: scalar,
            using: resolver, finalizeParameters: false)
        var result = plan
        result.parameters = try finalizeParameters ? ParameterBinding.finalize(resolver) : []
        result.withQuery = WithQueryPlan(bindings: bindings, body: plan)
        return result
    }

    static func resolveBindings(_ clause: SQLWith, snapshot: DomainSnapshot, settings: SessionSettings,
                                scalar: ScalarCasts.Context, using resolver: ExpressionResolver,
                                mutationContext: ExpressionContext? = nil) throws -> [CTEBinding] {
        var names: Set<SQLText> = [], bindings: [CTEBinding] = []
        for syntax in clause.bindings {
            guard names.insert(syntax.name).inserted else { throw JedError("42712", "WITH query name specified more than once: \(syntax.name)") }
            let child = ExpressionResolver(composites: snapshot.types, scalar: scalar)
            child.cteBindings = resolver.cteBindings
            child.outerScopes = resolver.outerScopes
            child.parameters = resolver.parameters; child.parameterUses = resolver.parameterUses
            child.viewExpansion = resolver.viewExpansion
            guard let query = syntax.body.queryBody else {
                guard let context = mutationContext else {
                    throw JedError("0A000", "WITH containing a data-modifying statement is only supported at the top level")
                }
                let counts = resolver.cteBindings.values.map { ($0, $0.references) }
                defer { for (binding, count) in counts { binding.references = count } }
                let domains = QueryDomains.statement(snapshot: snapshot, settings: settings)
                let owner = try StatementDomain.target(syntax.body, in: domains)
                var mutationSettings = settings; mutationSettings.targetDomain = owner
                let schema = try MutationExecutor.prepareReturning(syntax.body, snapshot: domains.snapshot(owner),
                    settings: mutationSettings, context: context, using: child)
                guard syntax.columns.count <= schema.columns.count else {
                    throw JedError("42P10", "WITH query has fewer columns than specified")
                }
                let columns = schema.columns.enumerated().map { index, column in
                    ResultColumn(name: index < syntax.columns.count ? syntax.columns[index] : column.name, type: column.type)
                }
                let binding = CTEBinding(name: syntax.name, columns: columns, statement: syntax.body,
                    schema: schema, settings: mutationSettings)
                guard syntax.search == nil && syntax.cycle == nil else { throw JedError("42601", "WITH query is not recursive") }
                resolver.parameters = child.parameters; resolver.parameterUses.formUnion(child.parameterUses)
                resolver.outerReferences.formUnion(child.outerReferences)
                bindings.append(binding); resolver.cteBindings[syntax.name] = binding
                continue
            }
            let marks = try RecursiveColumns.marks(syntax.cycle, resolver: child)
            let shape = try clause.recursive ? RecursiveCTEPlan.analyze(query, name: syntax.name,
                augmented: syntax.search != nil || syntax.cycle != nil) : nil
            if shape == nil && (syntax.search != nil || syntax.cycle != nil) {
                throw JedError("42601", "WITH query is not recursive")
            }
            let anchor = try SelectPlan.resolve(shape?.anchor ?? query, snapshot: snapshot, settings: settings, scalar: scalar,
                using: child, finalizeParameters: false)
            guard syntax.columns.count <= anchor.columns.count else { throw JedError("42P10", "WITH query has fewer columns than specified") }
            let columns = anchor.columns.enumerated().map { index, column in
                ResultColumn(name: index < syntax.columns.count ? syntax.columns[index] : column.name, type: column.type)
            }
            let binding = CTEBinding(name: syntax.name, columns: columns, plan: anchor, settings: settings, materialized: syntax.materialized)
            if let shape {
                let augmentation = try RecursiveColumns.resolve(syntax, columns: columns, marks: marks, resolver: child)
                if let augmentation {
                    for (name, output) in zip(augmentation.names, augmentation.anchor) {
                        binding.columns.append(ResultColumn(name: name, type: output.type))
                        binding.collations.append(output.collation)
                    }
                }
                child.cteBindings[syntax.name] = binding
                binding.resolvingSelf = true; binding.hiddenColumns = columns.count
                defer { binding.resolvingSelf = false; binding.hiddenColumns = nil }
                var termSyntax = shape.term
                if let augmentation, case .select(var select) = termSyntax.body {
                    for name in augmentation.names {
                        let expression = RecursiveColumns.expression(.column(qualifier: shape.label, name: name))
                        select.items.append(SQLSelectItem(expression: expression, span: expression.span))
                    }
                    termSyntax.body = .select(select)
                }
                let term = try SelectPlan.resolve(termSyntax, snapshot: snapshot, settings: settings, scalar: scalar,
                    using: child, finalizeParameters: false)
                try RecursiveCTEPlan.checkColumns(anchor: anchor, term: term, all: shape.all, carried: augmentation?.names.count ?? 0)
                binding.recursive = RecursiveCTEPlan(term: term, all: shape.all, augmentation: augmentation)
            }
            resolver.parameters = child.parameters; resolver.parameterUses.formUnion(child.parameterUses)
            resolver.outerReferences.formUnion(child.outerReferences)
            bindings.append(binding); resolver.cteBindings[syntax.name] = binding
        }
        return bindings
    }
}

extension SQLStatement {
    var queryBody: SQLQuery? {
        switch self {
        case .query(let query): return query
        case .with(let clause, let statement):
            guard let body = statement.queryBody else { return nil }
            return SQLQuery(body: .with(clause, body), span: body.span)
        default: return nil
        }
    }
}
