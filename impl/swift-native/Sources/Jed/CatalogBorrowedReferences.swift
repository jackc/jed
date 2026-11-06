/// Runtime validation facts for a private catalog. These contain names only;
/// no main catalog entry, store, page, or dependency ownership is imported.
/// File/open callers use the empty default and retain strict local validation.
internal struct CatalogBorrowedReferences: Sendable {
    var tables: [SQLText: Set<SQLText>] = [:]
    var views: Set<SQLText> = []
    var sequences: Set<SQLText> = []

    mutating func include(_ catalog: CatalogImage) {
        for table in catalog.tables { tables[table.name, default: []].formUnion(table.columns.map(\.name)) }
        views.formUnion(catalog.views.map(\.name))
        sequences.formUnion(catalog.sequences.map(\.name))
    }
}
