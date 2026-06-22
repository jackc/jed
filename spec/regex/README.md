# Regex cross-core fixtures

The **compile-determinism contract** for regular expressions ([../design/regex.md](../design/regex.md)),
the analogue of the order-preserving key-encoding vectors in [`../encoding/`](../encoding/). Because
there is no reference implementation (CLAUDE.md §2), the engine's compilation and step-cost are part
of the shared contract: every core (Rust, Go, TS) must compile a pattern to a **byte-identical NFA
program** and accrue **identical `regex_compile` / `regex_step` cost**. These two files pin that, and
each core's harness cross-checks against them in a per-core unit test (the cross-core identity is
structurally outside the SQL conformance corpus's reach — CLAUDE.md §10).

## `program_vectors.toml` — the lowering contract

Each `[[case]]` gives a `pattern` (and optional `flags`) and the exact compiled program as `prog`, an
array of instruction strings in emission order (the wrapper of [regex.md §3.2](../design/regex.md) plus
the body lowering of §3.3). `count` = `prog.length` = the `regex_compile` cost (one unit per emitted
instruction). When the program references character classes, `classes` lists the class table in append
order. The per-core test renders its compiled program to this canonical listing and asserts equality.

**Instruction listing format** (one string per instruction):

| string | opcode |
|---|---|
| `split A B` | `Split` to instruction indices A (higher priority) and B |
| `jmp A` | `Jmp` to index A |
| `char N` | `Char`, N = the matched code point (decimal) |
| `any` | `Any` (any code point except `\n`) |
| `class K` | `Class`, K = index into `classes` |
| `save N` | `Save` capture slot N |
| `assertstart` / `assertend` | `^` / `$` anchors |
| `match` | `Match` (accept) |

**Class listing format** (one string per class, in `classes` order): the ranges as `lo-hi` code-point
pairs (decimal) joined by `,`, prefixed with `^` when negated. E.g. `97-99` is `[a-c]`; `^48-57` is
`[^0-9]`. Ranges are sorted by `lo` and merged (regex.md §3.4).

`flags = "i"` folds the pattern with simple lowercasing *before* compiling (the `~*` / ILIKE
mechanism), so `(pattern="A", flags="i")` compiles identically to `(pattern="a")`.

## `match_vectors.toml` — the execution + step-cost contract

Each `[[case]]` gives `pattern` (+ `flags`), an `input`, and the expected `matched` (bool), the capture
spans `caps` (code-point `[start,end]` pairs per group; group 0 is the whole match; an unset group is
`[-1,-1]`), and `steps` = the `regex_step` cost (total Pike-VM thread-steps). Each core runs the VM and
asserts identical result, spans, and step count. `steps` is generated from the first conformant core
and audited against the VM spec (regex.md §4); like the on-disk goldens it is then the pinned contract.
