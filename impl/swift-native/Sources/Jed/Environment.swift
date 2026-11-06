import Synchronization
#if os(Linux)
import Glibc
#elseif canImport(Darwin)
import Darwin
#endif

public protocol RandomSource: AnyObject, Sendable {
    func bytes(count: Int) throws -> [UInt8]
}
public protocol ClockSource: AnyObject, Sendable {
    func microseconds() throws -> Int64
}

/// The only OS entropy seam. The C function borrows Swift-owned storage for
/// this synchronous call and retains no pointer. SQL cannot access the host.
public final class OSRandomSource: RandomSource {
    public init() {}
    public func bytes(count: Int) throws -> [UInt8] {
        guard count >= 0 else { throw JedError("22023", "negative random byte count") }
        var bytes = Array(repeating: UInt8(0), count: count)
        #if os(Linux) || canImport(Darwin)
        try bytes.withUnsafeMutableBytes { storage in
            var offset = 0
            while offset < count {
                let length = min(256, count - offset)
                guard getentropy(storage.baseAddress!.advanced(by: offset), length) == 0 else {
                    throw JedError("58030", "operating system entropy failed")
                }
                offset += length
            }
        }
        #else
        throw JedError("0A000", "OS random source is unavailable on this host")
        #endif
        return bytes
    }
}

/// Shared splitmix64 byte-stream contract, not a cryptographic source. A final
/// partial chunk consumes one whole draw and takes its most significant bytes.
public final class SeededRandomSource: RandomSource {
    private let state: Mutex<UInt64>
    public init(seed: UInt64) { state = Mutex(seed) }
    public func reseed(_ seed: UInt64) { state.withLock { $0 = seed } }
    public func bytes(count: Int) throws -> [UInt8] {
        guard count >= 0 else { throw JedError("22023", "negative random byte count") }
        return state.withLock { state in
            var output: [UInt8] = []; output.reserveCapacity(count)
            while output.count < count {
                state &+= 0x9e3779b97f4a7c15
                var value = state
                value = (value ^ (value >> 30)) &* 0xbf58476d1ce4e5b9
                value = (value ^ (value >> 27)) &* 0x94d049bb133111eb
                value ^= value >> 31
                for shift in stride(from: 56, through: 0, by: -8) {
                    if output.count == count { break }
                    output.append(UInt8(truncatingIfNeeded: value >> shift))
                }
            }
            return output
        }
    }
}

public final class WallClock: ClockSource {
    public init() {}
    public func microseconds() throws -> Int64 {
        #if os(Linux) || canImport(Darwin)
        var time = timespec()
        guard clock_gettime(CLOCK_REALTIME, &time) == 0 else { throw JedError("58030", "operating system clock failed") }
        let seconds = Int64(time.tv_sec).multipliedReportingOverflow(by: 1_000_000)
        let result = seconds.partialValue.addingReportingOverflow(Int64(time.tv_nsec) / 1000)
        guard !seconds.overflow, !result.overflow else { throw JedError("22008", "clock is out of range") }
        return result.partialValue
        #else
        throw JedError("0A000", "wall clock is unavailable on this host")
        #endif
    }
}

public final class ManualClock: ClockSource {
    private let value: Mutex<Int64>
    public init(microseconds: Int64) { value = Mutex(microseconds) }
    public func microseconds() -> Int64 { value.withLock { $0 } }
    public func set(_ microseconds: Int64) { value.withLock { $0 = microseconds } }
    public func advance(_ microseconds: Int64) throws {
        try value.withLock { value in
            let next = value.addingReportingOverflow(microseconds)
            guard !next.overflow else { throw JedError("22008", "clock is out of range") }; value = next.partialValue
        }
    }
}

public final class AdvancingClock: ClockSource {
    private struct State { var value: Int64; var step: Int64 }
    private let state: Mutex<State>
    public init(start: Int64, step: Int64) { state = Mutex(State(value: start, step: step)) }
    public func reset(start: Int64, step: Int64) { state.withLock { $0 = State(value: start, step: step) } }
    public func microseconds() throws -> Int64 {
        try state.withLock { state in
            let current = state.value, next = current.addingReportingOverflow(state.step)
            guard !next.overflow else { throw JedError("22008", "advancing clock is out of range") }
            state.value = next.partialValue; return current
        }
    }
}

