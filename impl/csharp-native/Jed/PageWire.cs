using System.Buffers.Binary;
using System.Text;

namespace Jed;

internal sealed record PageWireMessage(byte Code, uint RequestId, IReadOnlyDictionary<string, object> Fields)
{
    internal T Get<T>(string name) => (T)Fields[name];
}
internal readonly record struct PageWireDigest(uint Index, ulong Digest);
internal readonly record struct PageWirePage(uint Index, byte[] Bytes);
internal sealed record PageWireCredential(byte Kind, byte[] Bytes);
internal sealed record PageOwnerRecord(bool Serving, uint Uid, byte[] Incarnation, uint Pid, string Endpoint);

// Handwritten from spec/pageservice/messages.toml. All lengths and counts are
// validated before allocation; no untrusted field selects a runtime type.
internal static class PageWire
{
    internal const int MaximumFrameLength = 16777216;
    internal const ushort ProtocolVersion = 1;
    internal static readonly UTF8Encoding Utf8 = new(false, true);
    internal readonly record struct Definition(byte Code, string Name, string Fields);
    internal static readonly Definition[] Definitions =
    [
        new(0x01, "hello", "protocol_min:u16 protocol_max:u16 format_version:u16 flags:u8 client_name:string credential:credential"),
        new(0x02, "begin_read", "incarnation:id16 last_txid:u64"),
        new(0x03, "read_pages", "txid:u64 indices:u32_list"),
        new(0x04, "end_read", "txid:u64"),
        new(0x05, "begin_write", "incarnation:id16 last_txid:u64 lock_timeout_ms:u64 flags:u8"),
        new(0x06, "commit_begin", "txid:u64 meta:page manifest_pages:indexed_page_list"),
        new(0x07, "commit_pages", "pages:indexed_page_list"),
        new(0x08, "commit_end", ""), new(0x09, "abort", ""),
        new(0x0a, "commit_status", "txid:u64 digest:u64"),
        new(0x0b, "request_ownership", ""), new(0x0c, "snapshot_stream", ""), new(0x0d, "subscribe", ""),
        new(0x41, "hello_ok", "protocol:u16 format_version:u16 page_size:u32 connection_id:u64 flags:u8 max_read_pages:u32"),
        new(0x42, "read_begun", "incarnation:id16 txid:u64 invalidate:u8 indices:u32_list meta:page"),
        new(0x43, "pages", "pages:page_list"),
        new(0x45, "write_begun", "incarnation:id16 txid:u64 invalidate:u8 indices:u32_list reuse_grant:bool meta:page pending:digest_list reuse:u32_list"),
        new(0x48, "committed", "txid:u64"), new(0x4a, "commit_status_reply", "status:u8"),
        new(0x4b, "ownership_reply", "status:u8"), new(0x7e, "notice", "kind:u8 connection_id:u64"),
        new(0x7f, "error", "sqlstate:sqlstate message:string detail:string flags:u8")
    ];
    internal static Definition Find(byte code)
    {
        foreach (var definition in Definitions) if (definition.Code == code && code is not (0x0c or 0x0d)) return definition;
        throw Invalid("unknown message code");
    }
    internal static SqlException Invalid(string reason) => new(SqlStates.ProtocolViolation, "page-service protocol: " + reason);
    internal static int MaxReadPages(int pageSize) => pageSize > 0 ? Math.Min(1024, (MaximumFrameLength - 9) / pageSize) : throw Invalid("invalid page size");
    private static string[] Fields(Definition definition) => definition.Fields.Split(' ', StringSplitOptions.RemoveEmptyEntries);

    internal static byte[] Encode(PageWireMessage message, int pageSize = 0)
    {
        var definition = Find(message.Code);
        CheckId(message.Code, message.RequestId);
        var fields = Fields(definition);
        if (message.Fields.Count != fields.Length) throw Invalid("unexpected field count");
        long size = 5;
        foreach (var field in fields)
        {
            var parts = field.Split(':');
            if (!message.Fields.TryGetValue(parts[0], out var value)) throw Invalid("missing field");
            size += Size(parts[1], value, pageSize);
            if (size > MaximumFrameLength) throw Invalid("frame too large");
        }
        var bytes = new byte[(int)size + 4];
        var writer = new Writer(bytes);
        writer.U32((uint)size); writer.U8(message.Code); writer.U32(message.RequestId);
        foreach (var field in fields)
        {
            var parts = field.Split(':');
            writer.Field(parts[1], message.Fields[parts[0]], pageSize);
        }
        return bytes;
    }

