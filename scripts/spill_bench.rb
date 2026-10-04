# frozen_string_literal: true

# Larger-than-work-memory blocking-operator benchmark. Setup runs in its own process;
# each measured query opens the same persisted database with a 1 MiB page cache.
# No benchmark dependencies beyond the repository's existing toolchains.
require "json"
require "fileutils"
require "tmpdir"
require "open3"
require "toml-rb"

root = File.expand_path("..", __dir__)
workload = TomlRB.load_file("#{root}/bench/corpus/spill.toml")
rows = Integer(ARGV.fetch(0, workload.fetch("rows")))
width = Integer(ARGV.fetch(1, workload.fetch("payload_bytes")))
budget = Integer(ARGV.fetch(2, workload.fetch("work_mem")))
cache = workload.fetch("cache_bytes")
abort "rows/width/work_mem must be positive" unless [rows, width, budget].all?(&:positive?)
abort "workload must exceed work_mem" unless rows * (width + 64) > budget
out = File.expand_path(ENV.fetch("JED_SPILL_RESULTS", "bench/results/spill-#{Time.now.utc.strftime('%Y%m%d-%H%M%S')}"), root)
FileUtils.mkdir_p(out)

def checked(*command, **options)
  abort "failed: #{command.join(' ')}" unless system(*command, **options)
end

def workers(root, build: true)
  if build
    checked("go", "build", "-buildvcs=false", "-o", "bin/spill", "./cmd/spill", chdir: "#{root}/bench/go")
    # This worker needs only jed; avoid building the PG/SQLite benchmark drivers.
    checked("cargo", "build", "--release", "--quiet", "--manifest-path", "#{root}/impl/rust/Cargo.toml", "--lib")
    FileUtils.mkdir_p("#{root}/bench/rust/target/release")
    checked("rustc", "--edition=2024", "-O", "#{root}/bench/rust/src/bin/spill.rs", "--extern", "jed=#{root}/impl/rust/target/release/libjed.rlib", "-L", "dependency=#{root}/impl/rust/target/release/deps", "-o", "#{root}/bench/rust/target/release/spill")
  end
  {
    "go" => ["#{root}/bench/go/bin/spill"],
    "rust" => ["#{root}/bench/rust/target/release/spill"],
    "ts" => ["node", "#{root}/bench/ts/src/spill.ts"]
  }
end

commands = workers(root, build: ENV["JED_SPILL_SKIP_BUILD"] != "1")
queries = workload.fetch("queries")
File.write("#{out}/workload.json", JSON.pretty_generate({rows: rows, payload_bytes: width, estimated_input_bytes: rows * (width + 64), work_mem: budget, cache_bytes: cache, queries: queries}) + "\n")

Dir.mktmpdir("jed-spill-bench-") do |scratch|
  database = "#{scratch}/input.jed"
  setup = "#{scratch}/setup.tsv"
  File.open(setup, "w") do |f|
    f.puts "-\tCREATE TABLE wide (id bigint PRIMARY KEY, k bigint, v integer, s text)"
    batch = [[524288 / (width + 100), 1].max, 256].min
    payload = "x" * width
    (1..rows).step(batch) do |first|
      last = [first + batch - 1, rows].min
      values = (first..last).map { |id| "(#{id}, #{id}, #{id % 97}, '#{payload}#{id}')" }
      f.puts "-\tINSERT INTO wide VALUES #{values.join(', ')}"
    end
  end
  checked(*commands.fetch("go"), "create", database, "0", setup, cache.to_s)
  results = []
  commands.each do |core, command|
    [0, budget, 268435456].uniq.each do |work_mem|
      queries.each do |name, sql|
        input = "#{scratch}/query.tsv"
        File.write(input, "#{name}\t#{sql}\n")
        rss = "#{scratch}/rss"
        # GNU time measures the worker process, including startup, and excludes setup.
        # macOS users can run the worker directly; this RSS-reporting driver is Linux-only.
        argv = ["/usr/bin/time", "-f", "%M", "-o", rss, *command, "open", database, work_mem.to_s, input, cache.to_s]
        stdout, stderr, status = Open3.capture3(*argv)
        abort "#{core}/#{name}/#{work_mem}: #{stderr}" unless status.success?
        result = JSON.parse(stdout)
        result.merge!("core" => core, "work_mem" => work_mem, "peak_rss_kib" => Integer(File.read(rss)), "input_rows" => rows, "estimated_input_bytes" => rows * (width + 64))
        results << result
        puts JSON.generate(result)
        File.open("#{out}/results.jsonl", "a") { |f| f.puts JSON.generate(result) }
      end
    end
  end
  results.group_by { |result| result.fetch("name") }.each do |name, group|
    signatures = group.map { |result| result.values_at("rows", "cost", "checksum") }.uniq
    abort "results or costs diverged for #{name}: #{signatures.inspect}" unless signatures.one?
  end
end
puts "Spill results and costs agree across all cores and budgets: #{out}"
