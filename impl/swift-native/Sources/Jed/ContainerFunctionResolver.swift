/// Open-type signatures are resolved from catalog contracts. Recursive type
/// unification and literal adaptation remain handwritten, just like scalar
/// overload selection; the catalog supplies names, slots, and result rules.
internal enum ContainerFunctionResolver {
    static func resolve(_ call: SQLCall, originals supplied: [ResolvedExpression]? = nil, resolve: FunctionResolver.Resolve) throws -> ResolvedFunction? {
        let range = GeneratedSpec.ranges.first { fact in
            fact.id == call.name.description || fact.aliases.contains(call.name.description)
        }
        let rows = GeneratedSpec.functions.filter { row in
            row.kind == "function" && row.name == (range?.id ?? call.name.description) &&
                (range != nil || FunctionResolver.strings(row.metadata, "arg_families").contains { $0.hasPrefix("any") && $0 != "any" })
        }
        guard !rows.isEmpty || call.name.description == "width_bucket" && call.arguments.count == 2 else { return nil }
        let originals = try supplied ?? call.arguments.map { try resolve($0.value, nil) }
        return try finish(call, range: range, rows: rows, originals: originals, resolve: resolve)
    }

    private static func finish(_ call: SQLCall, range: RangeSpecFact?, rows: [FunctionSpecFact],
                               originals: [ResolvedExpression], resolve: FunctionResolver.Resolve) throws -> ResolvedFunction? {
        if call.name.description == "array_to_string", originals.first.map({ if case .array = $0.type?.type { return false }; return true }) == true {
            throw JedError(originals[0].type == nil || FunctionResolver.bareString(call.arguments[0].value) ? "42804" : "42883", "array_to_string requires an array")
        }
        // lower/upper share a name with ordinary text functions. Unknowns follow
        // the text-preference rule of the scalar resolver.
        if ["lower", "upper"].contains(call.name.description), originals.first?.type == nil || originals.first?.type?.type == .scalar(.text) { return nil }
        guard !call.star, !call.distinct, call.orderBy.isEmpty, call.withinGroup.isEmpty,
              call.filter == nil, call.over == nil, call.overName == nil,
              call.arguments.allSatisfy({ $0.name == nil && !$0.variadic }) else {
            throw JedError("42883", "invalid container function arguments")
        }
        if let range, let scalar = ScalarType.allCases.first(where: { $0.name == range.element }) {
            guard let row = rows.first(where: { FunctionResolver.strings($0.metadata, "arg_families").count == originals.count }) else {
                throw JedError("42883", "range constructor does not exist")
            }
            let element = TypeDescriptor(.scalar(scalar))
            var arguments: [ResolvedExpression] = []
            for index in call.arguments.indices {
                let target = index < 2 ? element : TypeDescriptor(.scalar(.text))
                let argument = try resolve(call.arguments[index].value, target)
                if let type = argument.type?.type, !TypeRules.admitsCast(from: type, to: target.type, mode: .assignment) {
                    throw JedError("42883", "range bound has an incompatible type")
                }
                arguments.append(argument)
            }
            return make(row, arguments, result: TypeDescriptor(.range(element.type)))
        }
        if call.name.description == "width_bucket" && call.arguments.count == 2 {
            if originals[0].type == nil && originals[1].type == nil { throw JedError("42P18", "could not determine width_bucket argument types") }
            if originals[1].type == nil || FunctionResolver.bareString(call.arguments[1].value), let operandType = originals[0].type {
                let thresholds = try resolve(call.arguments[1].value, TypeDescriptor(.array(operandType.type)))
                let row = FunctionSpecFact(category: "array", name: "width_bucket", kind: "function", metadata: ["result": .text("i32"), "null": .text("propagates")])
                return make(row, [originals[0], thresholds], result: TypeDescriptor(.scalar(.i32)))
            }
            guard case .array(let element) = originals[1].type?.type else { throw JedError("42883", "width_bucket requires an array of thresholds") }
            let operand = try resolve(call.arguments[0].value, TypeDescriptor(element))
            let thresholds = try resolve(call.arguments[1].value, operand.type.map { TypeDescriptor(.array($0.type)) })
            guard case .array(let actualElement) = thresholds.type?.type else { throw JedError("42883", "width_bucket requires an array of thresholds") }
            do { try TypeRules.comparable(operand.type?.type, actualElement) }
            catch { throw JedError("42883", "width_bucket arguments are not comparable") }
            let row = FunctionSpecFact(category: "array", name: "width_bucket", kind: "function", metadata: ["result": .text("i32"), "null": .text("propagates")])
            return make(row, [operand, thresholds], result: TypeDescriptor(.scalar(.i32)))
        }
        for row in rows {
            let families = FunctionResolver.strings(row.metadata, "arg_families")
            guard families.count == originals.count else { continue }
            var arguments = originals
            let firstArray = zip(families, originals).compactMap { family, argument -> SQLType? in
                guard family == "anyarray" || family == "anycompatiblearray", case .array(let element) = argument.type?.type else { return nil }; return element
            }.first
            if let firstArray, case .scalar = firstArray {
                for index in arguments.indices {
                    let family = families[index]
                    if family == "anyarray" || family == "anycompatiblearray" {
                        arguments[index] = try resolve(call.arguments[index].value, TypeDescriptor(.array(firstArray)))
                    } else if family == "anyelement" || family == "anycompatible" {
                        arguments[index] = try resolve(call.arguments[index].value, TypeDescriptor(firstArray))
                    }
                }
            }
            let compatible = families.contains { $0.hasPrefix("anycompatible") }
            var element: SQLType?, matches = true
            func unify(_ type: SQLType) {
                guard let previous = element else { element = type; return }
                if compatible { do { element = try TypeRules.common(previous, type) } catch { matches = false } }
                else if previous != type { matches = false }
            }
            for index in arguments.indices {
                guard let type = arguments[index].type?.type else { continue }
                switch families[index] {
                case "anyarray", "anycompatiblearray": if case .array(let item) = type { unify(item) } else { matches = false }
                case "anyrange": if case .range(let item) = type { unify(item) } else { matches = false }
                default: break
                }
            }
            for index in arguments.indices {
                guard let type = arguments[index].type?.type else { continue }
                switch families[index] {
                case "anyelement", "anycompatible": unify(type)
                case "anyarray", "anycompatiblearray", "anyrange": break
                default:
                    if case .scalar(let scalar) = type, TypeRules.family(scalar) == families[index] {} else { matches = false }
                }
            }
            guard matches else { continue }
            guard let element else { throw JedError("42P18", "could not determine polymorphic type") }
            for index in arguments.indices {
                let target: TypeDescriptor
                switch families[index] {
                case "anyarray", "anycompatiblearray": target = TypeDescriptor(.array(element))
                case "anyrange": target = TypeDescriptor(.range(element))
                case "anyelement", "anycompatible": target = TypeDescriptor(element)
                default:
                    guard let scalar = FunctionResolver.familyType(families[index]) else { continue }
                    target = TypeDescriptor(.scalar(scalar))
                }
                if arguments[index].type == nil { arguments[index] = try resolve(call.arguments[index].value, target) }
                else if compatible && arguments[index].type?.type != target.type {
                    let source = arguments[index], charge = changesValues(source.type!.type, target.type)
                    arguments[index] = ResolvedExpression(type: target, node: charge ? .cast(source, target, .explicit) : .convert(source, target), position: source.position)
                }
            }
            let output: TypeDescriptor
            switch FunctionResolver.text(row.metadata, "result") {
            case "anyarray", "anycompatiblearray": output = TypeDescriptor(.array(element))
            case "anyrange": output = TypeDescriptor(.range(element))
            case "anyelement", "anycompatible": output = TypeDescriptor(element)
            default: output = try FunctionResolver.resultType(row, arguments: arguments)
            }
            return make(row, arguments, result: output)
        }
        throw JedError("42883", "container function does not exist: \(call.name)")
    }

    private static func changesValues(_ source: SQLType, _ target: SQLType) -> Bool {
        switch (source, target) {
        case (.scalar(let a), .scalar(.decimal)) where a.integerWidth != nil: true
        case (.scalar(.f32), .scalar(.f64)): true
        case (.array(let a), .array(let b)): changesValues(a, b)
        default: false
        }
    }
    private static func make(_ row: FunctionSpecFact, _ arguments: [ResolvedExpression], result: TypeDescriptor) -> ResolvedFunction {
        ResolvedFunction(metadata: row, arguments: arguments, resultType: result,
                         evaluationOrder: Array(arguments.indices), constantArguments: arguments.map(FunctionResolver.isConstant))
    }
}
