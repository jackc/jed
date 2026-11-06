# frozen_string_literal: true

require_relative "swift_task"
require_relative "../../../scripts/lib/bundle_setup"
require "toml-rb"
require "digest"
require "json"
require "tmpdir"
require "timeout"
require "find"
require "time"

# Explicit experiment only. Shared benchmark inputs and peer runners are data
# and transport references; no default benchmark/CI task imports this module.
module SwiftNativeBenchmark
  ROOT = SwiftNativeTasks::ROOT
  BASELINE = "362e0798ac3a8bd04bd2da556044b6ec73bf468a"
  CORES = %w[swift rust go ts].freeze
  HASHES = {
    "benchmarks.toml" => "a7be0f426d9d4432b238eca05b73d6a29b4a114b9c8aaca6c85f71cb94fe3f96",
    "datasets.toml" => "b9bfb4cb1f27ae1757268a94434e96df67626bc6b39f005c3626ab3a6e289133",
    "spill.toml" => "53c7d9034296e5c6cb03c61f30323f859f034a0ceb8645a49032f00790f084fb"
  }.freeze
  INTEGER_FIELDS = %w[iterations warmup readers total_ns ns_per_op min_ns p50_ns p90_ns p99_ns rows_total].freeze
  SHARED_FIELDS = %w[bench dataset iterations warmup readers rows_total checksum fingerprint].freeze
  module_function

  def selected_cores(value)
    result = value.split(",", -1)
    raise "--cores requires unique tokens including swift: #{CORES.join(',')}" unless result.uniq == result && result.include?("swift") && (result - CORES).empty?
    result
  end

  def input(filter = nil)
    source = HASHES.to_h do |name, hash|
      bytes = File.binread(File.join(ROOT, "bench/corpus", name))
      raise "shared benchmark input changed: #{name}" unless Digest::SHA256.hexdigest(bytes) == hash
      pinned, status = Open3.capture2("git", "show", "#{BASELINE}:bench/corpus/#{name}", chdir: ROOT)
      raise "benchmark input is not the recorded baseline: #{name}" unless status.success? && pinned.b == bytes
      [name, bytes]
    end
    corpus = TomlRB.parse(source.fetch("benchmarks.toml").dup.force_encoding(Encoding::UTF_8))
    datasets = TomlRB.parse(source.fetch("datasets.toml").dup.force_encoding(Encoding::UTF_8))
    raise "unknown benchmark schema" unless corpus.fetch("schema_version") == 1 && datasets.fetch("schema_version") == 1
    all = corpus.fetch("bench").select { |bench| !bench.key?("engines") || bench.fetch("engines").include?("jed") }
    raise "expected exactly 143 unique jed workloads" unless all.length == 143 && all.map { _1.fetch("name") }.uniq.length == 143
    chosen = all.select { |bench| !filter || bench.fetch("name").include?(filter) }
    raise "benchmark filter selects no workloads" if chosen.empty?
    workloads = chosen.map { normalize(_1) }
    rows = datasets.fetch("dataset").to_h { |dataset| [dataset.fetch("name"), dataset.fetch("table").to_h { |table| [table.fetch("name"), table.fetch("rows")] }] }
    [{ schema: 1, fingerprint: HASHES.fetch("datasets.toml"), benchmarks: workloads, tableRows: rows }, source, all.map { _1.fetch("name") }]
  end

  def normalize(bench)
    parameters = bench.fetch("param", []).map do |parameter|
      fields = %w[min max start min_len max_len base off_min off_max elem_min elem_max].to_h { [_1, parameter.fetch(_1, 0)] }
      fields.transform_keys! { |key| key.gsub(/_([a-z])/) { Regexp.last_match(1).upcase } }
      fields.merge("gen" => parameter.fetch("gen"))
    end
    {
      name: bench.fetch("name"), dataset: bench.fetch("dataset"), kind: bench.fetch("kind"),
      sql: bench.fetch("sql_override", {}).fetch("jed", bench.fetch("sql")),
      setupSQL: bench.fetch("setup_sql_override", {}).fetch("jed", bench.fetch("setup_sql", [])),
      warmup: bench.fetch("warmup"), iterations: bench.fetch("iterations"), seed: bench.fetch("seed"),
      expectRowsPerIter: bench.fetch("expect_rows_per_iter", 0), batch: bench.fetch("batch", 0),
      readers: bench.fetch("readers", 0), relaxed: bench.fetch("relaxed", false),
      hostFunctions: bench.fetch("host_functions", false), parameters: parameters
    }
  end

  def datasets(data, workloads)
    errors = []
    result = workloads.map { _1.fetch(:dataset) }.uniq.reject { _1 == "scratch" }.sort.to_h do |name|
      file = File.join(data, "#{name}.jed"); sidecar = file + ".fingerprint"
      if !File.file?(file) || !File.file?(sidecar)
        errors << "missing #{name}.jed or its fingerprint"
        next [name, nil]
      end
      fingerprint = File.read(sidecar).strip
      unless fingerprint == HASHES.fetch("datasets.toml")
        errors << "stale #{name}.jed fingerprint #{fingerprint}"
        next [name, nil]
      end
      [name, { path: file, bytes: File.size(file), sha256: Digest::SHA256.file(file).hexdigest, fingerprint: fingerprint }]
    end
    raise "benchmark datasets unavailable (use mise run swift-native:bench-setup): #{errors.join('; ')}" unless errors.empty?
    result
  end

  def command(arguments, directory: ROOT, environment: {})
    output, error, status = Open3.capture3(environment, *arguments, chdir: directory)
    { command: arguments, directory: directory, status: status.exitstatus, stdout: output, stderr: error }
  end

  def launch(arguments, directory:, environment:, log:, seconds:)
    File.write(log + ".command.json", JSON.pretty_generate(command: arguments, directory: directory, environment: environment, timeout_seconds: seconds) + "\n")
    puts "running #{File.basename(log)}"; $stdout.flush
    File.open(log, "w") do |output|
      pid = Process.spawn(environment, *arguments, chdir: directory, out: output, err: [:child, :out], pgroup: true)
      begin
        _, status = Timeout.timeout(seconds) { Process.waitpid2(pid) }
        status.success?
      rescue Timeout::Error
        Process.kill("KILL", -pid) rescue Errno::ESRCH
        Process.waitpid(pid) rescue Errno::ECHILD
        output.puts("benchmark timed out after #{seconds}s"); false
      end
    end
  end

  def executable(source, destination)
    raise "missing executable #{source}; build required" unless File.file?(source) && File.executable?(source)
    before = Digest::SHA256.file(source).hexdigest
    FileUtils.copy_file(source, destination); File.chmod(0o500, destination)
    raise "executable changed during capture" unless Digest::SHA256.file(destination).hexdigest == before && Digest::SHA256.file(source).hexdigest == before
    { path: destination, sha256: before }
  end

  # Follow installed-package symlinks only inside this checkout, then copy bytes
  # into a self-contained relative import tree. No dependency is installed here.
  def tree_files(source, ancestors = [])
    real = File.realpath(source)
    raise "snapshot dependency escapes checkout: #{source}" unless real.start_with?(ROOT + "/")
    raise "snapshot dependency cycle: #{source}" if ancestors.include?(real)
    if File.directory?(source)
      Dir.children(source).sort.flat_map { |child| tree_files(File.join(source, child), ancestors + [real]) }
    elsif File.file?(source)
      [source]
    else
      raise "unexpected snapshot object: #{source}"
    end
  end

  def hash_files(paths) = paths.to_h { |path| [path.delete_prefix(ROOT + "/"), Digest::SHA256.file(path).hexdigest] }

  def capture_sources(output)
    # Git diff excludes untracked experimental source. Keep the exact native
    # source and build locks alongside each executable, even before checkpoint.
    inputs = %w[impl/swift-native/Sources impl/swift-native/Package.swift impl/swift-native/Package.resolved
                impl/swift-native/Tools/benchmark.rb mise-tasks/swift-native/bench
                bench/go/cmd/bench-jed bench/go/internal/bench bench/go/go.mod bench/go/go.sum
                bench/rust/src bench/rust/Cargo.toml bench/rust/Cargo.lock]
    paths = inputs.flat_map { tree_files(File.join(ROOT, _1)) }.uniq.sort
    before = hash_files(paths)
    destination = File.join(output, "source")
    paths.each do |path|
      relative = path.delete_prefix(ROOT + "/")
      copy = File.join(destination, relative)
      FileUtils.mkdir_p(File.dirname(copy)); FileUtils.copy_file(path, copy)
      raise "source snapshot copy changed: #{relative}" unless Digest::SHA256.file(copy).hexdigest == before.fetch(relative)
    end
    raise "source changed during capture" unless hash_files(paths) == before
    File.write(File.join(output, "source-sha256.json"), JSON.pretty_generate(before) + "\n")
    before
  end

  def actors(output, cores, no_build, timeout)
    directory = File.join(output, "actors"); FileUtils.mkdir(directory)
    unless no_build
      environment, swift = SwiftNativeTasks.command("build", configuration: "release", extra: ["--product", "jed-bench"])
      builds = [["swift", swift, SwiftNativeTasks::PACKAGE, environment]]
      builds << ["go", ["go", "build", "-o", "bin/bench-jed", "./cmd/bench-jed"], File.join(ROOT, "bench/go"), { "CGO_ENABLED" => "0", "GOPROXY" => "off" }] if cores.include?("go")
      builds << ["rust", ["cargo", "build", "--release", "--locked", "--offline", "--manifest-path", "bench/rust/Cargo.toml", "--bin", "bench-jed"], ROOT, {}] if cores.include?("rust")
      builds.each do |core, argv, cwd, env|
        raise "#{core} build failed" unless launch(argv, directory: cwd, environment: env, log: File.join(output, "build-#{core}.log"), seconds: timeout)
      end
    end
    result = {}
    swift = File.join(SwiftNativeTasks::BUILD, "release/jed-bench")
    result["swift"] = executable(swift, File.join(directory, "jed-bench"))
    { "go" => "bench/go/bin/bench-jed", "rust" => "bench/rust/target/release/bench-jed" }.each do |core, path|
      result[core] = executable(File.join(ROOT, path), File.join(directory, "#{core}-bench-jed")) if cores.include?(core)
    end
    if cores.include?("ts")
      paths = %w[bench/ts/src bench/ts/package.json bench/ts/package-lock.json bench/ts/node_modules impl/ts/src impl/ts/package.json impl/ts/package-lock.json impl/ts/node_modules impl/ts/jed_lock.node].flat_map { tree_files(File.join(ROOT, _1)) }.uniq.sort
      before = hash_files(paths)
      snapshot = File.join(directory, "ts")
      paths.each do |source|
        destination = File.join(snapshot, source.delete_prefix(ROOT + "/"))
        FileUtils.mkdir_p(File.dirname(destination)); FileUtils.copy_file(source, destination)
        raise "TS snapshot copy changed: #{source}" unless Digest::SHA256.file(destination).hexdigest == before.fetch(source.delete_prefix(ROOT + "/"))
      end
      raise "TS source changed during capture" unless hash_files(paths) == before
      node_path, node_status = Open3.capture2("which", "node")
      raise "Node executable not found" unless node_status.success? && !node_path.strip.empty?
      node = executable(File.realpath(node_path.strip), File.join(directory, "node"))
      result["ts"] = { path: File.join(snapshot, "bench/ts/src/bench-jed.ts"), node: node, files: before,
                       sha256: Digest::SHA256.hexdigest(JSON.generate(before)) }
    end
    result
  end

  def verify_actors(actors)
    actors.each do |core, actor|
      if core == "ts"
        root = File.expand_path("../../..", File.dirname(actor.fetch(:path)))
        actor.fetch(:files).each do |relative, hash|
          raise "captured TS source changed: #{relative}" unless Digest::SHA256.file(File.join(root, relative)).hexdigest == hash
        end
        node = actor.fetch(:node)
        raise "captured Node changed" unless Digest::SHA256.file(node.fetch(:path)).hexdigest == node.fetch(:sha256)
      else
        raise "captured #{core} actor changed" unless Digest::SHA256.file(actor.fetch(:path)).hexdigest == actor.fetch(:sha256)
      end
    end
  end

  def valid_started_at?(value)
    # All four actors emit UTC RFC3339 to whole seconds. Round-trip as well as
    # parsing: Time can normalize impossible dates or an out-of-range hour.
    return false unless value.is_a?(String) && value.match?(/\A[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z\z/)
    Time.iso8601(value).utc.iso8601 == value
  rescue ArgumentError
    false
  end

  def validate_results(path, core, workloads)
    expected = workloads.to_h { [_1.fetch(:name), _1] }
    found = {}
    File.foreach(path).with_index(1) do |line, number|
      row = JSON.parse(line)
      name = row.fetch("bench")
      raise "#{core}: duplicate or unexpected benchmark #{name}" unless expected.key?(name) && !found.key?(name)
      bench = expected.fetch(name)
      raise "#{core}: invalid result schema/identity at #{number}" unless row.fetch("schema") == 2 && row.fetch("engine") == "jed" && row.fetch("lang") == core
      raise "#{core}: wrong benchmark variant" unless row.fetch("variant") == (core == "swift" ? "experimental-native" : "core")
      raise "#{core}: malformed started_at at #{number}" unless valid_started_at?(row["started_at"])
      raise "#{core}: malformed numeric results" unless INTEGER_FIELDS.all? { |field| row.fetch(field).is_a?(Integer) && row.fetch(field) >= 0 }
      expected_fields = { "dataset" => bench.fetch(:dataset), "iterations" => bench.fetch(:iterations), "warmup" => bench.fetch(:warmup),
                          "readers" => bench.fetch(:readers), "fingerprint" => HASHES.fetch("datasets.toml") }
      raise "#{core}: workload parameters changed for #{name}" unless expected_fields.all? { |key, value| row.fetch(key) == value }
      raise "#{core}: malformed checksum" unless row.fetch("checksum").match?(/\A[0-9a-f]{16}\z/)
      raise "#{core}: malformed timing summary" unless row.fetch("ns_per_op") == row.fetch("total_ns") / row.fetch("iterations") &&
        %w[min_ns p50_ns p90_ns p99_ns].map { row.fetch(_1) }.each_cons(2).all? { |a, b| a <= b }
      found[name] = row
    end
    raise "#{core}: missing results #{(expected.keys - found.keys).join(',')}" unless found.keys.sort == expected.keys.sort
    found
  end

  def validate_samples(path, workloads, results)
    rows = File.readlines(path).map { JSON.parse(_1) }
    raise "Swift samples missing or duplicated" unless rows.map { _1.fetch("bench") }.sort == workloads.map { _1.fetch(:name) }.sort
    rows.each do |row|
      name = row.fetch("bench"); samples = row.fetch("samples_ns"); result = results.fetch(name)
      raise "Swift sample count/type mismatch: #{name}" unless samples.length == result.fetch("iterations") && samples.all? { _1.is_a?(Integer) && _1 >= 0 }
      sorted = samples.sort
      %w[min_ns p50_ns p90_ns p99_ns].zip([0, 50, 90, 99]).each do |field, percent|
        raise "Swift sample percentile mismatch: #{name}" unless result.fetch(field) == sorted[(samples.length - 1) * percent / 100]
      end
      if result.fetch("readers").zero?
        raise "Swift sample total mismatch: #{name}" unless samples.sum == result.fetch("total_ns")
      end
    end
  end

  def self_check
    projected, _, all = input
    raise "workload inventory" unless projected.fetch(:benchmarks).length == 143 && all.uniq.length == 143
    raise "five workload kinds" unless projected.fetch(:benchmarks).map { _1.fetch(:kind) }.uniq.sort == %w[cold_open concurrent_read query write_durable write_rollback]
    ["", "go", "swift,", "swift,swift", "swift,unknown"].each do |value|
      begin; selected_cores(value); raise "invalid cores accepted: #{value.inspect}"; rescue RuntimeError => error
        raise if error.message.start_with?("invalid cores accepted")
      end
    end
    begin; input("jed-benchmark-absent-314159"); raise "empty filter accepted"; rescue RuntimeError => error
      raise if error.message == "empty filter accepted"
    end
    Dir.mktmpdir("jed-swift-bench-adapter-") do |directory|
      bench = projected.fetch(:benchmarks).first
      good = { "schema" => 2, "engine" => "jed", "lang" => "swift", "variant" => "experimental-native", "bench" => bench.fetch(:name),
               "dataset" => bench.fetch(:dataset), "iterations" => bench.fetch(:iterations), "warmup" => bench.fetch(:warmup), "readers" => bench.fetch(:readers),
               "total_ns" => bench.fetch(:iterations), "ns_per_op" => 1, "min_ns" => 1, "p50_ns" => 1, "p90_ns" => 1, "p99_ns" => 1,
               "rows_total" => bench.fetch(:iterations), "checksum" => "0123456789abcdef", "fingerprint" => HASHES.fetch("datasets.toml"),
               "started_at" => "2026-11-06T12:34:56Z" }
      path = File.join(directory, "result.jsonl")
      # Go UTC/RFC3339, Rust's civil formatter, TS after removing milliseconds,
      # and Swift's default ISO8601DateFormatter emit this same whole-second form.
      CORES.each do |core|
        [good.fetch("started_at"), "2024-02-29T00:00:00Z"].each do |timestamp|
          row = good.merge("lang" => core, "variant" => core == "swift" ? "experimental-native" : "core", "started_at" => timestamp)
          File.write(path, JSON.generate(row) + "\n"); validate_results(path, core, [bench])
        end
      end
      ["", (JSON.generate(good) + "\n") * 2, JSON.generate(good.merge("warmup" => 0)) + "\n"].each do |invalid|
        File.write(path, invalid)
        begin; validate_results(path, "swift", [bench]); raise "invalid results accepted"; rescue RuntimeError => error
          raise if error.message == "invalid results accepted"
        end
      end
      invalid_timestamps = [nil, 0, "", "not-a-time", "2026-02-29T12:34:56Z", "2026-02-30T12:34:56Z",
                            "2026-13-01T12:34:56Z", "2026-11-06T24:00:00Z", "2026-11-06T12:60:00Z",
                            "2026-11-06T12:34:60Z", "2026-11-06T12:34:56", "2026-11-06 12:34:56Z",
                            "2026-11-06T12:34:56Z\n"]
      invalid_rows = [good.reject { |key, _| key == "started_at" }] + invalid_timestamps.map { good.merge("started_at" => _1) }
      invalid_rows.each do |row|
        File.write(path, JSON.generate(row) + "\n")
        begin; validate_results(path, "swift", [bench]); raise "invalid timestamp accepted"; rescue RuntimeError => error
          raise unless error.message == "swift: malformed started_at at 1"
        end
      end
    end
    puts "benchmark adapter: 143 shared workloads; membership/parameter/timestamp rejection checks PASS"
  end

  def run
    return self_check.then { 0 } if ENV["usage_check_adapter"] == "true"
    cores = selected_cores(ENV.fetch("usage_cores", CORES.join(",")))
    rounds = Integer(ENV.fetch("usage_rounds", "3"), 10); timeout = Integer(ENV.fetch("usage_timeout", "21600"), 10)
    raise "rounds and timeout must be positive" unless rounds.positive? && timeout.positive?
    filter = ENV["usage_filter"]
    raise "--filter must be nonempty" if filter == ""
    projected, sources, all = input(filter)
    data = File.expand_path(ENV.fetch("usage_data", File.join(ROOT, "bench/data/swift-native")))
    images = datasets(data, projected.fetch(:benchmarks))
    output = ENV["usage_output"] ? File.expand_path(ENV.fetch("usage_output")) : Dir.mktmpdir("jed-swift-benchmark-")
    FileUtils.mkdir_p(output)
    raise "output directory must be empty" unless Dir.empty?(output)
    corpus = File.join(output, "corpus"); FileUtils.mkdir(corpus)
    sources.each { |name, bytes| File.binwrite(File.join(corpus, name), bytes) }
    input_path = File.join(output, "input.json"); File.write(input_path, JSON.pretty_generate(projected) + "\n")
    metadata = {
      baseline: BASELINE, corpus_sha256: HASHES, dataset_images: images, data_directory: data,
      selected: projected.fetch(:benchmarks).map { _1.fetch(:name) }, omitted: all - projected.fetch(:benchmarks).map { _1.fetch(:name) },
      diagnostic: !filter.nil? || cores.sort != CORES.sort, cores: cores, rounds: rounds,
      commands: %w[swift rustc go node ruby].to_h { |tool| [tool, command([tool, tool == "go" ? "version" : "--version"])] },
      git_head: command(%w[git rev-parse HEAD]), git_status: command(%w[git status --porcelain]),
      uname: command(%w[uname -a]), cpu: command(%w[lscpu]), filesystem: command(["df", "-T", data]),
      environment: ENV.select { |key, _| %w[GOMAXPROCS GOGC GOMEMLIMIT RUSTFLAGS RUSTDOCFLAGS SWIFT_DETERMINISTIC_HASHING NODE_OPTIONS LD_PRELOAD MALLOC_ARENA_MAX OMP_NUM_THREADS MKL_NUM_THREADS].include?(key) },
      cleared_environment_names: ENV.keys.grep(/\AJED_|\A(?:NODE_OPTIONS|NODE_PATH|SWIFT_DETERMINISTIC_HASHING)\z/).sort,
      timing_policy: "sequential cores; rotating order per round; canonical warmup/iterations; native durable files; no optimization measurements before correctness acceptance"
    }
    File.write(File.join(output, "source.diff"), command(%w[git diff --binary HEAD]).fetch(:stdout))
    actors = actors(output, cores, ENV["usage_no_build"] == "true", timeout)
    metadata[:actors] = actors
    metadata[:source_files] = capture_sources(output)
    metadata[:source_provenance] = "Sources observed at actor capture; this snapshot alone does not establish the build provenance of an existing binary, especially with --no-build."
    File.write(File.join(output, "metadata.json"), JSON.pretty_generate(metadata) + "\n")
    report = { metadata: "metadata.json", completed: [], failures: [] }
    rounds.times do |round|
      comparisons = {}
      cores.rotate(round % cores.length).each do |core|
        directory = File.join(output, "round-#{round + 1}-#{core}"); FileUtils.mkdir(directory)
        # Put database and scratch files beside the canonical source data, not
        # under /tmp evidence: /tmp may be tmpfs and invalidate durable timing.
        run_data = Dir.mktmpdir("swift-bench-#{round + 1}-#{core}-", data)
        File.write(File.join(directory, "data-location.json"), JSON.generate(path: run_data, source: data) + "\n")
        begin
          verify_actors(actors)
          images.each do |name, image|
            raise "dataset changed before copy: #{name}" unless Digest::SHA256.file(image.fetch(:path)).hexdigest == image.fetch(:sha256)
            destination = File.join(run_data, "#{name}.jed")
            FileUtils.copy_file(image.fetch(:path), destination)
            raise "dataset copy mismatch: #{name}" unless Digest::SHA256.file(destination).hexdigest == image.fetch(:sha256)
            File.write(destination + ".fingerprint", image.fetch(:fingerprint) + "\n")
          end
          results = File.join(directory, "results.jsonl"); samples = File.join(directory, "samples.jsonl")
          argv = if core == "swift"
                   [actors.fetch(core).fetch(:path), input_path, run_data, results, samples]
                 else
                   (core == "ts" ? [actors.fetch(core).fetch(:node).fetch(:path), actors.fetch(core).fetch(:path)] : [actors.fetch(core).fetch(:path)]) + [corpus, run_data, results] + (filter ? [filter] : [])
                 end
          # Do not inherit experimental limits, lock overrides or loader flags
          # that would change a peer's workload/durability without a corpus edit.
          env = ENV.keys.grep(/\AJED_|\A(?:NODE_OPTIONS|NODE_PATH|SWIFT_DETERMINISTIC_HASHING)\z/).to_h { [_1, nil] }
          ok = launch(argv, directory: ROOT, environment: env, log: File.join(directory, "run.log"), seconds: timeout)
          raise "#{core} benchmark process failed; inspect #{directory}/run.log" unless ok
          verify_actors(actors)
          parsed = validate_results(results, core, projected.fetch(:benchmarks))
          validate_samples(samples, projected.fetch(:benchmarks), parsed) if core == "swift"
          comparisons[core] = parsed
          report[:completed] << { round: round + 1, core: core, workloads: parsed.length, results: results }
        rescue StandardError => error
          report[:failures] << { round: round + 1, core: core, error: error.message }
        ensure
          # Keep results/provenance; bounded disk use never retains N copies of
          # the large data set. The immutable source image remains untouched.
          FileUtils.remove_entry(run_data) if File.directory?(run_data)
        end
        File.write(File.join(output, "report.json"), JSON.pretty_generate(report) + "\n")
      end
      reference = comparisons["swift"]
      comparisons.each do |core, values|
        next if core == "swift" || !reference
        reference.each do |name, row|
          SHARED_FIELDS.each do |field|
            next if row.fetch(field) == values.fetch(name).fetch(field)
            report[:failures] << { round: round + 1, core: core, benchmark: name, field: field,
                                   swift: row.fetch(field), peer: values.fetch(name).fetch(field) }
          end
        end
      end
      File.write(File.join(output, "report.json"), JSON.pretty_generate(report) + "\n")
    end
    expected = rounds * cores.length
    report[:failures] << { error: "incomplete core/round membership" } unless report[:completed].length == expected
    report[:success] = report[:failures].empty?
    File.write(File.join(output, "report.json"), JSON.pretty_generate(report) + "\n")
    puts "#{report[:completed].length}/#{expected} core rounds; #{report[:failures].length} failures; evidence #{output}"
    report[:success] ? 0 : 1
  rescue StandardError => error
    warn error.message; 1
  end
end
