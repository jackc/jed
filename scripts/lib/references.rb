# frozen_string_literal: true

# scripts/lib/references.rb — local, read-only checkouts of reference databases (PostgreSQL,
# SQLite, DuckDB, …) used as differential-testing oracles and as design references (CLAUDE.md §7,
# §8, §12). Shared by the references:* tasks (mise-tasks/references/).
#
# Storage model (CLAUDE.md §12):
#   * A bare `--mirror` clone of each repo lives OUTSIDE any checkout, under MIRROR_ROOT. It holds
#     the full history + all branches/tags, is downloaded once, and survives container rebuilds. It
#     is the canonical copy, shared across every checkout of this project on the machine.
#   * Each checkout gets a `git worktree` at ./references/<name>. The worktree shares the mirror's
#     object store (no re-download, no Nx history) but has its own detached HEAD, so one checkout
#     can sit on a different branch/tag without disturbing the mirror or any other checkout.
#
# Provisioning a new checkout is therefore cheap: `git worktree add` against the already-present
# mirror, no network fetch.
#
# "Checkout" is deliberate: it means a devcontainer instance AND a native git worktree. The two
# differ only in where the shared mirror lives, which is what default_mirror_root resolves — the
# per-checkout half is identical either way.
#
# Note: the devcontainer sets `safe.bareRepository = explicit` globally, so every command that
# touches a bare mirror must name it with `--git-dir` AND override the guard with
# `-c safe.bareRepository=all`. git_bare does both.

require_relative "tasks"

module References
  module_function

  # Each entry is one reference repo. `ref` is the branch/tag checked out into the worktree; it is
  # explicit (not auto-detected) per CLAUDE.md's "boring, explicit" preference. PostgreSQL is pinned
  # to REL_18_STABLE to match the oracle's PG major (spec/conformance/oracle_profile.toml). All
  # licenses are free/OSS.
  REPOS = [
    { name: "postgres",        url: "https://github.com/postgres/postgres.git",            ref: "REL_18_STABLE", license: "PostgreSQL License" },
    { name: "sqlite",          url: "https://github.com/sqlite/sqlite.git",                 ref: "master",        license: "Public Domain"     },
    { name: "duckdb",          url: "https://github.com/duckdb/duckdb.git",                 ref: "main",          license: "MIT"               },
    { name: "bbolt",           url: "https://github.com/etcd-io/bbolt.git",                 ref: "main",          license: "MIT"               },
    { name: "sqllogictest-rs", url: "https://github.com/risinglightdb/sqllogictest-rs.git", ref: "main",          license: "MIT / Apache-2.0"  },
  ].freeze

  # Where the canonical bare mirrors live. They are machine-level state, not checkout-level:
  # multiple GB, read-only, and identical for every checkout — so they belong outside the tree, and
  # every checkout on the machine shares one copy.
  #
  #   * devcontainer — the shared persist volume, so every container for this project reuses one
  #     download and a rebuild costs nothing.
  #   * native — an XDG data directory under $HOME, where git worktrees of this repo share it the
  #     same way containers share the volume.
  #
  # REFERENCES_MIRROR_DIR overrides both (a second disk, a scratch location, a test).
  def default_mirror_root
    return "/persist/shared/references" if File.directory?("/persist/shared")

    xdg = ENV["XDG_DATA_HOME"]
    base = xdg.nil? || xdg.empty? ? File.join(Dir.home, ".local", "share") : xdg
    File.join(base, "jed", "references")
  end

  MIRROR_ROOT = ENV.fetch("REFERENCES_MIRROR_DIR") { default_mirror_root }
  WORKTREE_ROOT = File.join(Tasks::ROOT, "references")

  def mirror_path(repo) = File.join(MIRROR_ROOT, "#{repo[:name]}.git")
  def worktree_path(repo) = File.join(WORKTREE_ROOT, repo[:name])

  # A worktree dir is a real worktree if it has the `.git` gitdir pointer file.
  def worktree?(path) = File.exist?(File.join(path, ".git"))

  # Run a git command against a bare mirror. Names the gitdir explicitly and lifts the
  # safe.bareRepository=explicit guard (see header note). Aborts on failure.
  def git_bare(repo, *args)
    Tasks.sh "git", "-c", "safe.bareRepository=all", "--git-dir", mirror_path(repo), *args
  end

  # A mirror is valid only if it exists AND has at least one ref — this rejects a directory left
  # behind by an interrupted clone (which exists but is incomplete).
  def mirror_valid?(repo)
    return false unless File.directory?(mirror_path(repo))

    out, ok = Tasks.capture("git", "-c", "safe.bareRepository=all", "--git-dir", mirror_path(repo),
                            "for-each-ref", "--count=1")
    ok && !out.strip.empty?
  end

  # Clone the bare mirror if it is missing or broken.
  def ensure_mirror(repo)
    if mirror_valid?(repo)
      puts "  mirror cached: #{mirror_path(repo)}"
    else
      FileUtils.rm_rf(mirror_path(repo)) # clear any partial/broken clone
      puts "  cloning mirror (full history): #{repo[:url]}"
      Tasks.sh "git", "clone", "--mirror", repo[:url], mirror_path(repo)
    end
  end

  # Check out (or re-point) the worktree under references/ at the configured ref. Detached HEAD
  # keeps it purely a read-only reference checkout and avoids the "branch already checked out"
  # conflict with any other worktree.
  def ensure_worktree(repo)
    wp = worktree_path(repo)
    ref = repo[:ref]

    git_bare(repo, "worktree", "prune") # drop registrations for deleted worktrees

    if worktree?(wp)
      puts "  worktree present: #{wp} -> #{ref}"
      Tasks.sh "git", "-C", wp, "checkout", "--detach", ref
    else
      FileUtils.rm_rf(wp) if File.exist?(wp) # clear any non-worktree leftovers
      puts "  adding worktree: #{wp} -> #{ref}"
      git_bare(repo, "worktree", "add", "--detach", wp, ref)
    end
  end

  # Run a block per repo, collecting failures so one bad repo does not abort the rest. Tasks.sh
  # aborts by raising SystemExit, so that is what a failed repo surfaces as.
  def each_repo
    failures = []
    REPOS.each do |repo|
      puts "#{repo[:name]}:"
      begin
        yield repo
      rescue StandardError, SystemExit => e
        warn "  FAILED: #{e.message}"
        failures << repo[:name]
      end
    end
    abort "references: failed for #{failures.join(', ')}" unless failures.empty?
  end

  def print_status
    puts
    puts format("  %-16s %-14s %-14s %s", "REPO", "REF", "HEAD", "LICENSE")
    REPOS.each do |repo|
      wp = worktree_path(repo)
      state =
        if worktree?(wp)
          head, ok = Tasks.capture("git", "-C", wp, "rev-parse", "--short", "HEAD")
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
end
