/// Session history follows statement publication and transaction/savepoint
/// rollback. The counter itself remains part of the catalog snapshot.
internal struct SequenceHistory {
    var values: [SQLText: Int64] = [:]
    var lastName: SQLText?
}

/// An expression may advance sequences while DML mutates its private snapshot.
/// Keeping this small pending set separate avoids aliasing the inout snapshot;
/// successful statement publication applies both sets of edits together.
internal final class SequenceExecution {
    private let definitions: [SQLText: CatalogSequence]
    private var pending: [SQLText: CatalogSequence] = [:]
    private(set) var history: SequenceHistory
    private let meter: CostMeter
    private let writable: Bool

    init(snapshot: DomainSnapshot, history: SequenceHistory, meter: CostMeter, writable: Bool) {
        definitions = Dictionary(uniqueKeysWithValues: snapshot.catalog.sequences.map { ($0.name, $0) })
        self.history = history; self.meter = meter; self.writable = writable
    }

    func install(into context: inout ExpressionContext) {
        let fallback = context.functions.stateCall
        context.functions.stateCall = { name, values in
            if ["nextval", "setval", "currval", "lastval"].contains(name) { return try self.call(name, values) }
            guard let fallback else { throw JedError("55000", "function requires an execution-owned service: \(name)") }
            return try fallback(name, values)
        }
    }

    func apply(to snapshot: inout DomainSnapshot) throws {
        guard !pending.isEmpty else { return }
        var image = snapshot.catalog
        for index in image.sequences.indices {
            if let sequence = pending[image.sequences[index].name] { image.sequences[index] = sequence }
        }
        try snapshot.updateCatalog(image, schemaChanged: false)
    }

    private func call(_ function: String, _ values: [SQLValue]) throws -> SQLValue {
        if values.contains(where: \.isNull) { return .null }
        if function == "lastval" {
            guard let name = history.lastName, let value = history.values[name] else {
                throw JedError("55000", "lastval is not yet defined in this session")
            }
            return .integer(value)
        }
        if function == "nextval" || function == "setval" {
            guard writable else { throw JedError("25006", "cannot advance a sequence in a read-only transaction") }
            meter.charge(1)
        }
        guard let first = values.first, case .text(let text) = first else { throw JedError("42804", "sequence name must be text") }
        let name = try SequenceName.parse(text)
        guard var sequence = pending[name] ?? definitions[name] else { throw JedError("42P01", "relation does not exist: \(name)") }
        if function == "currval" {
            guard let value = history.values[name] else { throw JedError("55000", "currval of sequence \(name) is not yet defined in this session") }
            return .integer(value)
        }
        if function == "nextval" {
            if sequence.called {
                let (next, overflow) = sequence.last.addingReportingOverflow(sequence.increment)
                let crosses = overflow || (sequence.increment > 0 ? next > sequence.maximum : next < sequence.minimum)
                if crosses {
                    guard sequence.cycle else { throw JedError("2200H", "nextval reached the bound of sequence \(name)") }
                    sequence.last = sequence.increment > 0 ? sequence.minimum : sequence.maximum
                } else { sequence.last = next }
            }
            sequence.called = true
            history.values[name] = sequence.last; history.lastName = name
        } else {
            guard values.count == 2 || values.count == 3, case .integer(let value) = values[1] else {
                throw JedError("42804", "invalid setval arguments")
            }
            guard value >= sequence.minimum && value <= sequence.maximum else {
                throw JedError("22003", "setval value is outside sequence bounds")
            }
            var called = true
            if values.count == 3 {
                guard case .boolean(let flag) = values[2] else { throw JedError("42804", "invalid setval flag") }
                called = flag
            }
            sequence.last = value; sequence.called = called
            if called { history.values[name] = value }
        }
        pending[name] = sequence
        return .integer(sequence.last)
    }

    static func mutates(_ expressions: [ResolvedExpression]) -> Bool {
        var result = false
        StoredExpressionDependencies.walk(expressions) { expression in
            if case .call(let call) = expression.node, call.hostID == nil,
               call.name == "nextval" || call.name == "setval" { result = true }
        }
        return result
    }
}
