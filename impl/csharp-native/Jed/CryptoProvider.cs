using System.Security.Cryptography;
using Org.BouncyCastle.Crypto.Digests;
using Org.BouncyCastle.Crypto.Generators;
using Org.BouncyCastle.Crypto.Parameters;

namespace Jed;

// The only package boundary: SHA-224 and Argon2id, including provider-internal
// helpers. All other primitives come from System.Security.Cryptography.
internal static class CryptoProvider
{
    internal static byte[] Digest(string algorithm, ReadOnlySpan<byte> input)
    {
        if (algorithm == "sha224")
        {
            var digest = new Sha224Digest();
            try
            {
                digest.BlockUpdate(input);
                var output = new byte[28];
                digest.DoFinal(output);
                return output;
            }
            finally { digest.Reset(); }
        }
        return algorithm switch
        {
            "md5" => MD5.HashData(input),
            "sha256" => SHA256.HashData(input),
            "sha384" => SHA384.HashData(input),
            "sha512" => SHA512.HashData(input),
            _ => throw new ArgumentException("unsupported digest", nameof(algorithm))
        };
    }

    internal static byte[] Hkdf(ReadOnlySpan<byte> key, ReadOnlySpan<byte> salt, ReadOnlySpan<byte> info)
    {
        var result = new byte[32];
        HKDF.DeriveKey(HashAlgorithmName.SHA256, key, result, salt, info);
        return result;
    }

    // Validate untrusted file fields before the provider allocates its blocks.
    // Salt size is checked by the key-block decoder; the primitive also accepts
    // RFC vectors with shorter salts.
    internal static byte[] Argon2id(ReadOnlySpan<byte> password, ReadOnlySpan<byte> salt,
                                    uint memoryKiB, uint passes, uint lanes)
    {
        if (lanes is < 1 or > 16 || passes is < 1 or > 16 || memoryKiB < 8 * lanes || memoryKiB > 1048576)
            throw new SqlException(SqlStates.DataCorrupted, "invalid Argon2id parameters");
        if (salt.Length < 8) throw new ArgumentException("Argon2id salt must contain at least eight bytes", nameof(salt));
        var builder = new Argon2Parameters.Builder(Argon2Parameters.Argon2id);
        byte[] ownedPassword = [], ownedSalt = [], output = [];
        Argon2Parameters? parameters = null;
        try
        {
            ownedPassword = password.ToArray();
            ownedSalt = salt.ToArray();
            output = new byte[32];
            parameters = builder.WithVersion(Argon2Parameters.Version13).WithSalt(ownedSalt)
                .WithMemoryAsKB((int)memoryKiB).WithIterations((int)passes).WithParallelism((int)lanes).Build();
            // No TaskFactory: lanes are a cryptographic parameter, not a request
            // to dispatch CPU work or block on Task.WaitAll.
            var generator = new Argon2BytesGenerator();
            generator.Init(parameters);
            generator.GenerateBytes(ownedPassword, output);
            return output;
        }
        catch
        {
            CryptographicOperations.ZeroMemory(output);
            throw;
        }
        finally
        {
            CryptographicOperations.ZeroMemory(ownedPassword);
            CryptographicOperations.ZeroMemory(ownedSalt);
            builder.Clear();
            parameters?.Clear();
            // BC clears its main block array on successful GenerateBytes only.
            // It exposes no disposal/reset API for exception paths or scratch;
            // this wrapper cannot promise complete provider-secret erasure.
        }
    }

    internal static byte[] GcmSeal(ReadOnlySpan<byte> key, ReadOnlySpan<byte> nonce,
                                   ReadOnlySpan<byte> plaintext, ReadOnlySpan<byte> associatedData)
    {
        CheckKey(key);
        if (nonce.Length != 12) throw new ArgumentException("AES-GCM requires a 12-byte nonce", nameof(nonce));
        var result = new byte[checked(plaintext.Length + 16)];
        try
        {
            using var cipher = new AesGcm(key, 16);
            cipher.Encrypt(nonce, plaintext, result.AsSpan(0, plaintext.Length), result.AsSpan(plaintext.Length), associatedData);
            return result;
        }
        catch { CryptographicOperations.ZeroMemory(result); throw; }
    }