    internal static PageWireMessage Decode(ReadOnlySpan<byte> bytes, int pageSize = 0)
    {
        if (bytes.Length < 4) throw Invalid("short frame prefix");
        var length = BinaryPrimitives.ReadUInt32BigEndian(bytes);
        ValidateLength(length);
        if (bytes.Length != length + 4) throw Invalid("frame length mismatch");
        var reader = new Reader(bytes[4..]);
        var code = reader.U8(); var requestId = reader.U32();
        var definition = Find(code); CheckId(code, requestId);
        var values = new Dictionary<string, object>(StringComparer.Ordinal);
        foreach (var field in Fields(definition))
        {
            var parts = field.Split(':');
            values.Add(parts[0], reader.Field(parts[1], pageSize));
        }
        reader.End();
        return new(code, requestId, values);
    }

    internal static async ValueTask<PageWireMessage?> ReadAsync(Stream stream, int pageSize = 0, CancellationToken cancellationToken = default)
    {
        var prefix = new byte[4];
        var count = await stream.ReadAsync(prefix.AsMemory(0, 1), cancellationToken).ConfigureAwait(false);
        if (count == 0) return null;
        await stream.ReadExactlyAsync(prefix.AsMemory(1), cancellationToken).ConfigureAwait(false);
        var length = BinaryPrimitives.ReadUInt32BigEndian(prefix);
        ValidateLength(length); // reject a hostile size before reading/allocating the body
        var bytes = new byte[(int)length + 4]; prefix.CopyTo(bytes, 0);
        try { await stream.ReadExactlyAsync(bytes.AsMemory(4), cancellationToken).ConfigureAwait(false); return Decode(bytes, pageSize); }
        finally { if (bytes[4] == 1) System.Security.Cryptography.CryptographicOperations.ZeroMemory(bytes); }
    }
    internal static async ValueTask WriteAsync(Stream stream, PageWireMessage message, int pageSize = 0, CancellationToken cancellationToken = default)
    {
        var bytes = Encode(message, pageSize);
        try { await stream.WriteAsync(bytes, cancellationToken).ConfigureAwait(false); }
        finally { if (message.Code == 1) System.Security.Cryptography.CryptographicOperations.ZeroMemory(bytes); }
    }
    private static void ValidateLength(uint length)
    {
        if (length is < 5 or > MaximumFrameLength) throw Invalid("invalid frame length");
    }
    private static void CheckId(byte code, uint requestId)
    {
        if (code == 0x7e ? requestId != 0 : requestId == 0 && code != 0x7f) throw Invalid("invalid request id");
    }

    internal static byte[] EncodeOwner(PageOwnerRecord owner)
    {
        if (owner.Incarnation.Length != 16 || (owner.Serving ? !Path.IsPathFullyQualified(owner.Endpoint) : owner.Endpoint.Length != 0))
            throw Invalid("invalid owner record");
        int endpointSize;
        try { endpointSize = Utf8.GetByteCount(owner.Endpoint); }
        catch (EncoderFallbackException) { throw Invalid("invalid UTF-8 owner endpoint"); }
        if (endpointSize > 100) throw Invalid("owner endpoint too long");
        var bytes = new byte[43 + endpointSize];
        var writer = new Writer(bytes); writer.Raw("JEDOWNER"u8); writer.U16(1); writer.U8(owner.Serving ? (byte)1 : (byte)0);
        writer.U32(owner.Uid); writer.Raw(owner.Incarnation); writer.U32(owner.Pid); writer.String(owner.Endpoint);
        writer.U32(StorageFormat.Crc32(bytes.AsSpan(0, bytes.Length - 4)));
        return bytes;
    }
    internal static PageOwnerRecord? DecodeOwner(ReadOnlySpan<byte> bytes)
    {
        try
        {
            if (bytes.Length < 43 || !bytes[..8].SequenceEqual("JEDOWNER"u8) ||
                BinaryPrimitives.ReadUInt32BigEndian(bytes[^4..]) != StorageFormat.Crc32(bytes[..^4])) return null;
            var reader = new Reader(bytes[8..^4]);
            if (reader.U16() != 1) return null;
            var flags = reader.U8(); if (flags > 1) return null;
            var uid = reader.U32(); var incarnation = reader.Raw(16).ToArray(); var pid = reader.U32(); var endpoint = reader.String(); reader.End();
            if (Utf8.GetByteCount(endpoint) > 100 || (flags == 1 ? !Path.IsPathFullyQualified(endpoint) : endpoint.Length != 0)) return null;
            return new(flags == 1, uid, incarnation, pid, endpoint);
        }
        catch (SqlException error) when (error.SqlState == SqlStates.ProtocolViolation) { return null; }
    }

