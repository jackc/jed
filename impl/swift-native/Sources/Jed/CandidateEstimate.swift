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
    let units: [String: Int64]
    let cost: Int64
    let tieKey: String

    init(kind: Kind, indexName: String = "", scanRows: Int64, outputRows: Int64,
         accessPages: Int64, tableHeight: Int64, filterNodes: Int64 = 0,
         accessWork: Int64 = 0, producesRows: Bool = false) {
        let scanned = max(0, scanRows), output = max(0, outputRows)
        var units: [String: Int64] = [
            "storage_row_read": scanned,
            "page_read": max(0, accessPages),
            "operator_eval": QueryEstimate.multiply(scanned, max(0, filterNodes)),
            "row_produced": producesRows ? output : 0,
        ]
        if kind.fetchesTable {
            units["page_read"] = saturatingAdd(units["page_read"]!, QueryEstimate.multiply(scanned, max(0, tableHeight)))
        }
        if kind == .gin { units["gin_entry"] = max(0, accessWork) }
        if kind == .gist { units["gist_descent"] = max(0, accessWork) }
        self.rows = output; self.units = units
        cost = GeneratedSpec.costs.reduce(0) { saturatingAdd($0, QueryEstimate.multiply(units[$1.id] ?? 0, $1.weight)) }
        tieKey = String(kind.rank) + ":" + indexName
    }
}
