# Rakefile — task runner for the engine (see CLAUDE.md §10: prefer Ruby + Rake).
#
# references:* — provision local, read-only checkouts of reference databases
# (PostgreSQL, SQLite, DuckDB, …) used as differential-testing oracles and as
# design references (CLAUDE.md §7, §8, §12).
#
# Storage model (CLAUDE.md §12):
#   * A bare `--mirror` clone of each repo lives on the persist volume under
#     MIRROR_ROOT. It holds the full history + all branches/tags, is downloaded
#     once, and survives container rebuilds. It is the canonical copy, shared
#     across every container for this project.
#   * Each container checks out a `git worktree` into ./references/<name>. The
#     worktree shares the mirror's object store (no re-download, no Nx history)
#     but has its own detached HEAD, so a container can sit on a different
#     branch/tag without disturbing the mirror or any other container.
#
# Provisioning a new container is therefore cheap: `git worktree add` against
# the already-present mirror, no network fetch.
#
# Note: the devcontainer sets `safe.bareRepository = explicit` globally, so every
# command that touches a bare mirror must name it with `--git-dir` AND override
# the guard with `-c safe.bareRepository=all`. The git_bare helper does both.

require "bundler/setup" # load the gems pinned in Gemfile.lock (rake, toml-rb)
require "digest"        # lockfile content hashing for the npm bootstrap staleness check
require "fileutils"

# Each entry is one reference repo. `ref` is the branch/tag checked out into the
# worktree; it is explicit (not auto-detected) per CLAUDE.md's "boring, explicit"
# preference. PostgreSQL is pinned to REL_18_STABLE to match the live `postgres:18`
# oracle in .devcontainer/docker-compose.yml. All licenses are free/OSS.
REFERENCE_REPOS = [
  { name: "postgres",        url: "https://github.com/postgres/postgres.git",            ref: "REL_18_STABLE", license: "PostgreSQL License" },
  { name: "sqlite",          url: "https://github.com/sqlite/sqlite.git",                 ref: "master",        license: "Public Domain"     },
  { name: "duckdb",          url: "https://github.com/duckdb/duckdb.git",                 ref: "main",          license: "MIT"               },
  { name: "bbolt",           url: "https://github.com/etcd-io/bbolt.git",                 ref: "main",          license: "MIT"               },
  { name: "sqllogictest-rs", url: "https://github.com/risinglightdb/sqllogictest-rs.git", ref: "main",          license: "MIT / Apache-2.0"  },
].freeze

# Canonical mirrors live on the persist volume by default; overridable for use
# outside the devcontainer.
MIRROR_ROOT   = ENV.fetch("REFERENCES_MIRROR_DIR", "/persist/shared/references")
WORKTREE_ROOT = File.join(__dir__, "references")

def mirror_path(repo)   = File.join(MIRROR_ROOT, "#{repo[:name]}.git")
def worktree_path(repo) = File.join(WORKTREE_ROOT, repo[:name])

# A worktree dir is a real worktree if it has the `.git` gitdir pointer file.
def worktree?(path) = File.exist?(File.join(path, ".git"))

# Run a git command against a bare mirror. Names the gitdir explicitly and lifts
# the safe.bareRepository=explicit guard (see header note). Raises on failure.
def git_bare(repo, *args)
  sh "git", "-c", "safe.bareRepository=all", "--git-dir", mirror_path(repo), *args
end

# Capture stdout of a command given as an argv array (no shell parsing, no quoting
# pitfalls). Returns [stdout_string, success_boolean].
def capture(*args)
  out = IO.popen(args, err: File::NULL, &:read)
  [out.to_s, $?.success?]
end

# A mirror is valid only if it exists AND has at least one ref — this rejects a
# directory left behind by an interrupted clone (which exists but is incomplete).
def mirror_valid?(repo)
  return false unless File.directory?(mirror_path(repo))
  out, ok = capture("git", "-c", "safe.bareRepository=all", "--git-dir", mirror_path(repo), "for-each-ref", "--count=1")
  ok && !out.strip.empty?
end

# Clone the bare mirror onto the persist volume if it is missing or broken.
def ensure_mirror(repo)
  if mirror_valid?(repo)
    puts "  mirror cached: #{mirror_path(repo)}"
  else
    FileUtils.rm_rf(mirror_path(repo)) # clear any partial/broken clone
    puts "  cloning mirror (full history): #{repo[:url]}"
    sh "git", "clone", "--mirror", repo[:url], mirror_path(repo)
  end
end

# Check out (or re-point) the worktree under references/ at the configured ref.
# Detached HEAD keeps it purely a read-only reference checkout and avoids the
# "branch already checked out" conflict with any other worktree.
def ensure_worktree(repo)
  wp  = worktree_path(repo)
  ref = repo[:ref]

  git_bare(repo, "worktree", "prune") # drop registrations for deleted worktrees

  if worktree?(wp)
    puts "  worktree present: #{wp} -> #{ref}"
    sh "git", "-C", wp, "checkout", "--detach", ref
  else
    FileUtils.rm_rf(wp) if File.exist?(wp) # clear any non-worktree leftovers
    puts "  adding worktree: #{wp} -> #{ref}"
    git_bare(repo, "worktree", "add", "--detach", wp, ref)
  end
end

