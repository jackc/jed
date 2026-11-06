/// Content-keyed simulation decisions, simulation-testing.md §6.2. This is a
/// tooling model; engine entropy continues through RandomSource.
internal enum SimDecision {
    static let gamma: UInt64 = 0x9e3779b97f4a7c15
    static func mix(_ input: UInt64) -> UInt64 {
        var z = input
        z = (z ^ (z >> 30)) &* 0xbf58476d1ce4e5b9
        z = (z ^ (z >> 27)) &* 0x94d049bb133111eb
        return z ^ (z >> 31)
    }
    static func value(seed: UInt64, lane: UInt64, step: UInt64, offset: UInt64 = 0, version: UInt64 = 0) -> UInt64 {
        var h = seed
        for word in [lane, step, offset, version] { h = mix((h ^ word) &+ gamma) }
        return h
    }
    static func fileKey(_ name: String) -> UInt64 {
        if name == "sim.jed" { return 0 }
        var h: UInt32 = 2_166_136_261
        for byte in name.utf8 { h = (h ^ UInt32(byte)) &* 16_777_619 }
        return (UInt64(h) &+ 1) << 40
    }
}

internal struct SimFault: Sendable {
    enum Kind: Sendable { case powerLoss, processCrash, write, sync, read, noSpace, spill, disconnect }
    let kind: Kind
    var survive = 0
    var tear = 0
    var issue = 0
    var probability = 0
    var nth = 1
}
