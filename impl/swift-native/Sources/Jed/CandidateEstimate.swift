/// Runtime-unit arithmetic shared by physical access estimates. Cardinality and
/// page selection stay in the owning planner; this combines their work counts.
internal struct CandidateEstimate {
    enum Kind: String {
        case primary = "pk", ordered = "btree", gist, gin
        case primaryIntervals = "pk_interval", orderedIntervals = "index_interval", full
        var rank: Int {
            switch self { case .primary: 0; case .ordered: 1; case .gist: 2; case .gin: 3
            case .primaryIntervals: 4; case .orderedIntervals: 5; case .full: 6 }
        }
        var fetchesTable: Bool {
            switch self { case .ordered, .gist, .gin, .orderedIntervals: true; default: false }
        }
    }
    let rows: Int64
    let cost: Int64
    private let scanned: Int64, pages: Int64, evaluations: Int64, produced: Int64
    private let ginEntries: Int64?, gistDescents: Int64?
    /// Per-unit counts, built on demand for the shared estimator vectors.
    var units: [String: Int64] {
        var units: [String: Int64] = [
            "storage_row_read": scanned, "page_read": pages,
            "operator_eval": evaluations, "row_produced": produced,
        ]
        if let ginEntries { units["gin_entry"] = ginEntries }
        if let gistDescents { units["gist_descent"] = gistDescents }
        return units
    }
    let tieKey: String

    /// The schedule's weights for the units a candidate can estimate. Terms
    /// are products of non-negative counts and weights joined by a saturating
    /// sum, so adding only these in a fixed order equals the sum over the
    /// whole schedule.
    private static let weights: [String: Int64] = {
        var result: [String: Int64] = [:]
        for fact in GeneratedSpec.costs { result[fact.id] = fact.weight }
        return result
    }()
    private static let rowReadWeight = weights["storage_row_read"] ?? 0
    private static let pageReadWeight = weights["page_read"] ?? 0
    private static let operatorWeight = weights["operator_eval"] ?? 0
    private static let producedWeight = weights["row_produced"] ?? 0
    private static let ginWeight = weights["gin_entry"] ?? 0
    private static let gistWeight = weights["gist_descent"] ?? 0

    init(kind: Kind, indexName: String = "", scanRows: Int64, outputRows: Int64,
         accessPages: Int64, tableHeight: Int64, filterNodes: Int64 = 0,
         accessWork: Int64 = 0, producesRows: Bool = false) {
        let scanned = max(0, scanRows), output = max(0, outputRows)
        var pages = max(0, accessPages)
        if kind.fetchesTable { pages = saturatingAdd(pages, QueryEstimate.multiply(scanned, max(0, tableHeight))) }
        let evaluations = QueryEstimate.multiply(scanned, max(0, filterNodes))
        let produced = producesRows ? output : 0
        var total = QueryEstimate.multiply(scanned, Self.rowReadWeight)
        total = saturatingAdd(total, QueryEstimate.multiply(pages, Self.pageReadWeight))
        total = saturatingAdd(total, QueryEstimate.multiply(evaluations, Self.operatorWeight))
        total = saturatingAdd(total, QueryEstimate.multiply(produced, Self.producedWeight))
        let work = max(0, accessWork)
        if kind == .gin { total = saturatingAdd(total, QueryEstimate.multiply(work, Self.ginWeight)) }
        if kind == .gist { total = saturatingAdd(total, QueryEstimate.multiply(work, Self.gistWeight)) }
        self.rows = output; cost = total
        self.scanned = scanned; self.pages = pages; self.evaluations = evaluations; self.produced = produced
        ginEntries = kind == .gin ? work : nil; gistDescents = kind == .gist ? work : nil
        tieKey = String(kind.rank) + ":" + indexName
    }
}
