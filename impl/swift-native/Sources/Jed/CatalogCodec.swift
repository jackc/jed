internal enum CatalogEntry: Sendable {
    case composite(CatalogComposite), sequence(CatalogSequence), collation(CatalogCollation)
    case table(CatalogTable), statistic(CatalogStatisticEntry), view(CatalogView)
}

internal struct CatalogStatisticEntry: Sendable {
    enum Body: Sendable {
        case summary(CatalogStatistics, mcv: Int, histogram: Int, terms: Int)
        case mcv(Int, CatalogStatistics.MCV)
        case histogram(Int, [UInt8])
        case term(Int, CatalogStatistics.Term)
    }
    var table: SQLText
    var column: Int
    var body: Body
}

/// Handwritten v50 catalog bytes. Entry framing belongs to CatalogChain; type
/// graphs, references and statistics are checked after collecting the stream.
internal enum CatalogCodec {
    private static func short(_ text: SQLText, _ w: inout ByteWriter) throws {
        try count(text.utf8.count, &w); w.append(text.utf8)
    }
    private static func long(_ text: SQLText, _ w: inout ByteWriter) throws {
        guard text.utf8.count <= UInt32.max else { throw JedError("0A000", "catalog text exceeds its length field") }
        w.u32(UInt32(text.utf8.count)); w.append(text.utf8)
    }
    private static func count(_ value: Int, _ w: inout ByteWriter) throws {
        guard value >= 0, value <= UInt16.max else { throw JedError("0A000", "catalog field exceeds its length limit") }
        w.u16(UInt16(value))
    }
    private static func short(_ r: inout ByteReader) throws -> SQLText { try r.text(Int(r.u16())) }
    private static func long(_ r: inout ByteReader) throws -> SQLText { try r.text(Int(r.u32())) }
    private static func reference(_ value: PageReference, _ encrypted: Bool, _ w: inout ByteWriter) throws {
        guard value.tag.count == (encrypted ? 16 : 0), value.index != 0 || value.tag.allSatisfy({ $0 == 0 }) else {
            throw JedError.corruption("invalid catalog root tag")
        }
        w.u32(value.index); w.append(value.tag)
    }
    private static func reference(_ r: inout ByteReader, _ encrypted: Bool) throws -> PageReference {
        let index = try r.u32(), tag = encrypted ? Array(try r.take(16)) : []
        guard index != 0 || tag.allSatisfy({ $0 == 0 }) else { throw JedError.corruption("nonzero tag on absent catalog root") }
        return PageReference(index, tag: tag)
    }
    private static func scalar(_ code: UInt8) throws -> ScalarType {
        guard let type = ScalarType(rawValue: code) else { throw JedError.corruption("invalid scalar type code") }
        return type
    }
    private static func typeTail(_ type: CatalogType, _ w: inout ByteWriter) throws {
        switch type {
        case .scalar: break
        case .composite(let name): try short(name, &w)
        case .array(let element):
            guard element.code != 15, element.code != 17 else { throw JedError.corruption("invalid array element descriptor") }
            w.u8(element.code); try typeTail(element, &w)
        case .range(let type):
            _ = try ValueCodec.rangeElement(.scalar(type)); w.u8(type.rawValue)
        }
    }
    private static func typeTail(_ code: UInt8, _ r: inout ByteReader) throws -> CatalogType {
        switch code {
        case 14: return .composite(try short(&r))
        case 15:
            let code = try r.u8()
            guard code != 15, code != 17 else { throw JedError.corruption("invalid array element descriptor") }
            return .array(try typeTail(code, &r))
        case 17:
            let type = try scalar(r.u8())
            guard [ScalarType.i32, .i64, .decimal, .timestamp, .timestamptz, .date].contains(type) else {
                throw JedError.corruption("invalid range element descriptor")
            }
            return .range(type)
        default: return .scalar(try scalar(code))
        }
    }
    private static func typmod(_ type: CatalogTypeUse, _ w: inout ByteWriter) throws {
        switch type.type {
        case .scalar(.decimal): w.u16(UInt16(type.decimal?.precision ?? 0)); w.u16(UInt16(type.decimal?.scale ?? 0))
        case .scalar(.text):
            guard (0...10_485_760).contains(type.textLimit ?? 0) else { throw JedError.corruption("invalid varchar limit") }
            w.u32(UInt32(type.textLimit ?? 0))
        case .scalar(.timestamp), .scalar(.timestamptz): w.u8(type.temporal?.precision ?? 255)
        case .scalar(.interval): w.u8(type.temporal?.fields ?? 0); w.u8(type.temporal?.precision ?? 255)
        default: break
        }
    }
    private static func typmod(_ type: inout CatalogTypeUse, _ r: inout ByteReader) throws {
        switch type.type {
        case .scalar(.decimal):
            let precision = Int(try r.u16()), scale = Int(try r.u16())
            if precision == 0 {
                guard scale == 0 else { throw JedError.corruption("unconstrained decimal has a scale") }
            } else {
                do { type.decimal = try DecimalTypmod(precision: precision, scale: scale) }
                catch { throw JedError.corruption("invalid decimal typmod") }
            }
        case .scalar(.text):
            let limit = Int(try r.u32())
            guard limit <= 10_485_760 else { throw JedError.corruption("invalid varchar limit") }
            if limit != 0 { type.textLimit = limit }
        case .scalar(.timestamp), .scalar(.timestamptz):
            let precision = try r.u8()
            guard precision == 255 || precision <= 6 else { throw JedError.corruption("invalid timestamp precision") }
            if precision != 255 { type.temporal = try TemporalTypmod(precision: precision) }
        case .scalar(.interval):
            let fields = try r.u8(), precision = try r.u8()
            guard fields <= 13, precision == 255 || precision <= 6,
                  precision == 255 || DateTimeUnits.leastField(fields) == "second" else {
                throw JedError.corruption("invalid interval typmod")
            }
            if fields != 0 || precision != 255 { type.temporal = try TemporalTypmod(fields: fields, precision: precision == 255 ? nil : precision) }
        default: break
        }
    }
    private static func column(_ column: CatalogColumn, field: Bool, _ w: inout ByteWriter) throws {
        try short(column.name, &w); w.u8(column.type.type.code)
        var flags: UInt8 = column.notNull ? (field ? 1 : 2) : 0
        if field { try typeTail(column.type.type, &w) }
        else {
            switch column.defaultValue {
            case .constant: flags |= 4
            case .expression: flags |= 8
            case nil: break
            }
            if let always = column.identity { flags |= 16; if always { flags |= 32 } }
            if column.type.collation != nil { flags |= 64 }
        }
        w.u8(flags)
        if !field { try typeTail(column.type.type, &w) }
        try typmod(column.type, &w)
        if !field {
            switch column.defaultValue {
            case .constant(let value): w.append(try ValueCodec.encode(value, type: column.type.resolve([:])))
            case .expression(let expression): try short(expression.text, &w)
            case nil: break
            }
            if let collation = column.type.collation { try short(collation, &w) }
        }
    }
    private static func column(_ r: inout ByteReader, field: Bool) throws -> CatalogColumn {
        let name = try short(&r), code = try r.u8()
        let raw: CatalogType, flags: UInt8
        if field { raw = try typeTail(code, &r); flags = try r.u8() }
        else { flags = try r.u8(); raw = try typeTail(code, &r) }
        var type = CatalogTypeUse(raw)
        try typmod(&type, &r)
        var column = CatalogColumn(name: name, type: type, notNull: flags & (field ? 1 : 2) != 0)
        if field {
            guard flags & ~1 == 0 else { throw JedError.corruption("invalid composite field flags") }
            return column
        }
        guard flags & 0x81 == 0, flags & 12 != 12,
              flags & 32 == 0 || flags & 16 != 0 else { throw JedError.corruption("invalid column flags") }
        switch raw {
        case .composite:
            guard flags & ~10 == 0 else { throw JedError.corruption("invalid composite column flags") }
        case .array(let element):
            guard flags & ~66 == 0, flags & 64 == 0 || element == .scalar(.text) else {
                throw JedError.corruption("invalid array column flags")
            }
        case .range:
            guard flags & ~2 == 0 else { throw JedError.corruption("invalid range column flags") }
        case .scalar(let scalar):
            guard flags & 64 == 0 || scalar == .text else { throw JedError.corruption("collation on a non-text column") }
            if flags & 16 != 0 {
                guard scalar.integerWidth != nil, flags & 10 == 10 else { throw JedError.corruption("invalid identity column") }
                column.identity = flags & 32 != 0
            }
        }
        if flags & 4 != 0 {
            let present = try r.u8()
            guard present <= 1 else { throw JedError.corruption("invalid catalog default value") }
            column.defaultValue = .constant(present == 1 ? .null : try ValueCodec.readBody(&r, type: type.resolve([:])))
        } else if flags & 8 != 0 { column.defaultValue = .expression(CatalogExpression(try short(&r))) }
        if flags & 64 != 0 { column.type.collation = try short(&r) }
        return column
    }
    private static func hosts(_ values: [CatalogHostDependency], _ w: inout ByteWriter) throws {
        try count(values.count, &w)
        for value in values {
            try short(value.name, &w); try count(value.arguments.count, &w)
            for argument in value.arguments { w.u8(argument.rawValue) }
            w.u8(value.result.rawValue); try short(value.component, &w); w.u32(value.version)
        }
    }
    private static func hosts(_ r: inout ByteReader) throws -> [CatalogHostDependency] {
        var output: [CatalogHostDependency] = []
        let count = Int(try r.u16())
        guard count > 0 else { throw JedError.corruption("empty host dependency list") }
        for _ in 0..<count {
            let name = try short(&r), arity = Int(try r.u16())
            var arguments: [ScalarType] = []
            for _ in 0..<arity { arguments.append(try scalar(r.u8())) }
            let result = try scalar(r.u8()), component = try short(&r), version = try r.u32()
            guard !name.utf8.isEmpty, !component.utf8.isEmpty else { throw JedError.corruption("empty host dependency identity") }
            if let previous = output.last {
                guard previous.name < name || (previous.name == name && previous.arguments.map(\.rawValue).lexicographicallyPrecedes(arguments.map(\.rawValue))) else {
                    throw JedError.corruption("host dependencies are not uniquely ordered")
                }
            }
            output.append(CatalogHostDependency(name: name, arguments: arguments, result: result, component: component, version: version))
        }
        return output
    }
    private static func ordinals(_ values: [Int], _ w: inout ByteWriter) throws {
        try count(values.count, &w)
        for value in values { try count(value, &w) }
    }
    private static func ordinals(_ r: inout ByteReader) throws -> [Int] {
        let count = Int(try r.u16())
        var values: [Int] = []
        for _ in 0..<count { values.append(Int(try r.u16())) }
        return values
    }