# Run a block per repo, collecting failures so one bad repo does not abort the rest.
def for_each_repo
  failures = []
  REFERENCE_REPOS.each do |repo|
    puts "#{repo[:name]}:"
    begin
      yield repo
    rescue => e
      warn "  FAILED: #{e.message}"
      failures << repo[:name]
    end
  end
  abort "references: failed for #{failures.join(', ')}" unless failures.empty?
end

# Bare `rake` runs the engine test suites (the conventional default — `rake test`: the
# conformance corpus on all three cores + per-core unit tests + the CLI tests). It never
# provisions references — those multi-GB clones stay explicit via `rake references:setup`,
# and `rake references:status` remains available by name.
task default: :test

# verify — run the spec's data-table checkers (no engine required). Each checker is
# an independent reference implementation that recomputes values from the rules and
# asserts the canonical fixtures match (CLAUDE.md §5, §8). Add new checks here as
# subsystems gain verifiable data.
desc "Verify the spec data tables and byte fixtures"
task :verify do
  checks = [
    ["key encoding", "spec/encoding/verify.rb"],
    ["prng + uuid fixtures", "spec/encoding/prng_verify.rb"],
    ["conformance taxonomy", "spec/conformance/verify.rb"],
    ["file format", "spec/fileformat/verify.rb"],
    ["function catalog", "spec/functions/verify.rb"],
    ["cost schedule", "spec/cost/verify.rb"],
    ["operator codegen (drift)", "scripts/gen_catalog.rb", "--check"],
    ["cost codegen (drift)", "scripts/gen_costs.rb", "--check"],
    ["error codegen (drift)", "scripts/gen_errors.rb", "--check"],
  ]
  failures = []
  checks.each do |name, script, *args|
    puts "#{name}: #{script} #{args.join(' ')}".rstrip
    failures << name unless system(RbConfig.ruby, script, *args)
  end
  abort "verify: failed for #{failures.join(', ')}" unless failures.empty?
  puts "\nAll spec checks passed."
end

# fmt — formatting gate for every core + the web module. The formatters are VERSION-PINNED so
# they are reproducible across contributors; this task is what makes the pins load-bearing.
# Without a check, formatting silently drifts from the pinned tools (it had, in BOTH the Rust
# and Go cores, before this gate). The pins, one per surface:
#   rust              — rustfmt (ships with rust 1.92.0, mise-pinned); `cargo fmt`.
#   go                — gofumpt (mise-pinned go tool); a stricter SUPERSET of gofmt (its output
#                       is always gofmt-clean too), chosen because mise already pins it.
#   impl/ts, bench/ts — biome (mise-pinned; biome.json at repo root). 2-space, lineWidth 100. The
#                       @generated TS files (operators/costs/ranges_gen/sqlstate) are EXCLUDED
#                       there, so the codegen drift check (`rake verify`) stays their single source
#                       of truth. Biome's LINTER is on with a tailored ruleset (noNonNullAssertion /
#                       useTemplate / noVoidTypeReturn OFF — deliberate engine idioms), but FORMAT
#                       and LINT are split into separate gates: `rake fmt` runs `biome format`
#                       (format only), `rake lint` runs `biome lint` — so a lint finding never
#                       blocks the formatter, and vice versa.
#   web               — prettier + prettier-plugin-svelte (npm devDeps, pinned EXACT in
#                       web/package.json + lockfile). Formatter output changes between versions,
#                       so the exact pin is the reproducibility guarantee here — the npm analogue
#                       of the mise pins above. Config: web/.prettierrc.json (2-space; the
#                       single-quote / no-trailing-comma SvelteKit idiom is preserved). Markdown
#                       (mdsvex) is left to the author via web/.prettierignore. Self-bootstraps
#                       web deps with `npm ci` — reinstalling whenever the lockfile changes, not
#                       only when node_modules is missing (npm_ci_if_stale; the bench:* idiom too).
# `tsc --noEmit` stays a separate TYPE check (`npm run typecheck` in impl/ts), not a formatter.
# Kept SEPARATE from `verify`, which is deliberately toolchain-light (spec data only — no
# cargo/go/biome/npm needed).
RUST_MANIFEST = File.join(__dir__, "impl/rust/Cargo.toml")
CLI_MANIFEST  = File.join(__dir__, "cli/Cargo.toml")
RUBY_GEM_DIR  = File.join(__dir__, "impl/ruby")               # the jed Ruby gem (host artifact; spec/design/ruby.md)
RUBY_EXT_MANIFEST = File.join(RUBY_GEM_DIR, "ext/Cargo.toml") # its native-extension cdylib crate
WASM_MANIFEST = File.join(__dir__, "impl/wasm/Cargo.toml")    # the wasm32-wasip1 wrap of the core (host artifact)
WASM_TARGET   = "wasm32-wasip1"
WASM_ARTIFACT = File.join(__dir__, "impl/wasm/target", WASM_TARGET, "release/jed_wasm.wasm")
GO_DIR        = File.join(__dir__, "impl/go")
TS_DIR        = File.join(__dir__, "impl/ts")
TS_CORE_DIRS  = %w[impl/ts bench/ts] # biome (mise-pinned); biome.json scopes paths + excludes generated
WEB_DIR       = "web"                # prettier (npm-pinned); reuses web's format / format:check scripts

# The Go files gofumpt would rewrite. `gofumpt -l` exits 0 even when files differ, so the
# signal is the printed file list, not the exit status.
def gofumpt_unformatted = capture("gofumpt", "-l", GO_DIR).first.split("\n").map(&:strip).reject(&:empty?)

