# jed Node lock host

This is the narrow native OS-lock host used by the independent TypeScript engine's Node file layer.
It exposes only whole-file nonblocking shared/exclusive try-lock, unlock, close, and an ABI version.
It performs no database I/O and contains no parser, planner, executor, storage, or type behavior.

Build the local development artifact from the repository root with:

```text
rake ts:lock_build
```

The exact-pinned Node-API dependencies and bounded host-only native exception are recorded in
`CLAUDE.md` §14 and `spec/design/locking.md` §8. Browser/OPFS bundles never import this crate. A
production package must provide a matching prebuilt artifact and fails shared locking closed when it
does not; it must not download or compile code during installation.
