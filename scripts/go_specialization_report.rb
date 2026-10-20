# frozen_string_literal: true

# Whole-query benchmark samples, including controls: no outlier deletion.
require "json"
root = File.expand_path("..", __dir__)
dir = File.expand_path(ARGV.fetch(0, "bench/results/go-specialization"), root)
variants = %w[clean disabled ordinary selective]
samples = variants.to_h { |v| [v, {}] }
variants.each do |v|
  Dir["#{dir}/#{v}-[0-9]*.txt"].sort.each do |file|
    pair = File.basename(file)[/-(\d+)\.txt$/, 1].to_i
    File.readlines(file).grep(/^BenchmarkSpecialization\//).each do |line|
      fields = line.split
      name = fields.shift.sub(/-\d+$/, "").delete_prefix("BenchmarkSpecialization/")
      fields.shift # iterations
      metrics = fields.each_slice(2).to_h { |value, unit| [unit, Float(value)] }
      (samples[v][name] ||= {})[pair] = metrics
    end
  end
end
abort "no benchmark samples" if samples["clean"].empty?
variants.each do |v|
  abort "different workload inventory for #{v}" unless samples[v].keys.sort == samples["clean"].keys.sort
end
def median(values)

  sorted = values.sort
  n = sorted.size
  n.odd? ? sorted[n / 2] : (sorted[n / 2 - 1] + sorted[n / 2]) / 2
end
report = samples.fetch("clean").keys.to_h do |name|
  rows = variants.to_h { |v| [v, samples.fetch(v).fetch(name)] }
  # Every reported workload must have the same pairs, results and exact cost.
  pairs = rows["clean"].keys
  rows.each do |v, rs|
    abort "missing pairs: #{v}/#{name}" unless rs.keys == pairs
    pairs.each do |pair|
      %w[checksum cost checksum1 cost1].each do |field|
        abort "#{name} #{field} differs in #{v} pair #{pair}" unless rs[pair].fetch(field) == rows["clean"][pair].fetch(field)
      end
    end
  end
  stats = rows.to_h do |v, rs|
    times = rs.values.map { |r| r.fetch("ns/op") }
    [v, {median_ns: median(times), min_ns: times.min, max_ns: times.max,
         allocations: rs.values.map { |r| r.fetch("allocs/op") }.uniq,
         median_bytes: median(rs.values.map { |r| r.fetch("B/op") })}]
  end
  stats["selective_vs_clean_percent"] = 100 * (stats["selective"][:median_ns] / stats["clean"][:median_ns] - 1)
  stats["selective_vs_ordinary_percent"] = 100 * (stats["selective"][:median_ns] / stats["ordinary"][:median_ns] - 1)
  stats["faster_pairs"] = pairs.count { |p| rows["selective"][p]["ns/op"] < rows["clean"][p]["ns/op"] }
  [name, stats]
end
File.write("#{dir}/summary.json", JSON.pretty_generate(report) + "\n")
puts "| Query | Clean µs | Disabled µs | Ordinary µs | Kernels µs | vs clean | Faster pairs |"
puts "|---|---:|---:|---:|---:|---:|---:|"
report.each do |name, r|
  printf("| %s | %.2f | %.2f | %.2f | %.2f | %+.1f%% | %d |\n", name,
         *variants.map { |v| r[v][:median_ns] / 1000 }, r["selective_vs_clean_percent"], r["faster_pairs"])
end
