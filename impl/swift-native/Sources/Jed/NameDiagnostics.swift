/// Names retained only for diagnostics. The walk neither changes resolution nor
/// evaluates SQL, and follows FROM order and original column ordinals.
internal struct NameHintRelation {
    let label: SQLText
    let columns: [SQLText]
    let catalog: SQLText?
    var visible = false
    var lateralOK = false
    var qualifiedOnly = false
}

internal struct NameHintScope {
    var unseen: [NameHintRelation] = []
    var onClause = false
    var lateralActive = false
}

internal struct NameHintFrame {
    let depth: Int
    let parenthesizedJoin: Bool
    let relations: [NameHintRelation]
}

internal enum NameDiagnostics {
    static func relations(_ columns: [ExpressionColumn], hidden: Bool = false) -> [NameHintRelation] {
        var labels: [SQLText] = [], grouped: [SQLText: [ExpressionColumn]] = [:]
        // Merged USING columns can lead the visible list; physical slots retain
        // the authored relation and column order needed by the hint walk.
        let ordered = columns.enumerated().sorted {
            if $0.element.hintRelationOrder != $1.element.hintRelationOrder {
                return $0.element.hintRelationOrder < $1.element.hintRelationOrder
            }
            if !hidden && $0.element.hintSubrelation != $1.element.hintSubrelation { return !$0.element.hintSubrelation }
            if $0.element.index != $1.element.index { return $0.element.index < $1.element.index }
            return $0.offset < $1.offset
        }
        for (_, column) in ordered {
            guard let label = column.qualifier, !hidden || !column.hintSkipHidden else { continue }
            if grouped[label] == nil { labels.append(label) }
            grouped[label, default: []].append(column)
        }
        return labels.map { label in
            let fields = grouped[label]!, first = fields[0]
            return NameHintRelation(label: label, columns: fields.map(\.name), catalog: first.hintSubrelation ? nil : first.catalogName,
                visible: !hidden, lateralOK: hidden && !first.lateralDenied,
                qualifiedOnly: hidden && first.hintQualifiedOnly)
        }
    }

    private static func distance(_ a: SQLText, _ b: SQLText) -> Int {
        let right = Array(b.description.unicodeScalars)
        var previous = Array(0...right.count)
        for (row, left) in a.description.unicodeScalars.enumerated() {
            var current = Array(repeating: 0, count: right.count + 1)
            current[0] = row + 1
            for column in right.indices {
                current[column + 1] = min(previous[column] + (left == right[column] ? 0 : 1),
                    previous[column + 1] + 1, current[column] + 1)
            }
            previous = current
        }
        return previous[right.count]
    }

    static func column(_ name: SQLText, qualifier: SQLText?, entries: [NameHintRelation]) -> (hint: String?, detail: String?) {
        var best = 4, first: String?, second: String?, exact: [NameHintRelation] = []
        for entry in entries {
            let penalty = qualifier.map { distance($0, entry.label) } ?? 0
            for column in entry.columns where penalty <= best {
                let difference = distance(column, name)
                guard difference <= name.utf8.count / 2 else { continue }
                let score = penalty + difference, candidate = "\(entry.label).\(column)"
                if score < best { best = score; first = candidate; second = nil }
                else if score == best {
                    if second != nil { first = nil; second = nil }
                    else if first != nil { second = candidate }
                }
            }
            if penalty == 0 && entry.columns.contains(name) { exact.append(entry) }
        }
        if let entry = exact.first {
            if exact.count == 1 {
                let hint = entry.lateralOK ? "To reference that column, you must mark this subquery with LATERAL."
                    : qualifier == nil && entry.qualifiedOnly ? "To reference that column, you must use a table-qualified name." : nil
                return (hint, "There is a column named \"\(name)\" in table \"\(entry.label)\", but it cannot be referenced from this part of the query.")
            }
            return (qualifier == nil ? "Try using a table-qualified name." : nil,
                "There are columns named \"\(name)\", but they are in tables that cannot be referenced from this part of the query.")
        }
        guard let first else { return (nil, nil) }
        if let second { return ("Perhaps you meant to reference the column \"\(first)\" or the column \"\(second)\".", nil) }
        return ("Perhaps you meant to reference the column \"\(first)\".", nil)
    }

    static func table(_ qualifier: SQLText, entries: [NameHintRelation]) -> (hint: String?, detail: String?) {
        for entry in entries where entry.label == qualifier || entry.catalog == qualifier {
            if entry.visible && entry.label != qualifier {
                return ("Perhaps you meant to reference the table alias \"\(entry.label)\".", nil)
            }
            return (entry.lateralOK ? "To reference that table, you must mark this subquery with LATERAL." : nil,
                "There is an entry for table \"\(entry.label)\", but it cannot be referenced from this part of the query.")
        }
        return (nil, nil)
    }
}

extension ExpressionResolver {
    func nameHintWalk() -> [NameHintRelation] {
        var result: [NameHintRelation] = []
        let scopes = [columns] + outerScopes
        let hints = [nameHints] + outerNameHints
        var depth = scopes.count
        for (index, scope) in scopes.enumerated() {
            let hint = index < hints.count ? hints[index] : NameHintScope()
            for var entry in hint.unseen {
                entry.visible = false; entry.lateralOK = false; entry.qualifiedOnly = false
                result.append(entry)
            }
            result += NameDiagnostics.relations(scope)
            depth -= 1
            for frame in nameHintFrames.reversed() where frame.depth == depth {
                let blocked = hint.onClause && frame.parenthesizedJoin
                for var entry in frame.relations {
                    entry.lateralOK = entry.lateralOK && !blocked && !nameHints.lateralActive
                    entry.qualifiedOnly = entry.qualifiedOnly && !blocked
                    result.append(entry)
                }
            }
        }
        return result
    }

    func locateName(_ input: JedError, syntax: SQLExpression) -> JedError {
        guard !input.locationDecided, case .column(let qualifier, let name) = syntax.kind else { return input }
        let metadata: (hint: String?, detail: String?)
        if input.code == "42703" { metadata = NameDiagnostics.column(name, qualifier: qualifier, entries: nameHintWalk()) }
        else if input.code == "42P01", let qualifier { metadata = NameDiagnostics.table(qualifier, entries: nameHintWalk()) }
        else { return input }
        var error = input.hinted(metadata.hint)
        if let detail = metadata.detail { error = error.detailed(detail) }
        return error
    }
}