    static func encode(_ entry: CatalogEntry, encrypted: Bool) throws -> [UInt8] {
        var w = ByteWriter()
        switch entry {
        case .composite(let value):
            w.u8(1); try short(value.name, &w); try count(value.fields.count, &w)
            for field in value.fields { try column(field, field: true, &w) }
        case .sequence(let value):
            w.u8(2); try short(value.name, &w)
            for number in [value.increment, value.minimum, value.maximum, value.start, value.cache, value.last] { w.i64(number) }
            w.u8((value.cycle ? 1 : 0) | (value.called ? 2 : 0) | (value.owner == nil ? 0 : 4))
            if let owner = value.owner { try short(owner.table, &w); try count(owner.column, &w) }
        case .collation(let value):
            w.u8(3); w.u8(value.isDefault ? 1 : 0)
            for text in [value.name, value.unicode, value.cldr, value.description] { try short(text, &w) }
        case .table(let value):
            w.u8(0); try short(value.name, &w); try count(value.columns.count, &w)
            for item in value.columns { try column(item, field: false, &w) }
            try ordinals(value.primaryKey, &w); try count(value.checks.count, &w)
            for item in value.checks {
                try short(item.name, &w); try short(item.expression.text, &w); w.u8(item.hosts.isEmpty ? 0 : 1)
                if !item.hosts.isEmpty { try hosts(item.hosts, &w) }
            }
            try count(value.indexes.count, &w)
            for item in value.indexes {
                try short(item.name, &w); try count(item.keys.count, &w)
                for key in item.keys {
                    switch key {
                    case .column(let ordinal): try count(ordinal, &w)
                    case .expression(let expression): w.u16(.max); try short(expression.text, &w)
                    }
                }
                let zoneFlag = item.dynamicZones || !item.zones.isEmpty
                let options = item.keyOptions.contains { $0 != 0 }
                let flags: UInt8 = (item.unique ? 1 : 0) | (item.predicate == nil ? 0 : 2) |
                    (item.hosts.isEmpty ? 0 : 4) | (zoneFlag ? 8 : 0) | (options ? 16 : 0) | (item.isConstraint ? 32 : 0)
                w.u8(flags); w.u8(item.kind); try reference(item.root, encrypted, &w)
                if let predicate = item.predicate { try short(predicate.text, &w) }
                if !item.hosts.isEmpty { try hosts(item.hosts, &w) }
                if zoneFlag {
                    w.u8(item.dynamicZones ? 1 : 0); try count(item.zones.count, &w)
                    for zone in item.zones { try short(zone.name, &w); try short(zone.version, &w); w.u32(zone.checksum) }
                }
                if options {
                    guard item.keyOptions.count == item.keys.count else { throw JedError.corruption("index key-option count mismatch") }
                    w.append(item.keyOptions)
                }
            }
            try count(value.foreignKeys.count, &w)
            for item in value.foreignKeys {
                try short(item.name, &w); try ordinals(item.columns, &w); try short(item.referencedTable, &w)
                try ordinals(item.referencedColumns, &w)
                w.u8(item.onDelete | (item.onUpdate << 3) | (item.matchFull ? 64 : 0))
            }
            try count(value.exclusions.count, &w)
            for item in value.exclusions {
                try short(item.name, &w); try short(item.index, &w); try count(item.elements.count, &w)
                for element in item.elements { try count(element.column, &w); w.u8(element.strategy) }
            }
            try reference(value.root, encrypted, &w); w.i64(value.rowCount)
        case .statistic(let value): try writeStatistic(value, &w)
        case .view(let value):
            w.u8(5); try short(value.name, &w); try count(value.columns.count, &w)
            for column in value.columns {
                try short(column.name, &w); w.u8(column.defaultText == nil ? 0 : 1)
                if let text = column.defaultText { try long(text, &w) }
            }
            try long(value.definition, &w); try count(value.dependencies.count, &w)
            for dep in value.dependencies {
                w.u8(dep.kind); try short(dep.name, &w); try count(dep.columns.count, &w)
                for column in dep.columns { try short(column, &w) }
            }
            w.u8(value.checkOption); w.u8((value.securityBarrier ? 1 : 0) | (value.securityInvoker ? 2 : 0))
        }
        return w.bytes
    }

