# frozen_string_literal: true

# scripts/bench_diff.rb — machine-readable before/after diff of two benchmark runs
# (spec/design/benchmarks.md §9), the one-command form of the CLAUDE.md §10 obligation
# to report both numbers around a perf-sensitive change. Built for tooling and AI
# agents: deterministic field order, no layout to parse.
#
#   ruby scripts/bench_diff.rb [run_dir] [baseline_dir] [--json] [--fail-over=PCT]
#
# `run_dir` defaults to the newest dir under bench/results/, `baseline_dir` to the run
# immediately preceding it. Default output is JSONL to stdout — one object per
# (bench, dataset, engine, lang, variant) joined across the two runs:
#
#   {"bench":…,"dataset":…,"engine":…,"lang":…,"variant":…,
#    "before_ns_per_op":…,"after_ns_per_op":…,"delta_pct":…,"checksum_match":…,
#    "before_only":false,"after_only":false}
#
# Pairs present in only one run keep the missing side null and set the corresponding
# `before_only`/`after_only` flag — partial/filtered runs are explicit, never dropped.
# A trailing {"summary":…} line carries the run dirs, fingerprints, and counts
# (improved/regressed are |Δ| ≥ 5%, the same noise floor the HTML report colors by).
# `--json` emits the same data as one pretty-printed document instead.
#
# Exit 1 on verification failure in either run (checksum mismatch / mixed
# fingerprints — wrong answers make the timings meaningless, so no diff is emitted).
# `--fail-over=PCT` additionally exits 2 if any matched pair regressed by more than
# PCT% — a regression gate for scripts ("did anything get >10% slower?"). Wall-clock
# is noisy and environment-relative; this gate is for operator use, never `rake ci`.

require_relative "bench_results"
require "json"

json_doc = ARGV.delete("--json")
fail_over = nil
ARGV.reject! do |arg|
  next false unless (m = arg.match(/\A--fail-over=(.+)\z/))

  fail_over = Float(m[1], exception: false)
  abort "bad --fail-over value: #{m[1]}" if fail_over.nil?
  true
end

run_dir, baseline_dir = BenchResults.resolve_dirs(ARGV[0], ARGV[1])
abort "no baseline run to diff against (need two runs, or pass one explicitly)" if baseline_dir.nil?

results = BenchResults.load_dir(run_dir)
baseline = BenchResults.load_dir(baseline_dir)

failures = BenchResults.verification_failures(results, baseline)
unless failures.empty?
  failures.each { |f| warn "FAIL: #{f}" }
  exit 1
end

rows = BenchResults.join_runs(results, baseline).map do |row|
  bench, dataset, engine, lang, variant = row[:key]
  {
    "bench" => bench, "dataset" => dataset,
    "engine" => engine, "lang" => lang, "variant" => variant,
    "before_ns_per_op" => row[:before]&.fetch("ns_per_op"),
    "after_ns_per_op" => row[:after]&.fetch("ns_per_op"),
    "delta_pct" => row[:delta_pct]&.round(1),
    "checksum_match" => row[:checksum_match],
    "before_only" => row[:after].nil?,
    "after_only" => row[:before].nil?,
  }
end

deltas = rows.filter_map { |r| r["delta_pct"] }
summary = {
  "run" => run_dir, "baseline" => baseline_dir,
  "run_fingerprint" => results.first["fingerprint"],
  "baseline_fingerprint" => baseline.first["fingerprint"],
  "fingerprint_match" => results.first["fingerprint"] == baseline.first["fingerprint"],
  "matched" => deltas.size,
  "improved" => deltas.count { |d| d <= -BenchResults::NOISE_PCT },
  "regressed" => deltas.count { |d| d >= BenchResults::NOISE_PCT },
  "noise" => deltas.count { |d| d.abs < BenchResults::NOISE_PCT },
  "before_only" => rows.count { |r| r["before_only"] },
  "after_only" => rows.count { |r| r["after_only"] },
}

if json_doc
  puts JSON.pretty_generate({ "results" => rows, "summary" => summary })
else
  rows.each { |r| puts JSON.generate(r) }
  puts JSON.generate({ "summary" => summary })
end

exit 2 if fail_over && deltas.any? { |d| d > fail_over }
