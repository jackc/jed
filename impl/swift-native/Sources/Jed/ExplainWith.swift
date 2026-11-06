/// Full DML preparation is needed to describe a writable WITH, but execution
/// still belongs to its ordinary coordinator and its one pinned read map.
internal final class ExplainWithPlan {
    let plan: WritableCTEPlan
    let mutations: [ObjectIdentifier: MutationExecutor.Preparation]
    let primary: MutationExecutor.Preparation?
    let profileIdentity: QueryProfileIdentity
    let inlineAtReference: Set<ObjectIdentifier>
    init(_ plan: WritableCTEPlan, snapshot: DomainSnapshot, settings: SessionSettings,
         context: ExpressionContext) throws {
        self.plan = plan; self.profileIdentity = QueryProfileIdentity()
        // Early WITH resolution counts DML syntax to choose execution modes.
        // Only references already resolved in query bodies move a definition's
        // display subtree beneath its reference; late DML references do not.
        var references = Dictionary(uniqueKeysWithValues: plan.bindings.map { (ObjectIdentifier($0), $0.references) })
        let mutationStatements = plan.bindings.compactMap(\.mutation) + (WritableCTEPlan.isMutation(plan.body) ? [plan.body] : [])
        for statement in mutationStatements {
            for (name, count) in CTEReferenceCounts.count(statement) {
                if let binding = plan.visibleBindings[name] { references[ObjectIdentifier(binding), default: 0] -= count }
            }
        }
        inlineAtReference = Set(references.filter { $0.value > 0 }.map(\.key))
        let domains = QueryDomains.statement(snapshot: snapshot, settings: settings)
        var visible: [SQLText: CTEBinding] = [:], prepared: [ObjectIdentifier: MutationExecutor.Preparation] = [:]
        func resolve(_ statement: SQLStatement, visible: [SQLText: CTEBinding]) throws -> MutationExecutor.Preparation {
            let owner = try StatementDomain.target(statement, in: domains)
            var scoped = settings; scoped.domains = domains; scoped.targetDomain = owner
            let read = try domains.snapshot(owner)
            var current = context
            current.writeScope = StatementWriteScope(readSnapshot: read, cteBindings: visible,
                                                    foreignKeys: ForeignKeyActions.State(deferUntilStatementEnd: true))
            guard let result = try MutationExecutor.prepare(statement, snapshot: read, settings: scoped, context: current) else {
                throw JedError("0A000", "EXPLAIN of this writable WITH body is not yet connected")
            }
            return result
        }
        for binding in plan.bindings {
            if let statement = binding.mutation { prepared[ObjectIdentifier(binding)] = try resolve(statement, visible: visible) }
            visible[binding.name] = binding
        }
        mutations = prepared
        primary = plan.query == nil ? try resolve(plan.body, visible: plan.visibleBindings) : nil
    }
    private init(plan: WritableCTEPlan, mutations: [ObjectIdentifier: MutationExecutor.Preparation],
                 primary: MutationExecutor.Preparation, profileIdentity: QueryProfileIdentity,
                 inlineAtReference: Set<ObjectIdentifier>) {
        self.plan = plan; self.mutations = mutations; self.primary = primary; self.profileIdentity = profileIdentity
        self.inlineAtReference = inlineAtReference
    }
    /// CTAS changes only the primary query into its ordinary INSERT SELECT.
    /// Binding plans and their structural profile identities remain unchanged.
    func filling(_ plan: WritableCTEPlan, primary: MutationExecutor.Preparation) -> ExplainWithPlan {
        ExplainWithPlan(plan: plan, mutations: mutations, primary: primary, profileIdentity: profileIdentity,
                        inlineAtReference: inlineAtReference)
    }
}

extension ExplainTree {
    func writableWith(_ prepared: ExplainWithPlan) throws -> ExplainNode {
        let plan = prepared.plan
        var definitions: [ExplainNode] = [], cost: Int64 = 0
        for binding in plan.bindings {
            let body: ExplainNode
            let mutation = prepared.mutations[ObjectIdentifier(binding)]
            if let mutation { body = try self.mutation(mutation) }
            else if let query = binding.plan { body = try self.query(query) }
            else { throw JedError("XX000", "WITH binding has no prepared body") }
            let materialized = binding.buffers
            let expanded = !materialized && prepared.inlineAtReference.contains(ObjectIdentifier(binding))
            register(binding, body: body, inlineAtReference: expanded)
            let contribution = mutation != nil || materialized && binding.references > 0 ? body.cost : 0
            cost = saturatingAdd(cost, contribution)
            let definition = ExplainNode("CTE " + binding.name.description, [materialized ? "materialized" : "inlined"],
                rows: body.rows, cost: contribution, children: expanded ? [] : [body])
            let profile = context.executionProfile
            if let mutation {
                let identity = mutation.profileIdentity
                definition.actual = { profile?.value(identity, .total) ?? 0 }
            } else { definition.actual = { profile?.value(binding, .total) ?? 0 } }
            definitions.append(definition)
        }
        let body: ExplainNode
        if let query = plan.query { body = try self.query(query) }
        else if let mutation = prepared.primary { body = try self.mutation(mutation) }
        else { throw JedError("XX000", "WITH primary body has no prepared plan") }
        let result = ExplainNode("WITH", ["ctes=\(plan.bindings.count)"], rows: body.rows,
                                cost: saturatingAdd(cost, body.cost), children: definitions + [body])
        let profile = context.executionProfile, identity = prepared.profileIdentity
        result.actual = { profile?.value(identity, .total) ?? 0 }
        return result
    }
}
