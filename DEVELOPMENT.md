# Developing jed

Two supported environments, one command set. Native macOS is the fast inner loop; the
devcontainer is a Linux compatibility and isolation option. Both run the same mise tasks against
the same `process-compose.yaml`, so nothing here is specific to one of them except the
prerequisites in §1.

```
mise install        # tool versions from mise.toml
mise run dev:init   # gems, npm deps, this checkout's ports, the wasm target
mise run dev        # start this checkout's services (the PostgreSQL oracle)
mise run ci         # the full merge gate
```

---

## 1. Prerequisites

### Native macOS

[mise](https://mise.jdx.dev) provides Go, Node, Rust, Ruby, Biome, process-compose, and
port-tamer. Homebrew provides what mise does not:

```sh
brew install mise postgresql@18 sqlite
xcode-select --install          # C toolchain: cgo bench baseline, the Node-API lock adapter, Ruby
```

- **`postgresql@18`** is the differential oracle (§3). Do **not** `brew services start` it — jed
  runs its own cluster per checkout. The formula is *keg-only*, so `psql` and `pg_isready` are not
  on your `PATH` after install; `mise.toml` adds the formula's `bin` to `PATH` for this project, so
  project commands work without linking it globally.
- **Ruby** is built from source by mise. If the build fails, it is almost always a missing library:
  `brew install openssl@3 libyaml readline libffi` and retry.
- The major version tracks `[cluster] pg_major` in `spec/conformance/oracle_profile.toml`. If that
  ever moves, install the matching formula.

### Devcontainer

Reopen in the container; `.devcontainer/` handles the rest. It installs the same PostgreSQL 18
server and client, and runs the same per-checkout cluster — it is a Linux shell around this same
setup, not a second architecture.

---

## 2. Worktrees are the unit of isolation

A git worktree is the native equivalent of a second devcontainer instance. Each one gets:

```
.dev/                   # gitignored, per-checkout runtime state
  ports.env             # this checkout's TCP ports (port-tamer's state file)
  derived.env           # PostgreSQL defaults — PGHOST, PGUSER, PGDATABASE
  go-build/             # Go build cache (unless GOCACHE is explicitly set)
  postgres/data         # this checkout's PostgreSQL cluster
```

```sh
git worktree add ../jed-feature-x feature-x
cd ../jed-feature-x
mise run dev:init && mise run dev
```

Both checkouts can run simultaneously: different ports, independent database state, independent
process-compose instances. `mise run dev:ports` prints the allocation.

Ports are allocated once per checkout by [port-tamer](https://github.com/jackc/port-tamer) and then
persisted. `port-tamer.toml` declares which ports a checkout needs (append new entries at the end —
inserting or reordering renumbers the existing ones); the allocation itself lands in `.dev/`, which
mise loads. `mise run dev:init` creates it, and `dev:ports:ensure` is idempotent afterwards.

Go builds default to `.dev/go-build` through mise's `[env]`, so sandboxed CI does not need
write access to the home-directory cache. An explicit `GOCACHE` takes precedence; if it names a
read-only location, unset it to use the checkout default or point it at a writable cache.

A listening port never moves an existing allocation — it may well belong to this checkout's own
running services. When two checkouts genuinely collide, move one deliberately: stop its services
and run `mise run dev:ports:overwrite`.

**Reference sources** (`references/`, §12 of `CLAUDE.md`) are provisioned per checkout with `mise run
references:setup`, sharing one machine-level mirror. They are a multi-GB download; nothing
provisions them automatically.

---

## 3. The PostgreSQL oracle

jed's conformance corpus is filled and checked against a live PostgreSQL (`CLAUDE.md` §7). That
server is **this checkout's own cluster**, not a shared service:

```sh
mise run dev            # starts it (process-compose supervises)
mise run db:psql        # a shell against it
mise run oracle:status  # declared profile vs the live server
```

Its configuration is **declared data** — `spec/conformance/oracle_profile.toml`. The corpus is
calibrated to that profile, so the harness asserts it at connect and aborts on a mismatch rather
than importing different answers. Consequences worth internalising:

- **Corpus, RQG, and benchmark work need the stack running.** "oracle unreachable" almost always
  means you have not run `mise run dev` in this checkout.
- **`PGHOST` and `PGPORT` are a pair.** They come from `.dev/` via mise — `PGPORT` from the
  allocation, `PGHOST` derived from it. Setting one without the other names a real port on the
  wrong server, and the error will name a socket path nothing ever created. The same generated
  environment sets `PGUSER=postgres` and `PGDATABASE=postgres`, matching the role and database
  created by `initdb` on both macOS and in the devcontainer.
- **Changing a profile value changes what the oracle answers.** Treat it as a spec edit and re-run
  `mise run corpus:check` over the oracle-checkable corpus.
- When sweeping many corpus files, `mise run oracle:reset` between them — a `.test` carrying its own
  transaction control commits its tables for real, and later files then replay onto a dirty
  database in a way that looks exactly like a regression.

---

## 4. Everyday commands

| Command | What it does |
|---|---|
| `mise run ci` | the full merge gate (spec verification, formatting, lint, tests, process locking) |
| `mise run test` | conformance corpus on all three cores + unit + CLI + gem + migrate |
| `mise run verify` | spec data tables and byte fixtures; needs no engine build |
| `mise run fmt` / `fmt:fix` | formatting across cores, host artifacts, tooling, and web |
| `mise run dev` | start this checkout's services |
| `mise run dev:ports` | this checkout's port allocation, and what is listening on it |
| `mise run db:init` / `db:psql` | create / open this checkout's cluster |
| `mise run oracle:status` / `oracle:setup` / `oracle:reset` | the oracle's profile and database |
| `mise run dev:browsers` | Chromium for the two Playwright suites (~150 MB, not needed by `ci`) |

`mise tasks` lists every task, including the ones not in this table (`bench:*`, `corpus:*`,
`rqg:*`, `stress`, `fuzz`, `mutation`, `references:*`), and `mise run <task> --help` shows a task's
arguments. There is no Rakefile: composite tasks and straight-line commands are shell in `mise.toml`, and
every task with real logic is a Ruby script under `mise-tasks/` (`mise-tasks/stress` is `stress`).

Services are managed by process-compose, not bespoke tasks:

```sh
process-compose process list             # status, scriptable
process-compose process logs postgres    # logs for one process
process-compose process start web        # the website dev server (disabled by default)
process-compose down                     # stop this checkout's stack only
```

Each checkout runs its own instance on its own control port, which process-compose reads from
`PC_PORT_NUM`, so these need no flags and never reach another checkout's stack.

---

## 5. Platform notes

- **Benchmarks are not comparable across platforms.** macOS `fsync` goes through `F_FULLFSYNC`,
  which is far more expensive than Linux `fdatasync`, and the Go core falls back to a full `Sync`
  off Linux. Durable-write numbers taken on macOS cannot be compared with the Linux figures
  recorded in `spec/design/benchmarks.md`. Treat Linux as the canonical benchmark platform.
- **Corpus authoring is platform-independent** — deliberately. The oracle uses PostgreSQL's
  `builtin` `C.UTF-8` locale provider, which consults no host library, so a macOS cluster and a
  Linux one answer identically. That is why a native oracle is safe here.
- **The devcontainer forwards 5173/4173** as fallbacks. With a port allocation (the normal case)
  the web servers bind the allocated ports instead, which VS Code detects and forwards. Unset
  `WEB_DEV_PORT` / `WEB_PREVIEW_PORT` to pin them back.
