using System.Buffers.Binary;
using System.Diagnostics;
using System.Security.Cryptography;

namespace Jed;

public enum FileLocking { Auto, Exclusive, None }
public enum FileOwnership { Prefer, Yield, Never }
public enum Synchronous { On, Off }
public record DatabaseFileOptions
{
    public DatabaseEnvironment? Environment { get; init; }
    private Func<int, byte[]>? keyEntropy;
    internal DatabaseFileOptions WithEnvironment()
    {
        var environment = Environment ?? Jed.DatabaseEnvironment.Builder().Build();
        return this with { Environment = environment, KeyEntropy = keyEntropy ?? environment.NewKeyEntropy(), Clock = Clock ?? environment.ClockSource.NowMicroseconds };
    }
    internal IFileHost FileHost { get; init; } = NativeFileHost.Instance;
    internal IFileRoleHost? RoleHost { get; init; }
    internal IScratchDeviceHost? ScratchDevices { get; init; }
    internal bool EnableIdleFlush { get; init; } = true;
    public Synchronous Synchronous { get; init; } = Synchronous.On;
    public long FlushIntervalMilliseconds { get; init; } = 1000;
    /// <summary>Host clock in Unix microseconds, also used by sessions without a clock override.</summary>
    internal Func<long>? Clock { get; init; }
    public long WorkMem { get; init; }
    public long MaxStorageBytes { get; init; }
    public long CacheBytes { get; init; } = 256L * 1024 * 1024;
    public FileLocking Locking { get; init; } = FileLocking.Auto;
    public FileOwnership Ownership { get; init; } = FileOwnership.Prefer;
    public bool ReadOnly { get; init; }
    public long FileLockTimeoutMilliseconds { get; init; } = 5000;
    public long WriterLockTimeoutMilliseconds { get; init; }
    public long YieldGraceMilliseconds { get; init; } = 5000;
    public long ShutdownGraceMilliseconds { get; init; } = 30000;
    public long IdleSessionMilliseconds { get; init; }
    public long IdleTransactionMilliseconds { get; init; } = 60000;
    public int MaximumConnections { get; init; }
    internal Func<int, byte[]> Entropy { get; init; } = RandomNumberGenerator.GetBytes;
    internal Func<int, byte[]> KeyEntropy { get => keyEntropy ?? RandomNumberGenerator.GetBytes; init => keyEntropy = value; }
    public DatabaseCredential? Credential { get; init; }
    public ScratchEncryption ScratchEncryption { get; init; } = ScratchEncryption.Auto;
    internal Func<int, byte[]> ScratchEntropy { get; init; } = RandomNumberGenerator.GetBytes;
}

public sealed partial class Database
{
    private sealed class FileState(FileClaim? claim, string path, DatabaseFileOptions options)
    {
        internal readonly FileClaim? Claim = claim;
        internal readonly int ProcessId = System.Environment.ProcessId;
        internal readonly string Path = path;
        internal DatabaseFileOptions Options = options;
        internal readonly SemaphoreSlim RoleGate = new(1);
        internal readonly SemaphoreSlim AdoptionGate = new(1);
        internal readonly SemaphoreSlim Publication = new(1);
        internal CancellationTokenSource RoleEnded = new();
        internal FileLockBundle? Bundle;
        internal IBlockStore? LocalStore;
        internal PageClientConnection? Client;
        internal Func<CancellationToken, ValueTask<PageClientConnection>>? Reconnect;
        internal PageWireCredential? RemoteCredential;
        internal PageOwnerServer? Server;
        internal PageOwnerLedger? Ledger;
        internal string? Endpoint;
        internal long Role = 1;
        internal bool CatalogDirty;
        internal SqlException? Failure;
        internal Task? Handoff = null;
        internal Task? IdleFlush = null;
        internal TaskCompletionSource? Admission = null;
        internal long PauseUntil;
        internal bool PauseAfterDisconnect;
        internal bool NeedsElection;
        internal bool Closing;
        internal bool RepairKeys;
        internal SemaphoreSlim? RemoteWriterGate;
        internal bool DirectoryPending;
        internal ulong HandoffRequester;
    }
    private FileState? fileState;
    internal bool HasPageClient => fileState?.Client is not null;
    internal string FileRole => fileState is not { Options.Locking: FileLocking.Auto } ? "none" : fileState.Client is not null || fileState.NeedsElection ? "client" : "owner";
    internal async ValueTask<string> GetFileRoleAsync(CancellationToken token = default)
    { if (fileState is { Options.Ownership: not FileOwnership.Never }) await EnsureFileRoleAsync(token).ConfigureAwait(false); return FileRole; }
    internal static Task<Database> OpenFileAsync(string path, DatabaseFileOptions? options = null, CancellationToken cancellationToken = default) =>
        OpenFileCoreAsync(path, options ?? new(), false, 8192, cancellationToken);
    internal static Task<Database> CreateFileAsync(string path, DatabaseFileOptions? options = null, int pageSize = 8192, CancellationToken cancellationToken = default) =>
        OpenFileCoreAsync(path, options ?? new(), true, pageSize, cancellationToken);

