# impl/go/ — the Go core

The second native core (CLAUDE.md §2), built in genuine lockstep with the Rust core — the
divergence between the two is the project's honesty mechanism. The maintainer's
daily-driver language.

**Pure Go: no cgo, no FFI.** (A cgo wrapper around the Rust core would defeat the entire
point of an independent implementation.)

Style (CLAUDE.md §10): **boring, explicit code over clever abstraction.** Resist
over-interfacing. Flat, well-named, single-responsibility packages.

This core is a **consumer of [../../spec/](../../spec/)**, not an author of it. Type
definitions, comparison/coercion rules, and error codes come from the spec's data tables
(via build-time codegen, CLAUDE.md §5) — never hand-transcribed here.

> Status: not yet initialized. Go module setup + first code at CLAUDE.md §11 step 5.
