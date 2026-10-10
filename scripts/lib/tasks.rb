# frozen_string_literal: true

# scripts/lib/tasks.rb — shared helpers for the Ruby file tasks under mise-tasks/ (CLAUDE.md §10).
#
# mise is the task runner: mise.toml declares the composite tasks and straight-line commands as
# shell, and every task with real logic is an executable Ruby script under mise-tasks/
# (mise-tasks/stress is `mise run stress`). Each script requires this file for the repo layout and
# the few process helpers they share; scripts/npm_ci.rb exposes npm_ci_if_stale to the shell tasks. mise runs a task with the repo root as its working directory and with
# mise.toml's [env] loaded, so these helpers need not resolve either themselves.
#
# Deliberately NOT loaded here: bundler. The task scripts use only Ruby's standard library, and a
# loaded bundler would leak its environment into child processes, restricting any Ruby child to the
# Gemfile's gems. The spec and
# codegen scripts that need toml-rb load it themselves (scripts/lib/bundle_setup.rb).

require "digest"
require "fileutils"
require "rbconfig"
require "shellwords"
require_relative "dev_paths"

module Tasks
  module_function

  ROOT = DevPaths::ROOT

  GO_DIR = File.join(ROOT, "impl/go")
  TS_DIR = File.join(ROOT, "impl/ts")
  RUST_MANIFEST = File.join(ROOT, "impl/rust/Cargo.toml")
  WEB_DIR = File.join(ROOT, "web")
  STRESS_DIR = File.join(ROOT, "stress")

  # What the formatter gate covers — fmt:check checks it and fmt:fix rewrites it
  # (mise-tasks/fmt/check documents the pinned tools).
  FMT_CARGO_CRATES = {
    "rust" => "impl/rust/Cargo.toml",
    "ts-lock" => "impl/ts/native-lock/Cargo.toml",
    "cli" => "cli/Cargo.toml",
    "ruby-ext" => "impl/ruby/ext/Cargo.toml",   # the jed Ruby gem's native extension
    "wasm" => "impl/wasm/Cargo.toml",           # the wasm32-wasip1 wrap of the core
    "node-wrap" => "impl/node/Cargo.toml",      # the experimental Node-API wrap of the core
    "migrate-rust" => "migrate/rust/Cargo.toml", # jed-migrate, a consumer of the engine
  }.freeze
  FMT_GO_DIRS = %w[impl/go migrate/go].freeze
  FMT_TS_DIRS = %w[impl/ts impl/node bench/ts migrate/ts].freeze # biome.json excludes generated files

  # Print a command the way a shell would show it, then run it. Aborts the task on failure.
  # `env` adds variables for the child; `chdir` runs it elsewhere than the repo root.
  def sh(*cmd, env: {}, chdir: nil)
    abort "command failed (#{$?.exitstatus || $?}): #{cmd.shelljoin}" unless sh?(*cmd, env: env, chdir: chdir)
  end

  # Like `sh`, but returns whether the command succeeded instead of aborting — for gates that
  # collect every failure before reporting, and for analysis runs whose non-zero exit is a result.
  def sh?(*cmd, env: {}, chdir: nil)
    prefix = env.map { |k, v| "#{k}=#{v.to_s.shellescape}" }
    where = chdir ? "(cd #{chdir.to_s.delete_prefix("#{ROOT}/")}) " : ""
    $stdout.puts "#{where}#{[*prefix, cmd.shelljoin].join(' ')}"
    $stdout.flush
    opts = chdir ? { chdir: chdir } : {}
    system(env.transform_values(&:to_s), *cmd.map(&:to_s), **opts)
  end

  # Run a Ruby script with the interpreter running this task.
  def ruby(*args, **opts) = sh(RbConfig.ruby, *args, **opts)

  # Capture a command's stdout (argv form: no shell parsing). Returns [stdout, success].
  def capture(*args)
    out = IO.popen(args, err: File::NULL, &:read)
    [out.to_s, $?.success?]
  end

  # A UTC timestamp for a results directory name.
  def stamp = Time.now.utc.strftime("%Y%m%d-%H%M%S")

  # Bootstrap an npm project's deps with `npm ci` (reproducible from its committed lockfile —
  # CLAUDE.md §14), reinstalling whenever they are STALE — not only when node_modules is absent.
  # Stale = node_modules missing, OR package-lock.json changed since the last install (a new /
  # updated / removed dependency). We stamp the lockfile's content hash inside node_modules after
  # each successful install and compare against it; hashing the CONTENT (not the mtime) keeps the
  # check correct across git checkouts, and the stamp is wiped whenever node_modules is (including
  # by `npm ci` itself, which removes it first). Without this a gate silently skips the install
  # when the lockfile grows a dependency — the `prettier: not found` foot-gun.
  def npm_ci_if_stale(dir)
    dir = File.expand_path(dir, ROOT)
    modules = File.join(dir, "node_modules")
    lock = File.join(dir, "package-lock.json")
    stamp_file = File.join(modules, ".jed-deps-stamp")
    want = File.exist?(lock) ? Digest::SHA256.file(lock).hexdigest : ""
    return if File.directory?(modules) && File.exist?(stamp_file) && File.read(stamp_file) == want

    sh "npm", "ci", "--silent", "--prefix", dir
    File.write(stamp_file, want)
  end
end