# Bootstrap an npm project's deps with `npm ci` (reproducible from its committed lockfile —
# CLAUDE.md §14), reinstalling whenever they are STALE — not only when node_modules is absent.
# Stale = node_modules missing, OR package-lock.json changed since the last install (a new /
# updated / removed dependency). We stamp the lockfile's content hash inside node_modules after
# each successful install and compare against it; hashing the CONTENT (not the mtime) keeps the
# check correct across git checkouts, and the stamp is wiped whenever node_modules is (including
# by `npm ci` itself, which removes it first). Without this the gate silently skips the install
# when the lockfile grows a dependency — the `prettier: not found` foot-gun.
def npm_ci_if_stale(dir)
  modules = "#{dir}/node_modules"
  lock = "#{dir}/package-lock.json"
  stamp = "#{modules}/.jed-deps-stamp"
  want = File.exist?(lock) ? Digest::SHA256.file(lock).hexdigest : ""
  return if File.directory?(modules) && File.exist?(stamp) && File.read(stamp) == want

  sh "npm", "ci", "--silent", "--prefix", dir
  File.write(stamp, want)
end

namespace :fmt do
  desc "Check Rust + Go + TypeScript (cores) + web formatting against the pinned tools (the gate)"
  task :check do
    failures = []

    puts "rust: cargo fmt --check"
    unless system("cargo", "fmt", "--check", "--manifest-path", RUST_MANIFEST)
      failures << "rust"
    end

    puts "cli:  cargo fmt --check"
    unless system("cargo", "fmt", "--check", "--manifest-path", CLI_MANIFEST)
      failures << "cli"
    end

    puts "ruby-ext: cargo fmt --check (the gem's native extension)"
    unless system("cargo", "fmt", "--check", "--manifest-path", RUBY_EXT_MANIFEST)
      failures << "ruby-ext"
    end

    puts "wasm: cargo fmt --check (the wasm32-wasip1 wrap)"
    unless system("cargo", "fmt", "--check", "--manifest-path", WASM_MANIFEST)
      failures << "wasm"
    end

    puts "go:   gofumpt -l impl/go"
    unformatted = gofumpt_unformatted
    unless unformatted.empty?
      warn "  unformatted: #{unformatted.map { |f| f.delete_prefix("#{__dir__}/") }.join(', ')}"
      failures << "go"
    end

    puts "ts:   biome format #{TS_CORE_DIRS.join(' ')}"
    failures << "ts" unless system("biome", "format", *TS_CORE_DIRS)

    puts "web:  prettier --check #{WEB_DIR}"
    npm_ci_if_stale(WEB_DIR)
    failures << "web" unless system("npm", "run", "--silent", "--prefix", WEB_DIR, "format:check")

    abort "fmt: needs formatting in #{failures.join(', ')} — run `rake fmt:fix`" unless failures.empty?
    puts "\nFormatting clean (rust + go + ts + web)."
  end

  desc "Rewrite Rust + Go + TypeScript (cores) + web sources in place with the pinned formatters"
  task :fix do
    sh "cargo", "fmt", "--manifest-path", RUST_MANIFEST
    sh "cargo", "fmt", "--manifest-path", CLI_MANIFEST
    sh "cargo", "fmt", "--manifest-path", RUBY_EXT_MANIFEST
    sh "cargo", "fmt", "--manifest-path", WASM_MANIFEST
    sh "gofumpt", "-w", GO_DIR
    sh "biome", "format", "--write", *TS_CORE_DIRS
    npm_ci_if_stale(WEB_DIR)
    sh "npm", "run", "--silent", "--prefix", WEB_DIR, "format"
  end
end

# Bare `rake fmt` runs the gate; `rake fmt:fix` applies it.
desc "Check formatting of the cores + web (alias for fmt:check)"
task fmt: "fmt:check"

# lint — the Biome linter for the TS cores, kept deliberately SEPARATE from fmt (above) so a lint
# finding never blocks the formatter and vice versa. The tailored ruleset lives in biome.json;
# `biome lint` exits non-zero only on ERRORS, so the few advisory warnings (currently 5
# useOptionalChain suggestions, left as a matter of taste) are surfaced without failing the gate.
# Scope is the TS cores only: web is prettier-only here, with its own `npm run check` (svelte-check)
# as the web type gate (outside rake ci); the @generated files stay excluded via biome.json.
namespace :lint do
  desc "Lint the TypeScript cores with Biome's tailored ruleset (errors fail; warnings advise)"
  task :check do
    puts "ts:   biome lint #{TS_CORE_DIRS.join(' ')}"
    abort "lint: Biome reported errors — `rake lint:fix` applies the safe ones" unless system("biome", "lint", *TS_CORE_DIRS)
  end

  desc "Apply Biome's SAFE lint fixes to the TS cores (unsafe fixes stay per-rule + manual)"
  task :fix do
    sh "biome", "lint", "--write", *TS_CORE_DIRS
  end
end

# Bare `rake lint` runs the gate; `rake lint:fix` applies the safe fixes.
desc "Lint the TypeScript cores with Biome (alias for lint:check)"
task lint: "lint:check"

