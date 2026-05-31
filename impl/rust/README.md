# impl/rust/ — the Rust core

The first native core (CLAUDE.md §2). Manual ownership, no GC, no runtime.

Style (CLAUDE.md §10): **boring, explicit code over clever abstraction.** Resist deep
generics and macro magic. Flat, well-named, single-responsibility modules with small
context footprints.

This core is a **consumer of [../../spec/](../../spec/)**, not an author of it. Type
definitions, comparison/coercion rules, and error codes come from the spec's data tables
(via build-time codegen, CLAUDE.md §5) — never hand-transcribed here.

> Status: not yet initialized. `Cargo` setup + first code at CLAUDE.md §11 step 5.
