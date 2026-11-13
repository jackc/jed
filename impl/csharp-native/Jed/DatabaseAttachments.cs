namespace Jed;

internal sealed record AttachedDatabase(string Name, Database Database, bool ReadOnly);

public sealed partial class Database
{
    private readonly Dictionary<string, AttachedDatabase> attachments = new(StringComparer.Ordinal);
    private readonly HashSet<string> attaching = new(StringComparer.Ordinal);
    private int domainLeases;
    private bool attachmentsClosing;
    private static long domainIdentity;
    private readonly long domainOrder = Interlocked.Increment(ref domainIdentity);
    internal string DomainLockOrder => fileState?.Path ?? "\uffff" + domainOrder.ToString("D20", System.Globalization.CultureInfo.InvariantCulture);
    internal bool DomainReadOnly => fileState?.Options.ReadOnly == true;
    internal bool HasAttachments { get { lock (gate) return attachments.Count != 0; } }
    public IReadOnlyList<string> AttachedDatabases { get { lock (gate) { Check(); return attachments.Keys.Order(Utf8NameComparer.Instance).ToArray(); } } }
    private void ReserveAttachment(string name)
    {
        ArgumentException.ThrowIfNullOrEmpty(name);
        if (name.IndexOf('\0') >= 0) throw new SqlException(SqlStates.InvalidName, "attachment name contains NUL");
        if (TextEncoding.ByteCount(name) > 63) throw new SqlException(SqlStates.NameTooLong, "attachment name exceeds 63 bytes");
        lock (gate)
        {
            Check();
            if (attachmentsClosing) throw new SqlException(SqlStates.ObjectInUse, "database is closing");
            if (name is "main" or "temp" || attachments.ContainsKey(name) || !attaching.Add(name))
                throw new SqlException(SqlStates.DuplicateObject, "attachment name already exists or is reserved");
        }
    }
    /// <summary>Attach a new private memory database, visible to this handle's sessions under an exact name.</summary>
    public async Task AttachInMemoryAsync(string name, bool readOnly = false, CancellationToken cancellationToken = default)
    {
        ReserveAttachment(name); Database? created = null;
        try
        {
            created = await CreateInMemoryAsync(cancellationToken: cancellationToken).ConfigureAwait(false);
            lock (gate) { Check(); if (attachmentsClosing) throw new SqlException(SqlStates.ObjectInUse, "database is closing"); attachments.Add(name, new(name, created, readOnly)); created = null; }
        }
        finally
        {
            lock (gate) attaching.Remove(name);
            if (created is not null) await created.DisposeAsync().ConfigureAwait(false);
        }
    }
    /// <summary>Attach an existing file through the ordinary file-owner/page-client host seam.</summary>
    public async Task AttachFileAsync(string name, string path, DatabaseFileOptions? options = null, CancellationToken cancellationToken = default)
    {
        ReserveAttachment(name); Database? opened = null;
        try
        {
            if (ScratchHost?.Encrypted == true) options = (options ?? new()) with { ScratchEncryption = ScratchEncryption.On };
            opened = await OpenFileAsync(path, options, cancellationToken).ConfigureAwait(false);
            lock (gate)
            {
                Check();
                if (attachmentsClosing) throw new SqlException(SqlStates.ObjectInUse, "database is closing");
                if (opened.ScratchHost?.Encrypted == true && ScratchHost is { Encrypted: false })
                    throw new SqlException(SqlStates.ObjectNotInPrerequisiteState, "encrypted attachment requires encrypted scratch storage");
                if (DomainLockOrder == opened.DomainLockOrder || attachments.Values.Any(item => item.Database.DomainLockOrder == opened.DomainLockOrder))
                    throw new SqlException(SqlStates.DuplicateObject, "file is already attached");
                attachments.Add(name, new(name, opened, options?.ReadOnly == true)); opened = null;
            }
        }
        finally
        {
            lock (gate) attaching.Remove(name);
            if (opened is not null) await opened.DisposeAsync().ConfigureAwait(false);
        }
    }
    /// <summary>Detach an idle attachment. Live transactions and cursors keep it in use.</summary>
    public async Task DetachAsync(string name)
    {
        AttachedDatabase attached;
        lock (gate)
        {
            Check();
            if (name is "main" or "temp") throw new SqlException(SqlStates.InvalidParameterValue, "cannot detach a built-in domain");
            if (!attachments.TryGetValue(name, out attached!)) throw new SqlException(SqlStates.UndefinedObject, "database is not attached");
            if (domainLeases != 0) throw new SqlException(SqlStates.ObjectInUse, "attachment has live transactions or cursors");
            attachments.Remove(name); attaching.Add(name);
        }
        try { await attached.Database.DisposeAsync().ConfigureAwait(false); }
        catch { lock (gate) attachments.Add(name, attached); throw; }
        finally { lock (gate) attaching.Remove(name); }
    }
    internal AttachedDatabase[] AcquireAttachmentRegistry()
    {
        lock (gate)
        {
            Check(); if (attachmentsClosing || attaching.Count != 0) throw new SqlException(SqlStates.ObjectInUse, "attachment registry is changing");
            domainLeases++; return attachments.Values.OrderBy(item => item.Name, Utf8NameComparer.Instance).ToArray();
        }
    }
    internal void ReleaseAttachmentRegistry() { lock (gate) domainLeases--; }
    private async ValueTask DisposeAttachedAsync()
    {
        AttachedDatabase[] owned;
        lock (gate)
        {
            if (disposed || attachments.Count == 0) return;
            if (domainLeases != 0 || attaching.Count != 0 || pins.Count != 0 || writer.CurrentCount == 0)
                throw new SqlException(SqlStates.ObjectInUse, "database has live domain operations");
            attachmentsClosing = true; owned = attachments.Values.OrderBy(item => item.Name, Utf8NameComparer.Instance).ToArray();
        }
        foreach (var attachment in owned) await attachment.Database.DisposeAsync().ConfigureAwait(false);
        lock (gate) attachments.Clear();
    }
}
