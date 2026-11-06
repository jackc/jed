import Jed

extension UnicodeDefinition {
    /// The frozen LDML subset has single-scalar resets/targets. Deferred logical
    /// resets, settings, contractions, and dense-weight allocation stay rejected.
    mutating func applyRule(_ line: String) throws {
        let scalars = Array(line.unicodeScalars.map(\.value))
        var cursor = 1
        func skipSpace() { while cursor < scalars.count && DefinitionSyntax.space(scalars[cursor]) { cursor += 1 } }
        func character() throws -> UInt32 {
            skipSpace()
            guard cursor < scalars.count && scalars[cursor] != 60 && scalars[cursor] != 61 else {
                throw DefinitionSyntax.error("LDML relation requires a character")
            }
            let result = scalars[cursor]; cursor += 1; return result
        }
        let anchor = try character()
        guard let weights = mappings[[anchor]], weights.count == 1 else { throw DefinitionSyntax.error("LDML reset must name one collation element") }
        var current = weights[0]
        while true {
            skipSpace()
            if cursor == scalars.count { break }
            let strength: Int?
            if scalars[cursor] == 61 { strength = nil; cursor += 1 }
            else {
                let start = cursor
                while cursor < scalars.count && scalars[cursor] == 60 { cursor += 1 }
                guard (1...3).contains(cursor - start) else { throw DefinitionSyntax.error("LDML relation must be <, <<, <<<, or =") }
                strength = cursor - start - 1
            }
            let target = try character()
            if let strength {
                let lower = current.levels[strength]
                var upper: UInt16?
                for weights in mappings.values {
                    for weight in weights where weight.levels.prefix(strength).elementsEqual(current.levels.prefix(strength)) && weight.levels[strength] > lower {
                        if upper == nil || weight.levels[strength] < upper! { upper = weight.levels[strength] }
                    }
                }
                let next: UInt16
                if let upper {
                    guard upper - lower >= 2 else { throw JedError("0A000", "collation tailoring weight space is exhausted") }
                    next = lower + (upper - lower) / 2
                } else {
                    let gap: UInt16 = [0x0200, 0x0020, 0x0006][strength]
                    guard lower <= UInt16.max - gap else { throw JedError("0A000", "collation tailoring weight overflow") }
                    next = lower + gap
                }
                current.flags = 0; current.levels[strength] = next
                if strength == 0 { current.levels[1] = 0x0020 }
                if strength <= 1 { current.levels[2] = 0x0002 }
            }
            mappings[[target]] = [current]
        }
    }
}
