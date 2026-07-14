# frozen_string_literal: true

# scripts/bench_markdown.rb — render one benchmark run as Markdown (spec/design/
# benchmarks.md §9): the same data as the bench_html.rb report (the two share
# BenchResults.report_model), in a form readable both as plain text at the terminal
# and rendered in the VS Code markdown preview. Prints the markdown to stdout and also
# writes <run_dir>/report.md; table cells are space-padded so the raw text aligns at
# the terminal (GFM ignores the padding).
#
#   ruby scripts/bench_markdown.rb [run_dir] [baseline_dir] [--no-baseline]
#
# Same defaults and verification as bench_html.rb: `run_dir` defaults to the newest dir
# under bench/results/, `baseline_dir` to the run immediately preceding it
# (--no-baseline suppresses the comparison). Exits 1 on any checksum mismatch or mixed
# fingerprints (in the run or the baseline) — the report is still emitted, with the
# failures listed up top.

require_relative "bench_results"

no_baseline = ARGV.delete("--no-baseline")
run_dir, baseline_dir = BenchResults.resolve_dirs(ARGV[0], ARGV[1], baseline: !no_baseline)

results = BenchResults.load_dir(run_dir)
baseline = baseline_dir && BenchResults.load_dir(baseline_dir)
failures = BenchResults.verification_failures(results, baseline)

run_fp = results.first["fingerprint"]
baseline_fp = baseline&.first&.fetch("fingerprint")

joined = baseline ? BenchResults.join_runs(results, baseline) : []
model = BenchResults.report_model(results, joined)

# One block character per 5% of the group's slowest, so the slowest is 20 wide —
# the markdown analogue of the HTML bar.
def bar(width_pct)
  "█" * [(width_pct / 5.0).round, 1].max
end

# A GFM table with space-padded cells: column 0 (and the bar) left-aligned, the rest
# right-aligned, the separator row carrying the matching `---:` alignment markers.
def table(headers, rows, right_from: 2)
  widths = headers.each_index.map { |i| ([headers[i]] + rows.map { |r| r[i] }).map(&:size).max }
  line = ->(cells, pad) { "| #{cells.each_with_index.map { |c, i| c.send(pad.call(i), widths[i]) }.join(' | ')} |" }
  align = ->(i) { i < right_from ? :ljust : :rjust }
  sep = widths.each_with_index.map { |w, i| i < right_from ? "-" * w : "#{'-' * (w - 1)}:" }
  [line.call(headers, align), "| #{sep.join(' | ')} |"] + rows.map { |r| line.call(r, align) }
end

out = []
out << "# jed benchmarks — #{File.basename(run_dir)}"
out << ""
meta = []
meta << "baseline: #{File.basename(baseline_dir)}" if baseline_dir
meta << "fingerprint #{run_fp[0, 12]}"
meta << "#{results.size} results"
meta << "generated #{Time.now.utc.strftime('%Y-%m-%dT%H:%M:%SZ')}"
out << meta.join(" · ")
out << ""
if failures.empty?
  out << "✓ all #{results.size} results agree on answer checksums"
else
  out << "**FAIL:**"
  out << ""
  failures.each { |f| out << "- #{f}" }
end
if baseline && baseline_fp != run_fp
  out << ""
  out << "> ⚠ baseline has a different data fingerprint (#{baseline_fp[0, 12]} vs " \
         "#{run_fp[0, 12]}) — the runs measured different data; deltas may not be comparable"
end

model[:sections].each do |s|
  out << ""
  out << "### #{s[:bench]} (#{s[:dataset]})"
  out << ""
  desc = []
  desc << s[:description] if s[:description]
  desc << "`#{s[:sql]}`" if s[:sql]
  desc << s[:subtitle]
  out << desc.join("  \n") # two-space hard breaks: one paragraph, three lines
  out << ""
  headers = ["engine", "", "ns/op", "p50", "p90", "p99", "vs fastest"]
  headers << "Δ vs baseline" if baseline_dir
  rows = s[:rows].map do |r|
    cells = [r[:label], bar(r[:width_pct]), r[:ns], r[:p50], r[:p90], r[:p99], r[:mult]]
    cells << (r[:delta] ? format("%+.1f%%", r[:delta]) : "—") if baseline_dir
    cells
  end
  out.concat(table(headers, rows))
  out << "" << "*in baseline but not in this run: #{s[:missing].join(', ')}*" unless s[:missing].empty?
end

unless model[:baseline_only_benches].empty?
  listed = model[:baseline_only_benches].map { |b, d| "#{b} (#{d})" }.join(", ")
  out << "" << "*benches in the baseline but not in this run: #{listed}*"
end

out << ""
out << "---"
out << ""
out << "lower is better · bars are linear within each benchmark · |Δ| under " \
       "#{BenchResults::NOISE_PCT.to_i}% is wall-clock noise (spec/design/benchmarks.md §10) · " \
       "regenerate: `rake bench:markdown`"

markdown = "#{out.join("\n")}\n"
out_path = File.join(run_dir, "report.md")
File.write(out_path, markdown)
print markdown
warn "wrote #{out_path}"

unless failures.empty?
  failures.each { |f| warn "FAIL: #{f}" }
  exit 1
end
