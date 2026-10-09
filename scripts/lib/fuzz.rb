# frozen_string_literal: true

# scripts/lib/fuzz.rb — coverage-guided fuzzing of the Go core's crash / corruption durability
# surface (the fuzz and fuzz:one tasks). The oracle is INTRINSIC — never crash, never loop, fail
# closed (no differential needed) — so these live in Go, the maintainer's `go test -fuzz` daily
# driver. Like bench/stress/mutation this is a slow EXPLORER, deliberately OUTSIDE `mise run ci`;
# the fuzz seed corpora still run inside `mise run ci` via `unit:go` (every f.Add seed executes
# under a plain `go test`), so the paths are covered there — `-fuzz` just explores wider.

require_relative "tasks"

module Fuzz
  module_function

  TARGETS = %w[FuzzCorruptFile FuzzCommitCrash].freeze
  DEFAULT_TIME = "60s"

  def run(name, time)
    puts "go test -fuzz=^#{name}$ -fuzztime=#{time} (intrinsic oracle: no crash/hang, fail closed)"
    Tasks.sh "go", "test", "-run", "^$", "-fuzz", "^#{name}$", "-fuzztime", time, "./", chdir: Tasks::GO_DIR
  end
end
