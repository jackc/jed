/// A parsed statement can outlive the handle which prepared it. Execution
/// always belongs to the supplied Database or Session, including transactions,
/// privileges, temporary names, limits, and cancellation. Like Session and Rows,
/// a prepared statement is thread-affine; prepare another on a different thread.
public final class PreparedStatement {
    internal let source: SQLText
    internal let statement: SQLStatement
    internal var insertCache: PreparedInsertPlan?
    internal var mutationCache: PreparedMutationPlan?
    internal var selectCache: PreparedSelectCache?
    internal var selectCacheHits = 0
    /// Executions served by the cached single-row key read.
    internal var pointReads = 0
    internal var selectCacheMisses = 0
    internal var probePlans = 0
    internal var customPlans = 0
    internal private(set) var insertCacheHits = 0
    internal private(set) var insertCacheMisses = 0

    internal init(source: SQLText, statement: SQLStatement) {
        self.source = source; self.statement = statement
    }
    /// A cached write plan's target passed `CatalogEnvironment.checkWritable`
    /// for this exact schema and environment. That check reads only the
    /// table's definition, the catalog's types and collations, and the
    /// environment's loaded data, so it need not be repeated while all three
    /// are the same objects. A domain with borrowed types is never recorded.
    private var writableProof: (signature: PreparedSchemaSignature, environment: Environment)?
    internal func provenWritable(_ signature: PreparedSchemaSignature, snapshot: DomainSnapshot,
                                 settings: SessionSettings) -> Bool {
        guard snapshot.borrowedTypes.isEmpty, let proof = writableProof else { return false }
        return proof.signature == signature && proof.environment === settings.environment
    }
    internal func proveWritable(_ signature: PreparedSchemaSignature, snapshot: DomainSnapshot,
                                settings: SessionSettings) {
        if snapshot.borrowedTypes.isEmpty { writableProof = (signature, settings.environment) }
    }
    internal func insertHit() { insertCacheHits += 1 }
    internal func insertMiss() { insertCacheMisses += 1 }
}

/// Equality tokens retain no database, session, roots, or byte-device owner.
internal struct PreparedSchemaSignature: Equatable {
    let core: StorageRevision
    let domain: StorageRevision
    let catalog: StorageRevision
    let name: SQLText

    init(core: StorageRevision, snapshot: DomainSnapshot, name: SQLText) {
        self.core = core; domain = snapshot.identity; catalog = snapshot.catalogRevision; self.name = name
    }
    static func == (a: Self, b: Self) -> Bool {
        a.core === b.core && a.domain === b.domain && a.catalog === b.catalog && a.name == b.name
    }
}

internal enum PreparedExpressions {
    static func reusable(_ expressions: [ResolvedExpression]) -> Bool {
        var reusable = true
        StoredExpressionDependencies.walk(expressions, queryBodies: true) { expression in
            if expression.nonimmutable { reusable = false }
            switch expression.node {
            // Both frozen resolvers exclude every subquery from prepared
            // caches. Correlated plans also carry execution roots/settings.
            case .subquery: reusable = false
            case .pattern(let pattern): if pattern.program != nil { reusable = false }
            case .call(let call):
                if call.regex?.program != nil || call.regex?.similar != nil { reusable = false }
            default: break
            }
        }
        return reusable
    }
}