    private static long Size(string kind, object value, int pageSize)
    {
        switch (kind)
        {
            case "u8": return 1; case "u16": return 2; case "u32": return 4; case "u64": return 8; case "bool": return 1;
            case "string":
                try { return 4L + Utf8.GetByteCount((string)value); }
                catch (EncoderFallbackException) { throw Invalid("invalid UTF-8 string"); }
            case "sqlstate":
                if (((string)value).Length != 5 || ((string)value).Any(c => c is not (>= '0' and <= '9') and not (>= 'A' and <= 'Z'))) throw Invalid("invalid SQLSTATE");
                return 5;
            case "id16": if (((byte[])value).Length != 16) throw Invalid("invalid incarnation"); return 16;
            case "page": if (pageSize <= 0 || ((byte[])value).Length != pageSize) throw Invalid("invalid page size"); return pageSize;
            case "u32_list": return 4L + ((uint[])value).Length * 4L;
            case "digest_list": return 4L + ((PageWireDigest[])value).Length * 12L;
            case "page_list":
                foreach (var page in (byte[][])value) _ = Size("page", page, pageSize);
                return 4L + ((byte[][])value).Length * (long)pageSize;
            case "indexed_page_list":
                foreach (var page in (PageWirePage[])value) _ = Size("page", page.Bytes, pageSize);
                return 4L + ((PageWirePage[])value).Length * (4L + pageSize);
            case "credential":
                var credential = (PageWireCredential)value;
                if (credential.Kind == 0 && credential.Bytes.Length == 0) return 1;
                if (credential.Kind == 2 && credential.Bytes.Length == 32) return 33;
                if (credential.Kind == 1)
                {
                    try { _ = Utf8.GetCharCount(credential.Bytes); } catch (DecoderFallbackException) { throw Invalid("invalid UTF-8 passphrase"); }
                    return 5L + credential.Bytes.Length;
                }
                throw Invalid("invalid credential");
            default: throw new InvalidOperationException("unknown wire field");
        }
    }