    private static async Task<Database> OpenFileCoreAsync(string path, DatabaseFileOptions options, bool creating, int pageSize, CancellationToken token, FileClaim? preparedClaim = null, FileLockBundle? preparedBundle = null)
    {
        options = options.WithEnvironment();
        ArgumentNullException.ThrowIfNull(path); ArgumentNullException.ThrowIfNull(options.FileHost); ArgumentNullException.ThrowIfNull(options.Entropy); ArgumentNullException.ThrowIfNull(options.KeyEntropy); ArgumentNullException.ThrowIfNull(options.ScratchEntropy);
        if (!Enum.IsDefined(options.Synchronous) || options.FlushIntervalMilliseconds < 0 || options.MaxStorageBytes < 0 || options.WorkMem < 0 || options.CacheBytes < 0 || !Enum.IsDefined(options.ScratchEncryption) || !Enum.IsDefined(options.Locking) || !Enum.IsDefined(options.Ownership) || options.FileLockTimeoutMilliseconds < 0 || options.WriterLockTimeoutMilliseconds < 0 ||
            options.YieldGraceMilliseconds < 0 || options.ShutdownGraceMilliseconds < 0 || options.IdleSessionMilliseconds < 0 || options.IdleTransactionMilliseconds < 0 || options.MaximumConnections < 0)
            throw new ArgumentOutOfRangeException(nameof(options));
        if (options.FileHost is not NativeFileHost && options.Locking != FileLocking.None && options.RoleHost is null) throw new ArgumentException("injected file hosts require an explicit host-owned locking adapter", nameof(options));
        if (creating && options.ReadOnly) throw new SqlException(SqlStates.ReadOnlySqlTransaction, "cannot create a read-only database");
        if (!StorageFormat.ValidPageSize(pageSize)) throw new ArgumentOutOfRangeException(nameof(pageSize));
        if (creating && options.Credential is not null && pageSize < 1024) throw new SqlException(SqlStates.FeatureNotSupported, "encrypted page size must be at least 1024");
        FileClaim? claim = preparedClaim; FileLockBundle? bundle = preparedBundle; PageClientConnection? client = null; Database? database = null; IBlockStore? openedStore = null; StoragePageCodec? openingCodec = null;
        try
        {
            string normalized;
            if (options.Locking == FileLocking.None || options.RoleHost is not null) normalized = Path.GetFullPath(path);
            else { claim ??= FileClaim.Acquire(path, creating); normalized = claim.Path; }
            if (claim is not null && bundle is null)
            {
                bundle = await FileLockBundle.PrepareAsync(claim, options.FileLockTimeoutMilliseconds, token).ConfigureAwait(false);
                if (creating || options.Locking == FileLocking.Exclusive) await bundle.AcquireExclusiveAsync(options.FileLockTimeoutMilliseconds, token).ConfigureAwait(false);
                else client = await ElectAsync(bundle, options, 0, token).ConfigureAwait(false);
            }
            if (options.RoleHost is { } roleHost) client = await roleHost.ElectAsync(normalized, options.ReadOnly, token).ConfigureAwait(false);
            if (creating && client is not null) throw new SqlException(SqlStates.ObjectInUse, "database already has an owner");
            if (creating) await CreateImageAsync(normalized, pageSize, options, token).ConfigureAwait(false);
            var state = new FileState(claim, normalized, options) { Bundle = bundle, Client = client };
            if (client is null)
            {
                var store = await options.FileHost.OpenAsync(normalized, FileMode.Open, options.ReadOnly, token).ConfigureAwait(false); state.LocalStore = store; openedStore = store;
                var opened = await OpenFileStorageAsync(store, options, null, token).ConfigureAwait(false); openingCodec = opened.Pager.Codec;
                if (creating) opened = opened with { Recovered = opened.Recovered with { LastReclamationLive = 0 } };
                database = new(opened.Pager, opened.Recovered, opened.Catalog, options.Environment) { fileState = state, keyBlock = opened.KeyBlock };
                state.RepairKeys = true;
                await database.StartOwnerAsync(token).ConfigureAwait(false);
            }
            else
            {
                bundle?.Dispose(); state.Bundle = null;
                var read = await client.BeginReadAsync(new byte[16], 0, token).ConfigureAwait(false);
                try
                {
                    var store = new PageClientStore(client, read); openedStore = store;
                    var served = await UnlockClientAsync(store, read.Meta, options, null, false, token).ConfigureAwait(false); openingCodec = served.Codec;
                    var pager = new StoragePager(store, served.Meta.PageSize, options.CacheBytes, served.Codec);
                    var catalog = await ReadServedCatalogAsync(pager, served, options.Environment!, token).ConfigureAwait(false);
                    database = new(pager, new(served.Meta, 0, 0, [], [], []) { Encrypted = served.Encrypted }, catalog, options.Environment) { fileState = state, keyBlock = served.KeyBlock };
                }
                finally { await EndRemoteReadAsync(client, read.TransactionId).ConfigureAwait(false); }
                database.ObserveClient(client);
            }
            database.SetWorkMem(options.WorkMem == 0 ? DefaultWorkMem : options.WorkMem);
            database.SetStorageLimit(options.MaxStorageBytes);
            database.ScratchHost = new(options.ScratchEncryption == ScratchEncryption.On || database.ownedCodec is not null, options.ScratchEntropy, devices: options.ScratchDevices);
            return database;
        }
        catch (Exception error)
        {
            if (database is not null)
            {
                if (database.fileState?.Server is { } server) await server.DisposeAsync().ConfigureAwait(false);
                await database.pager.Store.DisposeAsync().ConfigureAwait(false);
                if (database.fileState?.Endpoint is { } endpoint) bundle?.RemoveEndpoint(endpoint);
            }
            else if (openedStore is not null) await openedStore.DisposeAsync().ConfigureAwait(false);
            openingCodec?.Dispose();
            if (client is not null) await client.DisposeAsync().ConfigureAwait(false);
            bundle?.Dispose(); claim?.Dispose(); options.RoleHost?.Release(Path.GetFullPath(path));
            if (error is FileNotFoundException or DirectoryNotFoundException) throw new SqlException(SqlStates.UndefinedFile, error.Message);
            if (error is IOException or UnauthorizedAccessException) throw new SqlException(SqlStates.IoError, error.Message);
            throw;
        }
    }
    private static async ValueTask<PageClientConnection?> ElectAsync(FileLockBundle bundle, DatabaseFileOptions options, long pauseUntil, CancellationToken token)
    {
        var elapsed = Stopwatch.StartNew();
        while (true)
        {
            token.ThrowIfCancellationRequested();
            if (options.Ownership != FileOwnership.Never && Stopwatch.GetTimestamp() >= pauseUntil && bundle.TryBecomeOwner()) return null;
            var owner = bundle.ReadOwner();
            if (owner is { Serving: true } && owner.Uid == LinuxFileHost.EffectiveUid)
            {
                using var attempt = CancellationTokenSource.CreateLinkedTokenSource(token);
                var remaining = Math.Max(1, options.FileLockTimeoutMilliseconds - elapsed.ElapsedMilliseconds);
                attempt.CancelAfter(TimeSpan.FromMilliseconds(Math.Min(remaining, int.MaxValue)));
                try { return await PageClientConnection.ConnectLocalAsync(owner.Endpoint, options.ReadOnly, attempt.Token).ConfigureAwait(false); }
                catch (SqlException error) when (error.SqlState is SqlStates.FeatureNotSupported or SqlStates.TooManyConnections) { throw; }
                catch (Exception error) when (error is IOException or System.Net.Sockets.SocketException or SqlException or OperationCanceledException) { token.ThrowIfCancellationRequested(); }
            }
            if (elapsed.ElapsedMilliseconds >= options.FileLockTimeoutMilliseconds)
            {
                var detail = owner is null ? "" : owner.Uid != LinuxFileHost.EffectiveUid ? "; owned by another user's process" : !owner.Serving ? "; owned by a non-serving process" : "";
                throw new SqlException(SqlStates.ObjectInUse, "database lock timeout: " + bundle.Claim.Path + detail);
            }
            await Task.Delay((int)Math.Min(10, Math.Max(1, options.FileLockTimeoutMilliseconds - elapsed.ElapsedMilliseconds)), token).ConfigureAwait(false);
        }
    }
    private static async ValueTask CreateImageAsync(string path, int pageSize, DatabaseFileOptions options, CancellationToken token)
    {
        var host = options.FileHost;
        if (await host.ExistsAsync(path, token).ConfigureAwait(false)) throw new SqlException(SqlStates.DuplicateFile, "database already exists: " + path);
        var temporary = path + ".jedtmp";
        // This reserved sibling may survive a failed create. Unlink it before
        // exclusive creation, so a retry cannot follow an old file or symlink.
        var ownsTemporary = false;
        try
        {
            await using (var store = await host.CreateTemporaryAsync(temporary, token).ConfigureAwait(false))
            {
                ownsTemporary = true;
                var pages = options.Credential is null
                    ? new[] { StorageFormat.EncodeMeta(new(pageSize, 1, 2, 3, 0, 1)), StorageFormat.EncodeMeta(new(pageSize, 1, 2, 3, 0, 1)), StorageFormat.EncodePage(pageSize, new(1, 0, 0, 0), []) }
                    : CreateEncryptedImage(pageSize, options);
                for (var page = 0; page < pages.Length; page++) await store.WriteAsync((long)page * pageSize, pages[page], token).ConfigureAwait(false);
                await store.SyncAsync(token).ConfigureAwait(false);
                try { await host.MoveAsync(temporary, path, overwrite: false, token).ConfigureAwait(false); ownsTemporary = false; }
                catch (IOException)
                {
                    if (await host.ExistsAsync(path, token).ConfigureAwait(false)) throw new SqlException(SqlStates.DuplicateFile, "database already exists: " + path);
                    throw;
                }
                await host.SyncDirectoryAsync(path, token).ConfigureAwait(false);
            }
        }
        finally
        {
            if (ownsTemporary) { try { await host.DeleteAsync(temporary).ConfigureAwait(false); } catch (IOException) { } catch (UnauthorizedAccessException) { } }
        }
    }
    private static byte[] Draw(DatabaseFileOptions options, int count)
    { var bytes = options.Entropy(count); return bytes is not null && bytes.Length == count ? bytes : throw new IOException("host entropy returned the wrong length"); }
    private static StorageMeta ParseServedMeta(byte[] raw)
    {
        if (raw.Length > 6 && raw[6] != 0) throw new SqlException(SqlStates.CredentialRequired, "encrypted file requires a credential");
        return StorageFormat.ParseMeta(raw, long.MaxValue) ?? throw StorageFormat.Corrupt("invalid served meta");
    }
    private async ValueTask StartOwnerAsync(CancellationToken token)
    {
        var state = fileState!;
        if (pager.Store is not FileRoleStore)
        {
            var ownerRole = state.Role; var recovered = committer.Current;
            pager = new(new FileRoleStore(pager.Store, () => CheckRole(ownerRole)), recovered.Meta.PageSize, state.Options.CacheBytes, ownedCodec);
            committer = new(pager, recovered);
        }
        // Opening only adopts a readable generation. A pending rename and an
        // adopted incremental generation become durable at the first new write.
        state.DirectoryPending = true;
        if (state.RepairKeys && !state.Options.ReadOnly && keyBlock is { NeedsRepair: true } repair)
        {
            await SyncOwnerDirectoryAsync(token).ConfigureAwait(false);
            await pager.Store.SyncAsync(token).ConfigureAwait(false);
            await pager.Store.WriteAsync((long)(repair.Page == 2 ? 3 : 2) * pager.PageSize, repair.Block.Bytes, token).ConfigureAwait(false);
            await pager.Store.SyncAsync(token).ConfigureAwait(false);
            keyBlock = repair with { Page = 2, NeedsRepair = false };
            committer.Adopt(committer.Current, stabilized: true);
        }
        state.RepairKeys = false;
        var meta = new byte[pager.PageSize]; await pager.Store.ReadAsync((long)committer.Current.Slot * pager.PageSize, meta, token).ConfigureAwait(false);
        var previous = new byte[pager.PageSize]; await pager.Store.ReadAsync((long)(1 - committer.Current.Slot) * pager.PageSize, previous, token).ConfigureAwait(false);
        var physicalLength = await pager.Store.GetLengthAsync(token).ConfigureAwait(false);
        if (ownedCodec is null)
        {
            var previousMeta = StorageFormat.ParseMeta(previous, physicalLength);
            if (previousMeta is null || await StorageRecovery.ValidateManifestAsync(pager.Store, previousMeta, previous, token).ConfigureAwait(false) is null) previous = null!;
        }
        else
        {
            var prior = StorageCryptoMeta.Parse(previous, physicalLength);
            var manifest = prior is null ? null : await StorageCryptoRecovery.ValidateManifestAsync(pager.Store, prior.Header, previous, token).ConfigureAwait(false);
            if (manifest is null) previous = null!;
            else
            {
                try { ownedCodec.AuthenticateMeta((uint)(1 - committer.Current.Slot), previous, manifest.Pages); }
                catch (SqlException error) when (error.SqlState == SqlStates.DataCorrupted) { previous = null!; }
            }
        }
        state.Ledger = new(gate, Draw(state.Options, 16), meta, previous);
        if (state.Bundle is null && state.Options.RoleHost is null) return;
        if (state.Bundle is not null)
        {
            state.Endpoint = state.Options.Locking == FileLocking.Auto ? state.Bundle.SelectEndpoint(state.Ledger.Incarnation) : null;
            var serving = state.Endpoint is not null;
            if (serving) state.Bundle.RemoveEndpoint(state.Endpoint!);
            await state.Bundle.PublishOwnerAsync(new(serving, LinuxFileHost.EffectiveUid, state.Ledger.Incarnation, (uint)System.Environment.ProcessId, state.Endpoint ?? ""), token).ConfigureAwait(false);
            if (!serving) return;
        }
        var role = state.Role;
        var backend = new PageOwnerBackend(state.Ledger,
            async (index, cancellation) =>
            {
                CheckRole(role); var bytes = new byte[pager.PageSize]; await pager.Store.ReadAsync((long)index * bytes.Length, bytes, cancellation).ConfigureAwait(false); return bytes;
            },
            (relaxed, cancellation) => BeginRemoteWriterAsync(role, relaxed, cancellation),
            async cancellation => { if (state.Admission is { } admission) await admission.Task.WaitAsync(cancellation).ConfigureAwait(false); CheckRole(role); },
            (requester, cancellation) => RequestHandoffAsync(requester, cancellation),
            requester => { lock (gate) state.Handoff ??= YieldOwnershipAsync(requester); },
            (incarnation, version, cancellation) => AcquireRemoteReadAsync(role, incarnation, version, cancellation));
        state.Server = new(backend, new(state.Options.ReadOnly, state.Options.Ownership == FileOwnership.Yield, state.Options.MaximumConnections,
            state.Options.IdleSessionMilliseconds, state.Options.IdleTransactionMilliseconds));
        if (state.Options.RoleHost is { } host) host.Publish(state.Path, this, state.Server);
        else state.Server.Listen(state.Endpoint!);
    }
    private async ValueTask SyncOwnerDirectoryAsync(CancellationToken token)
    {
        var state = fileState!;
        if (!state.DirectoryPending) return;
        await state.Options.FileHost.SyncDirectoryAsync(state.Path, token).ConfigureAwait(false);
        state.DirectoryPending = false;
    }
    private async ValueTask PrepareOwnerWriteAsync(CancellationToken token)
    {
        try
        {
            await SyncOwnerDirectoryAsync(token).ConfigureAwait(false);
            await committer.StabilizeAsync(token).ConfigureAwait(false);
        }
        catch (Exception error) { PoisonFile(error); throw; }
    }
    private async ValueTask<PageOwnerLedger.PageOwnerPin> AcquireRemoteReadAsync(long role, byte[] incarnation, ulong version, CancellationToken token)
    {
        while (true)
        {
            token.ThrowIfCancellationRequested(); Task wait;
            lock (gate)
            {
                CheckRole(role);
                if (fileState!.Admission is not { } admission) return fileState.Ledger!.Pin(incarnation, version);
                wait = admission.Task;
            }
            await wait.WaitAsync(token).ConfigureAwait(false);
        }
    }
    private void ObserveClient(PageClientConnection client)
    {
        var state = fileState!;
        client.Notice += (kind, requester) =>
        {
            if (kind == 1 && requester != client.ConnectionId) state.PauseAfterDisconnect = true;
            else if (kind == 2) { state.PauseAfterDisconnect = false; state.PauseUntil = 0; }
        };
        if (state.Reconnect is null && state.Options.Ownership == FileOwnership.Prefer && client.OwnerYields) _ = RequestPreferredOwnershipAsync(client);
    }
    private static async Task RequestPreferredOwnershipAsync(PageClientConnection client)
    { try { _ = await client.RequestOwnershipAsync().ConfigureAwait(false); } catch (SqlException) { } }
    private void CheckRole(long role)
    {
        lock (gate)
        {
            Check(); fileState!.Claim?.VerifyProcess();
            if (System.Environment.ProcessId != fileState.ProcessId) throw new SqlException(SqlStates.ObjectInUse, "database inherited across fork; reopen in child");
            if (fileState.Role != role) throw new SqlException(SqlStates.ConnectionFailure, "database role changed during transaction");
        }
    }
    internal void CheckPendingRole(PendingDatabase pending)
    {
        if (fileState is not null)
        {
            CheckRole(pending.Role);
            if (fileState.Closing) throw new SqlException(SqlStates.ConnectionFailure, "database is closing");
            if (pending.FileWrite is { Connection.IsConnected: false }) throw new SqlException(SqlStates.ConnectionFailure, "page-service transaction connection lost");
        }
    }
    private async ValueTask EnsureFileRoleAsync(CancellationToken token)
    {
        var state = fileState!;
        lock (gate) Check();
        if (!state.NeedsElection && (state.Client is null || state.Client.IsConnected)) return;
        await state.RoleGate.WaitAsync(token).ConfigureAwait(false);
        try
        {
            lock (gate) Check();
            if (!state.NeedsElection && (state.Client is null || state.Client.IsConnected)) return;
            if (state.Client is not null) await state.Client.DisposeAsync().ConfigureAwait(false);
            if (state.PauseAfterDisconnect) { state.PauseUntil = Stopwatch.GetTimestamp() + Stopwatch.Frequency * 2; state.PauseAfterDisconnect = false; }
            if (state.Reconnect is { } reconnect)
            {
                var remote = await reconnect(token).ConfigureAwait(false);
                lock (gate) { EndFileRole(); state.Client = remote; state.NeedsElection = false; }
                ObserveClient(remote); return;
            }
            if (state.Options.RoleHost is { } roleHost)
            {
                var elected = await roleHost.ElectAsync(state.Path, state.Options.ReadOnly, token).ConfigureAwait(false);
                lock (gate) { EndFileRole(); state.Client = elected; state.Ledger = null; state.NeedsElection = false; }
                if (elected is null)
                {
                    await AdoptElectedOwnerAsync(token).ConfigureAwait(false);
                }
                else ObserveClient(elected);
                return;
            }
            if (state.Claim is null) throw new SqlException(SqlStates.ConnectionFailure, "uncoordinated database has no role election");
            var bundle = await FileLockBundle.PrepareAsync(state.Claim, state.Options.FileLockTimeoutMilliseconds, token).ConfigureAwait(false);
            PageClientConnection? connection;
            try { connection = await ElectAsync(bundle, state.Options, state.PauseUntil, token).ConfigureAwait(false); }
            catch { bundle.Dispose(); throw; }
            lock (gate) { EndFileRole(); state.Client = connection; state.Bundle = connection is null ? bundle : null; state.Ledger = null; state.NeedsElection = false; }
            if (connection is null)
            {
                await AdoptElectedOwnerAsync(token).ConfigureAwait(false);
            }
            else { bundle.Dispose(); ObserveClient(connection); }
        }
        catch (SqlException error) when (error.SqlState == SqlStates.ObjectInUse) { throw new SqlException(SqlStates.ConnectionFailure, error.Message); }
        finally { state.RoleGate.Release(); }
    }
    private async ValueTask AdoptElectedOwnerAsync(CancellationToken token)
    {
        var state = fileState!;
        IBlockStore? store = null;
        try
        {
            store = await state.Options.FileHost.OpenAsync(state.Path, FileMode.Open, state.Options.ReadOnly, token).ConfigureAwait(false);
            var opened = await OpenFileStorageAsync(store, state.Options, ownedCodec, token).ConfigureAwait(false);
            lock (gate)
            {
                pager = opened.Pager; committer = new(pager, opened.Recovered);
                current = new(opened.Catalog, opened.Recovered.Meta) { Encrypted = opened.Recovered.Encrypted };
                keyBlock = opened.KeyBlock; state.CatalogDirty = false; state.LocalStore = store;
            }
            await StartOwnerAsync(token).ConfigureAwait(false);
        }
        catch
        {
            // In particular, a former client's held key may no longer unlock a
            // rekeyed file. Failed adoption must release the exclusive claim so
            // another process with the new key can become owner.
            lock (gate) { EndFileRole(); state.NeedsElection = true; }
            state.Server?.Abort();
            try
            {
                if (state.Server is not null) await state.Server.DisposeAsync().ConfigureAwait(false);
            }
            finally
            {
                try { if (store is not null) await store.DisposeAsync().ConfigureAwait(false); }
                finally
                {
                    if (state.Endpoint is not null && state.Bundle is not null) state.Bundle.RemoveEndpoint(state.Endpoint);
                    state.Bundle?.Dispose(); state.Options.RoleHost?.Release(state.Path);
                    state.Server = null; state.Ledger = null; state.Bundle = null;
                    state.LocalStore = null; state.Endpoint = null; state.Client = null;
                }
            }
            throw;
        }
    }
    private async ValueTask<DatabaseSnapshot> OwnerSnapshotAsync(CancellationToken token)
    {
        var state = fileState!;
        await state.AdoptionGate.WaitAsync(token).ConfigureAwait(false);
        try
        {
            while (true)
            {
                PageOwnerLedger ledger; StoragePager snapshotPager; long role; PageOwnerLedger.PageOwnerPin pinned;
                lock (gate)
                {
                    Check();
                    if (state.NeedsElection || state.Client is not null || state.Ledger is null) throw new SqlException(SqlStates.ConnectionFailure, "owner role ended during snapshot adoption");
                    if (!state.CatalogDirty) return current;
                    ledger = state.Ledger; snapshotPager = pager; role = state.Role;
                    pinned = ledger.Pin(ledger.Incarnation, ledger.CurrentTransactionId);
                }
                using var pin = pinned;
                var encrypted = ownedCodec is null ? null : StorageCryptoMeta.Parse(pin.Snapshot.Meta, long.MaxValue) ?? throw StorageFormat.Corrupt("invalid published encrypted meta");
                var meta = encrypted?.Header ?? ParseServedMeta(pin.Snapshot.Meta);
                var catalog = encrypted is null ? await Catalog.ReadAsync(snapshotPager, meta, token).ConfigureAwait(false) : await Catalog.ReadAsync(snapshotPager, encrypted, token).ConfigureAwait(false);
                catalog = BindEnvironment(catalog, Environment);
                catalog = await StorageTree.ValidateCatalogSpinesAsync(snapshotPager, catalog, meta, token).ConfigureAwait(false);
                lock (gate)
                {
                    CheckRole(role);
                    if (ledger.CurrentTransactionId != meta.TransactionId) continue;
                    current = new(catalog, meta) { Encrypted = encrypted }; state.CatalogDirty = false; return current;
                }
            }
        }
        finally { state.AdoptionGate.Release(); }
    }
    private async ValueTask<SnapshotPin> PinFileAsync(CancellationToken token)
    {
        var state = fileState!;
        for (var attempt = 0; ; attempt++)
        {
            await EnsureFileRoleAsync(token).ConfigureAwait(false);
            if (state.Admission is { } admission) await admission.Task.WaitAsync(token).ConfigureAwait(false);
            if (state.NeedsElection) continue;
            PageClientConnection? client; long role;
            lock (gate) { client = state.Client; role = state.Role; }
            if (client is null)
            {
                try { _ = await OwnerSnapshotAsync(token).ConfigureAwait(false); }
                catch (SqlException error) when (error.SqlState == SqlStates.ConnectionFailure) { continue; }
                lock (gate)
                {
                    Check();
                    if (role != state.Role || state.Admission is not null || state.NeedsElection) continue;
                    var pin = state.Ledger!.Pin(state.Ledger.Incarnation, current.Meta.TransactionId);
                    if (current.Meta.TransactionId != pin.Snapshot.TransactionId) { pin.Dispose(); continue; }
                    pins[current.Meta.TransactionId] = pins.GetValueOrDefault(current.Meta.TransactionId) + 1;
                    return new(this, current with { Pager = pager }, () => { pin.Dispose(); return ValueTask.CompletedTask; });
                }
            }
            PageReadSnapshot read;
            try { read = await client.BeginReadAsync(new byte[16], 0, token).ConfigureAwait(false); }
            catch (SqlException error) when (error.SqlState == SqlStates.ConnectionFailure && attempt == 0) { continue; }
            PageClientStore? store = null;
            try
            {
                store = new(client, read);
                var served = await UnlockClientAsync(store, read.Meta, state.Options, ownedCodec, true, token).ConfigureAwait(false);
                var meta = served.Meta; var snapshotPager = new StoragePager(store, meta.PageSize, state.Options.CacheBytes, served.Codec);
                var catalog = await ReadServedCatalogAsync(snapshotPager, served, Environment, token).ConfigureAwait(false); var snapshot = new DatabaseSnapshot(catalog, meta, snapshotPager) { Encrypted = served.Encrypted };
                var retainedStore = store;
                lock (gate)
                {
                    CheckRole(role); pins[meta.TransactionId] = pins.GetValueOrDefault(meta.TransactionId) + 1; current = snapshot; keyBlock = served.KeyBlock;
                    return new(this, snapshot, async () => { await EndRemoteReadAsync(client, read.TransactionId).ConfigureAwait(false); await retainedStore.DisposeAsync().ConfigureAwait(false); });
                }
            }
            catch { if (store is not null) await store.DisposeAsync().ConfigureAwait(false); await EndRemoteReadAsync(client, read.TransactionId).ConfigureAwait(false); throw; }
        }
    }
    private static async ValueTask EndRemoteReadAsync(PageClientConnection client, ulong version)
    { try { if (client.IsConnected) await client.EndReadAsync(version).ConfigureAwait(false); } catch (SqlException error) when (error.SqlState == SqlStates.ConnectionFailure) { } }
    private async ValueTask<PendingDatabase> BeginFileWriteAsync(CancellationToken token)
    {
        var state = fileState!;
        while (true)
        {
            await EnsureFileRoleAsync(token).ConfigureAwait(false);
            if (state.Options.ReadOnly) throw new SqlException(SqlStates.ReadOnlySqlTransaction, "database is read-only");
            if (state.Admission is { } admission) await admission.Task.WaitAsync(token).ConfigureAwait(false);
            if (state.NeedsElection) continue;
            long role; SemaphoreSlim held; CancellationToken roleEnded;
            lock (gate) { role = state.Role; held = writer; roleEnded = state.RoleEnded.Token; }
            if (!held.Wait(0))
            {
                using var waiting = CancellationTokenSource.CreateLinkedTokenSource(token, roleEnded);
                try { await held.WaitAsync(waiting.Token).ConfigureAwait(false); }
                catch (OperationCanceledException) when (!token.IsCancellationRequested) { continue; }
            }
            try
            {
                if (role != state.Role || state.Admission is not null) { held.Release(); continue; }
                CheckRole(role);
                if (state.Client is not { } client)
                {
                    var snapshot = await OwnerSnapshotAsync(token).ConfigureAwait(false);
                    return new(snapshot) { WriterGate = held, StorageBefore = committer.Current, Role = role };
                }
                if (client.OwnerReadOnly)
                {
                    await RequestWritableOwnerAsync(client, token).ConfigureAwait(false);
                    held.Release(); continue;
                }
                PageWriteSnapshot lease;
                try { lease = await client.BeginWriteAsync(new byte[16], 0, (ulong)state.Options.WriterLockTimeoutMilliseconds, relaxed: !SynchronousCommits, cancellationToken: token).ConfigureAwait(false); }
                catch (SqlException error) when (error.SqlState == SqlStates.ConnectionFailure) { held.Release(); continue; }
                var context = new FileWriteContext(client, lease);
                try
                {
                    var store = new PageClientStore(client, lease.Snapshot); context.Store = store;
                    var served = await UnlockClientAsync(store, lease.Snapshot.Meta, state.Options, ownedCodec, true, token).ConfigureAwait(false);
                    var meta = served.Meta; var snapshotPager = new StoragePager(store, meta.PageSize, state.Options.CacheBytes, served.Codec);
                    var catalog = await ReadServedCatalogAsync(snapshotPager, served, Environment, token).ConfigureAwait(false);
                    var (free, chain) = served.Encrypted is null
                        ? await StorageRecovery.ReadFreeListAsync(store, meta, [], token).ConfigureAwait(false)
                        : await StorageCryptoRecovery.ReadFreeListAsync(store, served.Encrypted, served.Codec!, [], token).ConfigureAwait(false);
                    var recovered = new StorageRecovered(meta, 0, 0, [], free, chain) { Encrypted = served.Encrypted,
                        Batch = lease.Pending.Length == 0 ? null : new(meta.TransactionId, lease.Reuse,
                            lease.Pending.Select(entry => new StorageManifestEntry(entry.Index, entry.Digest)).ToArray(), null) };
                    context.Granted = !lease.ReuseGrant ? [] : lease.Pending.Length == 0 ? new(free) : new(lease.Reuse);
                    var snapshot = new DatabaseSnapshot(catalog, meta, snapshotPager) { Encrypted = served.Encrypted };
                    lock (gate) { CheckRole(role); current = snapshot; keyBlock = served.KeyBlock; }
                    return new(snapshot) { WriterGate = held, StorageBefore = recovered, Role = role, FileWrite = context };
                }
                catch { await context.DisposeAsync().ConfigureAwait(false); throw; }
            }
            catch { held.Release(); throw; }
        }
    }
    private async ValueTask RequestWritableOwnerAsync(PageClientConnection client, CancellationToken token)
    {
        var state = fileState!;
        if (state.Options.Ownership == FileOwnership.Never) throw new SqlException(SqlStates.ReadOnlySqlTransaction, "read-only owner cannot be replaced by a never-owner client");
        try { await using var probe = await state.Options.FileHost.OpenAsync(state.Path, FileMode.Open, token: token).ConfigureAwait(false); }
        catch (Exception error) when (error is IOException or UnauthorizedAccessException)
        { throw new SqlException(SqlStates.ReadOnlySqlTransaction, "file cannot be opened for writing"); }
        using var deadline = CancellationTokenSource.CreateLinkedTokenSource(token);
        if (state.Options.WriterLockTimeoutMilliseconds != 0) deadline.CancelAfter(TimeSpan.FromMilliseconds(Math.Min(int.MaxValue, state.Options.WriterLockTimeoutMilliseconds)));
        try
        {
            _ = await client.RequestOwnershipAsync(deadline.Token).ConfigureAwait(false);
            while (client.IsConnected) await Task.Delay(5, deadline.Token).ConfigureAwait(false);
            await EnsureFileRoleAsync(deadline.Token).ConfigureAwait(false);
        }
        catch (OperationCanceledException) when (!token.IsCancellationRequested)
        { throw new SqlException(SqlStates.LockNotAvailable, "writer timed out waiting for read-only owner handoff"); }
    }
    internal sealed class FileWriteContext(PageClientConnection connection, PageWriteSnapshot snapshot) : IAsyncDisposable
    {
        internal readonly PageClientConnection Connection = connection;
        internal readonly PageWriteSnapshot Snapshot = snapshot;
        internal HashSet<uint> Granted = [];
        internal PageClientStore? Store;
        internal bool Committed;
        internal bool ResolvedAfterRoleChange;
        public async ValueTask DisposeAsync()
        {
            try { if (!Committed && Connection.IsConnected) await Connection.AbortAsync().ConfigureAwait(false); }
            catch (SqlException error) when (error.SqlState == SqlStates.ConnectionFailure) { }
            finally { if (Store is not null) await Store.DisposeAsync().ConfigureAwait(false); }
        }
    }
    private sealed class PublicationLease(SemaphoreSlim semaphore) : IDisposable
    { public void Dispose() => semaphore.Release(); }
    private async ValueTask<IDisposable?> CommitFileOrMemoryAsync(PendingDatabase pending, StorageCommitPlan plan, CancellationToken token)
    {
        if (fileState is null) { await committer.CommitAsync(plan, token).ConfigureAwait(false); return null; }
        var state = fileState; await state.Publication.WaitAsync(token).ConfigureAwait(false); var publicationHeld = true;
        try
        {
            CheckPendingRole(pending);
            if (pending.FileWrite is { } remote)
            {
                var endWritten = false;
                try { await remote.Connection.CommitAsync(plan, token, () => endWritten = true).ConfigureAwait(false); }
                catch (Exception error) when (endWritten && (error is IOException or OperationCanceledException || error is SqlException { SqlState: SqlStates.ConnectionFailure }))
                {
                    // Election takes RoleGate. Close takes that gate before
                    // waiting for publication, so resolution must not hold both.
                    state.Publication.Release(); publicationHeld = false;
                    byte status;
                    try
                    {
                        await EnsureFileRoleAsync(CancellationToken.None).ConfigureAwait(false);
                        var digest = PageOwnerLedger.ResolutionDigest(plan.MetaBytes);
                        status = state.Client is { } client
                            ? await client.CommitStatusAsync(plan.Meta.TransactionId, digest).ConfigureAwait(false)
                            : await state.Ledger!.CommitStatusAsync(plan.Meta.TransactionId, digest).ConfigureAwait(false);
                    }
                    catch (Exception resolutionError) when (resolutionError is SqlException or IOException or OperationCanceledException or ObjectDisposedException)
                    { throw new SqlException(SqlStates.TransactionResolutionUnknown, "could not resolve lost commit acknowledgement: " + resolutionError.Message); }
                    if (status == 1) throw new SqlException(SqlStates.ConnectionFailure, "unacknowledged commit was not committed");
                    if (status != 0) throw new SqlException(SqlStates.TransactionResolutionUnknown, "commit outcome is outside the owner's resolution window");
                    remote.ResolvedAfterRoleChange = true;
                    await state.Publication.WaitAsync().ConfigureAwait(false); publicationHeld = true;
                }
                remote.Committed = true;
            }
            else
            {
                state.Ledger!.BeginPublication();
                try { await PrepareOwnerWriteAsync(token).ConfigureAwait(false); await committer.CommitAsync(plan, token, SynchronousCommits, plan.DeadlineMicroseconds).ConfigureAwait(false); }
                catch (Exception error) { state.Ledger.FailPublication(error); if (committer.IsPoisoned) PoisonFile(new SqlException(SqlStates.IoError, "file commit failed; reopen for recovery")); throw; }
            }
            return new PublicationLease(state.Publication);
        }
        catch { if (publicationHeld) state.Publication.Release(); throw; }
    }
    private async ValueTask<IPageOwnerWrite> BeginRemoteWriterAsync(long role, bool relaxed, CancellationToken token)
    {
        var state = fileState!; SemaphoreSlim held;
        while (true)
        {
            if (state.Admission is { } admission) await admission.Task.WaitAsync(token).ConfigureAwait(false);
            CheckRole(role); held = writer; await held.WaitAsync(token).ConfigureAwait(false);
            lock (gate)
            {
                if (state.Admission is null) { state.RemoteWriterGate = held; break; }
                held.Release();
            }
        }
        try
        {
            CheckRole(role); if (state.Options.ReadOnly) throw new SqlException(SqlStates.ReadOnlySqlTransaction, "owner is read-only");
            if (committer.Current.Batch is not null)
            {
                if (!relaxed) await committer.FlushAsync(shrink: false, token: token).ConfigureAwait(false);
                else if (state.Options.FlushIntervalMilliseconds != 0)
                    await committer.BeginPlanAsync(false, (state.Options.Clock ?? SessionOptions.HostClock)(),
                        state.Options.FlushIntervalMilliseconds, shrink: false, token: token).ConfigureAwait(false);
            }
            var pin = state.Ledger!.Pin(state.Ledger.Incarnation, state.Ledger.CurrentTransactionId);
            var grant = state.Ledger.CanReuse(committer.Current.FreeGeneration);
            return new RemoteWriter(this, role, held, pin, committer.Current, grant, relaxed);
        }
        catch (Exception error)
        {
            if (committer.IsPoisoned) PoisonFile(error);
            lock (gate) if (ReferenceEquals(state.RemoteWriterGate, held)) state.RemoteWriterGate = null;
            held.Release(); throw;
        }
    }
    private sealed class RemoteWriter(Database database, long role, SemaphoreSlim held, PageOwnerLedger.PageOwnerPin pin,
        StorageRecovered before, bool grant, bool relaxed) : IPageOwnerWrite
    {
        private PageCommitValidator? validation;
        private int disposed;
        public PageOwnerLedger.PageOwnerPin Pin => pin;
        public bool ReuseGrant => grant;
        public PageWireDigest[] Pending => (before.Batch?.Pending ?? []).Select(entry => new PageWireDigest(entry.Page, entry.Digest)).ToArray();
        public uint[] Reuse => before.Batch is not { } batch || !grant ? [] : before.FreePages
            .Intersect(batch.FloorFree).Except(batch.Pending.Select(entry => entry.Page)).Order().ToArray();
        public async ValueTask BeginCommitAsync(ulong transactionId, byte[] meta, PageWirePage[] manifest, CancellationToken token)
        {
            database.CheckRole(role);
            validation = new(before.Meta, database.ownedCodec is not null,
                !grant ? new HashSet<uint>() : before.Batch is null ? before.FreePages : Reuse.ToHashSet(),
                (before.Batch?.Pending ?? []).ToDictionary(entry => entry.Page, entry => entry.Digest),
                transactionId, meta, manifest, database.ownedCodec);
            try
            {
                await database.PrepareOwnerWriteAsync(token).ConfigureAwait(false);
                var required = (long)validation.Meta.PageCount * before.Meta.PageSize;
                var length = await database.pager.Store.GetLengthAsync(token).ConfigureAwait(false);
                if (length < required)
                    await database.pager.Store.SetLengthAsync(StorageGrowth.Length(length, validation.Meta.PageCount, before.Meta.PageSize), token).ConfigureAwait(false);
                foreach (var page in manifest)
                { database.pager.Invalidate(page.Index); await database.pager.Store.WriteAsync((long)page.Index * before.Meta.PageSize, page.Bytes, token).ConfigureAwait(false); }
            }
            catch (Exception error) { database.PoisonFile(error); throw new SqlException(SqlStates.IoError, error.Message); }
        }
        public async ValueTask WritePagesAsync(PageWirePage[] pages, CancellationToken token)
        {
            database.CheckRole(role);
            foreach (var page in pages)
            {
                validation!.Receive(page.Index, page.Bytes);
                try { database.pager.Invalidate(page.Index); await database.pager.Store.WriteAsync((long)page.Index * before.Meta.PageSize, page.Bytes, token).ConfigureAwait(false); }
                catch (Exception error) { database.PoisonFile(error); throw new SqlException(SqlStates.IoError, error.Message); }
            }
        }
        public async ValueTask<ulong> CompleteCommitAsync(CancellationToken token)
        {
            database.CheckRole(role); validation!.Complete();
            var dependencies = validation.WrittenIndices.ToHashSet();
            SortedSet<uint> free; HashSet<uint> chain;
            try
            {
                (free, chain) = validation.EncryptedMeta is { } encrypted
                    ? await StorageCryptoRecovery.ReadFreeListAsync(database.pager.Store, encrypted, database.ownedCodec!, dependencies, token).ConfigureAwait(false)
                    : await StorageRecovery.ReadFreeListAsync(database.pager.Store, validation.Meta, dependencies, token).ConfigureAwait(false);
            }
            catch (SqlException error) when (error.SqlState == SqlStates.DataCorrupted) { throw PageWire.Invalid("invalid client free list"); }
            var state = database.fileState!; await state.Publication.WaitAsync(token).ConfigureAwait(false);
            try
            {
                database.CheckRole(role); state.Ledger!.BeginPublication();
                var slot = 1 - before.FloorSlot; var meta = validation.MetaForSlot((uint)slot, database.ownedCodec);
                await database.pager.Store.WriteAsync((long)slot * before.Meta.PageSize, meta, CancellationToken.None).ConfigureAwait(false);
                var recovered = new StorageRecovered(validation.Meta, slot, slot, dependencies, free, chain)
                {
                    ManifestPages = validation.Manifest.Select(page => page.Index).ToHashSet(), Encrypted = validation.EncryptedMeta,
                    LastReclamationLive = before.ReclamationLive, ReclamationTransactionId = before.FreeGeneration
                };
                long? deadline = before.Batch?.DeadlineMicroseconds;
                if (relaxed && before.Batch is null && state.Options.FlushIntervalMilliseconds != 0)
                {
                    var now = (state.Options.Clock ?? SessionOptions.HostClock)();
                    var next = (Int128)now + (Int128)state.Options.FlushIntervalMilliseconds * 1000;
                    deadline = next > long.MaxValue ? long.MaxValue : (long)next;
                }
                await database.committer.PublishClientAsync(recovered,
                    validation.Entries.Select(entry => new StorageManifestEntry(entry.Key, entry.Value)).ToArray(),
                    !relaxed, deadline, CancellationToken.None).ConfigureAwait(false);
                lock (database.gate)
                {
                    database.current = database.current with { Meta = validation.Meta };
                    state.CatalogDirty = true;
                    foreach (var index in free) if (!before.FreePages.Contains(index)) database.retiredAt[index] = validation.Meta.TransactionId;
                    foreach (var index in database.retiredAt.Keys.Where(index => !free.Contains(index)).ToArray()) database.retiredAt.Remove(index);
                    state.Ledger.Publish(meta, validation.WrittenIndices);
                }
                return validation.Meta.TransactionId;
            }
            catch (Exception error) { state.Ledger!.FailPublication(error); database.PoisonFile(error); throw; }
            finally { state.Publication.Release(); }
        }
        public async ValueTask AfterAcknowledgementAsync()
        {
            pin.Dispose();
            database.FinishCommitPolicy();
            if (!database.committer.IsPoisoned)
                await database.RunRemoteMaintenanceAsync(role, CancellationToken.None).ConfigureAwait(false);
        }
        public ValueTask DisposeAsync()
        {
            if (Interlocked.Exchange(ref disposed, 1) == 0)
            {
                pin.Dispose();
                lock (database.gate)
                    if (ReferenceEquals(database.fileState!.RemoteWriterGate, held)) database.fileState.RemoteWriterGate = null;
                held.Release();
            }
            return ValueTask.CompletedTask;
        }
    }
    private void PoisonFile(Exception error)
    {
        lock (gate) fileState!.Failure = error as SqlException ?? new(SqlStates.IoError, error.Message);
        fileState.Server?.Abort();
    }