    static func decode(_ r: inout ByteReader, encrypted: Bool) throws -> CatalogEntry {
        switch try r.u8() {
        case 0: return .table(try readTable(&r, encrypted: encrypted))
        case 1:
            let name = try short(&r), count = Int(try r.u16())
            guard count > 0 else { throw JedError.corruption("empty composite definition") }
            var fields: [CatalogColumn] = []
            for _ in 0..<count { fields.append(try column(&r, field: true)) }
            return .composite(CatalogComposite(name: name, fields: fields))
        case 2:
            let name = try short(&r), increment = try r.i64(), minimum = try r.i64(), maximum = try r.i64()
            let start = try r.i64(), cache = try r.i64(), last = try r.i64(), flags = try r.u8()
            guard flags & ~7 == 0, increment != 0, minimum < maximum, cache > 0,
                  start >= minimum, start <= maximum, last >= minimum, last <= maximum else {
                throw JedError.corruption("invalid sequence definition")
            }
            var sequence = CatalogSequence(name: name, increment: increment, minimum: minimum, maximum: maximum,
                start: start, cache: cache, last: last, cycle: flags & 1 != 0, called: flags & 2 != 0)
            if flags & 4 != 0 { sequence.owner = (try short(&r), Int(try r.u16())) }
            return .sequence(sequence)
        case 3:
            let flags = try r.u8()
            guard flags <= 1 else { throw JedError.corruption("invalid collation flags") }
            return .collation(CatalogCollation(name: try short(&r), isDefault: flags != 0, unicode: try short(&r), cldr: try short(&r), description: try short(&r)))
        case 4: return .statistic(try readStatistic(&r))
        case 5:
            let name = try short(&r), count = Int(try r.u16())
            guard count > 0 else { throw JedError.corruption("empty view column list") }
            var columns: [CatalogView.Column] = []
            for _ in 0..<count {
                let name = try short(&r), flags = try r.u8()
                guard flags <= 1 else { throw JedError.corruption("invalid view column flags") }
                columns.append(.init(name: name, defaultText: flags == 1 ? try long(&r) : nil))
            }
            let definition = try long(&r), dependencies = Int(try r.u16())
            var deps: [CatalogView.Dependency] = []
            for _ in 0..<dependencies {
                let kind = try r.u8(), name = try short(&r), count = Int(try r.u16())
                guard kind <= 3, kind == 0 || count == 0 else { throw JedError.corruption("invalid view dependency kind") }
                var columns: [SQLText] = []
                for _ in 0..<count { columns.append(try short(&r)) }
                deps.append(.init(kind: kind, name: name, columns: columns))
            }
            let check = try r.u8(), flags = try r.u8()
            guard check <= 2, flags & ~3 == 0 else { throw JedError.corruption("invalid view flags") }
            return .view(CatalogView(name: name, columns: columns, definition: definition, dependencies: deps,
                checkOption: check, securityBarrier: flags & 1 != 0, securityInvoker: flags & 2 != 0))
        default: throw JedError.corruption("invalid catalog entry kind")
        }
    }

