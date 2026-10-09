# frozen_string_literal: true

# scripts/lib/tasks.rb — shared helpers for the Ruby file tasks under mise-tasks/ (CLAUDE.md §10).
#
# mise is the task runner: mise.toml declares the composite and one-line tasks, and every task
# with real logic is an executable Ruby script under mise-tasks/ (mise-tasks/bench/run is
# `mise run bench:run`). Each script requires this file for the repo layout and the few process
# helpers they share. mise runs a task with the repo root as its working directory and with
# mise.toml's [env] loaded, so these helpers need not resolve either themselves.
#
# Deliberately NOT loaded here: bundler. The task scripts use only Ruby's standard library, and a
# loaded bundler would leak its environment into child processes (the minitest and bench-gem
# children need Ruby's bundled gems, which a Gemfile-restricted child cannot load). The spec and
# codegen scripts that need toml-rb load it themselves (scripts/lib/bundle_setup.rb).

require "digest"
require "fileutils"
require "rbconfig"
require "shellwords"
require_relative "dev_paths"

module Tasks
  module_function

  ROOT = DevPaths::ROOT

  RUST_MANIFEST = File.join(ROOT, "impl/rust/Cargo.toml")
  CLI_MANIFEST = File.join(ROOT, "cli/Cargo.toml")
  GO_DIR = File.join(ROOT, "impl/go")
  TS_DIR = File.join(ROOT, "impl/ts")
  # The jed Ruby gem (a host artifact; spec/design/ruby.md) and its native-extension cdylib crate.
  RUBY_GEM_DIR = File.join(ROOT, "impl/ruby")
  RUBY_EXT_MANIFEST = File.join(RUBY_GEM_DIR, "ext/Cargo.toml")
  # The wasm32-wasip1 wrap of the core (a host artifact; impl/wasm/README.md).
  WASM_MANIFEST = File.join(ROOT, "impl/wasm/Cargo.toml")
  WASM_TARGET = "wasm32-wasip1"
  # The experimental native Node-API wrap of the Rust core (spec/design/benchmarks.md §7.3).
  NODE_WRAP_DIR = File.join(ROOT, "impl/node")
  NODE_WRAP_MANIFEST = File.join(NODE_WRAP_DIR, "Cargo.toml")
  NODE_WRAP_MODULE = File.join(NODE_WRAP_DIR, "jed_node.node")
  # The TypeScript core's narrow native OS-lock adapter (spec/design/locking.md §8).
  TS_LOCK_MANIFEST = File.join(TS_DIR, "native-lock/Cargo.toml")
  TS_LOCK_MODULE = File.join(TS_DIR, "jed_lock.node")
  # jed-migrate (/migrate/design.md): a consumer of the engine per language, not a core.
  MIGRATE_GO_DIR = File.join(ROOT, "migrate/go")
  MIGRATE_RUST_MANIFEST = File.join(ROOT, "migrate/rust/Cargo.toml")
  MIGRATE_TS_DIR = File.join(ROOT, "migrate/ts")
  WEB_DIR = File.join(ROOT, "web")
  STRESS_DIR = File.join(ROOT, "stress")

  # The platform file name of a Rust cdylib built from crate `name`.
  def cdylib(name)
    case RbConfig::CONFIG["host_os"]
    when /darwin/ then "lib#{name}.dylib"
    when /mswin|mingw|cygwin/ then "#{name}.dll"
    else "lib#{name}.so"
    end
  end

  NODE_WRAP_ARTIFACT = File.join(NODE_WRAP_DIR, "target/release", cdylib("jed_node"))
  TS_LOCK_ARTIFACT = File.join(TS_DIR, "native-lock/target/release", cdylib("jed_lock"))

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
