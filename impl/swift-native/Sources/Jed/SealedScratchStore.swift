import JedCrypto
import Synchronization

/// Internal scratch framing from encrypted.md §11. No plaintext header or
/// plaintext data reaches the underlying file. Counters outlive truncation;
/// logical holes and discarded blocks read as zero without accepting old bytes.
internal final class SealedScratchStore: ScratchStore {
    static let blockBytes = 4096
    static let storedBytes = 4136
    private struct State {
        var length: UInt64 = 0
        var counters: [UInt64: UInt64] = [:]
        var live: Set<UInt64> = []
        var closed = false
        var poisoned = false
    }
    private let state = Mutex(State())
    private let inner: any ScratchStore
    private let cipher: XAES256GCM
    private let identifier: [UInt8]
    private let random: any RandomSource

    init(inner: any ScratchStore, random: any RandomSource) throws {
        guard try inner.size() == 0 else { throw JedError("55006", "sealed scratch storage must be empty") }
        let key = try random.bytes(count: 32), identifier = try random.bytes(count: 16)
        guard key.count == 32, identifier.count == 16 else { throw JedError("58030", "invalid scratch entropy length") }
        self.inner = inner; self.random = random; self.identifier = identifier
        do { cipher = try XAES256GCM(key: key) }
        catch { throw JedError("58030", "scratch key initialization failed") }
    }
    private func check(_ state: State) throws {
        guard !state.closed, !state.poisoned else { throw JedError("58030", "scratch storage is closed or unusable") }
    }
    private func aad(_ ordinal: UInt64, _ counter: UInt64) -> [UInt8] {
        var writer = ByteWriter(); writer.append("JEDB".utf8); writer.u8(1); writer.u8(3)
        writer.append(identifier); writer.u64(ordinal); writer.u64(counter); return writer.bytes
    }
    private func physical(_ ordinal: UInt64) throws -> UInt64 {
        let offset = ordinal.multipliedReportingOverflow(by: UInt64(Self.storedBytes))
        guard !offset.overflow, offset.partialValue <= UInt64(Int64.max) - UInt64(Self.storedBytes) else {
            throw JedError("58030", "scratch offset exceeds supported file size")
        }
        return offset.partialValue
    }
    private func load(_ ordinal: UInt64, state: State) throws -> [UInt8] {
        guard state.live.contains(ordinal) else { return Array(repeating: 0, count: Self.blockBytes) }
        guard let counter = state.counters[ordinal] else { throw JedError("58030", "scratch block has no write counter") }
        let stored = try inner.read(at: physical(ordinal), count: Self.storedBytes)
        guard stored.count == Self.storedBytes else { throw JedError("58030", "short sealed scratch read") }
        do {
            return try cipher.open(nonce: Array(stored[4096..<4120]),
                sealed: Array(stored[..<4096]) + Array(stored[4120...]), aad: aad(ordinal, counter))
        } catch { throw JedError("58030", "scratch block authentication failed") }
    }
    private func seal(_ bytes: [UInt8], ordinal: UInt64, state: inout State) throws {
        let old = state.counters[ordinal] ?? 0
        guard old < UInt64.max else { throw JedError("58030", "scratch block write counter exhausted") }
        let counter = old + 1, nonce = try random.bytes(count: 24)
        guard nonce.count == 24 else { throw JedError("58030", "invalid scratch nonce length") }
        let sealed: [UInt8]
        do { sealed = try cipher.seal(nonce: nonce, plaintext: bytes, aad: aad(ordinal, counter)) }
        catch { throw JedError("58030", "scratch block sealing failed") }
        state.counters[ordinal] = counter
        try inner.write(at: physical(ordinal), bytes: Array(sealed[..<4096]) + nonce + Array(sealed[4096...]))
        state.live.insert(ordinal)
    }
    func read(at offset: UInt64, count: Int) throws -> [UInt8] {
        try state.withLock { state in
            try check(state)
            guard count >= 0, offset <= state.length, UInt64(count) <= state.length - offset else {
                throw JedError("58030", "short scratch read")
            }
            var result: [UInt8] = []; result.reserveCapacity(count)
            var current = offset, remaining = count
            while remaining > 0 {
                let ordinal = current / 4096, within = Int(current % 4096), count = min(remaining, 4096 - within)
                let block = try load(ordinal, state: state)
                result.append(contentsOf: block[within..<(within + count)])
                current += UInt64(count); remaining -= count
            }
            return result
        }
    }
    func write(at offset: UInt64, bytes: [UInt8]) throws {
        try state.withLock { state in
            try check(state)
            guard offset <= UInt64(Int64.max), UInt64(bytes.count) <= UInt64(Int64.max) - offset else {
                throw JedError("58030", "scratch offset exceeds supported file size")
            }
            guard !bytes.isEmpty else { return }
            do {
                var position = 0
                while position < bytes.count {
                    let at = offset + UInt64(position), ordinal = at / 4096, within = Int(at % 4096)
                    let count = min(bytes.count - position, 4096 - within)
                    var block = count == 4096 ? Array(repeating: UInt8(0), count: 4096) : try load(ordinal, state: state)
                    block.replaceSubrange(within..<(within + count), with: bytes[position..<(position + count)])
                    try seal(block, ordinal: ordinal, state: &state)
                    position += count
                }
                state.length = max(state.length, offset + UInt64(bytes.count))
            } catch { state.poisoned = true; throw error }
        }
    }
    func size() throws -> UInt64 { try state.withLock { try check($0); return $0.length } }
    func setSize(_ bytes: UInt64) throws {
        try state.withLock { state in
            try check(state)
            guard bytes <= UInt64(Int64.max) else { throw JedError("58030", "scratch size exceeds supported file size") }
            do {
                if bytes < state.length {
                    let blocks = (bytes + 4095) / 4096, within = Int(bytes % 4096)
                    if within != 0, state.live.contains(bytes / 4096) {
                        var block = try load(bytes / 4096, state: state)
                        block.replaceSubrange(within..<4096, with: repeatElement(UInt8(0), count: 4096 - within))
                        try seal(block, ordinal: bytes / 4096, state: &state)
                    }
                    state.live = Set(state.live.filter { $0 < blocks })
                    let physicalLength = blocks.multipliedReportingOverflow(by: UInt64(Self.storedBytes))
                    guard !physicalLength.overflow else { throw JedError("58030", "scratch size overflow") }
                    try inner.setSize(physicalLength.partialValue)
                }
                state.length = bytes
            } catch { state.poisoned = true; throw error }
        }
    }
    func sync() throws { try state.withLock { try check($0) } }
    func close() throws {
        try state.withLock { state in
            if state.closed { return }; state.closed = true; state.live = []; state.counters = [:]
            try inner.close()
        }
    }
    deinit { try? close() }
}