    // The caller chooses context-specific SQLSTATE (28P01 key unwrap, XX001
    // durable page, or 58030 scratch). Authentication never returns plaintext.
    internal static byte[]? GcmOpen(ReadOnlySpan<byte> key, ReadOnlySpan<byte> nonce,
                                    ReadOnlySpan<byte> sealedBytes, ReadOnlySpan<byte> associatedData)
    {
        CheckKey(key);
        if (nonce.Length != 12) throw new ArgumentException("AES-GCM requires a 12-byte nonce", nameof(nonce));
        if (sealedBytes.Length < 16) return null;
        var result = new byte[sealedBytes.Length - 16];
        try
        {
            using var cipher = new AesGcm(key, 16);
            cipher.Decrypt(nonce, sealedBytes[..^16], sealedBytes[^16..], result, associatedData);
            return result;
        }
        catch (AuthenticationTagMismatchException) { CryptographicOperations.ZeroMemory(result); return null; }
        catch { CryptographicOperations.ZeroMemory(result); throw; }
    }

    internal static void CheckKey(ReadOnlySpan<byte> key)
    {
        if (key.Length != 32) throw new ArgumentException("expected a 32-byte key", nameof(key));
    }
}

// C2SP XAES-256-GCM derivation glue over provider AES, per encrypted.md §3.
// One instance belongs to a codec; callers must serialize use and disposal.
internal sealed class Xaes256Gcm : IDisposable
{
    private readonly Aes aes;
    private readonly byte[] subkey = new byte[16];
    private bool disposed;

    internal Xaes256Gcm(ReadOnlySpan<byte> key)
    {
        CryptoProvider.CheckKey(key);
        aes = Aes.Create();
        var ownedKey = key.ToArray();
        Span<byte> block = stackalloc byte[16];
        block.Clear();
        try
        {
            aes.Key = ownedKey;
            aes.EncryptEcb(block, subkey, PaddingMode.None);
            var carry = subkey[0] >> 7;
            for (var i = 0; i < 15; i++) subkey[i] = unchecked((byte)((subkey[i] << 1) | (subkey[i + 1] >> 7)));
            subkey[15] = unchecked((byte)((subkey[15] << 1) ^ (0x87 & -carry)));
        }
        catch { aes.Dispose(); CryptographicOperations.ZeroMemory(subkey); throw; }
        finally { CryptographicOperations.ZeroMemory(ownedKey); CryptographicOperations.ZeroMemory(block); }
    }

    private void Derive(ReadOnlySpan<byte> nonce, Span<byte> key)
    {
        lock (aes)
        {
        ObjectDisposedException.ThrowIf(disposed, this);
        if (nonce.Length != 24) throw new ArgumentException("XAES requires a 24-byte nonce", nameof(nonce));
        Span<byte> block = stackalloc byte[16];
        try
        {
            block[0] = 0; block[1] = 1; block[2] = 0x58; block[3] = 0;
            nonce[..12].CopyTo(block[4..]);
            for (var i = 0; i < 16; i++) block[i] ^= subkey[i];
            aes.EncryptEcb(block, key[..16], PaddingMode.None);
            block[1] ^= 3; // (1 xor subkey) -> (2 xor subkey).
            aes.EncryptEcb(block, key[16..], PaddingMode.None);
        }
        finally { CryptographicOperations.ZeroMemory(block); }
        }
    }

    internal byte[] Seal(ReadOnlySpan<byte> nonce, ReadOnlySpan<byte> plaintext, ReadOnlySpan<byte> associatedData)
    {
        Span<byte> key = stackalloc byte[32];
        try { Derive(nonce, key); return CryptoProvider.GcmSeal(key, nonce[12..], plaintext, associatedData); }
        finally { CryptographicOperations.ZeroMemory(key); }
    }

    internal byte[]? Open(ReadOnlySpan<byte> nonce, ReadOnlySpan<byte> sealedBytes, ReadOnlySpan<byte> associatedData)
    {
        Span<byte> key = stackalloc byte[32];
        try { Derive(nonce, key); return CryptoProvider.GcmOpen(key, nonce[12..], sealedBytes, associatedData); }
        finally { CryptographicOperations.ZeroMemory(key); }
    }

    public void Dispose()
    {
        lock (aes)
        {
        if (disposed) return;
        disposed = true;
        CryptographicOperations.ZeroMemory(subkey);
        aes.Dispose();
        }
    }
}