    private ref struct Reader(ReadOnlySpan<byte> bytes)
    {
        private ReadOnlySpan<byte> remaining = bytes;
        internal ReadOnlySpan<byte> Raw(int count)
        {
            if (count < 0 || count > remaining.Length) throw Invalid("short field");
            var result = remaining[..count]; remaining = remaining[count..]; return result;
        }
        internal byte U8() => Raw(1)[0];
        internal ushort U16() => BinaryPrimitives.ReadUInt16BigEndian(Raw(2));
        internal uint U32() => BinaryPrimitives.ReadUInt32BigEndian(Raw(4));
        internal ulong U64() => BinaryPrimitives.ReadUInt64BigEndian(Raw(8));
        internal string String()
        {
            var count = Count(1); var raw = Raw(count);
            try { return Utf8.GetString(raw); } catch (DecoderFallbackException) { throw Invalid("invalid UTF-8 string"); }
        }
        internal int Count(int itemSize)
        {
            var count = U32();
            if (itemSize <= 0 || count > remaining.Length / itemSize) throw Invalid("invalid list length");
            return (int)count;
        }
        internal void End() { if (!remaining.IsEmpty) throw Invalid("trailing body bytes"); }
        internal object Field(string kind, int pageSize)
        {
            switch (kind)
            {
                case "u8": return U8(); case "u16": return U16(); case "u32": return U32(); case "u64": return U64();
                case "bool": return U8() switch { 0 => false, 1 => true, _ => throw Invalid("invalid bool") };
                case "string": return String();
                case "id16": return Raw(16).ToArray();
                case "sqlstate":
                    var state = Raw(5);
                    foreach (var c in state) if (c is not (>= (byte)'0' and <= (byte)'9') and not (>= (byte)'A' and <= (byte)'Z')) throw Invalid("invalid SQLSTATE");
                    return Encoding.ASCII.GetString(state);
                case "page": if (pageSize <= 0) throw Invalid("page size not negotiated"); return Raw(pageSize).ToArray();
                case "u32_list":
                    var integers = new uint[Count(4)]; for (var i = 0; i < integers.Length; i++) integers[i] = U32(); return integers;
                case "digest_list":
                    var digests = new PageWireDigest[Count(12)]; for (var i = 0; i < digests.Length; i++) digests[i] = new(U32(), U64()); return digests;
                case "page_list":
                    var pages = new byte[Count(pageSize)][]; for (var i = 0; i < pages.Length; i++) pages[i] = Raw(pageSize).ToArray(); return pages;
                case "indexed_page_list":
                    if (pageSize <= 0 || pageSize > MaximumFrameLength) throw Invalid("page size not negotiated");
                    var indexed = new PageWirePage[Count(pageSize + 4)]; for (var i = 0; i < indexed.Length; i++) indexed[i] = new(U32(), Raw(pageSize).ToArray()); return indexed;
                case "credential":
                    var credentialKind = U8();
                    if (credentialKind == 0) return new PageWireCredential(0, []);
                    if (credentialKind == 2) return new PageWireCredential(2, Raw(32).ToArray());
                    if (credentialKind != 1) throw Invalid("invalid credential kind");
                    var secret = Raw(Count(1));
                    try { _ = Utf8.GetCharCount(secret); } catch (DecoderFallbackException) { throw Invalid("invalid UTF-8 passphrase"); }
                    return new PageWireCredential(1, secret.ToArray());
                default: throw new InvalidOperationException("unknown wire field");
            }
        }
    }
    private ref struct Writer(Span<byte> bytes)
    {
        private Span<byte> remaining = bytes;
        internal void Raw(ReadOnlySpan<byte> value) { value.CopyTo(remaining); remaining = remaining[value.Length..]; }
        internal void U8(byte value) { remaining[0] = value; remaining = remaining[1..]; }
        internal void U16(ushort value) { BinaryPrimitives.WriteUInt16BigEndian(remaining, value); remaining = remaining[2..]; }
        internal void U32(uint value) { BinaryPrimitives.WriteUInt32BigEndian(remaining, value); remaining = remaining[4..]; }
        internal void U64(ulong value) { BinaryPrimitives.WriteUInt64BigEndian(remaining, value); remaining = remaining[8..]; }
        internal void String(string value) { var size = Utf8.GetByteCount(value); U32((uint)size); Utf8.GetBytes(value, remaining); remaining = remaining[size..]; }
        internal void Field(string kind, object value, int pageSize)
        {
            switch (kind)
            {
                case "u8": U8((byte)value); break; case "u16": U16((ushort)value); break; case "u32": U32((uint)value); break; case "u64": U64((ulong)value); break;
                case "bool": U8((bool)value ? (byte)1 : (byte)0); break;
                case "string": String((string)value); break;
                case "sqlstate": foreach (var c in (string)value) U8((byte)c); break;
                case "id16": case "page": Raw((byte[])value); break;
                case "u32_list": var integers = (uint[])value; U32((uint)integers.Length); foreach (var item in integers) U32(item); break;
                case "digest_list": var digests = (PageWireDigest[])value; U32((uint)digests.Length); foreach (var item in digests) { U32(item.Index); U64(item.Digest); } break;
                case "page_list": var pages = (byte[][])value; U32((uint)pages.Length); foreach (var page in pages) Raw(page); break;
                case "indexed_page_list": var indexed = (PageWirePage[])value; U32((uint)indexed.Length); foreach (var page in indexed) { U32(page.Index); Raw(page.Bytes); } break;
                case "credential": var credential = (PageWireCredential)value; U8(credential.Kind); if (credential.Kind == 1) U32((uint)credential.Bytes.Length); Raw(credential.Bytes); break;
                default: throw new InvalidOperationException("unknown wire field");
            }
        }
    }
}
