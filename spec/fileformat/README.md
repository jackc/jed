# spec/fileformat/ — on-disk format + byte fixtures

The single-file on-disk format (CLAUDE.md §9), specified with **byte-exact fixtures**. The
load-bearing conformance test: a database file written by the Rust core must be
byte-readable by the Go core and vice versa (CLAUDE.md §8). That one round-trip catches an
entire class of cross-implementation divergence automatically.

Storage design targets **in-RAM datasets with SSD-backed persistence** (CLAUDE.md §9):
the in-memory representation is first-class, and on-disk layout/block size are chosen for
SSD characteristics. Writes batch in a private staging area and land at commit (CLAUDE.md
§3).

> Status: empty. Specified at CLAUDE.md §11 step 4 (storage seam) and exercised by step 5.
