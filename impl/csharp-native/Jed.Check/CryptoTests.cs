using System.Globalization;
using System.Security.Cryptography;
using System.Text;
using Jed;

internal static class CryptoTests
{
    internal static void Run(string root)
    {
        // The task passes the immutable semantic-baseline checkout. Read shared
        // facts directly rather than maintaining C# copies of crypto goldens.
        var path = Path.Combine(root, "spec/fileformat/encryption_vectors.toml");
        var sections = ReadVectors(path);
        foreach (var vector in sections.Where(s => s.Name == "xaes"))
        {
            using var cipher = new Xaes256Gcm(Hex(vector, "key_hex"));
            var nonce = Hex(vector, "nonce_hex"); var plaintext = Hex(vector, "plaintext_hex");
            var aad = Hex(vector, "aad_hex"); var expected = Hex(vector, "sealed_hex");
            Bytes(expected, cipher.Seal(nonce, plaintext, aad), vector.Values["name"] + " seal");
            Bytes(plaintext, cipher.Open(nonce, expected, aad)!, vector.Values["name"] + " open");
            for (var i = 0; i < expected.Length; i++)
            {
                var altered = expected.ToArray(); altered[i] ^= 1;
                Corpus.Equal<byte[]?>(null, cipher.Open(nonce, altered, aad), "tampered ciphertext/tag");
            }
            nonce[0] ^= 1;
            Corpus.Equal<byte[]?>(null, cipher.Open(nonce, expected, aad), "tampered nonce prefix");
            nonce[0] ^= 1; nonce[23] ^= 1;
            Corpus.Equal<byte[]?>(null, cipher.Open(nonce, expected, aad), "tampered nonce suffix");
            Corpus.Equal<byte[]?>(null, cipher.Open(Hex(vector, "nonce_hex"), expected, [1]), "wrong associated data");
        }
        var subkeys = sections.Single(s => s.Name == "subkeys");
        Bytes(Hex(subkeys, "page_key_hex"), CryptoProvider.Hkdf(Hex(subkeys, "dek_hex"), [], "jed/v1/page"u8), "page subkey");
        Bytes(Hex(subkeys, "meta_key_hex"), CryptoProvider.Hkdf(Hex(subkeys, "dek_hex"), [], "jed/v1/meta"u8), "meta subkey");
        var host = sections.Single(s => s.Name == "host_kek");
        Bytes(Hex(host, "kek_hex"), CryptoProvider.Hkdf(Hex(host, "key_hex"), Hex(host, "kdf_salt_hex"), "jed/v1/kek"u8), "host KEK");
        var block = sections.Single(s => s.Name == "key_block");
        var dek = Hex(block, "dek_hex");
        CheckWrap(Hex(block, "block_hex"), Hex(host, "key_hex"), dek);
        var rewrap = sections.Single(s => s.Name == "rewrap");
        CheckWrap(Hex(rewrap, "block_hex"), Hex(rewrap, "key_hex"), dek);
        var argon = sections.Single(s => s.Name == "argon2id");
        Bytes(Hex(argon, "key_hex"), CryptoProvider.Argon2id(Encoding.UTF8.GetBytes(argon.Values["password"]),
            Encoding.UTF8.GetBytes(argon.Values["salt"]), Number(argon, "memory_kib"), Number(argon, "iterations"), Number(argon, "parallelism")), "shared Argon2id");
        foreach (var (memory, passes, lanes) in new (uint, uint, uint)[]
                 { (0, 1, 1), (7, 1, 1), (31, 1, 4), (1048577, 1, 1), (8, 0, 1), (8, 17, 1), (8, 1, 0), (136, 1, 17), (uint.MaxValue, 1, uint.MaxValue) })
        {
            try { _ = CryptoProvider.Argon2id([], "12345678"u8, memory, passes, lanes); }
            catch (SqlException error) when (error.SqlState == SqlStates.DataCorrupted) { continue; }
            throw new InvalidDataException("Argon2id accepted invalid bounds");
        }
        using (var cipher = new Xaes256Gcm(new byte[32]))
        {
            var empty = cipher.Seal(new byte[24], [], []);
            Corpus.Equal(16, empty.Length, "empty XAES tag");
            Bytes([], cipher.Open(new byte[24], empty, [])!, "empty XAES plaintext");
            Corpus.Equal<byte[]?>(null, cipher.Open(new byte[24], new byte[15], []), "short XAES ciphertext");
            cipher.Dispose();
            try { _ = cipher.Seal(new byte[24], [], []); }
            catch (ObjectDisposedException) { goto Disposed; }
            throw new InvalidDataException("disposed XAES accepted a seal");
        }
        Disposed:
        Digests(Path.Combine(root, "spec/conformance/suites/expr/hash_functions.test"));
        Console.WriteLine($"PASS crypto provider: shared XAES/HKDF/key-wrap/Argon2id vectors, digest vectors, authentication and bounds; AVX2 available={System.Runtime.Intrinsics.X86.Avx2.IsSupported}");
    }