# codegen — the "middle path" (CLAUDE.md §5): (re)generate per-language source from the
# canonical spec data tables: the operator descriptor tables from spec/functions/catalog.toml,
# the cost-unit schedule from spec/cost/schedule.toml, and the SqlState enum + code mapping from
# spec/errors/registry.toml. `rake verify` fails if any of the checked-in generated files are stale.
desc "Generate per-language source from the spec data tables (codegen middle path)"
task :codegen do
  generators = ["scripts/gen_catalog.rb", "scripts/gen_costs.rb", "scripts/gen_errors.rb"]
  failures = generators.reject { |g| system(RbConfig.ruby, g) }
  abort "codegen failed for #{failures.join(', ')}" unless failures.empty?
end

# corpus — the Phase-8 testing tools (CLAUDE.md §7). These talk to the LIVE `db` PostgreSQL
# service, never the source checkout, so they do NOT trip the §12 reference-provisioning gate.
# psql-only (no `pg` gem): no §14 dependency decision.
namespace :corpus do
  desc "Check a .test's expected output against the live PostgreSQL oracle (no write)"
  task :check, [:file] do |_, args|
    file = args.fetch(:file) { abort "usage: rake 'corpus:check[path/to/file.test]'" }
    sh RbConfig.ruby, "scripts/oracle_import.rb", "--check", file
  end

  desc "Fill a .test's expected output from the live PostgreSQL oracle (writes the file)"
  task :import, [:file] do |_, args|
    file = args.fetch(:file) { abort "usage: rake 'corpus:import[path/to/file.test]'" }
    sh RbConfig.ruby, "scripts/oracle_import.rb", file
  end

  desc "Generate one metamorphic NoREC seed (SQLancer-style) and run it on Go + TS"
  task :norec, [:seed] do |_, args|
    sh RbConfig.ruby, "scripts/norec_gen.rb", *(args[:seed] ? [args[:seed]] : [])
  end

  # The CI-style metamorphic check: a fixed, reproducible sweep of seeds 1..N (deterministic, so
  # the generated tests are the same every run) generated into suites/metamorphic, run ONCE per
  # core, and removed. Exits non-zero if any (seed, core) disagrees — `sh` turns that into a task
  # failure. Runs all THREE cores, so each seed is checked metamorphically (optimized vs full
  # scan agree) AND differentially (the cores agree). Default N=20 (= 100 metamorphic pairs/core).
  desc "CI sweep: N reproducible NoREC seeds (default 20) on all three cores; fails on any divergence"
  task :norec_sweep, [:count] do |_, args|
    sh RbConfig.ruby, "scripts/norec_gen.rb", "--sweep", (args[:count] || "20")
  end

  desc "Reduce a failing .test to a minimal one (ddmin; [core] rust|go|ts, default rust)"
  task :reduce, [:file, :core] do |_, args|
    file = args.fetch(:file) { abort "usage: rake 'corpus:reduce[path/to/failing.test,rust]'" }
    extra = args[:core] ? ["--core", args[:core]] : []
    sh RbConfig.ruby, "scripts/reduce.rb", file, *extra
  end

  desc "Self-test the reducer on a fixed synthetic failure (rust-only, oracle-free)"
  task :reduce_selftest do
    sh RbConfig.ruby, "scripts/reduce_selftest.rb"
  end
end

# rqg — the RQG-vs-PG firehose (scripts/rqg_gen.rb; .scratch/testing-ideas.md §1 item 1). Generates
# random SQL over jed's supported subset, fills expected output from the LIVE PostgreSQL oracle, and
# runs the candidates through the Rust conformance harness (CI's exact comparator): a PASS is jed==PG
# agreement (emit a curated/capped/deduped corpus entry to suites/rqg/), a FAIL is a divergence
# (classify vs the override ledger, else ddmin-reduce + flag a candidate shared-core bug). OUTSIDE
# rake ci (slow, needs live PG — like mutation/stress/bench); the PRODUCT that flows INTO rake ci is
# the emitted suites/rqg/*.test, which then runs on all three cores.
namespace :rqg do
  desc "Run one RQG case (default seed 1) across every shape; check vs PG, no corpus emit"
  task :run, [:seed] do |_, args|
    sh RbConfig.ruby, "scripts/rqg_gen.rb", (args[:seed] || "1")
  end

  desc "RQG sweep: N seeds (default 200) vs PG; flag + reduce divergences, no corpus emit"
  task :sweep, [:count] do |_, args|
    sh RbConfig.ruby, "scripts/rqg_gen.rb", "--sweep", (args[:count] || "200")
  end

  desc "RQG sweep + emit curated/capped/deduped agreements to suites/rqg/ (default N=300)"
  task :emit, [:count] do |_, args|
    sh RbConfig.ruby, "scripts/rqg_gen.rb", "--sweep", (args[:count] || "300"), "--emit"
  end

  desc "Reproduce one RQG case: rake 'rqg:replay[<seed>,<shape>]'"
  task :replay, [:seed, :shape] do |_, args|
    seed = args.fetch(:seed) { abort "usage: rake 'rqg:replay[<seed>,<shape>]'" }
    extra = args[:shape] ? ["--shapes", args[:shape]] : []
    sh RbConfig.ruby, "scripts/rqg_gen.rb", seed, *extra
  end

  desc "Reduce a flagged RQG .test to a minimal one (ddmin; [core] rust|go|ts, default rust)"
  task :reduce, [:file, :core] do |_, args|
    file = args.fetch(:file) { abort "usage: rake 'rqg:reduce[path/to/flagged.test,rust]'" }
    extra = args[:core] ? ["--core", args[:core]] : []
    sh RbConfig.ruby, "scripts/reduce.rb", file, *extra
  end
