# frozen_string_literal: true

# scripts/bench_report.rb — aggregate one benchmark run's JSONL results into a comparison
# table (spec/design/benchmarks.md §9). Reads every *.jsonl in the given results dir
# (default: the newest under bench/results/), groups by (bench, dataset), and prints a
# fixed-width ns_per_op matrix with one column per engine/lang/variant.
#
#   ruby scripts/bench_report.rb [results_dir] [-v]
#
# Exits 1 if any two results for the same (bench, dataset) disagree on `checksum` — a
# wrong answer somewhere, treated like a failing conformance test — or if results in one
# run dir carry different `fingerprint`s (mixed-vintage data). Wall-clock values are
# never judged; only answers are.

require_relative "bench_results"

verbose = ARGV.delete("-v")
dir = ARGV[0]
if dir.nil?
  runs = BenchResults.run_dirs
  abort "no results under bench/results/ — run `rake bench:run`" if runs.empty?
  dir = runs.last
end

results = BenchResults.load_dir(dir)

# --- answer + fingerprint verification (the part that can fail) ---

failures = BenchResults.checksum_failures(results)
mixed = BenchResults.mixed_fingerprints(results)
failures << mixed if mixed

# --- the comparison matrix ---

columns = results.map { |r| "#{r['engine']}/#{r['lang']}/#{r['variant']}" }.uniq.sort
rows = results.group_by { |r| [r["bench"], r["dataset"]] }

label_w = (rows.keys.map { |b, d| "#{b} (#{d})".size } + [10]).max
col_w = (columns.map(&:size) + [10]).max

puts "results: #{dir}"
puts
puts "#{' ' * label_w}  #{columns.map { |c| c.rjust(col_w) }.join('  ')}"
rows.each do |(bench, dataset), group|
  by_col = group.to_h { |r| ["#{r['engine']}/#{r['lang']}/#{r['variant']}", r] }
  cells = columns.map do |c|
    r = by_col[c]
    (r ? BenchResults.humanize(r["ns_per_op"]) : "-").rjust(col_w)
  end
  puts "#{"#{bench} (#{dataset})".ljust(label_w)}  #{cells.join('  ')}"
  next unless verbose

  detail = columns.map do |c|
    r = by_col[c]
    (r ? "#{BenchResults.humanize(r['min_ns'])}/#{BenchResults.humanize(r['p50_ns'])}" : "-").rjust(col_w)
  end
  puts "#{'  min/p50'.ljust(label_w)}  #{detail.join('  ')}"
end

unless failures.empty?
  puts
  failures.each { |f| warn "FAIL: #{f}" }
  exit 1
end