    private static void CheckWrap(byte[] block, byte[] hostKey, byte[] dek)
    {
        var kek = CryptoProvider.Hkdf(hostKey, block.AsSpan(32, 32), "jed/v1/kek"u8);
        try
        {
            Bytes(block.AsSpan(76, 48).ToArray(), CryptoProvider.GcmSeal(kek, block.AsSpan(64, 12), dek, block.AsSpan(0, 76)), "key-block wrap");
            Bytes(dek, CryptoProvider.GcmOpen(kek, block.AsSpan(64, 12), block.AsSpan(76, 48), block.AsSpan(0, 76))!, "key-block unwrap");
            kek[0] ^= 1;
            Corpus.Equal<byte[]?>(null, CryptoProvider.GcmOpen(kek, block.AsSpan(64, 12), block.AsSpan(76, 48), block.AsSpan(0, 76)), "wrong wrap credential");
        }
        finally { CryptographicOperations.ZeroMemory(kek); }
    }

    private static void Digests(string path)
    {
        var lines = File.ReadAllLines(path);
        string Expected(string sql, int column = 0)
        {
            var position = Array.IndexOf(lines, sql);
            if (position < 0 || lines[position + 1] != "----") throw new InvalidDataException("shared digest vector missing: " + sql);
            return lines[position + 2 + column].Replace("\\x", "", StringComparison.Ordinal);
        }
        void Check(string algorithm, byte[] input, string sql, int column = 0) =>
            Corpus.Equal(Expected(sql, column), Convert.ToHexStringLower(CryptoProvider.Digest(algorithm, input)), algorithm + " shared vector");
        Check("md5", [], "SELECT md5(''), md5('abc')");
        Check("md5", "abc"u8.ToArray(), "SELECT md5(''), md5('abc')", 1);
        Check("md5", Encoding.UTF8.GetBytes("héllo"), "SELECT md5(bytea '\\x616263'), md5('héllo'), md5(bytea '\\x68c3a96c6c6f')", 1);
        Check("md5", Enumerable.Repeat((byte)'a', 1000).ToArray(), "SELECT md5(repeat('a', 1000))");
        string[] algorithms = ["sha224", "sha256", "sha384", "sha512"];
        for (var i = 0; i < algorithms.Length; i++)
        {
            Check(algorithms[i], [], $"SELECT {algorithms[i]}(bytea '\\x')");
            Check(algorithms[i], "abc"u8.ToArray(), "SELECT encode(sha224(bytea '\\x616263'), 'hex'), encode(sha256(bytea '\\x616263'), 'hex'), encode(sha384(bytea '\\x616263'), 'hex'), encode(sha512(bytea '\\x616263'), 'hex')", i);
        }
        const string longSql = "SELECT encode(sha256(decode(repeat('ab', 100), 'hex')), 'hex'), encode(sha512(decode(repeat('ab', 200), 'hex')), 'hex')";
        Check("sha256", Enumerable.Repeat((byte)0xab, 100).ToArray(), longSql);
        Check("sha512", Enumerable.Repeat((byte)0xab, 200).ToArray(), longSql, 1);
    }

    internal sealed record Vector(string Name, Dictionary<string, string> Values);
    // This reader intentionally accepts only the flat sections and literal
    // strings/integers of this shared vector file, not general TOML or SQL.
    internal static List<Vector> ReadVectors(string path)
    {
        var result = new List<Vector>(); Vector? section = null;
        foreach (var source in File.ReadLines(path))
        {
            var line = source.Trim();
            if (line.Length == 0 || line.StartsWith('#') || line == "schema_version = 1") continue;
            if (line.StartsWith('[') && line.EndsWith(']'))
            {
                section = new(line.Trim('[', ']'), new(StringComparer.Ordinal)); result.Add(section); continue;
            }
            var parts = line.Split(" = ", 2, StringSplitOptions.None);
            if (parts.Length != 2 || section is null) throw new InvalidDataException("unsupported crypto fixture line: " + line);
            var value = parts[1];
            if (value.StartsWith('"') && value.EndsWith('"') && !value.Contains('\\')) value = value[1..^1];
            else if (!uint.TryParse(value, NumberStyles.None, CultureInfo.InvariantCulture, out _)) throw new InvalidDataException("unsupported crypto fixture value");
            section.Values.Add(parts[0], value);
        }
        if (result.Count != 8 || result.Count(s => s.Name == "xaes") != 3) throw new InvalidDataException("unexpected crypto fixture coverage");
        return result;
    }
    internal static byte[] Hex(Vector vector, string key) => Convert.FromHexString(vector.Values[key]);
    private static uint Number(Vector vector, string key) => uint.Parse(vector.Values[key], CultureInfo.InvariantCulture);
    private static void Bytes(byte[] expected, byte[] actual, string label) => Corpus.Equal(Convert.ToHexString(expected), Convert.ToHexString(actual), label);
}
