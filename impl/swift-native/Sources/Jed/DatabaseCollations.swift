/// The file's version pin and the loaded table's deterministic content identity.
/// C is intrinsic byte order and has no reference entry.
public struct CollationInfo: Sendable {
    public enum Verdict: String, Sendable { case full, skewed }
    public let name: SQLText
    public let unicodeVersion: SQLText
    public let cldrVersion: SQLText
    public let description: SQLText
    public let contentHash: UInt32
    public let isDefault: Bool
    public let verdict: Verdict
}

extension Environment {
    public func loadedCollations() -> [CollationInfo] {
        unicode.loadedCollations.sorted { $0.name < $1.name }.map {
            CollationInfo(name: $0.name, unicodeVersion: $0.unicodeVersion, cldrVersion: $0.cldrVersion,
                description: $0.description, contentHash: $0.contentHash, isDefault: false, verdict: .full)
        }
    }
}

extension Session {
    public func defaultCollation() throws -> SQLText {
        try withHostCatalogRead { $0.catalog.collations.first(where: \.isDefault)?.name ?? SQLText(validatedUTF8: [67]) }
    }
    public func collations() throws -> [CollationInfo] {
        try withHostCatalogRead { snapshot in
            try CollationMigration.references(snapshot.catalog).map { name in
                guard let loaded = try self.database.environment.unicode.resolve(name) else { throw JedError.corruption("C collation reference") }
                let pin = snapshot.catalog.collations.first { $0.name == name }
                let full = pin == nil || (pin!.unicode == loaded.unicodeVersion && pin!.cldr == loaded.cldrVersion)
                return CollationInfo(name: name, unicodeVersion: pin?.unicode ?? loaded.unicodeVersion,
                    cldrVersion: pin?.cldr ?? loaded.cldrVersion, description: pin?.description ?? loaded.description,
                    contentHash: loaded.contentHash, isDefault: pin?.isDefault ?? false, verdict: full ? .full : .skewed)
            }
        }
    }
    public func setDefaultCollation(_ name: SQLText) throws {
        try withHostCatalogWrite { snapshot in
            let loaded = try self.database.environment.unicode.resolve(name)
            let current = snapshot.catalog.collations.first(where: \.isDefault)?.name ?? SQLText(validatedUTF8: [67])
            guard current != name else { return }
            var image = snapshot.catalog
            for index in image.collations.indices { image.collations[index].isDefault = image.collations[index].name == name }
            if let loaded, !image.collations.contains(where: { $0.name == name }) {
                image.collations.append(CatalogCollation(name: name, isDefault: true, unicode: loaded.unicodeVersion,
                    cldr: loaded.cldrVersion, description: loaded.description))
            }
            let references = Set(CollationMigration.references(image))
            image.collations.removeAll { !references.contains($0.name) }
            try snapshot.updateCatalog(image, schemaChanged: true)
        }
    }
    /// Atomically re-encodes every key under a skewed referenced collation and
    /// advances its pin only after every affected table and index has rebuilt.
    @discardableResult public func upgradeCollations() throws -> Int {
        try withHostCatalogWrite { snapshot in
            try CollationMigration.upgrade(&snapshot, environment: self.database.environment)
        }
    }
}

extension Database {
    public func defaultCollation() throws -> SQLText {
        let session = try session(); defer { session.close() }; return try session.defaultCollation()
    }
    public func collations() throws -> [CollationInfo] {
        let session = try session(); defer { session.close() }; return try session.collations()
    }
    public func setDefaultCollation(_ name: SQLText) throws {
        let session = try session(); defer { session.close() }; try session.setDefaultCollation(name)
    }
    @discardableResult public func upgradeCollations() throws -> Int {
        let session = try session(); defer { session.close() }; return try session.upgradeCollations()
    }
}
