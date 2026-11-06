# frozen_string_literal: true
require_relative "benchmark_analysis" unless defined?(BenchmarkComparison)
require "tmpdir"

# Synthetic two-workload evidence exercises the strict loader without reading
# retained benchmark runs or invoking the production inventory/build helpers.
module BenchmarkAnalysisChecks
  module_function
  def check(value, message)
    raise message unless value
  end

  def rejected(label)
    yield
    raise "accepted invalid fixture: #{label}"
  rescue BenchmarkComparison::Invalid
    true
  end

  def write_json(path, value) = File.write(path, JSON.generate(value) + "\n")
  def edit(path)
    value = JSON.parse(File.read(path)); yield(value); write_json(path, value)
  end

  def fixture(directory, contract, stage)
    FileUtils.mkdir_p(File.join(directory, "corpus"))
    contract.hashes.keys.each { File.write(File.join(directory, "corpus", _1), _1) }
    write_json(File.join(directory, "input.json"), contract.input)
    metadata = {
      "baseline" => contract.baseline, "corpus_sha256" => contract.hashes, "diagnostic" => false, "omitted" => [], "rounds" => 3,
      "cores" => BenchmarkComparison::CORES, "selected" => contract.names,
      "dataset_images" => { "tiny" => { "bytes" => 123, "sha256" => "a" * 64, "fingerprint" => contract.hashes.fetch("datasets.toml") } },
      "actors" => BenchmarkComparison::CORES.to_h { [_1, { "sha256" => "b" * 64 }] },
      "source_provenance" => "Synthetic no-build caveat", "source_files" => { "fake.swift" => "f" * 64 }, "git_status" => { "stdout" => "synthetic" },
      "environment" => {}, "timing_policy" => "synthetic sequential test", "data_directory" => "/synthetic/data"
    }
    write_json(File.join(directory, "metadata.json"), metadata)
    completed = []
    (1..3).each do |round|
      BenchmarkComparison::CORES.rotate(round - 1).each do |core|
        folder = File.join(directory, "round-#{round}-#{core}"); FileUtils.mkdir_p(folder)
        rows = contract.workloads.each_with_index.map do |workload, index|
          ns = if index.zero?
                 core == "swift" ? (stage == :before ? [100, 1000, 200] : [50, 500, 100])[round - 1] :
                   (stage == :before ? [50, 150, 100] : [100, 300, 200])[round - 1]
               else
                 (stage == :before ? [10, 30, 20] : [20, 60, 40])[round - 1]
               end
          { "schema" => 2, "engine" => "jed", "lang" => core, "variant" => core == "swift" ? "experimental-native" : "core",
            "bench" => workload.fetch("name"), "dataset" => workload.fetch("dataset"), "iterations" => workload.fetch("iterations"), "warmup" => 0,
            "readers" => 0, "rows_total" => 8, "checksum" => "0123456789abcdef", "fingerprint" => contract.hashes.fetch("datasets.toml"),
            "started_at" => "2026-11-06T12:00:00Z", "total_ns" => ns * workload.fetch("iterations"), "ns_per_op" => ns, "min_ns" => ns, "p50_ns" => ns, "p90_ns" => ns, "p99_ns" => ns }
        end
        File.write(File.join(folder, "results.jsonl"), rows.map { JSON.generate(_1) + "\n" }.join)
        if core == "swift"
          File.write(File.join(folder, "samples.jsonl"), rows.map { JSON.generate(bench: _1.fetch("bench"), samples_ns: [_1.fetch("ns_per_op")] * _1.fetch("iterations")) + "\n" }.join)
        end
        completed << { "round" => round, "core" => core, "workloads" => 2, "results" => File.join(folder, "results.jsonl") }
      end
    end
    write_json(File.join(directory, "report.json"), { "metadata" => "metadata.json", "completed" => completed, "failures" => [], "success" => true })
  end

  def run
    started = Process.clock_gettime(Process::CLOCK_MONOTONIC)
    hashes = %w[benchmarks.toml datasets.toml spill.toml].to_h { [_1, Digest::SHA256.hexdigest(_1)] }
    workloads = %w[query_probe batch_probe].each_with_index.map do |name, index|
      { "name" => name, "dataset" => index.zero? ? "tiny" : "scratch", "kind" => index.zero? ? "query" : "write_rollback",
        "sql" => "SELECT1", "setupSQL" => [], "warmup" => 0, "iterations" => index.zero? ? 2 : 4, "seed" => 42, "expectRowsPerIter" => 0,
        "batch" => index.zero? ? 0 : 10, "readers" => 0, "relaxed" => false, "hostFunctions" => false, "parameters" => [] }
    end
    contract = BenchmarkComparison::Contract.new(baseline: "synthetic", hashes: hashes,
      input: { "schema" => 1, "fingerprint" => hashes.fetch("datasets.toml"), "benchmarks" => workloads, "tableRows" => {} })
    checks = 0
    Dir.mktmpdir("jed-bench-report-unit-") do |root|
      before_path = File.join(root, "before"); after_path = File.join(root, "after")
      fixture(before_path, contract, :before); fixture(after_path, contract, :after)
      before = BenchmarkComparison.load_run(before_path, contract)
      after = BenchmarkComparison.load_run(after_path, contract)
      report = BenchmarkComparison.compare(before, after, contract)
      first = report.fetch(:workloads).first
      check(first.fetch(:before_swift_median_ns) == 200, "median incorrectly became mean or pooled quantile")
      check(first.fetch(:swift_speedup) == 2.0, "before/after speedup direction")
      check(first.fetch(:before_swift_over_rust) == 2.0 && first.fetch(:after_swift_over_rust) == 0.5, "Swift/peer direction")
      check(first.fetch(:rust_speedup) == 0.5, "peer drift direction")
      all = report.fetch(:categories).first
      check((all.fetch(:swift_speedup_gmean) - 1.0).abs < 1e-12 && all.fetch(:swift_speedup_median) == 1.25, "unweighted category arithmetic")
      check(report.fetch(:categories).select { _1[:dimension] == "kind" }.map { _1[:workloads] } == [1, 1], "category membership")
      check(report.fetch(:before).fetch(:source_provenance) == "Synthetic no-build caveat" && report.fetch(:before).fetch(:source_files).keys == ["fake.swift"], "source provenance caveat lost")
      checks += 7
      output = File.join(root, "output"); BenchmarkComparison.emit(report, output)
      check(CSV.read(File.join(output, "workloads.csv"), headers: true).length == 2, "CSV membership")
      check(JSON.parse(File.read(File.join(output, "comparison.json"))).fetch("validated_workloads") == 2, "JSON output")
      rejected("nonempty output") { BenchmarkComparison.emit(report, output) }; checks += 3
      baseline = BenchmarkComparison.summarize(before, contract)
      baseline_first = baseline.fetch(:workloads).first
      check(baseline_first.fetch(:swift_round_ns) == [100, 1000, 200] && baseline_first.fetch(:swift_median_ns) == 200, "baseline round order/median")
      check(BenchmarkComparison::PEERS.all? { baseline_first.fetch("swift_over_#{_1}".to_sym) == 2.0 }, "baseline Swift/peer direction")
      baseline_all = baseline.fetch(:categories).first
      check((baseline_all.fetch(:swift_over_rust_gmean) - Math.sqrt(2)).abs < 1e-12 && baseline_all.fetch(:swift_over_rust_median) == 1.5, "baseline unweighted category arithmetic")
      check(baseline.fetch(:categories).select { _1[:dimension] == "kind" }.map { _1[:workloads] } == [1, 1] &&
            baseline.fetch(:categories).select { _1[:dimension] == "dataset" }.map { _1[:category] } == %w[scratch tiny], "baseline category membership")
      check(baseline.fetch(:run).fetch(:source_provenance) == "Synthetic no-build caveat" && baseline.fetch(:run).fetch(:source_files).keys == ["fake.swift"], "baseline provenance")
      baseline_output = File.join(root, "baseline-output"); BenchmarkComparison.emit(baseline, baseline_output)
      baseline_json = JSON.parse(File.read(File.join(baseline_output, "baseline.json")))
      baseline_csv = CSV.read(File.join(baseline_output, "workloads.csv"), headers: true)
      check(baseline_json.fetch("mode") == "baseline" && baseline_csv.length == 2 && baseline_csv.first.fetch("swift_round_ns") == "[100,1000,200]", "baseline output membership/rounds")
      check(!File.exist?(File.join(baseline_output, "comparison.json")) &&
            !JSON.generate(baseline_json).match?(/"(?:[^" ]*speedup|before|after|before_[^" ]*|after_[^" ]*)"\s*:/) &&
            !File.read(File.join(baseline_output, "report.md")).match?(/speedup|improvement|before\/after/), "baseline contains change fields or claims")
      checks += 7
      check(BenchmarkComparison.parse_options(%w[--baseline B --output O])[:baseline] == "B", "baseline option selection")
      check(BenchmarkComparison.parse_options(%w[--before B --after A --output O])[:after] == "A", "comparison option selection")
      check(BenchmarkComparison.parse_options(%w[--check-adapter])[:check_adapter], "synthetic-check option selection")
      check(BenchmarkComparison.parse_options(%w[--baseline B --output O])[:repo] == File.expand_path("../../..", __dir__), "portable repository discovery")
      [ %w[--baseline B --before B --output O], %w[--baseline B --after A --output O],
        %w[--before B --output O], %w[--after A --output O], %w[--baseline B], %w[--output O],
        %w[--baseline B --output O unexpected], %w[--check-adapter --baseline B],
        %w[--check-adapter --output O] ].each do |arguments|
        rejected("ambiguous/incomplete mode options") { BenchmarkComparison.parse_options(arguments) }; checks += 1
      end
      checks += 4
      mutations = {
        "unsuccessful" => ["report.json", ->(x) { x["success"] = false }],
        "incomplete" => ["report.json", ->(x) { x.delete("success") }],
        "failure" => ["report.json", ->(x) { x["failures"] << { "error" => "bad" } }],
        "missing round" => ["report.json", ->(x) { x["completed"].pop }],
        "duplicate round" => ["report.json", ->(x) { x["completed"][1] = x["completed"][0] }],
        "wrong workload count" => ["report.json", ->(x) { x["completed"][0]["workloads"] = 143 }],
        "redirected results" => ["report.json", ->(x) { x["completed"][0]["results"] = "/wrong/other/results.jsonl" }],
        "diagnostic" => ["metadata.json", ->(x) { x["diagnostic"] = true }],
        "two rounds" => ["metadata.json", ->(x) { x["rounds"] = 2 }],
        "float rounds" => ["metadata.json", ->(x) { x["rounds"] = 3.0 }],
        "missing core" => ["metadata.json", ->(x) { x["cores"].pop }],
        "wrong corpus" => ["metadata.json", ->(x) { x["corpus_sha256"]["spill.toml"] = "c" * 64 }],
        "float data bytes" => ["metadata.json", ->(x) { x["dataset_images"]["tiny"]["bytes"] = 123.0 }],
        "different seed" => ["input.json", ->(x) { x["benchmarks"][0]["seed"] = 43 }],
        "float iterations" => ["input.json", ->(x) { x["benchmarks"][0]["iterations"] = 2.0 }],
        "changed parameter" => ["input.json", ->(x) { x["benchmarks"][0]["parameters"] << { "gen" => "different" } }]
      }
      mutations.each do |label, (relative, mutation)|
        path = File.join(after_path, relative); saved = File.binread(path)
        edit(path, &mutation)
        rejected(label) { BenchmarkComparison.load_run(after_path, contract) }
        File.binwrite(path, saved); checks += 1
      end
      result_path = File.join(after_path, "round-1-rust/results.jsonl"); saved = File.binread(result_path)
      row_mutations = {
        "checksum" => ->(x) { x["checksum"] = "abcdef0123456789" }, "row count" => ->(x) { x["rows_total"] += 1 },
        "fractional count" => ->(x) { x["rows_total"] = 8.0 }, "wrong total" => ->(x) { x["total_ns"] += 2 },
        "zero timing" => ->(x) { x["total_ns"] = x["ns_per_op"] = 0 }, "bad timestamp" => ->(x) { x["started_at"] = "2026-02-30T12:00:00Z" }
      }
      row_mutations.each do |label, mutation|
        lines = saved.lines; row = JSON.parse(lines.first); mutation.call(row); lines[0] = JSON.generate(row) + "\n"
        File.write(result_path, lines.join)
        rejected(label) { BenchmarkComparison.load_run(after_path, contract) }; checks += 1
      end
      [saved.lines.first, saved + saved.lines.first, saved.sub('"schema":2', '"schema":2,"schema":2')].each do |invalid|
        File.write(result_path, invalid)
        rejected("missing/duplicate result or JSON key") { BenchmarkComparison.load_run(after_path, contract) }; checks += 1
      end
      File.binwrite(result_path, saved)
      metadata_path = File.join(after_path, "metadata.json"); saved_metadata = File.binread(metadata_path)
      %w[sha256 bytes].each do |field|
        edit(metadata_path) { |x| x["dataset_images"]["tiny"][field] = field == "bytes" ? 124 : "c" * 64 }
        changed = BenchmarkComparison.load_run(after_path, contract)
        rejected("changed data #{field}") { BenchmarkComparison.compare(before, changed, contract) }; checks += 1
        File.binwrite(metadata_path, saved_metadata)
      end
      edit(metadata_path) { |x| x["environment"]["RUSTFLAGS"] = "different" }
      changed = BenchmarkComparison.load_run(after_path, contract)
      rejected("changed environment") { BenchmarkComparison.compare(before, changed, contract) }; checks += 1
      File.binwrite(metadata_path, saved_metadata)
      (1..3).each do |round|
        BenchmarkComparison::CORES.each do |core|
          p = File.join(after_path, "round-#{round}-#{core}/results.jsonl")
          File.write(p, File.readlines(p).map { |line| x = JSON.parse(line); x["checksum"] = "abcdef0123456789"; JSON.generate(x) + "\n" }.join)
        end
      end
      changed = BenchmarkComparison.load_run(after_path, contract)
      rejected("consistent but different after checksums") { BenchmarkComparison.compare(before, changed, contract) }; checks += 1
      File.write(File.join(before_path, "report.json"), "{}\n")
      rejected("evidence changes before output") { BenchmarkComparison.emit(report, File.join(root, "raced-output")) }; checks += 1
      rejected("baseline evidence changes before output") { BenchmarkComparison.emit(baseline, File.join(root, "raced-baseline-output")) }; checks += 1
    end
    elapsed = (Process.clock_gettime(Process::CLOCK_MONOTONIC) - started) * 1000
    puts "PASS #{checks} tiny synthetic arithmetic/validation checks in #{format('%.1f', elapsed)}ms; no benchmark data or workloads read"
  end
end
