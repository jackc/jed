/// Catalog type references retain names until the complete type graph has been
/// collected. Stream name order is deliberately unrelated to dependency order.
internal indirect enum CatalogType: Sendable, Equatable {
    case scalar(ScalarType), composite(SQLText), array(CatalogType), range(ScalarType)
    var code: UInt8 {
        switch self {
        case .scalar(let value): value.rawValue
        case .composite: 14
        case .array: 15
        case .range: 17
        }
    }
}

internal struct CatalogTypeUse: Sendable {
    var type: CatalogType
    var decimal: DecimalTypmod?
    var textLimit: Int?
    var temporal: TemporalTypmod?
    var collation: SQLText?

    init(_ type: CatalogType) { self.type = type }
    func resolve(_ types: [SQLText: CompositeType]) throws -> TypeDescriptor {
        func resolveType(_ raw: CatalogType) throws -> SQLType {
            switch raw {
            case .scalar(let type): return .scalar(type)
            case .composite(let name):
                guard let found = types[name] else { throw JedError.corruption("missing composite type") }
                return .composite(found)
            case .array(let element): return .array(try resolveType(element))
            case .range(let element): return .range(.scalar(element))
            }
        }
        var result = TypeDescriptor(try resolveType(type))
        result.decimal = decimal; result.textLimit = textLimit
        result.temporal = temporal; result.collation = collation
        return result
    }
}

internal struct CatalogExpression: Sendable {
    let text: SQLText
    let parsed: SQLExpression?
    init(_ text: SQLText) {
        self.text = text
        parsed = try? SQLParser(utf8: text.utf8).parseExpressionOnly()
    }
}

internal enum CatalogDefault: Sendable {
    case constant(SQLValue), expression(CatalogExpression)
}

internal struct CatalogColumn: Sendable {
    var name: SQLText
    var type: CatalogTypeUse
    var notNull = false
    var defaultValue: CatalogDefault?
    /// nil = ordinary, false = BY DEFAULT, true = ALWAYS.
    var identity: Bool?
}

internal struct CatalogComposite: Sendable {
    var name: SQLText
    var fields: [CatalogColumn]
}

internal struct CatalogHostDependency: Sendable {
    var name: SQLText
    var arguments: [ScalarType]
    var result: ScalarType
    var component: SQLText
    var version: UInt32
}

internal struct CatalogZoneDependency: Sendable {
    var name: SQLText
    var version: SQLText
    var checksum: UInt32
}

internal struct CatalogCheck: Sendable {
    var name: SQLText
    var expression: CatalogExpression
    var hosts: [CatalogHostDependency] = []
}

internal enum CatalogIndexKey: Sendable {
    case column(Int), expression(CatalogExpression)
}

internal struct CatalogIndex: Sendable {
    var name: SQLText
    var keys: [CatalogIndexKey]
    var unique = false
    var isConstraint = false
    var kind: UInt8 = 0
    var root: PageReference
    var predicate: CatalogExpression?
    var hosts: [CatalogHostDependency] = []
    var dynamicZones = false
    var zones: [CatalogZoneDependency] = []
    var keyOptions: [UInt8] = []
}

internal struct CatalogForeignKey: Sendable {
    var name: SQLText
    var columns: [Int]
    var referencedTable: SQLText
    var referencedColumns: [Int]
    var onDelete: UInt8 = 0
    var onUpdate: UInt8 = 0
    var matchFull = false
}

internal struct CatalogExclusion: Sendable {
    struct Element: Sendable { var column: Int; var strategy: UInt8 }
    var name: SQLText
    var index: SQLText
    var elements: [Element]
}

internal struct CatalogTable: Sendable {
    var name: SQLText
    var columns: [CatalogColumn]
    var primaryKey: [Int] = []
    var checks: [CatalogCheck] = []
    var indexes: [CatalogIndex] = []
    var foreignKeys: [CatalogForeignKey] = []
    var exclusions: [CatalogExclusion] = []
    var root: PageReference
    var rowCount: Int64 = 0
}

