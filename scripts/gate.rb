# frozen_string_literal: true

# scripts/gate.rb — `mise run test` and `mise run ci`: run every step of the gate at once.
#
#   ruby scripts/gate.rb test|ci
#
# Each step is a `mise run <task>` child process. The steps are independent, so they run
# concurrently; each one's output is buffered and printed in the order listed below
# (scripts/lib/parallel.rb), then a table of every step's result and time. Every step runs even when
# another fails, and the gate fails if any did.
#
# Steps that already fan out (conformance:all, concurrency:process, unit:ts) share the machine with
# the rest; the OS schedules them. The one constraint is the corpus tree: corpus:norec_sweep and
# corpus:reduce_selftest WRITE files into spec/conformance/suites, which `verify` and
# conformance:all READ, so they start only after those finish. Likewise e2e:web builds the website
# (`vite build` rewrites web/.svelte-kit), which web:check's `svelte-kit sync` also writes, so it
# starts after web:check.
#
# `ci` is `test` plus the steps below it, by construction: add a step to test_steps and both gates run it.

require "tmpdir"
require_relative "lib/parallel"
require_relative "lib/tasks"
include Tasks

$stdout.sync = true # keep stdout in order with the abort message on stderr

def step(task, *args, after: [])
  Parallel::Job.new(name: [task, *args].join(" "), argv: ["mise", "run", task, *args], chdir: ROOT, after: after)
end

# Example-based correctness against authored expectations — the inner dev loop.
def test_steps(checked:)
  [
    step("conformance:all", *("--checked" if checked)),
    step("unit"),
    step("cli:test"),
    step("ruby:test"),
    step("migrate:test"),
  ]
end

# The merge gate: `test` (with the Rust corpus walks under debug assertions + overflow checks) plus
# spec-data + byte-fixture verification + codegen drift, formatting, the TS linter, the downstream
# web type check, the real-process shared-file corpus, deterministic simulation (the regression
# traces plus a fixed sweep), the real-browser e2e suites (headless Chromium: the TS core's OPFS host
# and the website), and generative differential testing.
def ci_steps
  corpus_readers = ["verify", "conformance:all --checked"]
  [
    step("verify"),
    step("fmt"),
    step("lint"),
    step("web:check"),
    *test_steps(checked: true),
    step("concurrency:process"),
    step("sim:regress"),
    step("sim:sweep", "1..8"),
    step("e2e:ts"),
    step("e2e:web", after: ["web:check"]),
    step("corpus:norec_sweep", after: corpus_readers),
    step("corpus:reduce_selftest", after: corpus_readers),
  ]
end

# Start the longest steps first; the rest fit around them.
SLOW = ["conformance:all", "concurrency:process", "verify", "unit"].freeze

gate = ARGV.fetch(0) { abort "usage: ruby scripts/gate.rb test|ci" }
steps = case gate
        when "test" then test_steps(checked: false)
        when "ci" then ci_steps
        else abort "unknown gate #{gate.inspect} (have: test, ci)"
        end

started = Parallel.now
Dir.mktmpdir("jed-gate") do |dir|
  rank = ->(job) { SLOW.index(job.argv[2]) || SLOW.size }
  Parallel.run(steps, dir: dir, rank: rank) do |job|
    puts "==> #{job.name}"
    File.foreach(job.log) { |line| puts line }
    puts "==> #{job.name}: #{job.status ? 'ok' : 'FAILED'} (#{format('%.1f', job.seconds)}s)\n\n"
    $stdout.flush
  end
end

puts "#{gate}: #{steps.size} steps in #{format('%.1f', Parallel.now - started)}s"
steps.each { |job| puts format("  %-6s %-30s %6.1fs", job.status ? "ok" : "FAILED", job.name, job.seconds) }
failed = steps.reject(&:status)
abort "#{gate} failed: #{failed.map(&:name).join(', ')}" unless failed.empty?
