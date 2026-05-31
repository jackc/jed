# spec/ — the canonical source of truth

Per CLAUDE.md §2, the **language-neutral specification and conformance corpus is the
canonical artifact** of this project. Every implementation under `impl/`, *including the
first*, is a downstream consumer of what lives here. No implementation is canonical; the
spec is.

## What lives here

| Directory | Holds |
|---|---|
| [design/](design/) | Prose design docs per subsystem — the **why** behind decisions. |
| [grammar/](grammar/) | One EBNF grammar. Parsers are hand-written per language from it. |
| [types/](types/) | Scalar set + comparison / coercion / promotion matrix, **as data**. |
| [functions/](functions/) | Function / operator catalog, **as data**. |
| [errors/](errors/) | Error-code registry — errors are structured data, not free text. |
| [fileformat/](fileformat/) | On-disk format spec + byte-exact fixtures. |
| [encoding/](encoding/) | Order-preserving key-encoding spec + byte test vectors. |
| [conformance/](conformance/) | sqllogictest-style corpus + the differential oracle harness. |

## Data format

Data tables (`types/`, `functions/`, `errors/`) are authored in **TOML**. TOML is a
**build-time-only** dependency: it feeds per-language codegen that emits source (CLAUDE.md
§5). No shipped engine parses TOML at runtime. The format is provisional — a custom spec
format may replace it later; keep the data shape clean so migration stays mechanical.

## Determinism

Everything here must be deterministic and language-neutral: defined result ordering,
stable canonical names, no iteration-order or wall-clock leakage. The multi-core honesty
mechanism (CLAUDE.md §2) depends on bit-reproducibility.