internal struct CatalogSequence: Sendable {
    var name: SQLText
    var increment: Int64 = 1
    var minimum: Int64 = 1
    var maximum: Int64 = .max
    var start: Int64 = 1
    var cache: Int64 = 1
    var last: Int64 = 1
    var cycle = false
    var called = false
    var owner: (table: SQLText, column: Int)?
}

internal struct CatalogCollation: Sendable {
    var name: SQLText
    var isDefault = false
    var unicode: SQLText
    var cldr: SQLText
    var description: SQLText
}

internal struct CatalogStatistics: Sendable {
    struct MCV: Sendable { var frequency: UInt32; var bytes: [UInt8] }
    struct Term: Sendable { var count: Int64; var bytes: [UInt8] }
    var table: SQLText
    var column: Int
    var stale: Bool
    var distributionEligible: Bool
    var termEligible: Bool
    var analyzed: Int64
    var nulls: Int64
    var widthSum: Int64
    var distinct: Int64
    var sampleRows: UInt32
    var sampleNonnull: UInt32
    var termOccurrences: Int64 = 0
    var untrackedTermRows: Int64 = 0
    var mcv: [MCV] = []
    var histogram: [[UInt8]] = []
    var terms: [Term] = []
}

internal struct CatalogView: Sendable {
    struct Column: Sendable { var name: SQLText; var defaultText: SQLText? }
    struct Dependency: Sendable { var kind: UInt8; var name: SQLText; var columns: [SQLText] }
    var name: SQLText
    var columns: [Column]
    var definition: SQLText
    var dependencies: [Dependency]
    var checkOption: UInt8 = 0
    var securityBarrier = false
    var securityInvoker = false
}

internal struct CatalogImage: Sendable {
    var composites: [CatalogComposite] = []
    var sequences: [CatalogSequence] = []
    var collations: [CatalogCollation] = []
    var tables: [CatalogTable] = []
    var statistics: [CatalogStatistics] = []
    var views: [CatalogView] = []

    /// Resolve nominal identities without trusting file order. The catalog ID
    /// is supplied by the owning database; persisted bytes never select it.
    func resolvedTypes(catalogID: UInt64) throws -> [SQLText: CompositeType] {
        var definitions: [SQLText: CatalogComposite] = [:], ids: [SQLText: UInt32] = [:]
        for (index, definition) in composites.sorted(by: { $0.name < $1.name }).enumerated() {
            guard !definition.name.utf8.isEmpty, !definition.fields.isEmpty,
                  definitions.updateValue(definition, forKey: definition.name) == nil else {
                throw JedError.corruption("duplicate or empty composite definition")
            }
            ids[definition.name] = UInt32(index + 1)
        }
        var resolved: [SQLText: CompositeType] = [:], active: Set<SQLText> = []
        func visit(_ name: SQLText) throws {
            if resolved[name] != nil { return }
            guard let definition = definitions[name], active.insert(name).inserted else {
                throw JedError.corruption("dangling or cyclic composite type")
            }
            guard active.count <= 256 else { throw JedError.corruption("composite type graph exceeds nesting limit") }
            var names: Set<SQLText> = []
            for field in definition.fields {
                guard !field.name.utf8.isEmpty, names.insert(field.name).inserted else {
                    throw JedError.corruption("duplicate or empty composite field")
                }
                switch field.type.type {
                case .composite(let dependency), .array(.composite(let dependency)): try visit(dependency)
                default: break
                }
            }
            let fields = try definition.fields.map {
                TypeField(name: $0.name, descriptor: try $0.type.resolve(resolved), notNull: $0.notNull)
            }
            resolved[name] = CompositeType(catalogID: catalogID, id: ids[name]!, name: name, fields: fields)
            active.remove(name)
        }
        for name in definitions.keys.sorted() { try visit(name) }
        return resolved
    }
}
