# frozen_string_literal: true

require_relative "benchmark"
require "set"

# The native worker executes SQL through public Swift APIs. This driver only
# converts frozen TOML data, launches real processes and checks complete output.
module SwiftNativeStress
  extend self
  ROOT = SwiftNativeTasks::ROOT
  BASELINE = SwiftNativeBenchmark::BASELINE
  CORES = %w[swift rust go ts].freeze
  PROFILE_HASHES = { "balance_transfer.stress.toml" => "474d387221395587874f488b5de52e9cac2d6001f93d64ad56834a9b5abb67d8" }.freeze

  def inventory
    names = Dir.glob("*.stress.toml", base: File.join(ROOT, "stress")).sort
    raise "stress inventory differs from frozen baseline" unless names == PROFILE_HASHES.keys.sort
    sources = names.to_h do |name|
      bytes = File.binread(File.join(ROOT, "stress", name))
      pinned, status = Open3.capture2("git", "show", "#{BASELINE}:stress/#{name}", chdir: ROOT)
      unless status.success? && pinned.b == bytes && Digest::SHA256.hexdigest(bytes) == PROFILE_HASHES.fetch(name)
        raise "stress profile differs from frozen baseline: #{name}"
      end
      [name, bytes]
    end
    profiles = sources.values.map do |source|
      input = TomlRB.parse(source.dup.force_encoding(Encoding::UTF_8))
      meta = input.fetch("meta"); final = input.fetch("final", {})
      {
        name: meta.fetch("name"), parallel: meta.fetch("parallel", "optional"), seed: meta.fetch("seed", 0),
        setup: input.fetch("setup").fetch("sql"),
        workers: input.fetch("worker").map { |worker| {
          kind: worker.fetch("kind"), count: worker.fetch("count"), iterations: worker.fetch("iterations"),
          op: worker.fetch("op", ""), invariantQuery: worker.fetch("invariant_query", ""),
          invariantExpect: worker.fetch("invariant_expect", "")
        } },
        finalQuery: final["query"], finalExpect: final.fetch("expect", []), crossCoreChecksum: final.fetch("cross_core_checksum", false)
      }
    end
    raise "duplicate stress profile names" unless profiles.map { _1.fetch(:name) }.uniq.length == profiles.length
    [profiles, sources]
  end

  def validate(rows, profiles, core:, storage:)
    failures = []
    expected = profiles.map { _1.fetch(:name) }
    failures << "#{core}/#{storage}: missing/duplicate/unexpected profile outputs" unless rows.map { _1["name"] } == expected
    rows.zip(profiles).each do |row, profile|
      next unless row && profile
      writers = profile.fetch(:workers).select { _1.fetch(:kind) == "writer" }
      checks = profile.fetch(:workers).select { _1.fetch(:kind) == "reader" }.sum { _1.fetch(:count) * _1.fetch(:iterations) }
      required = { "schema" => 1, "name" => profile.fetch(:name), "lang" => core, "status" => "pass",
        "mode" => core == "ts" ? "sequential" : "threaded", "invariant_checks" => checks,
        "writers" => writers.sum { _1.fetch(:count) }, "writer_iters" => writers.sum { _1.fetch(:count) * _1.fetch(:iterations) },
        "final_ok" => true, "cross_core_checksum" => profile.fetch(:crossCoreChecksum) }
      required["storage"] = storage if core == "swift"
      required.each do |key, value|
        matches = row[key] == value && (!value.is_a?(Integer) || row[key].is_a?(Integer))
        failures << "#{core}/#{storage}/#{profile.fetch(:name)}: #{key}=#{row[key].inspect}, expected #{value.inspect}" unless matches
      end
      failures << "#{core}/#{storage}: invalid checksum" unless row["checksum"].is_a?(String) && row["checksum"].match?(/\A[0-9a-f]{16}\z/)
    end
    failures
  end

  def check_adapter
    profiles, = inventory
    profile = profiles.fetch(0)
    valid = { "schema" => 1, "name" => profile.fetch(:name), "lang" => "swift", "storage" => "memory", "mode" => "threaded", "status" => "pass",
      "invariant_checks" => 4000, "writers" => 4, "writer_iters" => 1000, "final_ok" => true,
      "cross_core_checksum" => true, "checksum" => "0000000000000000" }
    raise "valid stress output rejected" unless validate([valid], profiles, core: "swift", storage: "memory").empty?
    [[], [valid, valid], [valid.merge("invariant_checks" => 3999)], [valid.merge("status" => "skip")],
     [valid.merge("final_ok" => false)], [valid.merge("checksum" => "garbage")], [valid.reject { |key, _| key == "schema" }],
     [valid.merge("schema" => 2)], [valid.merge("invariant_checks" => 4000.0)]].each do |rows|
      raise "incomplete stress output accepted" if validate(rows, profiles, core: "swift", storage: "memory").empty?
    end
    puts "stress adapter checks passed (frozen input, complete workers/invariants, exact final-state status)"
    0
  end

  def launch(command, directory:, environment: {}, log:, seconds: 300)
    SwiftNativeBenchmark.launch(command, directory: directory, environment: environment, log: log, seconds: seconds)
  end

  def run
    return check_adapter if ENV["usage_check_adapter"] == "true"
    cores = ENV.fetch("usage_cores", CORES.join(",")).split(",", -1)
    raise "cores must be unique supported names including swift" unless cores.include?("swift") && cores.uniq == cores && (cores - CORES).empty?
    configuration = ENV.fetch("usage_configuration", "debug")
    profiles, sources = inventory
    output = ENV["usage_output"] ? File.expand_path(ENV["usage_output"]) : Dir.mktmpdir("jed-swift-stress-")
    raise "output directory must be absent or empty" if File.exist?(output) && !Dir.empty?(output)
    FileUtils.mkdir_p(output)
    puts "stress evidence: #{output}"
    profile_directory = File.join(output, "profiles"); FileUtils.mkdir(profile_directory)
    sources.each { |name, bytes| File.binwrite(File.join(profile_directory, name), bytes) }
    normalized = File.join(output, "profiles.json")
    File.write(normalized, JSON.pretty_generate(schema: 1, profiles: profiles) + "\n")
    unless ENV["usage_no_swift_build"] == "true"
      env, command = SwiftNativeTasks.command("build", configuration: configuration, extra: ["--product", "jed-stress"])
      raise "Swift stress build failed" unless launch(command, directory: SwiftNativeTasks::PACKAGE, environment: env, log: File.join(output, "build-swift.log"), seconds: 1200)
    end
    actor_directory = File.join(output, "actors"); FileUtils.mkdir(actor_directory)
    swift = SwiftNativeBenchmark.executable(File.join(SwiftNativeTasks::BUILD, configuration, "jed-stress"), File.join(actor_directory, "jed-stress"))
    actors = { "swift" => swift }
    if cores.include?("go")
      path = File.join(actor_directory, "go-stress")
      raise "Go race stress build failed" unless launch(["go", "build", "-race", "-o", path, "./cmd/stress"],
        directory: File.join(ROOT, "bench/go"), environment: { "CGO_ENABLED" => "1", "GOPROXY" => "off" }, log: File.join(output, "build-go.log"))
      actors["go"] = { path: path, sha256: Digest::SHA256.file(path).hexdigest }
    end
    if cores.include?("rust")
      raise "Rust stress build failed" unless launch(["cargo", "build", "--release", "--locked", "--offline", "--manifest-path", "bench/rust/Cargo.toml", "--bin", "stress"],
        directory: ROOT, log: File.join(output, "build-rust.log"))
      actors["rust"] = SwiftNativeBenchmark.executable(File.join(ROOT, "bench/rust/target/release/stress"), File.join(actor_directory, "rust-stress"))
    end
    if cores.include?("ts")
      paths = %w[bench/ts/src bench/ts/package.json bench/ts/package-lock.json bench/ts/node_modules impl/ts/src impl/ts/package.json impl/ts/package-lock.json impl/ts/node_modules impl/ts/jed_lock.node].flat_map { SwiftNativeBenchmark.tree_files(File.join(ROOT, _1)) }.uniq.sort
      hashes = SwiftNativeBenchmark.hash_files(paths)
      snapshot = File.join(actor_directory, "ts")
      paths.each do |source|
        relative = source.delete_prefix(ROOT + "/"); destination = File.join(snapshot, relative)
        FileUtils.mkdir_p(File.dirname(destination)); FileUtils.copy_file(source, destination)
        raise "TS copy changed" unless Digest::SHA256.file(destination).hexdigest == hashes.fetch(relative)
      end
      raise "TS source changed during capture" unless SwiftNativeBenchmark.hash_files(paths) == hashes
      node, status = Open3.capture2("which", "node")
      raise "Node unavailable" unless status.success?
      node_actor = SwiftNativeBenchmark.executable(File.realpath(node.strip), File.join(actor_directory, "node"))
      actors["ts"] = { path: File.join(snapshot, "bench/ts/src/stress.ts"), node: node_actor, files: hashes,
        sha256: Digest::SHA256.hexdigest(JSON.generate(hashes)) }
    end
    File.write(File.join(output, "actors.json"), JSON.pretty_generate(actors) + "\n")
    failures = []; runs = []
    cores.each do |core|
      (core == "swift" ? %w[memory disk] : %w[memory]).each do |storage|
        SwiftNativeBenchmark.verify_actors(actors)
        result_path = File.join(output, "#{core}-#{storage}.jsonl")
        if core == "swift"
          command = [actors.fetch(core).fetch(:path), normalized, result_path, storage, File.join(output, "scratch-#{storage}")]
        elsif core == "ts"
          command = [actors.fetch(core).fetch(:node).fetch(:path), actors.fetch(core).fetch(:path), profile_directory, result_path]
        else
          command = [actors.fetch(core).fetch(:path), profile_directory, result_path]
        end
        passed = launch(command, directory: ROOT, log: File.join(output, "#{core}-#{storage}.log"), seconds: 120)
        failures << "#{core}/#{storage}: process failed" unless passed
        SwiftNativeBenchmark.verify_actors(actors)
        rows = File.file?(result_path) ? File.readlines(result_path, chomp: true).map { JSON.parse(_1) } : []
        failures.concat(validate(rows, profiles, core: core, storage: storage))
        runs << { core: core, storage: storage, rows: rows }
      end
    end
    profiles.each do |profile|
      next unless profile.fetch(:crossCoreChecksum)
      answers = runs.flat_map { _1.fetch(:rows) }.select { _1["name"] == profile.fetch(:name) }.map { _1["checksum"] }
      failures << "#{profile.fetch(:name)}: cross-core/storage checksum mismatch" unless answers.uniq.length == 1
    end
    report = { baseline: BASELINE, profile_sha256: PROFILE_HASHES, actors: actors, runs: runs,
      failures: failures, complete_matrix: cores.to_set == CORES.to_set }
    File.write(File.join(output, "report.json"), JSON.pretty_generate(report) + "\n")
    warn failures.join("\n") unless failures.empty?
    puts "stress: #{runs.length} core/storage runs, #{runs.sum { |run| run.fetch(:rows).sum { _1.fetch('invariant_checks', 0) } }} snapshot checks, #{failures.length} failures"
    failures.empty? ? 0 : 1
  rescue StandardError => error
    warn error.message; 1
  end
end
