# frozen_string_literal: true

require "json"
require "csv"
require "digest"
require "time"
require "fileutils"
require "optparse"

# Opt-in analysis of completed harness evidence. Never builds or runs a core.
module BenchmarkComparison
  ROOT = File.expand_path("../../..", __dir__)
  CORES = %w[swift rust go ts].freeze
  PEERS = CORES.drop(1).freeze
  ROUNDS = 3
  INTEGER_FIELDS = %w[iterations warmup readers total_ns ns_per_op min_ns p50_ns p90_ns p99_ns rows_total].freeze
  SHARED_FIELDS = %w[bench dataset iterations warmup readers rows_total checksum fingerprint].freeze
  SHA256 = /\A[0-9a-f]{64}\z/

  class Invalid < StandardError; end
  Contract = Struct.new(:baseline, :hashes, :input, keyword_init: true) do
    def workloads = input.fetch("benchmarks")
    def names = workloads.map { _1.fetch("name") }

    def self.load(repo)
      # Read only the existing normalization/validation code. No run/build API is called.
      require File.join(File.expand_path(repo), "impl/swift-native/Tools/benchmark")
      projected, _, all = SwiftNativeBenchmark.input
      expected = SwiftNativeBenchmark.const_defined?(:WORKLOAD_COUNT) ? SwiftNativeBenchmark::WORKLOAD_COUNT : 143
      raise Invalid, "canonical inventory is not exactly #{expected} unique workloads" unless all.length == expected && all.uniq.length == expected
      new(baseline: SwiftNativeBenchmark::BASELINE, hashes: SwiftNativeBenchmark::HASHES,
          input: JSON.parse(JSON.generate(projected)))
    end
  end

  module_function

  def require_that(condition, message)
    raise Invalid, message unless condition
  end

  def canonical(value)
    case value
    when Hash then value.keys.sort.to_h { |key| [key, canonical(value.fetch(key))] }
    when Array then value.map { canonical(_1) }
    else value
    end
  end

  # JSON encoding preserves numeric types, unlike Ruby's 3 == 3.0 comparison.
  def identical?(left, right) = JSON.generate(canonical(left)) == JSON.generate(canonical(right))
  def parse(bytes) = JSON.parse(bytes, allow_duplicate_key: false)
  def integer?(value) = value.is_a?(Integer)

  def timestamp?(value)
    value.is_a?(String) && value.match?(/\A[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z\z/) &&
      Time.iso8601(value).utc.iso8601 == value
  rescue ArgumentError
    false
  end

  def read_json(path, evidence)
    bytes = File.binread(path)
    evidence[path] = Digest::SHA256.hexdigest(bytes)
    parse(bytes)
  end

  def validate_row(row, core, workload, fingerprint)
    name = workload.fetch("name")
    require_that(integer?(row.fetch("schema")) && row.fetch("schema") == 2 && row.fetch("engine") == "jed" && row.fetch("lang") == core,
                 "#{core}/#{name}: result schema or identity differs")
    require_that(row.fetch("variant") == (core == "swift" ? "experimental-native" : "core"), "#{core}/#{name}: variant differs")
    require_that(timestamp?(row.fetch("started_at")), "#{core}/#{name}: malformed timestamp")
    require_that(INTEGER_FIELDS.all? { integer?(row.fetch(_1)) && row.fetch(_1) >= 0 }, "#{core}/#{name}: timing/count fields must be nonnegative integers")
    require_that(row.fetch("iterations").positive? && row.fetch("ns_per_op").positive?, "#{core}/#{name}: zero iterations/timing cannot form a ratio")
    expected = workload.slice("dataset", "iterations", "warmup", "readers").merge("fingerprint" => fingerprint)
    require_that(expected.all? { |key, value| identical?(row.fetch(key), value) }, "#{core}/#{name}: workload parameters differ")
    require_that(row.fetch("checksum").is_a?(String) && row.fetch("checksum").match?(/\A[0-9a-f]{16}\z/), "#{core}/#{name}: malformed checksum")
    require_that(row.fetch("ns_per_op") == row.fetch("total_ns") / row.fetch("iterations"), "#{core}/#{name}: ns_per_op does not match total/iterations")
    require_that(%w[min_ns p50_ns p90_ns p99_ns].map { row.fetch(_1) }.each_cons(2).all? { |a, b| a <= b }, "#{core}/#{name}: percentiles not ordered")
  end

  def load_run(directory, contract)
    directory = File.realpath(directory)
    evidence = {}
    report = read_json(File.join(directory, "report.json"), evidence)
    metadata = read_json(File.join(directory, "metadata.json"), evidence)
    input = read_json(File.join(directory, "input.json"), evidence)
    require_that(report.fetch("success") == true && report.fetch("failures") == [], "#{directory}: report is incomplete or failed")
    require_that(report.fetch("metadata") == "metadata.json", "#{directory}: unexpected metadata reference")
    require_that(metadata.fetch("diagnostic") == false && metadata.fetch("omitted") == [], "#{directory}: filtered/subset run cannot be compared")
    require_that(integer?(metadata.fetch("rounds")) && metadata.fetch("rounds") == ROUNDS, "#{directory}: exactly3 rounds are required")
    cores = metadata.fetch("cores")
    require_that(cores.is_a?(Array) && cores.length == CORES.length && cores.sort == CORES.sort, "#{directory}: all four cores are required exactly once")
    require_that(metadata.fetch("selected") == contract.names, "#{directory}: workload inventory/order differs")
    require_that(metadata.fetch("baseline") == contract.baseline && identical?(metadata.fetch("corpus_sha256"), contract.hashes), "#{directory}: corpus revision/hashes differ")
    require_that(identical?(input, contract.input), "#{directory}: normalized SQL/setup/parameters/seed/iteration contract differs")
    contract.hashes.each do |name, hash|
      path = File.join(directory, "corpus", name)
      actual = Digest::SHA256.file(path).hexdigest
      require_that(actual == hash, "#{directory}: copied corpus #{name} has changed")
      evidence[path] = actual
    end
    images = metadata.fetch("dataset_images")
    expected_images = contract.workloads.map { _1.fetch("dataset") }.uniq.reject { _1 == "scratch" }.sort
    require_that(images.keys.sort == expected_images, "#{directory}: dataset image inventory differs")
    data_facts = images.to_h do |name, image|
      require_that(integer?(image.fetch("bytes")) && image.fetch("bytes").positive? && image.fetch("sha256").is_a?(String) && image.fetch("sha256").match?(SHA256) &&
                   image.fetch("fingerprint") == contract.hashes.fetch("datasets.toml"), "#{directory}: invalid dataset facts for #{name}")
      [name, image.slice("bytes", "sha256", "fingerprint")]
    end
    require_that(metadata.fetch("actors").keys.sort == CORES.sort, "#{directory}: actor inventory differs")
    metadata.fetch("actors").each do |core, actor|
      require_that(actor.fetch("sha256").is_a?(String) && actor.fetch("sha256").match?(SHA256), "#{directory}: invalid actor hash for #{core}")
    end
    expected_rounds = (1..ROUNDS).flat_map { |round| cores.rotate((round - 1) % cores.length).map { [round, _1] } }
    completed = report.fetch("completed")
    require_that(completed.is_a?(Array) && completed.map { [_1.fetch("round"), _1.fetch("core")] } == expected_rounds &&
                 completed.all? { integer?(_1.fetch("round")) && integer?(_1.fetch("workloads")) && _1.fetch("workloads") == contract.names.length },
                 "#{directory}: missing, duplicate, reordered or malformed core/round completion")
    rows = CORES.to_h { [_1, {}] }
    shared = {}
    workloads = contract.workloads.to_h { [_1.fetch("name"), _1] }
    completed.each do |entry|
      core = entry.fetch("core"); round = entry.fetch("round")
      round_name = "round-#{round}-#{core}"
      # Evidence may have been archived elsewhere. Never follow stored absolute paths.
      reference = entry.fetch("results")
      require_that(reference.is_a?(String) && File.basename(reference) == "results.jsonl" && File.basename(File.dirname(reference)) == round_name,
                   "#{directory}: unexpected results reference")
      path = File.join(directory, round_name, "results.jsonl")
      bytes = File.binread(path); evidence[path] = Digest::SHA256.hexdigest(bytes)
      found = {}
      bytes.each_line.with_index(1) do |line, number|
        row = parse(line); name = row.fetch("bench")
        require_that(workloads.key?(name) && !found.key?(name), "#{directory}/#{round_name}: unexpected/duplicate workload #{name.inspect} at line#{number}")
        validate_row(row, core, workloads.fetch(name), contract.hashes.fetch("datasets.toml"))
        values = row.slice(*SHARED_FIELDS)
        require_that(!shared.key?(name) || identical?(shared.fetch(name), values), "#{directory}: checksum/rowcount/shared fields differ across rounds or cores for #{name}")
        shared[name] = values
        found[name] = row
      end
      require_that(found.keys.sort == contract.names.sort, "#{directory}/#{round_name}: incomplete workload membership")
      rows.fetch(core)[round] = found
      if core == "swift"
        sample_path = File.join(directory, round_name, "samples.jsonl")
        require_that(File.file?(sample_path) && File.size(sample_path).positive?, "#{directory}/#{round_name}: missing raw Swift samples")
      end
    end
    { directory: directory, metadata: metadata, rows: rows, shared: shared, data_facts: data_facts, evidence: evidence }
  rescue KeyError, TypeError, NoMethodError, JSON::ParserError, Errno::ENOENT => error
    raise Invalid, "#{directory}: malformed/incomplete evidence: #{error.message}"
  end

  def median(values)
    ordered = values.sort
    middle = ordered.length / 2
    ordered.length.odd? ? ordered[middle] : (ordered[middle - 1] + ordered[middle]) / 2.0
  end

  def gmean(values) = Math.exp(values.sum { Math.log(_1) } / values.length)

  def category(rows, dimension, category)
    result = { dimension: dimension, category: category, workloads: rows.length,
               swift_speedup_gmean: gmean(rows.map { _1.fetch(:swift_speedup) }), swift_speedup_median: median(rows.map { _1.fetch(:swift_speedup) }) }
    PEERS.each do |peer|
      %w[before after].each do |stage|
        values = rows.map { _1.fetch("#{stage}_swift_over_#{peer}".to_sym) }
        result["#{stage}_swift_over_#{peer}_gmean".to_sym] = gmean(values)
        result["#{stage}_swift_over_#{peer}_median".to_sym] = median(values)
      end
      result["#{peer}_speedup_gmean".to_sym] = gmean(rows.map { _1.fetch("#{peer}_speedup".to_sym) })
    end
    result
  end

  def baseline_category(rows, dimension, category)
    result = { dimension: dimension, category: category, workloads: rows.length }
    PEERS.each do |peer|
      values = rows.map { _1.fetch("swift_over_#{peer}".to_sym) }
      result["swift_over_#{peer}_gmean".to_sym] = gmean(values)
      result["swift_over_#{peer}_median".to_sym] = median(values)
    end
    result
  end

  def summarize(run, contract)
    rows = contract.workloads.map do |workload|
      name = workload.fetch("name")
      result = { workload: name, kind: workload.fetch("kind"), dataset: workload.fetch("dataset"),
                 iterations: workload.fetch("iterations"), readers: workload.fetch("readers"), batch: workload.fetch("batch"),
                 rows_total: run.fetch(:shared).fetch(name).fetch("rows_total"), checksum: run.fetch(:shared).fetch(name).fetch("checksum") }
      CORES.each do |core|
        samples = (1..ROUNDS).map { run.fetch(:rows).fetch(core).fetch(_1).fetch(name).fetch("ns_per_op") }
        result["#{core}_round_ns".to_sym] = samples
        result["#{core}_median_ns".to_sym] = median(samples)
      end
      PEERS.each do |peer|
        result["swift_over_#{peer}".to_sym] = result.fetch(:swift_median_ns).fdiv(result.fetch("#{peer}_median_ns".to_sym))
      end
      result
    end
    summaries = [baseline_category(rows, "all", "all")]
    %i[kind dataset].each do |dimension|
      rows.group_by { _1.fetch(dimension) }.sort.each { |key, subset| summaries << baseline_category(subset, dimension.to_s, key) }
    end
    { schema: 1, mode: "baseline", validated_workloads: contract.names.length, cores: CORES, rounds: ROUNDS,
      definitions: { central_timing: "Median of three per-round ns_per_op values; rollback timings are per authored batch, concurrent ns/op measures wall-time throughput.",
                     swift_over_peer: "Swift median / peer median within this run. Greater than1 means Swift is slower.",
                     category: "Unweighted workload ratios only; geometric mean and median. Never sum unlike operation timings." },
      baseline: contract.baseline, corpus_sha256: contract.hashes, dataset_images: run.fetch(:data_facts),
      run: provenance(run), workloads: rows, categories: summaries }
  end

  def compare(before, after, contract)
    require_that(identical?(before.fetch(:data_facts), after.fetch(:data_facts)), "before/after dataset bytes/hash/fingerprint differ")
    %w[cores environment timing_policy data_directory].each do |field|
      require_that(identical?(before.fetch(:metadata).fetch(field), after.fetch(:metadata).fetch(field)), "before/after #{field} differs")
    end
    require_that(identical?(before.fetch(:shared), after.fetch(:shared)), "before/after checksums/rowcounts/shared fields differ")
    rows = contract.workloads.map do |workload|
      name = workload.fetch("name")
      result = { workload: name, kind: workload.fetch("kind"), dataset: workload.fetch("dataset"),
                 iterations: workload.fetch("iterations"), readers: workload.fetch("readers"), batch: workload.fetch("batch"),
                 rows_total: before.fetch(:shared).fetch(name).fetch("rows_total"), checksum: before.fetch(:shared).fetch(name).fetch("checksum") }
      [[:before, before], [:after, after]].each do |stage, run|
        CORES.each do |core|
          samples = (1..ROUNDS).map { run.fetch(:rows).fetch(core).fetch(_1).fetch(name).fetch("ns_per_op") }
          result["#{stage}_#{core}_round_ns".to_sym] = samples
          result["#{stage}_#{core}_median_ns".to_sym] = median(samples)
        end
      end
      CORES.each do |core|
        result["#{core}_speedup".to_sym] = result.fetch("before_#{core}_median_ns".to_sym).fdiv(result.fetch("after_#{core}_median_ns".to_sym))
      end
      PEERS.each do |peer|
        %w[before after].each do |stage|
          result["#{stage}_swift_over_#{peer}".to_sym] = result.fetch("#{stage}_swift_median_ns".to_sym).fdiv(result.fetch("#{stage}_#{peer}_median_ns".to_sym))
        end
      end
      result
    end
    summaries = [category(rows, "all", "all")]
    %i[kind dataset].each do |dimension|
      rows.group_by { _1.fetch(dimension) }.sort.each { |key, subset| summaries << category(subset, dimension.to_s, key) }
    end
    { schema: 1, validated_workloads: contract.names.length, cores: CORES, rounds: ROUNDS,
      definitions: { central_timing: "Median of three per-round ns_per_op values; rollback timings are per authored batch, concurrent ns/op measures wall-time throughput.",
                     swift_over_peer: "Swift median / peer median within the same run. Greater than1 means Swift is slower.",
                     speedup: "Before median / after median for the same core/workload. Greater than1 means faster after.",
                     category: "Unweighted workload ratios only; geometric mean and median. Never sum unlike operation timings." },
      baseline: contract.baseline, corpus_sha256: contract.hashes, dataset_images: before.fetch(:data_facts),
      before: provenance(before), after: provenance(after), workloads: rows, categories: summaries }
  end

  def provenance(run)
    metadata = run.fetch(:metadata)
    { directory: run.fetch(:directory), actors: metadata.fetch("actors").transform_values { _1.fetch("sha256") },
      git_head: metadata["git_head"], git_status: metadata["git_status"], source_provenance: metadata["source_provenance"],
      source_files: metadata["source_files"], environment: metadata.fetch("environment"), commands: metadata["commands"],
      cpu: metadata["cpu"], filesystem: metadata["filesystem"], uname: metadata["uname"], evidence_sha256: run.fetch(:evidence) }
  end

  def write_csv(path, rows)
    columns = rows.first.keys
    CSV.open(path, "w") do |csv|
      csv << columns
      rows.each { |row| csv << columns.map { |column| row[column].is_a?(Array) ? JSON.generate(row[column]) : row[column] } }
    end
  end

  def report_markdown(report)
    return baseline_markdown(report) if report[:mode] == "baseline"
    lines = ["Validated #{report.fetch(:validated_workloads)} workloads ×4 cores ×3 rounds in each run.", "",
             "Timings are medians of the three round-level ns/op values. Swift/peer >1 means Swift is slower; before/after speedup >1 means improvement.", "",
             "Operation units stay workload-specific: rollback timing includes its complete authored batch. Concurrent ns/op is wall time divided by total iterations (throughput), not per-reader-block latency. Category summaries give each workload equal weight and combine only dimensionless ratios.", "",
             "| Category | Workloads | Swift speedup, geometric mean | Swift/Rust after | Swift/Go after | Swift/TS after |",
             "|---|---:|---:|---:|---:|---:|"]
    report.fetch(:categories).select { %w[all kind].include?(_1.fetch(:dimension)) }.each do |row|
      lines << "| #{row.fetch(:category)} | #{row.fetch(:workloads)} | #{format('%.4fx', row.fetch(:swift_speedup_gmean))} | #{PEERS.map { format('%.4fx', row.fetch("after_swift_over_#{_1}_gmean".to_sym)) }.join(' | ')} |"
    end
    lines += ["", "Category ratios above are unweighted geometric means. Both geometric means and medians, before/after peer ratios, and peer speedups appear in categories.csv.", "",
              "workloads.csv retains all three round timings and the median for every core. Three rounds do not establish statistical significance; inspect round variation and peer drift before attributing small changes to code.", "",
              "Validation relies on the successful harness report for actor immutability, raw-sample checks and dataset-copy checks. This helper revalidates result rows and small corpus/evidence files; it does not reread dataset binaries, actor binaries or full sample arrays.", ""]
    lines.join("\n")
  end

  def baseline_markdown(report)
    lines = ["Validated one complete baseline: #{report.fetch(:validated_workloads)} workloads ×4 cores ×3 rounds.", "",
             "Timings are medians of the three round-level ns/op values. Swift/peer >1 means Swift is slower within this run.", "",
             "Operation units stay workload-specific: rollback timing includes its complete authored batch. Concurrent ns/op is wall time divided by total iterations (throughput), not per-reader-block latency. Category summaries give each workload equal weight and combine only dimensionless ratios.", "",
             "| Category | Workloads | Swift/Rust | Swift/Go | Swift/TS |",
             "|---|---:|---:|---:|---:|"]
    report.fetch(:categories).select { %w[all kind].include?(_1.fetch(:dimension)) }.each do |row|
      lines << "| #{row.fetch(:category)} | #{row.fetch(:workloads)} | #{PEERS.map { format('%.4fx', row.fetch("swift_over_#{_1}_gmean".to_sym)) }.join(' | ')} |"
    end
    lines += ["", "Category ratios above are unweighted geometric means. Both geometric means and medians appear in categories.csv, including dataset groups.", "",
              "workloads.csv retains all three round timings and the median for every core. Three rounds do not establish statistical significance; inspect round variation before interpreting small ratios.", "",
              "Validation relies on the successful harness report for actor immutability, raw-sample checks and dataset-copy checks. This helper revalidates result rows and small corpus/evidence files; it does not reread dataset binaries, actor binaries or full sample arrays.", ""]
    lines.join("\n")
  end

  def emit(report, output)
    output = File.expand_path(output)
    require_that(!File.exist?(output) || (File.directory?(output) && Dir.empty?(output)), "report output must be absent or empty")
    # Recheck small evidence files immediately before publishing the derived report.
    stages = report[:mode] == "baseline" ? [:run] : %i[before after]
    stages.each do |stage|
      report.fetch(stage).fetch(:evidence_sha256).each do |path, hash|
        require_that(Digest::SHA256.file(path).hexdigest == hash, "evidence changed during analysis: #{path}")
      end
    end
    FileUtils.mkdir_p(output)
    name = report[:mode] == "baseline" ? "baseline.json" : "comparison.json"
    File.write(File.join(output, name), JSON.pretty_generate(report) + "\n")
    write_csv(File.join(output, "workloads.csv"), report.fetch(:workloads))
    write_csv(File.join(output, "categories.csv"), report.fetch(:categories))
    File.write(File.join(output, "report.md"), report_markdown(report))
  end

  def parse_options(argv)
    options = { repo: ROOT }
    OptionParser.new do |parser|
      parser.banner = "mise run swift-native:bench-report --baseline COMPLETE_RUN --output EMPTY_DIR\n" \
                      "mise run swift-native:bench-report --before COMPLETE_RUN --after COMPLETE_RUN --output EMPTY_DIR"
      parser.on("--baseline PATH", "Summarize one successful full pinned-workload, four-core, three-round run") { options[:baseline] = _1 }
      parser.on("--before PATH", "Complete baseline run for before/after comparison") { options[:before] = _1 }
      parser.on("--after PATH", "Complete later run; requires --before") { options[:after] = _1 }
      parser.on("--output PATH", "Absent or empty destination for JSON, workload/category CSVs and report.md") { options[:output] = _1 }
      parser.on("--repo PATH", "Canonical repository; defaults to this tool's checkout") { options[:repo] = _1 }
      parser.on("--check-adapter", "Run tiny synthetic validation/arithmetic checks only") { options[:check_adapter] = true }
      parser.on("-h", "--help") { puts parser; return nil }
    end.parse!(argv)
    if options[:check_adapter]
      require_that(argv.empty? && %i[baseline before after output].none? { options.key?(_1) }, "--check-adapter cannot be combined with run/output arguments")
      return options
    end
    require_that(argv.empty? && options.key?(:output), "--output is required; no positional arguments")
    valid = if options.key?(:baseline)
              !options.key?(:before) && !options.key?(:after)
            else
              options.key?(:before) && options.key?(:after)
            end
    require_that(valid, "use either --baseline or both --before and --after")
    options
  end

  def cli(argv)
    options = parse_options(argv)
    return 0 unless options
    if options[:check_adapter]
      require_relative "benchmark_analysis_checks"
      BenchmarkAnalysisChecks.run
      return 0
    end
    contract = Contract.load(options.fetch(:repo))
    report = if options.key?(:baseline)
               summarize(load_run(options.fetch(:baseline), contract), contract)
             else
               before = load_run(options.fetch(:before), contract); after = load_run(options.fetch(:after), contract)
               compare(before, after, contract)
             end
    emit(report, options.fetch(:output))
    puts "Validated #{contract.names.length} workloads ×4 cores ×3 rounds per run; report: #{File.expand_path(options.fetch(:output))}"
    0
  rescue Invalid, KeyError, JSON::ParserError, OptionParser::ParseError, SystemCallError => error
    warn "benchmark analysis rejected: #{error.message}"
    1
  end
end

exit BenchmarkComparison.cli(ARGV) if $PROGRAM_NAME == __FILE__
