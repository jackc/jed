extension CatalogDDL {
    static func createView(_ syntax: SQLCreateView, source: SQLText, snapshot: inout DomainSnapshot,
                           settings: SessionSettings, context: ExpressionContext) throws {
        let name = try nameInMain(syntax.name)
        try CatalogSQL.reserved(name, kind: "view")
        let occupied = CatalogSQL.relationNames(snapshot.catalog).contains(name)
        if occupied && syntax.ifNotExists { return }
        guard !syntax.temporary else { throw JedError("0A000", "temporary view catalog is not yet connected") }
        guard !syntax.recursive else { throw JedError("0A000", "recursive view catalog is not yet connected") }
        let prepared = try ViewDefinition.prepare(syntax.query, source: source, snapshot: snapshot, settings: settings, context: context)
        var view = CatalogView(name: name, columns: [], definition: prepared.definition, dependencies: prepared.dependencies)
        try ViewOptions.apply(syntax.options, trailingCheck: syntax.checkOption, to: &view)
        if view.checkOption != 0 { try UpdatableView.checkShape(syntax.query, name: name, operation: "check") }
        guard syntax.columns.count <= prepared.columns.count else { throw JedError("42601", "CREATE VIEW specifies more column names than columns") }
        var names: Set<SQLText> = []
        for (index, column) in prepared.columns.enumerated() {
            let name = index < syntax.columns.count ? syntax.columns[index] : column.name
            guard names.insert(name).inserted else { throw JedError("42701", "column \(name) specified more than once") }
            _ = try CatalogSQL.typeUse(column.type ?? TypeDescriptor(.scalar(.text)))
            view.columns.append(.init(name: name))
        }
        var image = snapshot.catalog
        if occupied {
            guard syntax.replace else { throw JedError("42P07", "relation \(name) already exists") }
            guard let index = image.views.firstIndex(where: { $0.name == name }) else { throw JedError("42809", "\(name) is not a view") }
            let old = image.views[index]
            guard view.columns.count >= old.columns.count else { throw JedError("42P16", "cannot drop columns from view") }
            for position in old.columns.indices {
                guard old.columns[position].name == view.columns[position].name else { throw JedError("42P16", "cannot change name of view column") }
                view.columns[position].defaultText = old.columns[position].defaultText
            }
            if let oldQuery = try? ViewDefinition.query(old),
               let oldPlan = try? SelectPlan.resolve(oldQuery, snapshot: snapshot, settings: settings, scalar: context.scalar) {
                for position in old.columns.indices {
                    guard (oldPlan.columns[position].type ?? TypeDescriptor(.scalar(.text))) == prepared.columns[position].type else { throw JedError("42P16", "cannot change data type of view column") }
                }
            }
            image.views[index] = view
        } else { image.views.append(view) }
        try ViewOptions.checkCycle(name, image: image)
        var next = snapshot; try next.updateCatalog(image, schemaChanged: true)
        if occupied { try revalidateViews(dependingOn: name, old: snapshot, new: next, settings: settings, context: context) }
        snapshot = next
    }

    static func alterView(_ syntax: SQLAlterView, source: SQLText, snapshot: inout DomainSnapshot,
                          settings: SessionSettings, context: ExpressionContext) throws {
        let name = try nameInMain(syntax.name)
        guard let index = snapshot.catalog.views.firstIndex(where: { $0.name == name }) else {
            if CatalogSQL.relationNames(snapshot.catalog).contains(name) { throw JedError("42809", "\(name) is not a view") }
            if syntax.ifExists { return }
            throw JedError("42P01", "view does not exist: \(name)")
        }
        var image = snapshot.catalog, view = image.views[index]
        for action in syntax.actions {
            let wasInvoker = view.securityInvoker
            switch action {
            case .setDefault(let column, let expression):
                let position = try viewColumn(column, view: view)
                let query = try ViewDefinition.query(view)
                var bodySettings = settings
                if !view.securityInvoker { bodySettings.privileges = settings.privileges.withTableAccess() }
                let body = try SelectPlan.resolve(query, snapshot: snapshot, settings: bodySettings, scalar: context.scalar)
                let target = body.columns[position].type ?? TypeDescriptor(.scalar(.text))
                try CatalogExpressionPolicy.validate(expression, isDefault: true)
                guard case .scalar = target.type else { throw JedError("0A000", "view container columns do not support defaults") }
                let resolver = try resolver(snapshot: snapshot, settings: settings, context: context)
                let value = try defaultValue(expression, target: target, source: source, resolver: resolver, context: context)
                switch value {
                case .none: view.columns[position].defaultText = nil
                case .expression(let expression): view.columns[position].defaultText = expression.text
                case .constant(let value): view.columns[position].defaultText = try ViewOptions.literal(value, type: target)
                }
            case .dropDefault(let column): view.columns[try viewColumn(column, view: view)].defaultText = nil
            case .setOptions(let options):
                try ViewOptions.apply(options, to: &view)
                if view.checkOption != 0 { try UpdatableView.checkShape(ViewDefinition.query(view), name: name, operation: "check") }
            case .resetOptions(let names):
                for name in names {
                    switch name.description {
                    case "check_option": view.checkOption = 0
                    case "security_barrier": view.securityBarrier = false
                    case "security_invoker": view.securityInvoker = false
                    default: break
                    }
                }
            case .rename(let renamed):
                try CatalogSQL.reserved(renamed, kind: "view")
                guard !CatalogSQL.relationNames(image).contains(renamed) else { throw JedError("42P07", "relation \(renamed) already exists") }
                guard !image.views.contains(where: { $0.dependencies.contains(where: { $0.kind == 1 && $0.name == view.name }) }) else {
                    throw JedError("0A000", "dependent view rename rewriting is not yet connected")
                }
                view.name = renamed
            case .renameColumn(let old, let new):
                let position = try viewColumn(old, view: view)
                guard !view.columns.contains(where: { $0.name == new }) else { throw JedError("42701", "column \(new) of relation \(view.name) already exists") }
                guard !image.views.contains(where: { $0.dependencies.contains(where: { $0.kind == 1 && $0.name == view.name }) }) else {
                    throw JedError("0A000", "dependent view column rename rewriting is not yet connected")
                }
                view.columns[position].name = new
            }
            if wasInvoker && !view.securityInvoker {
                // Becoming a definer view establishes a new table-capability
                // boundary and therefore repeats the creator's owner check.
                _ = try SelectPlan.resolve(ViewDefinition.query(view), snapshot: snapshot,
                                           settings: settings, scalar: context.scalar)
            }
        }
        image.views[index] = view
        try snapshot.updateCatalog(image, schemaChanged: true)
    }

    static func dropViews(_ syntax: SQLDrop, snapshot: inout DomainSnapshot) throws {
        var removed: Set<SQLText> = []
        for input in syntax.names {
            let name = try nameInMain(input)
            if snapshot.catalog.views.contains(where: { $0.name == name }) { removed.insert(name) }
            else if CatalogSQL.relationNames(snapshot.catalog).contains(name) { throw JedError("42809", "\(name) is not a view") }
            else if !syntax.ifExists { throw JedError("42P01", "view does not exist: \(name)") }
        }
        var changed = true
        while changed {
            changed = false
            for view in snapshot.catalog.views.sorted(by: { $0.name < $1.name }) where !removed.contains(view.name) {
                if view.dependencies.contains(where: { $0.kind == 1 && removed.contains($0.name) }) {
                    guard syntax.cascade else { throw JedError("2BP01", "cannot drop view because view \(view.name) depends on it") }
                    removed.insert(view.name); changed = true
                }
            }
        }
        guard !removed.isEmpty else { return }
        var image = snapshot.catalog; image.views.removeAll { removed.contains($0.name) }
        try snapshot.updateCatalog(image, schemaChanged: true)
    }
    private static func viewColumn(_ name: SQLText, view: CatalogView) throws -> Int {
        guard let position = view.columns.firstIndex(where: { $0.name == name }) else { throw JedError("42703", "column \(name) of relation \(view.name) does not exist") }
        return position
    }
    static func revalidateViews(dependingOn changed: SQLText, old: DomainSnapshot, new: DomainSnapshot,
                                settings: SessionSettings, context: ExpressionContext) throws {
        var affected: Set<SQLText> = [changed], pending = true
        while pending {
            pending = false
            for view in old.catalog.views where !affected.contains(view.name) {
                if view.dependencies.contains(where: { ($0.kind == 0 || $0.kind == 1) && affected.contains($0.name) }) {
                    affected.insert(view.name); pending = true
                }
            }
        }
        for view in old.catalog.views.sorted(by: { $0.name < $1.name }) where view.name != changed && affected.contains(view.name) {
            let before = try ViewDefinition.query(view)
            guard let afterView = new.catalog.views.first(where: { $0.name == view.name }) else { continue }
            do {
                var bodySettings = settings; bodySettings.privileges = settings.privileges.withTableAccess()
                let prior = try SelectPlan.resolve(before, snapshot: old, settings: bodySettings, scalar: context.scalar)
                let next = try ViewDefinition.prepare(ViewDefinition.query(afterView), source: afterView.definition,
                    snapshot: new, settings: bodySettings, context: context)
                guard prior.columns.count == next.columns.count,
                      zip(prior.columns, next.columns).allSatisfy({ $0.name == $1.name && ($0.type ?? TypeDescriptor(.scalar(.text))) == $1.type }),
                      view.dependencies.count == next.dependencies.count,
                      zip(view.dependencies, next.dependencies).allSatisfy({ $0.kind == $1.kind && $0.name == $1.name && $0.columns == $1.columns }) else {
                    throw JedError("0A000", "changed view definition")
                }
            } catch { throw JedError("0A000", "cannot alter relation \(changed) because it would change the definition of view \(view.name)") }
        }
    }
}