public struct HostFunction: Sendable {
    public enum Volatility: String, Sendable { case immutable, stable, volatile }
    public typealias Kernel = @Sendable ([SQLValue]) throws -> SQLValue
    public typealias BatchKernel = @Sendable ([[SQLValue]], inout [SQLValue]) throws -> Void
    public let name: SQLText
    public let arguments: [ScalarType]
    public let result: ScalarType
    public var cost: Int64
    public var volatility: Volatility
    public var crossCore: Bool
    public var componentID: SQLText?
    public var semanticVersion: UInt32?
    public let kernel: Kernel?
    public let batchKernel: BatchKernel?
    public init(name: SQLText, arguments: [ScalarType], result: ScalarType, cost: Int64 = 1,
                volatility: Volatility = .volatile, crossCore: Bool = false,
                componentID: SQLText? = nil, semanticVersion: UInt32? = nil,
                kernel: Kernel? = nil, batchKernel: BatchKernel? = nil) {
        self.name = name; self.arguments = arguments; self.result = result; self.cost = cost
        self.volatility = volatility; self.crossCore = crossCore; self.componentID = componentID
        self.semanticVersion = semanticVersion; self.kernel = kernel; self.batchKernel = batchKernel
    }
    internal func validateResult(_ value: SQLValue) throws -> SQLValue {
        if case .integer(let number) = value, result.integerWidth != nil, !result.admits(number) {
            throw JedError("22003", "value out of range for type \(result.name)").withDataType(result.name)
        }
        do { return try ContainerCasts.store(value, to: TypeDescriptor(.scalar(result))) }
        catch { throw JedError("22000", "host function returned a value outside its declared type") }
    }
    internal func evaluate(_ values: [SQLValue]) throws -> SQLValue {
        guard values.count == arguments.count else { throw JedError("22000", "host function argument count mismatch") }
        if values.contains(where: \.isNull) { return .null }
        if let kernel { return try validateResult(kernel(values)) }
        guard let batchKernel else { throw JedError("22023", "host function has no kernel") }
        var output: [SQLValue] = []
        do { try batchKernel(values.map { [$0] }, &output) }
        catch {
            guard output.isEmpty else { throw JedError("22000", "host batch failed after answering every row") }
            throw error
        }
        guard output.count == 1 else { throw JedError("22000", "host batch returned an invalid result count") }
        return try validateResult(output[0])
    }
}

/// Immutable outside inputs shared by databases and their sessions. A derived
/// builder shares the value-semantic data and source objects until changed.
public final class Environment: Sendable {
    internal let unicode: UnicodeDatabase
    internal let timezones: TimeZoneDatabase
    internal let functions: [HostFunction]
    public let randomSource: any RandomSource
    public let clockSource: any ClockSource
    fileprivate let keySeed: UInt64?
    fileprivate init(_ builder: EnvironmentBuilder) {
        unicode = builder.unicode; timezones = builder.timezones; functions = builder.functions
        randomSource = builder.randomSource; clockSource = builder.clockSource; keySeed = builder.keySeed
    }
    public static func builder() -> EnvironmentBuilder { EnvironmentBuilder() }
    public func toBuilder() -> EnvironmentBuilder { EnvironmentBuilder(self) }
    internal func keyEntropy() -> KeyEntropy {
        let source = newKeySource()
        return { try source.bytes(count: $0) }
    }
    internal func newKeySource() -> any RandomSource {
        if let keySeed { return SeededRandomSource(seed: keySeed) }
        return OSRandomSource()
    }
    internal var signatures: [HostFunctionSignature] {
        functions.enumerated().map { index, function in
            HostFunctionSignature(id: UInt64(index), name: function.name, arguments: function.arguments,
                                  result: function.result, cost: function.cost, volatility: function.volatility.rawValue)
        }
    }
    internal func call(_ id: UInt64, arguments: [SQLValue]) throws -> SQLValue {
        guard id < functions.count else { throw JedError("42883", "host function is unavailable") }
        return try functions[Int(id)].evaluate(arguments)
    }
}

public struct EnvironmentBuilder {
    fileprivate var unicode = UnicodeDatabase()
    fileprivate var timezones = TimeZoneDatabase()
    fileprivate var functions: [HostFunction] = []
    fileprivate var randomSource: any RandomSource = OSRandomSource()
    fileprivate var clockSource: any ClockSource = WallClock()
    fileprivate var keySeed: UInt64?
    public init() {}
    fileprivate init(_ environment: Environment) {
        unicode = environment.unicode; timezones = environment.timezones; functions = environment.functions
        randomSource = environment.randomSource; clockSource = environment.clockSource; keySeed = environment.keySeed
    }
    public mutating func loadUnicodeData(_ bytes: [UInt8]) throws { try unicode.load(bytes) }
    public mutating func loadTimeZoneData(_ bytes: [UInt8]) throws { try timezones.load(bytes) }
    public mutating func setRandomSource(_ source: any RandomSource) { randomSource = source }
    public mutating func setClockSource(_ source: any ClockSource) { clockSource = source }
    /// Test/fixture-only key stream. Every database takes a fresh stream from
    /// this seed; SQL random reseeding can never affect it.
    public mutating func useInsecureKeyEntropy(seed: UInt64) { keySeed = seed }
    public mutating func useOSKeyEntropy() { keySeed = nil }
    public mutating func register(_ function: HostFunction) throws {
        guard function.cost >= 0, (function.kernel == nil) != (function.batchKernel == nil) else {
            throw JedError("22023", "host function requires a nonnegative cost and exactly one kernel")
        }
        guard !functions.contains(where: { $0.name == function.name && $0.arguments == function.arguments }) else {
            throw JedError("42723", "host function signature already exists")
        }
        functions.append(function)
    }
    public func build() -> Environment { Environment(self) }
}
