namespace Jed;

// A host-owned, crash-clean election boundary. Native hosts use FileLockBundle;
// deterministic hosts supply equivalent exclusivity without kernel resources.
// A null election result grants exclusive ownership until Release. A nonnull
// result is an established protocol-v2 connection to that exclusive owner.
internal interface IFileRoleHost
{
    ValueTask<PageClientConnection?> ElectAsync(string path, bool readOnly, CancellationToken token);
    void Publish(string path, Database database, PageOwnerServer server);
    void Release(string path);
    long NowMicroseconds { get; }
    ValueTask PollAsync(CancellationToken token);
}