    private static func readTable(_ r: inout ByteReader, encrypted: Bool) throws -> CatalogTable {
        let name = try short(&r), count = Int(try r.u16())
        var columns: [CatalogColumn] = []
        for _ in 0..<count { columns.append(try column(&r, field: false)) }
        var table = CatalogTable(name: name, columns: columns, root: PageReference(0))
        table.primaryKey = try ordinals(&r)
        guard Set(table.primaryKey).count == table.primaryKey.count, table.primaryKey.allSatisfy({ $0 < count }) else {
            throw JedError.corruption("invalid primary-key ordinals")
        }
        let checks = Int(try r.u16())
        for _ in 0..<checks {
            let name = try short(&r), expression = CatalogExpression(try short(&r)), flags = try r.u8()
            guard flags <= 1 else { throw JedError.corruption("invalid check flags") }
            table.checks.append(CatalogCheck(name: name, expression: expression, hosts: flags == 1 ? try hosts(&r) : []))
        }
        let indexes = Int(try r.u16())
        for _ in 0..<indexes {
            let name = try short(&r), keys = Int(try r.u16())
            guard keys > 0 else { throw JedError.corruption("empty index key list") }
            var elements: [CatalogIndexKey] = []
            for _ in 0..<keys {
                let ordinal = Int(try r.u16())
                if ordinal == 65535 { elements.append(.expression(CatalogExpression(try short(&r)))) }
                else {
                    guard ordinal < count else { throw JedError.corruption("invalid index column ordinal") }
                    elements.append(.column(ordinal))
                }
            }
            let flags = try r.u8(), kind = try r.u8(), root = try reference(&r, encrypted)
            guard flags & 192 == 0, kind <= 2, kind == 0 || flags & 29 == 0,
                  kind != 1 || flags & 2 == 0, flags & 32 == 0 || flags & 1 != 0 else {
                throw JedError.corruption("invalid index flags or kind")
            }
            var index = CatalogIndex(name: name, keys: elements, unique: flags & 1 != 0,
                isConstraint: flags & 32 != 0, kind: kind, root: root)
            if flags & 2 != 0 { index.predicate = CatalogExpression(try short(&r)) }
            if flags & 4 != 0 { index.hosts = try hosts(&r) }
            if flags & 8 != 0 {
                let dynamic = try r.u8(), count = Int(try r.u16())
                guard dynamic <= 1, dynamic == 1 || count > 0 else { throw JedError.corruption("invalid timezone dependency list") }
                index.dynamicZones = dynamic != 0
                for _ in 0..<count {
                    let name = try short(&r), version = try short(&r), checksum = try r.u32()
                    guard !name.utf8.isEmpty, !version.utf8.isEmpty,
                          index.zones.last.map({ $0.name < name }) ?? true else { throw JedError.corruption("invalid timezone dependency identity or order") }
                    index.zones.append(CatalogZoneDependency(name: name, version: version, checksum: checksum))
                }
            }
            if flags & 16 != 0 {
                index.keyOptions = Array(try r.take(keys))
                guard index.keyOptions.allSatisfy({ $0 <= 3 }), index.keyOptions.contains(where: { $0 != 0 }) else {
                    throw JedError.corruption("invalid index key options")
                }
            }
            table.indexes.append(index)
        }
        let foreignKeys = Int(try r.u16())
        for _ in 0..<foreignKeys {
            let name = try short(&r), local = try ordinals(&r), parent = try short(&r), remote = try ordinals(&r), flags = try r.u8()
            guard !local.isEmpty, local.count == remote.count, local.allSatisfy({ $0 < count }),
                  flags & 128 == 0, flags & 7 <= 4, (flags >> 3) & 7 <= 4 else {
                throw JedError.corruption("invalid foreign key definition")
            }
            table.foreignKeys.append(CatalogForeignKey(name: name, columns: local, referencedTable: parent,
                referencedColumns: remote, onDelete: flags & 7, onUpdate: (flags >> 3) & 7, matchFull: flags & 64 != 0))
        }
        let exclusions = Int(try r.u16())
        for _ in 0..<exclusions {
            let name = try short(&r), index = try short(&r), elements = Int(try r.u16())
            guard elements > 0 else { throw JedError.corruption("empty exclusion constraint") }
            var keys: [CatalogExclusion.Element] = []
            for _ in 0..<elements {
                let column = Int(try r.u16()), strategy = try r.u8()
                guard column < count, strategy <= 1 else { throw JedError.corruption("invalid exclusion element") }
                keys.append(.init(column: column, strategy: strategy))
            }
            table.exclusions.append(CatalogExclusion(name: name, index: index, elements: keys))
        }
        table.root = try reference(&r, encrypted); table.rowCount = try r.i64()
        guard table.rowCount >= 0, (table.root.index == 0) == (table.rowCount == 0) else { throw JedError.corruption("table cardinality disagrees with its root") }
        for index in table.indexes where index.kind == 2 && index.predicate != nil {
            guard table.exclusions.contains(where: { $0.index == index.name }) else { throw JedError.corruption("partial GiST has no exclusion constraint") }
        }
        return table
    }

