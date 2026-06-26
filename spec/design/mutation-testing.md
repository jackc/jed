# Mutation testing — design

> Why and how the Go core is mutation-tested, and what the result means. Mutation testing
> injects deliberate bugs into one core's source and checks whether the conformance corpus
> (CLAUDE.md §7) catches them. A **surviving** mutant — one the whole corpus still passes —
> is untested logic, located to a line. It answers "are we only testing the obvious?"
> (`.scratch/testing-ideas.md` §1.2) with a map, not a guess. This is an **analysis tool**,
> deliberately outside `rake ci`; the harness is `impl/go/cmd/mutate`, driven by `rake mutation`.

## 1. The gap this closes

jed's headline test strategy is *differential*: three cores (Rust, Go, TS) run the identical
sqllogictest corpus and any disagreement is a bug (CLAUDE.md §2). That net is strong but has one
structural blind spot — **it is blind to a bug all three cores share**, and equally blind to logic
no test exercises at all. Coverage tools tell you a line *ran*; they do not tell you whether any
assertion would *notice* if that line were wrong.

Mutation testing closes exactly that. If we change `<` to `<=` in the comparator and the entire
corpus still passes, then no test distinguishes the two — that boundary is unverified. The set of
surviving mutants is a precise, ranked to-do list for the corpus: "these are the spots where you
could introduce a bug today and ship it green."

It is **complementary** to the differential net, not a replacement: differential testing finds
divergence between cores; mutation testing finds *under-specification* within the shared corpus.

## 2. Why Go (and why this targets the Go core)

Mutation testing is inherently **single-core**: you perturb one implementation's source and ask
whether the tests notice. So "mutation-test jed" always means "pick a core and mutate it." The
original sketch (`.scratch/testing-ideas.md` §3, §5) proposed a Ruby driver mutating the *Rust*
core. We do it in **Go**, for two reinforcing reasons:

- **The mutator wants a real parser, not regex.** Principled, type-preserving source mutation
  (flip *this* `<`, wrap *that* integer literal, negate *this* `if`) needs an AST. Go ships
  `go/parser`/`go/ast`/`go/token` in the standard library, so the harness mutates Go source
  natively, with zero third-party dependencies (CLAUDE.md §14). Mutating Rust from Ruby would be
  text-hacking; mutating Go from Go is a tree walk.
- **It matches the carve-out the testing strategy already made.** `.scratch/testing-ideas.md` §3
  routes coverage-guided fuzzing to Go precisely because the pure-Go, memory-safe core is the
  maintainer's daily driver and frictionless to manipulate and rebuild. The same logic applies
  here: native builds are ~1 s, the toolchain is in-process, and there is no FFI to wrestle.

The **oracle stays the shared corpus** (CLAUDE.md §7), which is the point: a Go-core kill is a kill
by a test that also runs on Rust and TS, so a surviving mutant flags a hole in the *cross-core*
contract, not a Go-only quirk. Nothing here is Go-core-specific except the source being perturbed;
the same harness shape could later target Rust or TS (§9).

## 3. Mechanism

For each mutant, end to end:

1. **Enumerate.** Parse a target file with `go/parser` and walk the AST, emitting a candidate edit
   at every site a mutation operator applies (§4). Each candidate is recorded as a **byte range over
   the original source** plus a replacement string — not an AST rewrite.
2. **Apply.** Splice the one replacement into a pristine copy of the file. Because every other byte
   is untouched, the mutant is minimal and the compiler's verdict is unambiguous.
3. **Build.** Recompile the conformance binary (`go build ./cmd/conformance`) in an isolated
   workspace (§6). A compile failure means the mutant is **stillborn** — see INVALID (§5).
4. **Run the corpus.** Execute the binary under a timeout. Its **exit code is the oracle**: the Go
   conformance harness exits 0 iff every applicable `.test` passed.
5. **Classify** (§5) and restore the file to pristine for the next mutant.

The whole loop is dominated by the ~1 s package rebuild (the corpus run itself is ~0.3 s), so
throughput is roughly one mutant per second per worker, and workers run in parallel (§6).

## 4. Mutation operators

Each operator maps to a bug class the design brief explicitly names (flip a comparison, off-by-one
a boundary, drop a guard, swap a connective — `.scratch/testing-ideas.md` §1.2). They are chosen to
be **type-preserving** wherever possible, so the overwhelming majority compile and only a small
INVALID tail is wasted:

| Operator     | Site                         | Mutation |
|--------------|------------------------------|----------|
| `comparison` | `<` `<=` `>` `>=` `==` `!=`   | one boundary shift (`<`→`<=`) and one negation (`<`→`>=`); `==`↔`!=` |
| `arith`      | `+` `-` `*` `/` `%`          | `+`↔`-`, `*`↔`/`, `%`→`*` |
| `logic`      | `&&` `\|\|`                   | `&&`↔`\|\|` |
| `bool`       | `true` / `false` literal     | flip (skips a field/method named `true`/`false`) |
| `offbyone`   | integer literal `N`          | `(N + 1)` and `(N - 1)` — wraps rather than re-parses, valid for any base/width |
| `condneg`    | `if` condition               | wrap in `!(…)` — the "drop a NULL check" proxy (negates a `v == nil` guard) |
| `incdec`     | `x++` / `x--`                | `++`↔`--` |