    private ValueTask<byte> RequestHandoffAsync(ulong requester, CancellationToken token)
    {
        token.ThrowIfCancellationRequested();
        lock (gate)
        {
            var state = fileState!;
            if (state.Closing || state.Admission is not null || (!state.Options.ReadOnly && state.Options.Ownership != FileOwnership.Yield))
                return ValueTask.FromResult((byte)1);
            state.HandoffRequester = requester;
            state.Admission = new(TaskCreationOptions.RunContinuationsAsynchronously);
            return ValueTask.FromResult((byte)0);
        }
    }
    private async Task YieldOwnershipAsync(ulong requester)
    {
        await Task.Yield();
        var state = fileState!; await state.RoleGate.WaitAsync().ConfigureAwait(false);
        try
        {
            lock (gate) if (disposed || state.Closing) return;
            var server = state.Server!;
            await server.BroadcastNoticeAsync(1, requester).ConfigureAwait(false);
            if (!await DrainAsync(state.Options.YieldGraceMilliseconds, false, requester).ConfigureAwait(false))
            {
                await server.BroadcastNoticeAsync(2).ConfigureAwait(false); return;
            }
            await ReleaseOwnershipAsync().ConfigureAwait(false);
            state.PauseUntil = Stopwatch.GetTimestamp() + Stopwatch.Frequency * 2;
        }
        catch (Exception error) { PoisonFile(error); }
        finally
        {
            lock (gate) { state.Handoff = null; state.HandoffRequester = 0; state.Admission?.TrySetResult(); state.Admission = null; }
            state.RoleGate.Release();
        }
    }
    private async ValueTask<bool> DrainAsync(long grace, bool closing, ulong requester = 0)
    {
        var state = fileState!; var elapsed = Stopwatch.StartNew();
        var simulatedStart = state.Options.RoleHost?.NowMicroseconds;
        while (true)
        {
            lock (gate)
            {
                if (!closing && !state.Server!.HasConnection(requester)) return false;
                var localPins = pins.Values.Sum();
                var remotePins = (state.Ledger?.PinCount ?? 0) - localPins;
                if (closing ? remotePins == 0 : (state.Ledger?.PinCount ?? 0) == 0 && writer.CurrentCount != 0) return true;
                if (grace != 0 && (simulatedStart is { } start ?
                    (Int128)state.Options.RoleHost!.NowMicroseconds - start >= (Int128)grace * 1000 : elapsed.ElapsedMilliseconds >= grace)) return true;
            }
            if (state.Options.RoleHost is { } host) await host.PollAsync(CancellationToken.None).ConfigureAwait(false);
            else await Task.Delay(5).ConfigureAwait(false);
        }
    }
    private async ValueTask ReleaseOwnershipAsync()
    {
        var state = fileState!;
        if (state.Server is not null) await state.Server.DisposeAsync().ConfigureAwait(false);
        // A local commit already writing holds Publication until its catalog and
        // ledger are published. No old role can issue another write afterwards.
        await state.Publication.WaitAsync().ConfigureAwait(false);
        try
        {
            try
            {
                if (state.LocalStore is not null && !state.Options.ReadOnly)
                    await committer.FlushAsync(token: CancellationToken.None).ConfigureAwait(false);
            }
            finally
            {
                lock (gate) { EndFileRole(); state.NeedsElection = true; }
                try { await pager.Store.DisposeAsync().ConfigureAwait(false); }
                finally
                {
                    if (state.Endpoint is not null && state.Bundle is not null) state.Bundle.RemoveEndpoint(state.Endpoint);
                    state.Bundle?.Dispose();
                    state.Options.RoleHost?.Release(state.Path);
                    state.Server = null; state.Ledger = null; state.Bundle = null; state.LocalStore = null; state.Endpoint = null; state.Client = null;
                }
            }
        }
        finally { state.Publication.Release(); }
    }
    private void EndFileRole()
    {
        var state = fileState!;
        state.Role++; pins.Clear(); writer = new(1, 1);
        state.RoleEnded.Cancel(); state.RoleEnded.Dispose(); state.RoleEnded = new();
    }
    private async ValueTask DisposeFileAsync()
    {
        var state = fileState!;
        await state.RoleGate.WaitAsync().ConfigureAwait(false);
        try
        {
        lock (gate)
        {
            if (disposed) return;
            state.Closing = true;
            state.Admission ??= new(TaskCreationOptions.RunContinuationsAsynchronously);
        }
        if (state.Server is not null)
        {
            await state.Server.BroadcastNoticeAsync(3).ConfigureAwait(false);
            await DrainAsync(state.Options.ShutdownGraceMilliseconds, true).ConfigureAwait(false);
        }
        if (state.Client is not null) await state.Client.DisposeAsync().ConfigureAwait(false);
        try { await ReleaseOwnershipAsync().ConfigureAwait(false); }
        finally
        {
            lock (gate) { disposed = true; state.Admission?.TrySetResult(); }
            try { if (ScratchHost is not null) await ScratchHost.DisposeAsync().ConfigureAwait(false); }
            finally { ownedCodec?.Dispose(); state.Claim?.Dispose(); if (state.RemoteCredential is { } secret) CryptographicOperations.ZeroMemory(secret.Bytes); }
        }
        }
        finally { state.RoleGate.Release(); }
    }
}
