import Foundation
import Crypto
import CryptoExtras
// The selected swift-crypto release has no public Swift SHA-224 digest. Keep
// its pinned, approved backend import confined to this cryptographic module.
import CCryptoBoringSSL
import CArgon2

public enum JedCryptoError: Error, Equatable, Sendable {
    case invalidKeySize
    case invalidBlockSize
    case invalidNonceSize
    case invalidCiphertextSize
    case invalidDerivationParameters
    case authenticationFailed
    case backendFailure(Int32)
}

/// Cryptographic utilities for the native Swift experiment. Inputs are exact
/// bytes. Entropy is supplied by the caller, never selected inside this module.
public enum JedCrypto {
    public static func md5(_ bytes: [UInt8]) -> [UInt8] {
        Array(Insecure.MD5.hash(data: bytes))
    }

    public static func sha224(_ bytes: [UInt8]) -> [UInt8] {
        var digest = [UInt8](repeating: 0, count: 28)
        // Both buffers are borrowed synchronously; the library retains neither.
        // SHA224 always returns the caller's 28-byte output buffer.
        bytes.withUnsafeBufferPointer { input in
            digest.withUnsafeMutableBufferPointer { output in
                _ = CCryptoBoringSSL_SHA224(input.baseAddress, input.count, output.baseAddress)
            }
        }
        return digest
    }

    public static func sha256(_ bytes: [UInt8]) -> [UInt8] {
        Array(SHA256.hash(data: bytes))
    }

    public static func sha384(_ bytes: [UInt8]) -> [UInt8] {
        Array(SHA384.hash(data: bytes))
    }

    public static func sha512(_ bytes: [UInt8]) -> [UInt8] {
        Array(SHA512.hash(data: bytes))
    }

    public static func aesBlock(key: [UInt8], block: [UInt8]) throws -> [UInt8] {
        guard key.count == 32 else { throw JedCryptoError.invalidKeySize }
        guard block.count == 16 else { throw JedCryptoError.invalidBlockSize }
        var result = block
        try AES.permute(&result, key: SymmetricKey(data: key))
        return result
    }

    /// Returns ciphertext followed by its 16-byte tag (without a nonce prefix).
    public static func gcmSeal(
        key: [UInt8], nonce: [UInt8], plaintext: [UInt8], aad: [UInt8]
    ) throws -> [UInt8] {
        guard key.count == 32 else { throw JedCryptoError.invalidKeySize }
        guard nonce.count == 12 else { throw JedCryptoError.invalidNonceSize }
        let box = try AES.GCM.seal(
            plaintext, using: SymmetricKey(data: key),
            nonce: AES.GCM.Nonce(data: nonce), authenticating: aad
        )
        return Array(box.ciphertext) + Array(box.tag)
    }

    /// Accepts ciphertext followed by its 16-byte tag, with a separate nonce.
    public static func gcmOpen(
        key: [UInt8], nonce: [UInt8], sealed: [UInt8], aad: [UInt8]
    ) throws -> [UInt8] {
        guard key.count == 32 else { throw JedCryptoError.invalidKeySize }
        guard nonce.count == 12 else { throw JedCryptoError.invalidNonceSize }
        guard sealed.count >= 16 else { throw JedCryptoError.invalidCiphertextSize }
        let box = try AES.GCM.SealedBox(
            nonce: AES.GCM.Nonce(data: nonce), ciphertext: sealed.dropLast(16), tag: sealed.suffix(16)
        )
        do {
            return Array(try AES.GCM.open(box, using: SymmetricKey(data: key), authenticating: aad))
        } catch {
            throw JedCryptoError.authenticationFailed
        }
    }

    public static func hkdfSHA256(
        inputKeyMaterial: [UInt8], salt: [UInt8], info: [UInt8], outputByteCount: Int = 32
    ) throws -> [UInt8] {
        guard outputByteCount > 0, outputByteCount <= 255 * 32 else {
            throw JedCryptoError.invalidDerivationParameters
        }
        let key = HKDF<SHA256>.deriveKey(
            inputKeyMaterial: SymmetricKey(data: inputKeyMaterial), salt: salt, info: info,
            outputByteCount: outputByteCount
        )
        return key.withUnsafeBytes { Array($0) }
    }

