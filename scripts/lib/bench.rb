# frozen_string_literal: true

# scripts/lib/bench.rb — what the bench:* file tasks (mise-tasks/bench/) share: the per-language
# harness binaries and the report step every run ends with (spec/design/benchmarks.md).

require_relative "tasks"

module Bench
  module_function

  GO_BINS = %w[bench-jed bench-pg bench-sqlite bench-sqlite-cgo].freeze
  RUST_BINS = %w[bench-jed bench-pg bench-sqlite].freeze
  TS_BINS = %w[bench-jed bench-pg bench-sqlite].freeze

  # A fresh results directory, bench/results/<stamp><suffix>.
  def results_dir(suffix = "")
    dir = File.join("bench/results", "#{Tasks.stamp}#{suffix}")
    FileUtils.mkdir_p(dir)
    dir
  end

  # Aggregate `dir` into the comparison table (fails on any cross-engine checksum disagreement),
  # then render it with `renderer` — scripts/bench_html.rb or scripts/bench_markdown.rb.
  def report(dir, renderer)
    Tasks.ruby "scripts/bench_report.rb", dir
    Tasks.ruby renderer, dir
  end
end