end

# cli — the `jed` terminal client (spec/design/cli.md), a HOST PROGRAM at /cli: a
# standalone crate so its TUI dependencies never enter the zero-dep engine cores. Its
# tests run as part of `rake test` (and so `rake ci`), alongside the engine cores'
# conformance corpus (`rake conformance`) and per-core unit suites (`rake unit`).
namespace :cli do
  desc "Build the jed CLI (release) to cli/target/release/jed"
  task :build do
    sh "cargo", "build", "--release", "--manifest-path", CLI_MANIFEST
  end

  desc "Run the jed CLI's unit + end-to-end golden tests"
  task :test do
    sh "cargo", "test", "--manifest-path", CLI_MANIFEST
  end
end

# ruby — the jed Ruby GEM (spec/design/ruby.md), a HOST ARTIFACT at impl/ruby that WRAPS the safe
# Rust core (CLAUDE.md §2/§13), NOT a core. A standalone cdylib crate (impl/ruby/ext, the cli/
# precedent — its FFI concerns never enter the hermetic engine crate) plus a pure-Ruby gem that
# loads it through stdlib `fiddle`. Its minitest suite tests the BINDING SEAM only (marshalling,
# lifecycle); SQL semantics are inherited from Rust by construction (cores.md §1), so they stay in
# the shared corpus. Like cli:test, ruby:test runs as part of `rake test` (and so `rake ci`).
# RUBY_GEM_DIR / RUBY_EXT_MANIFEST are defined up top (beside RUST_MANIFEST) so the fmt gate reaches
# the ext crate too.
namespace :ruby do
  desc "Build the Ruby gem's native extension cdylib (release) to impl/ruby/ext/target/release"
  task :build do
    sh "cargo", "build", "--release", "--manifest-path", RUBY_EXT_MANIFEST
  end

  desc "Build the cdylib, then run the Ruby gem's minitest seam tests"
  task test: :build do
    # The Rakefile loads `bundler/setup` (line ~24), which injects bundler's env into child
    # processes — restricting them to the repo Gemfile's gems (rake, toml-rb). minitest is a Ruby
    # bundled gem NOT in that Gemfile, so a plain spawn can't load it. `with_unbundled_env` reverts
    # bundler's env for the spawn, so the test ruby resolves minitest from the mise install — no
    # Gemfile entry, no new dependency (CLAUDE.md §14). The gem uses only stdlib `fiddle` at
    # runtime; minitest is test-only tooling, exactly like rake/toml-rb.
    tests = FileList[File.join(RUBY_GEM_DIR, "test/**/*_test.rb")]
    Bundler.with_unbundled_env do
      sh RbConfig.ruby,
        "-I#{File.join(RUBY_GEM_DIR, 'lib')}", "-I#{File.join(RUBY_GEM_DIR, 'test')}",
        "-e", "ARGV.each { |f| require f }", "--", *tests
    end
  end
end

# concurrency — the stepped-THREADED concurrency conformance (spec/design/concurrency-testing.md
# §4.3), run under the race detector. The binary harnesses already run every `# format: concurrency`
# schedule stepped-SEQUENTIALLY inside the normal conformance walk (the canonical, timing-free result
# every core must produce). This task drives the SAME schedules in the OPT-IN threaded mode — one
# goroutine/thread per session under a turn token — against the real concurrent code paths, so the
# race detector (Go `-race`; Rust `Send`/`Sync` + the threaded run) exercises the actual SharedDb
# implementation. Deliberately NOT in `rake ci` (it needs the race-instrumented toolchains); run it
# when touching the shared-handle concurrency model. TS has no threaded mode (JS has no shared-memory
# threads for live objects), so it is sequential-only and not run here.
namespace :concurrency do
  desc "Run the stepped-threaded concurrency conformance under the race detector (Go + Rust)"
  task :race do
    puts "go:   go test -race (one goroutine per session, turn-token order)"
    Dir.chdir(GO_DIR) do
      sh "go", "test", "-race", "-run", "TestConcurrencySchedulesThreaded", "./cmd/conformance"
    end
    puts "rust: cargo test --bin conformance (Send/Sync + the turn-token threaded run)"
    sh "cargo", "test", "--bin", "conformance", "--manifest-path", RUST_MANIFEST
  end
end

