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

require "json"

verbose = ARGV.delete("-v")
dir = ARGV[0]
if dir.nil?
  runs = Dir.glob("bench/results/*").select { |d| File.directory?(d) }.sort
  abort "no results under bench/results/ — run `rake bench:run`" if runs.empty?
  dir = runs.last
end

results = Dir.glob(File.join(dir, "*.jsonl")).sort.flat_map do |path|
  File.readlines(path).map { |line| JSON.parse(line) }
end
abort "no results in #{dir}" if results.empty?

# --- answer + fingerprint verification (the part that can fail) ---

failures = []

results.group_by { |r| [r["bench"], r["dataset"]] }.each do |(bench, dataset), group|
  sums = group.map { |r| r["checksum"] }.uniq
  next if sums.size == 1

  failures << "checksum mismatch for #{bench} (#{dataset}):"
  group.group_by { |r| r["checksum"] }.each do |sum, rs|
    who = rs.map { |r| "#{r['engine']}/#{r['lang']}/#{r['variant']}" }.join(", ")
    failures << "  #{sum}: #{who}"
  end
end

fingerprints = results.map { |r| r["fingerprint"] }.uniq
if fingerprints.size > 1
  failures << "mixed fingerprints in one run dir (regenerate with `rake bench:setup` and re-run): #{fingerprints.join(', ')}"
end

# --- the comparison matrix ---

def humanize(ns)
  if ns >= 1_000_000_000 then format("%.2fs", ns / 1e9)
  elsif ns >= 1_000_000   then format("%.2fms", ns / 1e6)
  elsif ns >= 1_000       then format("%.1fµs", ns / 1e3)
  else                         "#{ns}ns"
  end
end

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
    (r ? humanize(r["ns_per_op"]) : "-").rjust(col_w)
  end
  puts "#{"#{bench} (#{dataset})".ljust(label_w)}  #{cells.join('  ')}"
  next unless verbose

  detail = columns.map do |c|
    r = by_col[c]
    (r ? "#{humanize(r['min_ns'])}/#{humanize(r['p50_ns'])}" : "-").rjust(col_w)
  end
  puts "#{'  min/p50'.ljust(label_w)}  #{detail.join('  ')}"
end

unless failures.empty?
  puts
  failures.each { |f| warn "FAIL: #{f}" }
  exit 1
end
