/// Host-only validation of a pinned committed database generation.
public enum IntegrityLevel: String, Sendable { case quick, full }

public struct IntegrityCheckOptions: Sendable {
    public var level: IntegrityLevel
    public var constraints: Bool
    /// Zero means unlimited. The default is 100.
    public var maxFindings: Int
    public init(level: IntegrityLevel = .full, constraints: Bool = false, maxFindings: Int = 100) {
        self.level = level; self.constraints = constraints; self.maxFindings = maxFindings
    }
}
public struct IntegrityFinding: Sendable {
    public let kind: String
    public let database: String
    public let object: String?
    public let page: UInt32?
    public let key: [UInt8]?
    public let detail: String
}
public struct IntegritySkip: Equatable, Sendable {
    public let object: String
    public let reason: String
}
public struct IntegrityReport: Sendable {
    public internal(set) var findings: [IntegrityFinding] = []
    public internal(set) var truncated = false
    public internal(set) var skipped: [IntegritySkip] = []
    public internal(set) var pages: UInt64 = 0
    public internal(set) var rows: UInt64 = 0
    public var ok: Bool { findings.isEmpty && !truncated }
}
internal struct IntegrityGeneration: Sendable {
    let selected: ValidatedMeta
    let freePages: [UInt32]
    let metaSlots: [[UInt8]]?
    let remote: Bool
    let memory: Bool
}
internal enum IntegrityStop: Error { case truncated, unreadable }