# bench — the wall-clock benchmark subsystem (spec/design/benchmarks.md). Deliberately NOT part
# of `rake ci`: timings are environment-relative and nondeterministic. Answers are still checked —
# every result carries a cross-engine checksum and bench:report fails on any disagreement.
namespace :bench do
  BENCH_GO_BINS = %w[bench-jed bench-pg bench-sqlite bench-sqlite-cgo].freeze
  BENCH_RUST_BINS = %w[bench-jed bench-pg bench-sqlite].freeze
  BENCH_TS_BINS = %w[bench-jed bench-pg bench-sqlite].freeze

  desc "Build all benchmark binaries (Go + Rust release; TS installs deps if absent)"
  task :build do
    # Every Go binary except the cgo SQLite baseline builds with CGO_ENABLED=0, proving the
    # cgo surface stays confined to bench-sqlite-cgo (benchmarks.md §7).
    pure = (%w[bench-setup] + BENCH_GO_BINS - %w[bench-sqlite-cgo]).map { |b| "./cmd/#{b}" }
    sh({ "CGO_ENABLED" => "0" }, "go", "build", "-o", "bin/", *pure, chdir: "bench/go")
    sh({ "CGO_ENABLED" => "1" }, "go", "build", "-o", "bin/", "./cmd/bench-sqlite-cgo", chdir: "bench/go")
    sh "cargo", "build", "--release", "--quiet", "--manifest-path", "bench/rust/Cargo.toml"
    npm_ci_if_stale("bench/ts")
    # The Ruby gem bench (bench/ruby) drives the gem, so it needs the gem's native extension built.
    sh "cargo", "build", "--release", "--quiet", "--manifest-path", RUBY_EXT_MANIFEST
    # The wasm bench (bench/ts/src/bench-wasm.ts) drives the core compiled to wasm32-wasip1. Build
    # the artifact here (needs `rustup target add wasm32-wasip1` once; cargo errors clearly if absent).
    sh "cargo", "build", "--release", "--quiet", "--target", WASM_TARGET, "--manifest-path", WASM_MANIFEST
  end

  desc "Generate/refresh the benchmark databases (fingerprint-gated; [force] to override)"
  task :setup, [:force] do |_, args|
    sh({ "CGO_ENABLED" => "0" }, "go", "build", "-o", "bin/", "./cmd/bench-setup", chdir: "bench/go")
    cmd = %w[bench/go/bin/bench-setup bench/corpus bench/data]
    cmd << "--force" if args[:force]
    sh(*cmd)
  end

  desc "Run every benchmark binary sequentially; results to bench/results/<stamp>/ + report"
  task :run, [:filter] => :build do |_, args|
    stamp = Time.now.utc.strftime("%Y%m%d-%H%M%S")
    dir = File.join("bench/results", stamp)
    FileUtils.mkdir_p(dir)
    filter = args[:filter] ? [args[:filter]] : []
    BENCH_GO_BINS.each do |bin|
      sh "bench/go/bin/#{bin}", "bench/corpus", "bench/data", File.join(dir, "go-#{bin}.jsonl"), *filter
    end
    BENCH_RUST_BINS.each do |bin|
      sh "bench/rust/target/release/#{bin}", "bench/corpus", "bench/data", File.join(dir, "rust-#{bin}.jsonl"), *filter
    end
    BENCH_TS_BINS.each do |bin|
      sh "node", "bench/ts/src/#{bin}.ts", "bench/corpus", "bench/data", File.join(dir, "ts-#{bin}.jsonl"), *filter
    end
    # The Ruby gem bench (jed/ruby/wrap) — its delta vs jed/rust/core is the binding overhead
    # (benchmarks.md §7). Spawned under `with_unbundled_env` because the Rakefile loads
    # `bundler/setup`, which would otherwise restrict the child to the root Gemfile and block the
    # gem's `bigdecimal` require (the bundled-gem trap, same as ruby:test).
    Bundler.with_unbundled_env do
      sh RbConfig.ruby, "bench/ruby/bench_jed.rb", "bench/corpus", "bench/data", File.join(dir, "ruby-bench-jed.jsonl"), *filter
    end
    # The wasm wrap (jed/wasm/wrap) — the Rust core compiled to wasm32-wasip1, driven from Node over
    # WebAssembly + node:wasi (the --experimental flag enables the preview1 host). Its delta vs
    # jed/ts/core is the wasm-vs-native-JS comparison; vs jed/rust/core, the wasm sandbox tax.
    sh "node", "--experimental-wasi-unstable-preview1", "bench/ts/src/bench-wasm.ts",
       "bench/corpus", "bench/data", File.join(dir, "wasm-bench-jed.jsonl"), *filter
    Rake::Task["bench:report"].invoke(dir)
    Rake::Task["bench:html"].invoke(dir)
  end

  desc "Aggregate a results dir into a comparison table (default: newest)"
  task :report, [:dir] do |_, args|
    sh RbConfig.ruby, "scripts/bench_report.rb", *(args[:dir] ? [args[:dir]] : [])
  end

  desc "Static HTML report for a results dir (default: newest, diffed against the previous run)"
  task :html, [:dir, :baseline] do |_, args|
    sh RbConfig.ruby, "scripts/bench_html.rb", *[args[:dir], args[:baseline]].compact
  end

  desc "Markdown report to stdout + <dir>/report.md (default: newest, diffed against the previous run)"
  task :markdown, [:dir, :baseline] do |_, args|
    sh RbConfig.ruby, "scripts/bench_markdown.rb", *[args[:dir], args[:baseline]].compact
  end

  desc "Machine-readable JSONL diff of two runs (default: newest vs previous)"
  task :diff, [:dir, :baseline] do |_, args|
    sh RbConfig.ruby, "scripts/bench_diff.rb", *[args[:dir], args[:baseline]].compact
  end
end

# stress — Layer 3 of the concurrency contract (spec/design/concurrency-testing.md §6): the
# parallelism-stress suite. Like `bench:*` it is bench-family — DELIBERATELY OUTSIDE `rake ci`
# (timing-nondeterministic schedule), but its ANSWERS are still checked: every core runs each
# `stress/*.stress.toml` and emits a confluent final-state checksum, and aggregate_stress fails on
# any per-core failure OR any cross-core checksum disagreement. Go runs under `-race` (one goroutine
# per worker over the shared handle), Rust over real OS threads, TS via the seeded interleaver.
STRESS_DIR = File.join(__dir__, "stress")

