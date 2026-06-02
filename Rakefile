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

# Bare `rake` is read-only on purpose — provisioning (multi-GB clones) must be
# explicit via `rake references:setup`.
task default: "references:status"

# verify — run the spec's data-table checkers (no engine required). Each checker is
# an independent reference implementation that recomputes values from the rules and
# asserts the canonical fixtures match (CLAUDE.md §5, §8). Add new checks here as
# subsystems gain verifiable data.
desc "Verify the spec data tables and byte fixtures"
task :verify do
  checks = [
    ["key encoding", "spec/encoding/verify.rb"],
    ["conformance taxonomy", "spec/conformance/verify.rb"],
    ["file format", "spec/fileformat/verify.rb"],
    ["function catalog", "spec/functions/verify.rb"],
  ]
  failures = []
  checks.each do |name, script|
    puts "#{name}: #{script}"
    failures << name unless system(RbConfig.ruby, script)
  end
  abort "verify: failed for #{failures.join(', ')}" unless failures.empty?
  puts "\nAll spec checks passed."
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
