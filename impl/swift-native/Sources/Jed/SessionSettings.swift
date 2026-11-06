public struct PrivilegeSet: OptionSet, Sendable {
    public let rawValue: UInt8
    public init(rawValue: UInt8) { self.rawValue = rawValue }
    public static let select = PrivilegeSet(rawValue: 1)
    public static let insert = PrivilegeSet(rawValue: 2)
    public static let update = PrivilegeSet(rawValue: 4)
    public static let delete = PrivilegeSet(rawValue: 8)
    public static let execute = PrivilegeSet(rawValue: 16)
    public static let allTables: PrivilegeSet = [.select, .insert, .update, .delete]
}

public struct SessionOptions: Sendable {
    public var maxCost: Int64 = 0
    public var lifetimeMaxCost: Int64 = 0
    public var maxSQLLength: Int = 1_048_576
    public var lockTimeoutMilliseconds: UInt64 = 0
    public var maxScalarBytes: Int64 = 64 * 1024 * 1024
    public var maxQueryMemoryBytes: Int64 = 0
    public var workMemoryBytes: Int64 = 256 * 1024 * 1024
    public var defaultPrivileges: PrivilegeSet = .allTables
    public var allowDDL = true
    public var allowTemporaryDDL: Bool?
    public var temporaryBufferBytes: Int64 = 32 * 1024 * 1024
    public var maxTemporaryStorageBytes: Int64 = 1024 * 1024 * 1024
    public var timeZone: SQLText = SQLText(validatedUTF8: [85, 84, 67])
    public init() {}
}

/// Value-semantic capabilities freeze with a cursor. Denials take precedence
/// over grants, including when the host changes the two in either order.
internal struct SessionPrivileges {
    struct Object: Hashable { let name: SQLText; let function: Bool }
    var defaults: PrivilegeSet
    private var grants: [Object: PrivilegeSet] = [:]
    private var revocations: [Object: PrivilegeSet] = [:]
    init(defaults: PrivilegeSet) { self.defaults = defaults }
    mutating func grant(_ privileges: PrivilegeSet, on name: SQLText, function: Bool = false) {
        grants[Object(name: name, function: function), default: []].formUnion(privileges)
    }
    mutating func revoke(_ privileges: PrivilegeSet, on name: SQLText, function: Bool = false) {
        revocations[Object(name: name, function: function), default: []].formUnion(privileges)
    }
    /// Definer views cross a table-capability boundary, while every function
    /// still executes under the invoking session's explicit EXECUTE denials.
    func withTableAccess() -> SessionPrivileges {
        var result = self
        result.defaults = .allTables
        result.grants = grants.filter { $0.key.function }
        result.revocations = revocations.filter { $0.key.function }
        return result
    }
    func require(_ privileges: PrivilegeSet, on name: SQLText, function: Bool = false) throws {
        let object = Object(name: name, function: function)
        let effective = (function ? PrivilegeSet.execute : defaults).union(grants[object] ?? []).subtracting(revocations[object] ?? [])
        guard effective.isSuperset(of: privileges) else {
            throw JedError("42501", "permission denied for \(function ? "function" : "table") \(name)")
        }
    }
}

/// Session settings are separate from transactional snapshots. Copies retain
/// the environment and lifetime counter but copy variables and capabilities.
internal struct SessionSettings {
    let environment: Environment
    var options: SessionOptions
    let lifetime: LifetimeCost
    var privileges: SessionPrivileges
    private(set) var variables: [SQLText: SQLText] = [:]
    private(set) var timeZone: SQLTimeZone
    private(set) var timeZoneName: SQLText

    init(environment: Environment, options: SessionOptions) {
        self.environment = environment; self.options = options
        lifetime = LifetimeCost(limit: options.lifetimeMaxCost)
        privileges = SessionPrivileges(defaults: options.defaultPrivileges)
        if let zone = try? environment.timezones.resolve(options.timeZone) {
            timeZone = zone; timeZoneName = options.timeZone
        } else { timeZone = .utc; timeZoneName = SQLText(validatedUTF8: [85, 84, 67]) }
        if self.options.workMemoryBytes <= 0 { self.options.workMemoryBytes = 256 * 1024 * 1024 }
        if self.options.maxScalarBytes <= 0 { self.options.maxScalarBytes = GeneratedSpec.defaultScalarBytes }
        self.options.maxQueryMemoryBytes = max(0, options.maxQueryMemoryBytes)
        self.options.maxCost = max(0, options.maxCost)
        self.options.maxSQLLength = max(0, options.maxSQLLength)
    }
    mutating func setTimeZone(_ name: SQLText) throws {
        let zone = try environment.timezones.resolve(name)
        timeZone = zone; timeZoneName = name
    }
    mutating func setVariable(_ name: SQLText, value: SQLText?) throws {
        guard name.utf8.contains(46) else { throw JedError("42704", "unrecognized configuration parameter \"\(name)\"") }
        variables[name.asciiLowercased] = value
    }
    func currentSetting(_ name: SQLText, missingOK: Bool) throws -> SQLValue {
        if let value = variables[name.asciiLowercased] { return .text(value) }
        guard missingOK else { throw JedError("42704", "unrecognized configuration parameter \"\(name)\"") }
        return .null
    }
    func newMeter(pendingBytes: Int64 = 0, cancellation: CancellationToken? = nil) throws -> CostMeter {
        let resources = StatementResources(queryLimit: options.maxQueryMemoryBytes, scalarLimit: options.maxScalarBytes)
        let meter = CostMeter(limit: options.maxCost, lifetime: lifetime, resources: resources, cancellation: cancellation)
        try meter.checkpoint()
        try meter.syncPending(pendingBytes)
        return meter
    }
    func checkSQLLength(_ sql: SQLText) throws {
        guard options.maxSQLLength == 0 || sql.utf8.count <= options.maxSQLLength else {
            throw JedError("54000", "SQL input exceeds the maximum length of \(options.maxSQLLength) bytes")
        }
    }
    func render(_ value: SQLValue, type: TypeDescriptor) throws -> SQLText {
        try ContainerText.render(value, type: type, context: .init(timezone: timeZone))
    }
}