    /// Argon2id v0x13 with a 32-byte result. The file reader must additionally
    /// enforce the format's fixed 32-byte salt; the primitive accepts the shared
    /// 8-byte test-vector salt as well. Limits are checked before C allocation.
    public static func argon2id(
        password: [UInt8], salt: [UInt8], memoryKiB: UInt32, iterations: UInt32, parallelism: UInt32
    ) throws -> [UInt8] {
        guard (1...16).contains(parallelism), (1...16).contains(iterations),
              memoryKiB >= 8 * parallelism, memoryKiB <= 1_048_576,
              salt.count >= 8, salt.count <= 32,
              UInt64(password.count) <= UInt64(UInt32.max) else {
            throw JedCryptoError.invalidDerivationParameters
        }
        var result = [UInt8](repeating: 0, count: 32)
        // The raw Argon2 call is synchronous and cannot retain any pointer. Its
        // internal allocator/wiping policy stays upstream's unchanged default.
        let status: Int32 = password.withUnsafeBytes { passwordBytes in
            salt.withUnsafeBytes { saltBytes in
                result.withUnsafeMutableBytes { output in
                    argon2id_hash_raw(
                        iterations, memoryKiB, parallelism,
                        passwordBytes.baseAddress, passwordBytes.count,
                        saltBytes.baseAddress, saltBytes.count,
                        output.baseAddress, output.count
                    )
                }
            }
        }
        guard status == 0 else { throw JedCryptoError.backendFailure(status) }
        return result
    }

    public static func xaesSeal(
        key: [UInt8], nonce: [UInt8], plaintext: [UInt8], aad: [UInt8]
    ) throws -> [UInt8] {
        try XAES256GCM(key: key).seal(nonce: nonce, plaintext: plaintext, aad: aad)
    }

    public static func xaesOpen(
        key: [UInt8], nonce: [UInt8], sealed: [UInt8], aad: [UInt8]
    ) throws -> [UInt8] {
        try XAES256GCM(key: key).open(nonce: nonce, sealed: sealed, aad: aad)
    }
}

/// C2SP XAES-256-GCM derivation glue over library AES and AES-GCM. The
/// key-dependent doubling block is computed once at unlock, as encrypted.md §3
/// specifies. This type owns no entropy source and never reuses a chosen nonce.
public struct XAES256GCM: Sendable {
    private let key: SymmetricKey
    private let doubledBlock: [UInt8]

    public init(key: [UInt8]) throws {
        guard key.count == 32 else { throw JedCryptoError.invalidKeySize }
        self.key = SymmetricKey(data: key)
        var block = [UInt8](repeating: 0, count: 16)
        try AES.permute(&block, key: self.key)
        let reduction = (UInt8(0) &- (block[0] >> 7)) & 0x87
        for index in 0..<15 { block[index] = (block[index] << 1) | (block[index + 1] >> 7) }
        block[15] = (block[15] << 1) ^ reduction
        self.doubledBlock = block
    }

    private func derivedKey(nonce: [UInt8]) throws -> [UInt8] {
        guard nonce.count == 24 else { throw JedCryptoError.invalidNonceSize }
        var first: [UInt8] = [0, 1, 0x58, 0]
        first.append(contentsOf: nonce.prefix(12))
        var second = first
        second[1] = 2
        for index in 0..<16 {
            first[index] ^= doubledBlock[index]
            second[index] ^= doubledBlock[index]
        }
        try AES.permute(&first, key: key)
        try AES.permute(&second, key: key)
        return first + second
    }

    public func seal(nonce: [UInt8], plaintext: [UInt8], aad: [UInt8]) throws -> [UInt8] {
        let derived = try derivedKey(nonce: nonce)
        return try JedCrypto.gcmSeal(key: derived, nonce: Array(nonce.suffix(12)), plaintext: plaintext, aad: aad)
    }

    public func open(nonce: [UInt8], sealed: [UInt8], aad: [UInt8]) throws -> [UInt8] {
        let derived = try derivedKey(nonce: nonce)
        return try JedCrypto.gcmOpen(key: derived, nonce: Array(nonce.suffix(12)), sealed: sealed, aad: aad)
    }
}
