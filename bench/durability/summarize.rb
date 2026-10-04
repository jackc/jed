# frozen_string_literal: true

# No gems: read machine-readable lines from one or more bench:durability logs.
abort "usage: ruby bench/durability/summarize.rb RUN.log [...]" if ARGV.empty?

def median(values)
  sorted = values.sort
  (sorted[(sorted.length - 1) / 2] + sorted[sorted.length / 2]) / 2.0
end

rows = ARGV.flat_map do |path|
  File.foreach(path).filter_map do |line|
    next unless line.start_with?("DURABILITY,")

    row = line.strip.split(",").drop(1).to_h { |pair| pair.split("=", 2) }
    abort "unverified sample in #{path}" unless row.fetch("verified") == "true"
    row
  end
end
abort "no DURABILITY records" if rows.empty?
puts "rows,stride,batch,checkpoint,sync,delay_us,mode,repeats,commits,mean_us,p50_us,p99_us,syncs_per_commit,bytes_per_commit,file_bytes"
rows.group_by { |r| r.values_at("rows", "stride", "batch", "checkpoint", "sync", "delay_us", "mode", "commits") }.each do |key, group|
  per_commit = ->(field) { median(group.map { |r| r.fetch(field).to_f / r.fetch("commits").to_i }) }
  values = [*key.take(7), group.length, key.last,
            (per_commit.call("elapsed_ns") / 1000).round(3),
            (median(group.map { |r| r.fetch("p50_ns").to_f }) / 1000).round(3),
            (median(group.map { |r| r.fetch("p99_ns").to_f }) / 1000).round(3),
            per_commit.call("syncs").round(4), per_commit.call("bytes").round(3),
            group.first.fetch("file_bytes")]
  puts values.join(",")
end