    private static func writeStatistic(_ value: CatalogStatisticEntry, _ w: inout ByteWriter) throws {
        w.u8(4)
        switch value.body {
        case .summary: w.u8(0)
        case .mcv: w.u8(1)
        case .histogram: w.u8(2)
        case .term: w.u8(3)
        }
        try short(value.table, &w); try count(value.column, &w)
        switch value.body {
        case .summary(let summary, let mcv, let histogram, let terms):
            w.u8((summary.stale ? 1 : 0) | (summary.distributionEligible ? 2 : 0) | (summary.termEligible ? 4 : 0))
            for number in [summary.analyzed, summary.nulls, summary.widthSum, summary.distinct] { w.i64(number) }
            w.u32(summary.sampleRows); w.u32(summary.sampleNonnull); try count(mcv, &w); try count(histogram, &w)
            if summary.termEligible { w.i64(summary.termOccurrences); w.i64(summary.untrackedTermRows); try count(terms, &w) }
        case .mcv(let ordinal, let item): try count(ordinal, &w); w.u32(item.frequency); try count(item.bytes.count, &w); w.append(item.bytes)
        case .histogram(let ordinal, let bytes): try count(ordinal, &w); try count(bytes.count, &w); w.append(bytes)
        case .term(let ordinal, let item): try count(ordinal, &w); w.i64(item.count); try count(item.bytes.count, &w); w.append(item.bytes)
        }
    }