A few `arith` mutants do not type-check (`+` on strings → `-`); those surface as INVALID, not as
false signal. The set is intentionally small per site — two mutants for an ordering comparison, one
for equality — to keep the sweep bounded; breadth comes from the volume of sites, not from
exhaustive per-site rewriting.

The default target files are the "executor / evaluator / comparator" trio the brief names
(`.scratch/testing-ideas.md` §5), plus the two highest-stakes value subsystems:
`executor.go`, `operators.go`, `value.go`, `decimal.go` (exact-decimal arithmetic + the
round-half-away rounding), `encoding.go` (order-preserving key encoding). `-files` overrides this.

## 5. Classification and scoring

| Verdict     | Meaning | In score? |
|-------------|---------|-----------|
| **killed**  | corpus failed → a test caught the bug | yes (numerator) |
| **survived**| corpus still green → untested logic | yes (denominator) |
| **timeout** | mutant hung past the deadline → the timeout caught it | yes, as killed |
| **invalid** | did not compile (stillborn) | **no** — excluded |
| **error**   | harness failure (e.g. cannot launch `go`) | no — aborts |

**Mutation score = killed / (killed + survived)**, reported overall, per file, and per operator.
INVALID mutants are excluded because a bug that does not compile is not a bug the tests *could*
catch — counting it would inflate the score with free wins. A KILLED mutant also records the
**first failing corpus file**, so a kill is actionable ("`< → <=` at value.go:142 is caught by
`compare/ordering.test`").

**Equivalent mutants are the known caveat.** Some surviving mutants are semantically equivalent to
the original (e.g. perturbing a `make([]T, 0, N)` *capacity hint* changes no observable behavior),
so they *cannot* be killed by any test and are not real gaps. Mutation testing cannot decide
equivalence automatically (it is undecidable in general); the survivor list is a *candidate* list a
human triages. A genuine gap (e.g. the NULL key-tag byte in `encoding.go` that no test pins) and an
equivalent mutant both appear as survivors — reading the two-line context tells them apart.

## 6. Workspace isolation and determinism

Each worker owns a **temporary workspace**: a full copy of `impl/go` plus a symlink to the real
`spec/`, so the conformance harness's walk-up finds the corpus, collation, and tz fixtures
unchanged. The live working tree is **never** mutated, and workers cannot collide. A mutant is
applied by overwriting one file in the worker's copy; the file is restored to pristine after each
run, so a worker can move between target files safely. Workspaces are removed on exit.

The run is **reproducible** (CLAUDE.md §10 in spirit, though this is an analysis tool not a gate):
AST enumeration order is deterministic, and sampling — when `-n` is below the enumerated total —
shuffles with a seeded PRNG and takes the first `n`. Same `(files, mutators, n, seed)` ⇒ identical
mutant set every run. A **baseline** check runs the pristine workspace first and aborts if the
corpus is not already green (mutation-testing a red suite is meaningless), and times the baseline to
set a generous per-mutant timeout.

## 7. Where it sits

`rake mutation` (`impl/go/cmd/mutate`) is **outside `rake ci`**, exactly like `rake bench` and
`rake stress`: it is slow and exploratory, and a surviving mutant is a *finding to triage*, not a
build break. So a non-zero exit (survivors present) does not fail the rake invocation; the printed
report and the JSONL artifact under `bench/results/mutation/<stamp>/` are the deliverable.

```
rake mutation                  # default targets, 300 sampled mutants (seed 1)
rake mutation[value.go]        # scope to one file, full sweep
rake mutation[value.go,500,7]  # file, max mutants, seed
go run ./cmd/mutate -h         # the full flag set (-mutators, -workers, -timeout, -list, -v, -json)
```

The mutator enumeration and splicing — pure, internal logic the corpus cannot express — is unit
tested in `impl/go/cmd/mutate/mutator_test.go` (CLAUDE.md §10's rule for what earns a per-core test).

## 8. Relationship to the rest of the test strategy

This is item (2) in the `.scratch/testing-ideas.md` idea menu, and §5 calls it out as the natural
*first* step "because it tells you where coverage is thin, so everything else gets aimed." The
survivor list is meant to be consumed: each genuine survivor is either a new corpus entry (preferred
— it tests all three cores at once, CLAUDE.md §10) or, rarely, a documented equivalent mutant. As
the corpus grows to kill survivors, the mutation score rises and the next sweep finds the next layer.

## 9. Limitations and future work

- **Equivalent mutants** are reported as survivors and need human triage (§5). A future refinement
  could maintain a small ledger of known-equivalent sites to suppress them from the survivor list.
- **INVALID tail.** A few mutants do not type-check. Running `go/types` during enumeration could
  prune most type-violating `arith` mutants before they cost a build, at the cost of harness
  complexity; today they are simply cheap-ish failed builds.
- **Other cores.** The harness is structured so the only core-specific part is the source being
  perturbed and the build command. A Rust variant (mutating `impl/rust` via `syn`, `cargo build`)
  is a natural sibling if the Go survivor list is ever exhausted — but the §2 reasoning makes Go the
  right first and default target.
- **Targeting.** Sampling is uniform; a future version could bias toward operators/files with the
  highest survivor density from a prior run, converging on thin spots faster.
