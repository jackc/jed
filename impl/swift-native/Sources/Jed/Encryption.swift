import JedCrypto

/// An embedding credential, kept outside SQL settings and diagnostics.
public struct Credential: Sendable, CustomStringConvertible, CustomDebugStringConvertible {
    fileprivate let kind: UInt8
    fileprivate let bytes: [UInt8]

    public static func hostKey(_ bytes: [UInt8]) throws -> Credential {
        guard bytes.count == 32 else { throw JedError("22023", "host key must contain 32 bytes") }
        return Credential(kind: 2, bytes: bytes)
    }

    public static func passphrase(_ text: String) -> Credential {
        Credential(kind: 1, bytes: Array(text.utf8))
    }

    public var description: String { "Credential(<redacted>)" }
    public var debugDescription: String { description }
}

public struct Argon2Parameters: Sendable, Equatable {
    public let memoryKiB: UInt32
    public let iterations: UInt32
    public let parallelism: UInt32

    public init(memoryKiB: UInt32 = 65_536, iterations: UInt32 = 3, parallelism: UInt32 = 4) throws {
        guard Self.valid(memoryKiB, iterations, parallelism) else {
            throw JedError("22023", "invalid Argon2id parameters")
        }
        self.memoryKiB = memoryKiB
        self.iterations = iterations
        self.parallelism = parallelism
    }

    private init(defaults: Void) { memoryKiB = 65_536; iterations = 3; parallelism = 4 }
    public static let `default` = Argon2Parameters(defaults: ())

    fileprivate static func valid(_ memory: UInt32, _ iterations: UInt32, _ lanes: UInt32) -> Bool {
        (1...16).contains(lanes) && (1...16).contains(iterations)
            && memory >= 8 * lanes && memory <= 1_048_576
    }
}

/// The host's dedicated cryptographic stream. This is never the SQL random
/// stream. Each call must return exactly the requested number of bytes.
public typealias KeyEntropy = (_ count: Int) throws -> [UInt8]

internal enum EncryptionPolicy {
    static func checkCredential(encrypted: Bool, inMemory: Bool = false, credential: Credential?) throws {
        if inMemory, credential != nil { throw JedError("55000", "in-memory databases cannot use credentials") }
        if encrypted, credential == nil { throw JedError("28P02", "credential required") }
        if !encrypted, credential != nil { throw JedError("28P03", "credential does not apply to a plaintext database") }
    }

    static func entropy(_ count: Int, from source: KeyEntropy) throws -> [UInt8] {
        let bytes = try source(count)
        guard bytes.count == count else { throw JedError("58030", "key entropy source returned an invalid byte count") }
        return bytes
    }
}

/// One validated, immutable 128-byte key-block record. Selection is structural
/// and finishes before credentials or a potentially expensive KDF are touched.
internal struct KeyBlock: Sendable, Equatable {
    let record: [UInt8]
    let generation: UInt64
    private let kdf: UInt8
    private let memoryKiB: UInt32
    private let iterations: UInt32
    private let parallelism: UInt32

    static func parse(_ page: [UInt8], pageSize: Int) throws -> KeyBlock {
        try PageEnvelope.validateSize(pageSize, encrypted: true, opening: true)
        guard page.count == pageSize else { throw JedError.corruption("invalid key-block page length") }
        var reader = ByteReader(page)
        guard Array(try reader.take(4)) == Array("JEDK".utf8), try reader.u16() == 1,
              try reader.u8() == 1 else { throw JedError.corruption("invalid key-block header") }
        let kdf = try reader.u8()
        let generation = try reader.u64()
        let memory = try reader.u32(), iterations = try reader.u32(), lanes = try reader.u32()
        guard generation > 0, try reader.u32() == 0 else {
            throw JedError.corruption("invalid key-block generation or reserved field")
        }
        let validParameters = kdf == 1 ? Argon2Parameters.valid(memory, iterations, lanes)
            : kdf == 2 && memory == 0 && iterations == 0 && lanes == 0
        guard validParameters else { throw JedError.corruption("invalid key-block KDF parameters") }
        _ = try reader.take(92)
        let crc = try reader.u32()
        guard crc == Checksums.crc32(page.prefix(124)), page.dropFirst(128).allSatisfy({ $0 == 0 }) else {
            throw JedError.corruption("invalid key-block checksum or padding")
        }
        return KeyBlock(record: Array(page.prefix(128)), generation: generation, kdf: kdf,
                        memoryKiB: memory, iterations: iterations, parallelism: lanes)
    }

    func encode(pageSize: Int) throws -> [UInt8] {
        try PageEnvelope.validateSize(pageSize, encrypted: true, opening: false)
        return record + [UInt8](repeating: 0, count: pageSize - record.count)
    }

