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
    var isUnrestricted: Bool { defaults.isSuperset(of: .allTables) && revocations.values.allSatisfy(\.isEmpty) }
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
    /// Creation fills a newly defined table without requiring INSERT on that
    /// target. All source relation and function checks retain their envelope.
    func allowing(_ privileges: PrivilegeSet, on name: SQLText) -> SessionPrivileges {
        var result = self
        let object = Object(name: name, function: false)
        result.grants[object, default: []].formUnion(privileges)
        result.revocations[object]?.subtract(privileges)
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
    /// Stored fields live in one copy-on-write box, so passing this value
    /// around costs one retain instead of a field-by-field copy.
    private final class Storage {
        var environment: Environment
        var domains: QueryDomains?
        var scratchFileHost: (any ScratchFileHost)?
        var planningParameters: [SQLValue]?
        var planningProbe: PlanningProbe?
        var targetDomain: SQLText
        var options: SessionOptions
        var lifetime: LifetimeCost
        var privileges: SessionPrivileges
        var variables: [SQLText: SQLText]
        var timeZone: SQLTimeZone
        var timeZoneName: SQLText
        init(environment: Environment, domains: QueryDomains?, scratchFileHost: (any ScratchFileHost)?, planningParameters: [SQLValue]?, planningProbe: PlanningProbe?, targetDomain: SQLText, options: SessionOptions, lifetime: LifetimeCost, privileges: SessionPrivileges, variables: [SQLText: SQLText], timeZone: SQLTimeZone, timeZoneName: SQLText) {
            self.environment = environment
            self.domains = domains
            self.scratchFileHost = scratchFileHost
            self.planningParameters = planningParameters
            self.planningProbe = planningProbe
            self.targetDomain = targetDomain
            self.options = options
            self.lifetime = lifetime
            self.privileges = privileges
            self.variables = variables
            self.timeZone = timeZone
            self.timeZoneName = timeZoneName
        }
        func copy() -> Storage {
            Storage(environment: environment, domains: domains, scratchFileHost: scratchFileHost, planningParameters: planningParameters, planningProbe: planningProbe, targetDomain: targetDomain, options: options, lifetime: lifetime, privileges: privileges, variables: variables, timeZone: timeZone, timeZoneName: timeZoneName)
        }
    }
    private var storage: Storage
    @inline(__always) private mutating func unique() {
        if !isKnownUniquelyReferenced(&storage) { storage = storage.copy() }
    }
    var environment: Environment { storage.environment }
    var domains: QueryDomains? {
        get { storage.domains }
        _modify { unique(); yield &storage.domains }
    }
    var scratchFileHost: (any ScratchFileHost)? {
        get { storage.scratchFileHost }
        _modify { unique(); yield &storage.scratchFileHost }
    }
    /// Statement-local input for logical proofs. Admission still binds every
    /// parameter after complete type resolution; no execution value is cached.
    var planningParameters: [SQLValue]? {
        get { storage.planningParameters }
        _modify { unique(); yield &storage.planningParameters }
    }
    var planningProbe: PlanningProbe? {
        get { storage.planningProbe }
        _modify { unique(); yield &storage.planningProbe }
    }
    var targetDomain: SQLText {
        get { storage.targetDomain }
        _modify { unique(); yield &storage.targetDomain }
    }
    var options: SessionOptions {
        get { storage.options }
        _modify { unique(); yield &storage.options }
    }
    var lifetime: LifetimeCost { storage.lifetime }
    var privileges: SessionPrivileges {
        get { storage.privileges }
        _modify { unique(); yield &storage.privileges }
    }
    private(set) var variables: [SQLText: SQLText] {
        get { storage.variables }
        _modify { unique(); yield &storage.variables }
    }
    private(set) var timeZone: SQLTimeZone {
        get { storage.timeZone }
        _modify { unique(); yield &storage.timeZone }
    }
    private(set) var timeZoneName: SQLText {
        get { storage.timeZoneName }
        _modify { unique(); yield &storage.timeZoneName }
    }

    init(environment: Environment, options supplied: SessionOptions) {
        var options = supplied
        // Inheritance happens once when the session is created. Later changes
        // to either DDL gate do not change its independently mutable sibling.
        options.allowTemporaryDDL = supplied.allowTemporaryDDL ?? supplied.allowDDL
        let timeZone: SQLTimeZone, timeZoneName: SQLText
        if let zone = try? environment.timezones.resolve(supplied.timeZone) {
            timeZone = zone; timeZoneName = supplied.timeZone
        } else { timeZone = .utc; timeZoneName = SQLText(validatedUTF8: [85, 84, 67]) }
        if options.workMemoryBytes <= 0 { options.workMemoryBytes = 256 * 1024 * 1024 }
        if options.maxScalarBytes <= 0 { options.maxScalarBytes = GeneratedSpec.defaultScalarBytes }
        options.maxQueryMemoryBytes = max(0, supplied.maxQueryMemoryBytes)
        options.maxCost = max(0, supplied.maxCost)
        options.maxSQLLength = max(0, supplied.maxSQLLength)
        storage = Storage(environment: environment, domains: nil, scratchFileHost: nil, planningParameters: nil,
            planningProbe: nil, targetDomain: DomainNames.main, options: options,
            lifetime: LifetimeCost(limit: supplied.lifetimeMaxCost),
            privileges: SessionPrivileges(defaults: supplied.defaultPrivileges), variables: [:],
            timeZone: timeZone, timeZoneName: timeZoneName)
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
        let resources = StatementResources(queryLimit: options.maxQueryMemoryBytes, scalarLimit: options.maxScalarBytes,
            pendingBytes: pendingBytes)
        let meter = CostMeter(limit: options.maxCost, lifetime: lifetime, resources: resources, cancellation: cancellation)
        try meter.checkpoint()
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