internal enum ViewOptions {
    static func apply(_ options: [SQLViewOption], trailingCheck: String? = nil, to view: inout CatalogView) throws {
        var seen: Set<SQLText> = []
        for option in options {
            guard seen.insert(option.name).inserted else { throw JedError("22023", "parameter \(option.name) specified more than once") }
            let value = option.value?.asciiLowercased.description ?? "true"
            switch option.name.description {
            case "check_option":
                guard trailingCheck == nil else { throw JedError("22023", "parameter check_option specified more than once") }
                guard value == "local" || value == "cascaded" else { throw JedError("22023", "invalid value for enum option check_option: \(value)") }
                view.checkOption = value == "local" ? 1 : 2
            case "security_barrier": view.securityBarrier = try boolean(value, name: option.name)
            case "security_invoker": view.securityInvoker = try boolean(value, name: option.name)
            default: throw JedError("22023", "unrecognized parameter \(option.name)")
            }
        }
        if let trailingCheck { view.checkOption = trailingCheck == "local" ? 1 : 2 }
    }
    private static func boolean(_ value: String, name: SQLText) throws -> Bool {
        if value == "1" || value == "on" || !value.isEmpty && ("true".hasPrefix(value) || "yes".hasPrefix(value)) { return true }
        if value == "0" || !value.isEmpty && ("false".hasPrefix(value) || "no".hasPrefix(value)) || value.count >= 2 && "off".hasPrefix(value) { return false }
        throw JedError("22023", "invalid value for boolean option \(name): \(value)")
    }
    static func literal(_ value: SQLValue, type: TypeDescriptor) throws -> SQLText {
        switch value {
        case .null: return SQLText(validatedUTF8: Array("null".utf8))
        case .integer(let value): return SQLText(validatedUTF8: Array(String(value).utf8))
        case .decimal(let value): return SQLText(validatedUTF8: Array(value.description.utf8))
        case .boolean(let value): return SQLText(validatedUTF8: Array((value ? "true" : "false").utf8))
        default:
            let text = try ContainerText.render(value, type: type, context: .init(timezone: .utc))
            return SQLText(validatedUTF8: [39] + text.utf8.flatMap { byte -> [UInt8] in byte == 39 ? [39, 39] : [byte] } + [39])
        }
    }
    static func checkCycle(_ name: SQLText, image: CatalogImage) throws {
        var pending = [name], visited: Set<SQLText> = []
        while let current = pending.popLast() {
            guard visited.insert(current).inserted else { continue }
            if let view = image.views.first(where: { $0.name == current }) {
                for dependency in view.dependencies where dependency.kind == 1 {
                    guard dependency.name != name else { throw JedError("42P17", "view definition would create a dependency cycle") }
                    pending.append(dependency.name)
                }
            }
        }
    }
}