    private static func readStatistic(_ r: inout ByteReader) throws -> CatalogStatisticEntry {
        let kind = try r.u8(), table = try short(&r), column = Int(try r.u16())
        let body: CatalogStatisticEntry.Body
        if kind == 0 {
            let flags = try r.u8(), analyzed = try r.i64(), nulls = try r.i64(), width = try r.i64(), distinct = try r.i64()
            let sampled = try r.u32(), nonnull = try r.u32(), mcv = Int(try r.u16()), histogram = Int(try r.u16())
            guard flags & ~7 == 0, analyzed >= 0, nulls >= 0, nulls <= analyzed, width >= 0,
                  distinct >= 0, distinct <= analyzed - nulls, flags & 2 != 0 || distinct == 0,
                  sampled <= min(analyzed, 30_000), nonnull <= sampled, mcv <= 100,
                  histogram <= 101, histogram != 1, flags & 2 != 0 || (mcv == 0 && histogram == 0) else {
                throw JedError.corruption("invalid statistics summary")
            }
            var summary = CatalogStatistics(table: table, column: column, stale: flags & 1 != 0,
                distributionEligible: flags & 2 != 0, termEligible: flags & 4 != 0,
                analyzed: analyzed, nulls: nulls, widthSum: width, distinct: distinct, sampleRows: sampled, sampleNonnull: nonnull)
            var terms = 0
            if summary.termEligible {
                summary.termOccurrences = try r.i64(); summary.untrackedTermRows = try r.i64(); terms = Int(try r.u16())
                guard summary.termOccurrences >= 0, summary.untrackedTermRows >= 0,
                      summary.untrackedTermRows <= analyzed - nulls, terms <= 100 else { throw JedError.corruption("invalid term statistics summary") }
            }
            body = .summary(summary, mcv: mcv, histogram: histogram, terms: terms)
        } else {
            guard kind <= 3 else { throw JedError.corruption("invalid statistics entry kind") }
            let ordinal = Int(try r.u16()), frequency = kind == 1 ? try r.u32() : 0
            let termCount = kind == 3 ? try r.i64() : 0, length = Int(try r.u16())
            guard (1...128).contains(length) else { throw JedError.corruption("invalid statistics value length") }
            let bytes = Array(try r.take(length))
            switch kind {
            case 1: body = .mcv(ordinal, .init(frequency: frequency, bytes: bytes))
            case 2: body = .histogram(ordinal, bytes)
            default: body = .term(ordinal, .init(count: termCount, bytes: bytes))
            }
        }
        return CatalogStatisticEntry(table: table, column: column, body: body)
    }
}
