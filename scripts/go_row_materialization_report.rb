# frozen_string_literal: true

require "json"
require "pathname"

dir = Pathname(ARGV.fetch(0))
out = Pathname(ARGV.fetch(1, dir.to_s))
samples = Hash.new { |h, k| h[k] = {} }
Dir[dir.join("{baseline,candidate}-*.txt")].sort.each do |path|
  name, pair = File.basename(path).match(/\A(baseline|candidate)-(\d+)\.txt\z/)&.captures
  next unless name
  File.foreach(path) do |line|
    match = line.match(%r{\ABenchmarkRowMaterialization/(.+)-\d+\s+\d+\s+(.*)})
    next unless match
    fields = match[2].split.each_slice(2).to_h { |v, unit| [unit, Float(v)] }
    (samples[match[1]][name] ||= {})[Integer(pair)] = fields
  end
end
median = ->(xs) { s = xs.sort; (s[(s.size - 1) / 2] + s[s.size / 2]) / 2.0 }
report = samples.sort.to_h do |lane, variants|
  base, candidate = variants.values_at("baseline", "candidate")
  abort "missing pair for #{lane}" unless base && candidate && base.keys.sort == candidate.keys.sort
  base.each do |pair, b|
    %w[checksum checksum1 cost cost1].each do |unit|
      abort "#{lane}/#{pair} #{unit} differs" unless b.fetch(unit) == candidate.fetch(pair).fetch(unit)
    end
  end
  stats = variants.to_h do |name, pairs|
    [name, %w[ns/op B/op allocs/op].to_h do |unit|
      xs = pairs.values.map { |r| r.fetch(unit) }
      [unit, {median: median.call(xs), min: xs.min, max: xs.max, samples: xs}]
    end]
  end
  stats[:delta_percent] = 100 * (stats["candidate"]["ns/op"][:median] / stats["baseline"]["ns/op"][:median] - 1)
  stats[:faster_pairs] = base.count { |i, b| candidate[i]["ns/op"] < b["ns/op"] }
  stats[:pairs] = base.size
  [lane, stats]
end
File.write(out.join("summary.json"), JSON.pretty_generate(report) + "\n")
lines = ["| Host/query | Baseline µs | Candidate µs | Change | Faster pairs | B/query (before → after) | Allocs/query (before → after) |",
         "|---|---:|---:|---:|---:|---:|---:|"]
report.each do |lane, s|
  b, c = s.values_at("baseline", "candidate")
  lines << format("| %s | %.2f | %.2f | %+.1f%% | %d/%d | %.0f → %.0f | %.0f → %.0f |", lane,
    b["ns/op"][:median] / 1000, c["ns/op"][:median] / 1000, s[:delta_percent], s[:faster_pairs], s[:pairs],
    b["B/op"][:median], c["B/op"][:median], b["allocs/op"][:median], c["allocs/op"][:median])
end
File.write(out.join("summary.md"), lines.join("\n") + "\n")
puts lines
