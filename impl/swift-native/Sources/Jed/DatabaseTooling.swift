/// Explicit experimental tooling only. This SPI does not name a public backup
/// API and is consumed by the opt-in native process/conformance executable.
@_spi(ExperimentalTools) public enum DatabaseTooling {
    public static func backup(_ database: Database, destination: String, pageIdentical: Bool) throws {
        try database.backup(destination, form: pageIdentical ? .pageIdentical : .compacted)
    }
    public static func pageCount(_ database: Database) throws -> UInt32 {
        try database.checkOpen(); return database.core.storage.pager.selected.meta.pageCount
    }
    public static func role(_ database: Database, attachment: String = "") throws -> String {
        try database.checkOpen()
        let selected: Database
        if attachment.isEmpty || attachment == "main" { selected = database }
        else {
            guard let attached = try? database.domains.handle(SQLText(attachment)) else { return "none" }
            selected = attached
        }
        #if os(Linux)
        if selected.host is RemoteDatabaseHost { return "client" }
        return (selected.host as? ServiceDatabaseHost)?.toolingRole ?? "none"
        #else
        return "none"
        #endif
    }
    public static func hangUpNextAcknowledgement(_ database: Database) throws {
        try database.checkOpen()
        #if os(Linux)
        guard let owner = (database.host as? ServiceDatabaseHost)?.serviceOwner else { throw JedError("55000", "ack hook requires a page-service owner") }
        owner.dropNextCommitAcknowledgement()
        #else
        throw JedError("0A000", "page-service owner is unavailable")
        #endif
    }
    public static func beforeNextAcknowledgement(_ database: Database, action: @escaping @Sendable () -> Void) throws {
        try database.checkOpen()
        #if os(Linux)
        guard let owner = (database.host as? ServiceDatabaseHost)?.serviceOwner else { throw JedError("55000", "ack hook requires a page-service owner") }
        owner.beforeNextCommitAcknowledgement(action)
        #else
        throw JedError("0A000", "page-service owner is unavailable")
        #endif
    }
}