    static func select(page2: [UInt8], page3: [UInt8], pageSize: Int) throws -> KeyBlockSelection {
        try PageEnvelope.validateSize(pageSize, encrypted: true, opening: true)
        let first = try? parse(page2, pageSize: pageSize)
        let second = try? parse(page3, pageSize: pageSize)
        switch (first, second) {
        case (nil, nil): throw JedError.corruption("neither key-block copy is valid")
        case (let first?, nil): return KeyBlockSelection(block: first, selectedPage: 2, needsRepair: true)
        case (nil, let second?): return KeyBlockSelection(block: second, selectedPage: 3, needsRepair: true)
        case (let first?, let second?):
            if first.generation == second.generation {
                guard first.record == second.record else {
                    throw JedError.corruption("key-block copies disagree at equal generation")
                }
                return KeyBlockSelection(block: first, selectedPage: 2, needsRepair: false)
            }
            return first.generation > second.generation
                ? KeyBlockSelection(block: first, selectedPage: 2, needsRepair: true)
                : KeyBlockSelection(block: second, selectedPage: 3, needsRepair: true)
        }
    }

    func unlock(_ credential: Credential?) throws -> [UInt8] {
        guard let credential else { throw JedError("28P02", "credential required") }
        guard credential.kind == kdf else { throw JedError("28P01", "invalid credential") }
        let key = try Self.deriveKEK(credential, salt: Array(record[32..<64]),
                                    memory: memoryKiB, iterations: iterations, lanes: parallelism)
        do {
            return try JedCrypto.gcmOpen(key: key, nonce: Array(record[64..<76]),
                                         sealed: Array(record[76..<124]), aad: Array(record.prefix(76)))
        } catch { throw JedError("28P01", "invalid credential") }
    }

    static func create(credential: Credential, parameters: Argon2Parameters = .default,
                       entropy: KeyEntropy) throws -> (block: KeyBlock, dataKey: [UInt8]) {
        let dataKey = try EncryptionPolicy.entropy(32, from: entropy)
        let block = try wrap(dataKey: dataKey, generation: 1, credential: credential,
                             parameters: parameters, entropy: entropy)
        return (block, dataKey)
    }

    func rewrap(dataKey: [UInt8], credential: Credential, parameters: Argon2Parameters = .default,
                entropy: KeyEntropy) throws -> KeyBlock {
        guard generation < UInt64.max else { throw JedError.corruption("key-block generation exhausted") }
        return try Self.wrap(dataKey: dataKey, generation: generation + 1, credential: credential,
                             parameters: parameters, entropy: entropy)
    }

    private static func wrap(dataKey: [UInt8], generation: UInt64, credential: Credential,
                             parameters: Argon2Parameters, entropy: KeyEntropy) throws -> KeyBlock {
        guard dataKey.count == 32 else { throw JedError("22023", "data key must contain 32 bytes") }
        let memory: UInt32 = credential.kind == 1 ? parameters.memoryKiB : 0
        let iterations: UInt32 = credential.kind == 1 ? parameters.iterations : 0
        let lanes: UInt32 = credential.kind == 1 ? parameters.parallelism : 0
        let salt = try EncryptionPolicy.entropy(32, from: entropy)
        let nonce = try EncryptionPolicy.entropy(12, from: entropy)
        var header = ByteWriter()
        header.append("JEDK".utf8); header.u16(1); header.u8(1); header.u8(credential.kind)
        header.u64(generation); header.u32(memory); header.u32(iterations); header.u32(lanes); header.u32(0)
        header.append(salt); header.append(nonce)
        let key = try deriveKEK(credential, salt: salt, memory: memory, iterations: iterations, lanes: lanes)
        do {
            let wrapped = try JedCrypto.gcmSeal(key: key, nonce: nonce, plaintext: dataKey, aad: header.bytes)
            header.append(wrapped)
            header.u32(Checksums.crc32(header.bytes))
        } catch { throw JedError("58030", "key wrapping failed") }
        return KeyBlock(record: header.bytes, generation: generation, kdf: credential.kind,
                        memoryKiB: memory, iterations: iterations, parallelism: lanes)
    }

    private static func deriveKEK(_ credential: Credential, salt: [UInt8], memory: UInt32,
                                  iterations: UInt32, lanes: UInt32) throws -> [UInt8] {
        do {
            if credential.kind == 2 {
                return try JedCrypto.hkdfSHA256(inputKeyMaterial: credential.bytes, salt: salt, info: Array("jed/v1/kek".utf8))
            }
            return try JedCrypto.argon2id(password: credential.bytes, salt: salt,
                                          memoryKiB: memory, iterations: iterations, parallelism: lanes)
        } catch { throw JedError("58030", "key derivation failed") }
    }
}

internal struct KeyBlockSelection: Sendable {
    let block: KeyBlock
    let selectedPage: UInt32
    let needsRepair: Bool
    var unselectedPage: UInt32 { selectedPage == 2 ? 3 : 2 }

    /// Must be rechecked under the writer gate immediately before rewrap writes.
    func validateReplacement(_ replacement: KeyBlock) throws {
        guard block.generation < UInt64.max, replacement.generation == block.generation + 1 else {
            throw JedError("55006", "credential changed concurrently")
        }
    }
}