# aggregate_stress reads every core's JSONL result file, prints a per-file matrix, and aborts on a
# failure or a cross-core checksum disagreement (the one thing no single core can catch on its own).
def aggregate_stress(dir)
  require "json"
  results = Dir[File.join(dir, "*.jsonl")].sort.flat_map do |f|
    File.readlines(f, chomp: true).reject(&:empty?).map { |l| JSON.parse(l) }
  end
  failures = []
  results.group_by { |r| r["name"] }.sort.each do |name, rows|
    puts "  #{name}"
    rows.sort_by { |r| r["lang"] }.each do |r|
      status = r["status"].upcase
      detail = r["status"] == "pass" ? "checksum=#{r['checksum']} checks=#{r['invariant_checks']}" : r["error"].to_s
      puts format("    %-6s %-12s %-12s %s", r["lang"], status, r["mode"], detail)
      failures << "#{r['lang']}/#{name}: #{r['error']}" if r["status"] == "fail"
    end
    # Cross-core agreement: a confluent workload's final checksum must match across every passing
    # core, regardless of mode (real threads vs. the interleaver) — concurrency-testing.md §6.
    passed = rows.select { |r| r["status"] == "pass" && r["cross_core_checksum"] }
    sums = passed.map { |r| r["checksum"] }.uniq
    if sums.length > 1
      detail = passed.map { |r| "#{r['lang']}=#{r['checksum']}" }.join(" ")
      failures << "#{name}: CROSS-CORE CHECKSUM DISAGREEMENT (#{detail})"
    end
  end
  abort "\nstress FAILED:\n  - #{failures.join("\n  - ")}" unless failures.empty?
  puts "\nstress OK (all cores agree)"
end

desc "Layer 3 concurrency-stress suite on all three cores (bench-family, OUTSIDE rake ci)"
task :stress, [:filter] do |_, args|
  stamp = Time.now.utc.strftime("%Y%m%d-%H%M%S")
  dir = File.join(__dir__, "bench/results/stress", stamp)
  FileUtils.mkdir_p(dir)
  filter = args[:filter] ? [args[:filter]] : []
  # The brace block swallows a non-zero exit (a per-core FAIL exits 1) so aggregation still runs —
  # every runner writes its JSONL before exiting, so the verdict survives in the result files.

  puts "go:   go run -race ./cmd/stress (one goroutine per worker, under the race detector)"
  sh({ "CGO_ENABLED" => "1" }, "go", "run", "-race", "./cmd/stress", STRESS_DIR, File.join(dir, "go.jsonl"), *filter, chdir: "bench/go") { |_ok, _res| }

  puts "rust: cargo build --release --bin stress + run over real OS threads"
  sh "cargo", "build", "--release", "--quiet", "--manifest-path", "bench/rust/Cargo.toml", "--bin", "stress"
  sh("bench/rust/target/release/stress", STRESS_DIR, File.join(dir, "rust.jsonl"), *filter) { |_ok, _res| }

  puts "ts:   node src/stress.ts (the seeded-sequential interleaver)"
  npm_ci_if_stale("bench/ts")
  sh("node", "bench/ts/src/stress.ts", STRESS_DIR, File.join(dir, "ts.jsonl"), *filter) { |_ok, _res| }

  puts
  aggregate_stress(dir)
end

# mutation — mutation-testing of the Go core (spec/design/mutation-testing.md). Inject deliberate
# bugs into the core's source and check the conformance corpus catches each one; a SURVIVING mutant
# is untested logic, located precisely. Like bench/stress, this is a slow ANALYSIS tool, deliberately
# OUTSIDE `rake ci` (not a merge gate): it answers "are we only testing the obvious?" with a map.
# Args are all optional and forward to the Go harness:
#   rake mutation                       # default targets (executor/operators/value/decimal/encoding), 300 sampled
#   rake mutation[value.go]             # scope to one file, full sweep that file
#   rake mutation[value.go,500,7]       # file, max mutants, sample seed
# A non-zero exit (survivors present) is swallowed — the printed report + JSONL are the deliverable.
desc "Mutation-test the Go core: inject bugs, check the conformance corpus catches them (OUTSIDE rake ci)"
task :mutation, [:files, :n, :seed] do |_, args|
  stamp = Time.now.utc.strftime("%Y%m%d-%H%M%S")
  dir = File.join(__dir__, "bench/results/mutation", stamp)
  FileUtils.mkdir_p(dir)
  flags = []
  flags += ["-files", args[:files]] if args[:files]
  flags += ["-n", args[:n]] if args[:n]
  flags += ["-seed", args[:seed]] if args[:seed]
  puts "go:   go run ./cmd/mutate (mutate the Go core; the conformance corpus is the oracle)"
  Dir.chdir(GO_DIR) { sh("go", "run", "./cmd/mutate", "-json", File.join(dir, "go.jsonl"), *flags) { |_ok, _res| } }
end

