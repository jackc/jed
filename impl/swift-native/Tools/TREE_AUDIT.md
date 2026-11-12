# B+tree invariant audit

The Swift tree uses immutable copied paths, packed dirty and clean leaves, a
resident interior skeleton, lazy clean leaf references, and a bounded CLOCK
pool. Its cursor holds a root-to-leaf stack. Logical node counts derive from the
skeleton, independently of physical cache reads. Overflow disposition remains
outside the tree; fresh overflow metadata travels with records through edits.

The tree tests rebuild the frozen `pk_table`, `tall_tree`, and `max_sep_table`
golden node pages byte for byte, exercise bounds/snapshot isolation, lazy open
and cache eviction, fresh overflow export, single-flight cache invalidation,
and malformed routing pages. Root's integrated Swift run on 2026-11-05 passed
all 113 tests, including eight tree tests. This is storage-foundation evidence,
not completion of database execution or shared SQL conformance.

## Empty unary subtree correction

The audit discovered an existing deletion defect at the frozen baseline
`0b76dd6e9c2675b552eb8af9e820065351ffa4b3`:

1. Use a 256-byte page, hence payload capacity 240, and an index tree with no
   value columns.
2. Insert six keys in ascending order. Each is a four-byte big-endian integer
   0 through 5 followed by 108 bytes `0xAB`, for 112 bytes total.
3. Delete keys 4 and 5.

Two separators require `2 * 112 + 20 = 244` bytes, so the pinned two-separator
split creates a zero-separator right interior. Deleting that interior's last
leaf record leaves an empty leaf with no same-parent sibling. The old ancestor
merge splits back into the same degenerate shape and retains the empty leaf.

A temporary Go structural probe reproduced the defect with:

```
mise run unit:go -- -run '^TestSwiftNativeEmptyUnarySubtreeProbe$' -count=1
```

It exited 1 with `empty non-root leaf after six inserts/two deletes; child
path=[1 0], height=3`. The audited Go `pmap.go` blob was exactly the baseline's
`dfbbe8f4f801a8e202c678c9186f2a3827adae72`. The temporary probe was removed.

The correction removes an empty unary interior subtree at its first branching
ancestor, removing separator `min(child_index, N-1)`, and repeats root collapse
through zero-key interiors. Ordinary empty-leaf merging and both merge-abandon
guards are unchanged. It preserves the existing nonempty committed leaf
invariant; no byte layout, format version, or golden fixture changed.

Supported Rust, Go, and TypeScript apply the same correction. Their existing
near-capacity structural tests used 110-byte keys, which make two separators
fit exactly (`2 * 110 + 20 = 240`) despite comments claiming overflow. They now
use 112-byte keys and check nonempty leaves after every delete. Run these
focused checks with `mise run tree-regression` (or append `go`, `rust`, `ts`).

The shared trace
`spec/conformance/sim/regress/delete-empty-unary-subtree.sim` drives the equivalent
secondary index through commit, reopen, ordered reads, and complete deletion at
256-byte pages. Run it with `mise run sim:replay
spec/conformance/sim/regress/delete-empty-unary-subtree.sim`. Its current integrity
oracle did not reject the old empty index leaf in Go, so the internal structural
assertions are necessary; the shared trace additionally checks cross-core image
and transcript agreement under the clarified rule.

Verification of the supported-core correction on 2026-11-05: Go's `TestPMap`
selection passed, all ten Rust `pmap::tests` passed, and TypeScript's focused
`tests/pmap.test.ts` passed. Both the new trace and the existing
`delete-merge-separator-overflow.sim` passed all oracles and exact cross-core
transcript/image comparison on Rust, Go, and TypeScript. The new trace's
post-deletion check has five live pages (previously eight), and its final empty
table has one live catalog page (previously two, including an empty index root).
