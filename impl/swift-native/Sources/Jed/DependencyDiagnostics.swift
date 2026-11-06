/// Dependency hints belong to the operation that rejected a drop, because
/// TYPE and SEQUENCE deliberately do not support the suggested CASCADE form.
internal enum DependencyDiagnostics {
    static let cascade = "Use DROP ... CASCADE to drop the dependent objects too."
    static func ownedIndex(_ index: SQLText, table: CatalogTable) -> String {
        let constraint = table.exclusions.first(where: { $0.index == index })?.name ?? index
        return "You can drop constraint \(constraint) on table \(table.name) instead."
    }
}