# conformance — the §7 contract: every core walks the hand-authored corpus
# (spec/conformance/suites) and must produce identical pass/fail. This is the spine of the
# project (CLAUDE.md §7/§10) — the example-based truth `rake test`/`rake ci` rest on. Each
# core's harness locates the corpus by walking up to spec/conformance/suites, so cwd only has
# to be inside the repo (Go needs impl/go for `./cmd/conformance`, Node the impl/ts scripts).
# Rust runs RELEASE: the debug evaluator's giant frames overflow the native stack on
# resource/depth_limit.test's deep nesting (the 256-depth limit is budgeted for the release stack).
namespace :conformance do
  desc "Walk the conformance corpus on the Rust core (release — debug overflows depth_limit)"
  task :rust do
    puts "conformance: rust"
    sh "cargo", "run", "--release", "--quiet", "--bin", "conformance", "--manifest-path", RUST_MANIFEST
  end

  desc "Walk the conformance corpus on the Go core"
  task :go do
    puts "conformance: go"
    Dir.chdir(GO_DIR) { sh "go", "run", "./cmd/conformance" }
  end

  desc "Walk the conformance corpus on the TS core"
  task :ts do
    puts "conformance: ts"
    Dir.chdir(TS_DIR) { sh "npm", "run", "--silent", "conformance" }
  end
end
desc "Walk the conformance corpus on all three cores (the §7 cross-core contract)"
task conformance: %w[conformance:rust conformance:go conformance:ts]

# unit — each core's per-language unit + integration suite: the checks the corpus CANNOT
# express (CLAUDE.md §10) — deliberate PG divergences, catalog/byte-level/cost introspection,
# host-API surface, internal invariants. Rust runs RELEASE for the same depth_limit.rs
# native-stack reason as conformance above.
namespace :unit do
  desc "Run the Rust core's unit + integration tests (release — depth_limit.rs)"
  task :rust do
    puts "unit: rust"
    sh "cargo", "test", "--release", "--manifest-path", RUST_MANIFEST
  end

  desc "Run the Go core's unit tests"
  task :go do
    puts "unit: go"
    Dir.chdir(GO_DIR) { sh "go", "test", "./..." }
  end

  desc "Run the TS core's unit tests"
  task :ts do
    puts "unit: ts"
    Dir.chdir(TS_DIR) { sh "npm", "run", "--silent", "test" }
  end
end
desc "Run every core's per-language unit + integration suite"
task unit: %w[unit:rust unit:go unit:ts]

# test — the engine test suites: example-based correctness against authored expectations. The
# conformance corpus on all three cores + each core's unit suite + the CLI golden tests. This is
# the inner dev loop; `rake ci` is the SUPERSET that wraps it with the static/spec/metamorphic
# gates below.
desc "Engine test suites: conformance corpus (×3 cores) + per-core unit tests + CLI + Ruby-gem tests"
task test: %w[conformance unit cli:test ruby:test]

# ci — the full merge gate, a SUPERSET of `rake test`. Adds the checks that aren't example-based
# tests: spec-data + byte-fixture verification + codegen-drift (`verify`), the formatter gate
# (`fmt`), the TS linter (`lint`), and generative differential testing — the NoREC/TLP metamorphic
# sweep + the reducer self-test. So `test` is example-based truth (corpus ×3 + unit + CLI); `ci`
# wraps it with static hygiene, spec integrity, and property/metamorphic fuzzing. Each step is
# `sh`/task-failure propagating, so `rake ci` exits non-zero on the first failure.
desc "Full merge gate: rake test (corpus ×3 + unit + CLI) + spec verify + fmt + lint + NoREC/TLP sweep + reducer self-test"
task ci: %w[verify fmt lint test] do
  Rake::Task["corpus:norec_sweep"].invoke
  Rake::Task["corpus:reduce_selftest"].invoke
end

namespace :references do
  desc "Clone/refresh reference mirrors on persist and check out worktrees into references/"
  task :setup do
    FileUtils.mkdir_p(MIRROR_ROOT)
    FileUtils.mkdir_p(WORKTREE_ROOT)
    for_each_repo do |repo|
      ensure_mirror(repo)
      ensure_worktree(repo)
    end
    puts
    puts "Done. Reference sources are in #{WORKTREE_ROOT}"
    Rake::Task["references:status"].invoke
  end

  desc "Fetch latest upstream for all mirrors and re-point worktrees"
  task :update do
    for_each_repo do |repo|
      abort "mirror missing for #{repo[:name]}; run `rake references:setup`" unless mirror_valid?(repo)
      git_bare(repo, "remote", "update", "--prune")
      ensure_worktree(repo)
    end
  end

  desc "Show provisioned reference repos, their pinned ref, and current HEAD"
  task :status do
    puts
    puts format("  %-16s %-14s %-14s %s", "REPO", "REF", "HEAD", "LICENSE")
    REFERENCE_REPOS.each do |repo|
      wp = worktree_path(repo)
      state =
        if worktree?(wp)
          head, ok = capture("git", "-C", wp, "rev-parse", "--short", "HEAD")
          ok && !head.strip.empty? ? head.strip : "(invalid)"
        else
          "(not set up)"
        end
      puts format("  %-16s %-14s %-14s %s", repo[:name], repo[:ref], state, repo[:license])
    end
    puts
    puts "  mirrors:   #{MIRROR_ROOT}"
    puts "  worktrees: #{WORKTREE_ROOT}"
  end

  desc "Remove worktrees from references/ (keeps the cached mirrors on persist)"
  task :clean do
    REFERENCE_REPOS.each do |repo|
      wp = worktree_path(repo)
      next unless worktree?(wp)
      puts "removing worktree: #{wp}"
      git_bare(repo, "worktree", "remove", "--force", wp)
    end
  end
end
