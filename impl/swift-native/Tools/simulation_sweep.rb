# frozen_string_literal: true

require_relative "swift_task"
require_relative "generate_spec"
require "digest"
require "json"
require "tmpdir"

# Host tooling only. Trace generation and its per-core comparison exemptions
# remain the frozen shared driver's; no SQL or storage implementation lives here.
module SwiftSimulationSweep
  SOURCES = {
    "scripts/sim_gen.rb" => "bd497ee9703f0340f350008fc8de6b04847b20c8ae9f784bb78b392400293b12",
    "scripts/sim.rb" => "3fce68d66dbd9a6bffe0e79b588d5c699025672fabd768974c855c295546eaa2",
    "spec/fileformat/verify.rb" => "64308c4c1044d6d30eafb4ba34fb3ea49a45970545ecbf3c7018b7e439bc3dec",
    "spec/fileformat/lz4.rb" => "c4eb2a3fd9fe6d455d83351deb6512f8277661f1b6c3e287125e65417663e609",
    "spec/fileformat/xaes.rb" => "aeebcb1541f0c3312006852ec0928ce2aef0fa50fce328bc0b8bbde24b7d1882",
    "spec/fileformat/expr.rb" => "7b09424c5beb719a4527d8fed113010cd219bfc55752b1ccf9120bdff6111e76"
  }.freeze
  PROFILE_ROOT = File.join(SwiftNativeTasks::PACKAGE, "Tests/JedTests/Fixtures/Simulation/profiles")
  REGRESS_ROOT = File.join(SwiftNativeTasks::PACKAGE, "Tests/JedTests/Fixtures/Simulation/regress")
  module_function

  def verified_shared_tools
    SOURCES.each do |path, expected|
      blob, error, status = Open3.capture3("git", "show", "#{SwiftSpecGenerator::BASELINE}:#{path}", chdir: SwiftNativeTasks::ROOT)
      abort "missing frozen simulation source #{path}: #{error}" unless status.success?
      abort "frozen source hash mismatch: #{path}" unless Digest::SHA256.hexdigest(blob) == expected
      live = File.join(SwiftNativeTasks::ROOT, path)
      abort "shared #{path} differs from frozen baseline; snapshot it explicitly before updating this task" unless File.binread(live) == blob.b
    end
    require File.join(SwiftNativeTasks::ROOT, "scripts/sim")
    # This changes only the shared generator's input location, not its algorithm
    # or defaults. Every profile blob is checked by SwiftSpecGenerator first.
    SimGen.send(:remove_const, :PROFILE_DIR)
    SimGen.const_set(:PROFILE_DIR, PROFILE_ROOT)
    abort "frozen simulation profile inventory is not 22" unless SimGen.profile_names.length == 22
  end

  def parse_seeds(text)
    abort "seeds must be N or LO..HI" unless /\A\d+(?:\.\.\d+)?\z/.match?(text)
    low, high = text.split("..").map { Integer(_1) }; high ||= low
    abort "invalid seed interval" unless low <= high && high <= 0xffff_ffff_ffff_ffff
    (low..high).to_a
  end

  def result_records(path, expected_names, failures, core)
    records = {}
    unless File.file?(path)
      failures << "#{core}: no results file"; return records
    end
    File.foreach(path) do |line|
      next if line.strip.empty?
      begin
        row = JSON.parse(line); name = row.fetch("trace")
        if records.key?(name) || !expected_names.include?(name) || row["core"] != core
          failures << "#{core}: duplicate, unselected, or mislabelled result #{name.inspect}"
        else
          records[name] = row
        end
      rescue JSON::ParserError, KeyError => error
        failures << "#{core}: malformed result: #{error.message}"
      end
    end
    records
  end

  def expected_images(transcript, name, core, output)
    expected = {}
    transcript.each_line do |line|
      row = JSON.parse(line)
      if row["op"] == "backup" && row["version"]
        expected[File.join(output, "#{name}.#{core}.step#{row['i']}.backup.img")] = row.merge("recovered" => row["version"])
      elsif row["op"] == "crash"
        expected[File.join(output, "#{name}.#{core}.step#{row['i']}.img")] = row
        (row["dbs"] || {}).each do |domain, facts|
          expected[File.join(output, "#{name}.#{core}.step#{row['i']}.#{domain}.img")] = facts
        end
      end
    end
    expected
  end

  def check_images(expected, failures)
    expected.keys.each_slice(100) do |paths|
      stdout, stderr, status = Open3.capture3("ruby", File.join(SwiftNativeTasks::ROOT, "spec/fileformat/verify.rb"), "--check-image", *paths)
      failures << "independent image checker exited #{status.exitstatus}: #{stderr}" unless status.success?
      seen = {}
      stdout.each_line do |line|
        begin
          actual = JSON.parse(line); path = actual.fetch("file"); wanted = expected[path]
          if wanted.nil? || seen[path]
            failures << "independent checker returned an unrequested/duplicate image #{path}"; next
          end
          seen[path] = true
          got = actual.values_at("version", "live", "pages", "free")
          want = [wanted["recovered"], wanted["live"], wanted["pages"], wanted["free"]]
          failures << "#{File.basename(path)}: checker #{actual['error'] || "#{got.inspect} != #{want.inspect}"}" if actual["error"] || got != want
        rescue JSON::ParserError, KeyError => error
          failures << "independent checker malformed output: #{error.message}"
        end
      end
      (paths - seen.keys).each { failures << "independent checker omitted #{File.basename(_1)}" }
    end
  end

  def run
    SwiftSpecGenerator.run(["--check"])
    verified_shared_tools
    configuration = ENV.fetch("usage_configuration", "debug")
    abort "configuration must be debug or release" unless %w[debug release].include?(configuration)
    supported = ENV.fetch("usage_cores", "go,rust,ts").split(",")
    abort "cores must be a nonempty unique subset of go,rust,ts" if supported.empty? || supported.uniq != supported || (supported - Sim::ALL_CORES).any?
    profiles = ENV.fetch("usage_profile", "all")
    profiles = profiles == "all" ? SimGen.profile_names : [profiles]
    abort "unknown frozen profile" unless (profiles - SimGen.profile_names).empty?
    seeds = parse_seeds(ENV.fetch("usage_seeds", "1..8"))
    steps = ENV["usage_steps"] && Integer(ENV["usage_steps"])
    abort "steps must be positive" if steps && steps <= 0
    output = File.expand_path(ENV["usage_output"] || Dir.mktmpdir("jed-swift-sim-sweep-"))
    FileUtils.mkdir_p(output)
    abort "output directory must be empty" unless Dir.empty?(output)
    paths = if ENV["usage_regress"] == "true"
      Dir[File.join(REGRESS_ROOT, "*.sim")].sort.tap { abort "frozen regression inventory is not 19" unless _1.length == 19 }
    else
      directory = File.join(output, "traces"); FileUtils.mkdir_p(directory)
      profiles.product(seeds).map do |profile, seed|
        path = File.join(directory, "#{profile}-#{seed}.sim")
        File.write(path, SimGen.generate(profile, seed, steps: steps)); path
      end
    end
    supported_sources = supported.to_h do |core|
      directory = File.join(SwiftNativeTasks::ROOT, "impl", core == "rust" ? "rust" : core)
      glob = core == "go" ? "*.go" : "src/**/*.{rs,ts}"
      digest = Digest::SHA256.new
      Dir[File.join(directory, glob)].sort.each do |file|
        relative = file.delete_prefix(directory + "/"); bytes = File.binread(file)
        digest.update(relative + "\0" + bytes.bytesize.to_s + "\0"); digest.update(bytes)
      end
      [core, digest.hexdigest]
    end
    snapshot_facts = JSON.parse(File.read(File.join(SwiftNativeTasks::PACKAGE, "Tests/JedTests/Fixtures/SpecSnapshot.json")))
    manifest = {
      input_fingerprint: snapshot_facts.fetch("fingerprint"), baseline: SwiftSpecGenerator::BASELINE, generator_sha256: SOURCES.fetch("scripts/sim_gen.rb"),
      sources: SOURCES, supported_source_sha256: supported_sources, mode: ENV["usage_regress"] == "true" ? "regress" : "sweep",
      profiles: ENV["usage_regress"] == "true" ? [] : profiles, seeds: ENV["usage_regress"] == "true" ? [] : seeds, steps_override: steps, cores: ["swift-native"] + supported,
      traces: paths.to_h { [File.basename(_1, ".sim"), Digest::SHA256.file(_1).hexdigest] }
    }
    File.write(File.join(output, "manifest.json"), JSON.pretty_generate(manifest) + "\n")
    warn "Swift simulation #{manifest[:mode]}: #{paths.length} traces; profiles #{manifest[:profiles].join(',')}; seeds #{manifest[:seeds].first}..#{manifest[:seeds].last}; output #{output}"
    unless ENV["usage_no_build"] == "true"
      env, command = SwiftNativeTasks.command("build", configuration: configuration, extra: ["--product", "jed-sim"])
      abort "Swift simulator build failed" unless system(env, *command, chdir: SwiftNativeTasks::PACKAGE)
      Sim.build(supported)
    end
    actor = File.join(SwiftNativeTasks::BUILD, configuration, "jed-sim")
    abort "Swift simulator is not built" unless File.executable?(actor)
    failures = []; results = {}; commands = {}; names = manifest[:traces].keys
    list = File.join(output, "traces.list"); File.write(list, paths.join("\n") + "\n")
    Dir.mktmpdir("jed-swift-sim-sweep-actor-") do |directory|
      snapshot = File.join(directory, "jed-sim")
      FileUtils.copy_file(actor, snapshot); File.chmod(0o700, snapshot)
      manifest[:swift_actor_sha256] = Digest::SHA256.file(snapshot).hexdigest
      warn "Swift simulation immutable actor: #{manifest[:swift_actor_sha256]}"
      (["swift-native"] + supported).each do |core|
        result_path = File.join(output, "#{core}.results.jsonl")
        environment = {"JED_SIM_IN" => list, "JED_SIM_OUT" => output, "JED_SIM_RESULTS" => result_path, "JED_SIM_IMAGES" => "1"}
        if core == "swift-native"
          command = [snapshot]; working = SwiftNativeTasks::PACKAGE
          environment["JED_SIM_TRACEFILE"] = File.join(output, "swift-native.progress.log")
        else
          command, working = Sim::RUNNERS.fetch(core).fetch(:run)
          if %w[go rust].include?(core)
            source = File.expand_path(command.first, working)
            executable = File.join(directory, "#{core}-sim")
            FileUtils.copy_file(source, executable); File.chmod(0o700, executable)
            command = [executable] + command.drop(1)
          end
        end
        warn "Simulation runner starting: #{core} (#{paths.length} selected traces)"
        commands[core] = {command: command, working_directory: working}
        executable = File.expand_path(command.first, working)
        commands[core][:executable_sha256] = Digest::SHA256.file(executable).hexdigest if File.file?(executable)
        stdout, stderr, status = Open3.capture3(environment, *command, chdir: working)
        File.write(File.join(output, "#{core}.stdout.log"), stdout); File.write(File.join(output, "#{core}.stderr.log"), stderr)
        failures << "#{core}: runner exited #{status.exitstatus || "signal #{status.termsig}"}" unless status.success?
        results[core] = result_records(result_path, names, failures, core)
        warn "Simulation runner finished: #{core}, #{results[core].length}/#{paths.length} results"
      end
    end
    comparisons = 0; exemptions = []; image_facts = {}; image_count = 0
    paths.each do |path|
      name = File.basename(path, ".sim"); transcripts = {}; images = {}
      per_core = Sim.per_core?(path); exemptions << name if per_core
      manifest[:cores].each do |core|
        row = results.fetch(core)[name]
        success = core == "swift-native" ? %w[ok pass] : ["pass"]
        failures << "#{name} [#{core}]: #{row ? row['violation'] : 'no result'}" unless row && success.include?(row["status"])
        text = Sim.transcript(output, name, core)
        unless text && !text.empty?
          failures << "#{name} [#{core}]: missing or empty transcript"; next
        end
        transcripts[core] = text
        images[core] = expected_images(text, name, core, output)
        images[core].each do |image, facts|
          unless File.file?(image)
            failures << "#{name} [#{core}]: missing required image #{File.basename(image)}"; next
          end
          image_count += 1
          image_facts[image] = facts if core == "swift-native"
        end
      end
      next if per_core
      reference = transcripts["swift-native"]
      supported.each do |core|
        next unless reference && transcripts[core]
        comparisons += 1
        if reference != transcripts[core]
          a = reference.lines; b = transcripts[core].lines
          index = (0...[a.length, b.length].max).find { a[_1] != b[_1] }
          failures << "#{name}: Swift/#{core} transcript differs at line #{index + 1}: #{a[index]&.strip.inspect} / #{b[index]&.strip.inspect}"
        end
      end
      # Compare exact logical image bytes, excluding only host preallocated tail.
      page_size = File.read(path)[/^# page_size:\s*(\d+)/, 1].to_i
      unless page_size.positive?
        failures << "#{name}: missing or invalid page_size header"; next
      end
      (images["swift-native"] || {}).each do |image, facts|
        next unless File.file?(image)
        count = facts.fetch("pages") * page_size
        swift_bytes = File.binread(image, count)
        failures << "#{File.basename(image)}: image shorter than selected high-water" unless swift_bytes.bytesize == count
        supported.each do |core|
          other = image.sub(".swift-native.", ".#{core}.")
          if !File.file?(other) || File.binread(other, count) != swift_bytes
            failures << "#{name}: Swift/#{core} logical image differs: #{File.basename(image)}"
          end
        end
      end
    end
    check_images(image_facts, failures)
    report = {selected: paths.length, cores: manifest[:cores], counts: results.transform_values { |rows| rows.values.group_by { _1['status'] }.transform_values(&:length) },
      transcript_comparisons: comparisons, per_core_fault_comparison_exemptions: exemptions,
      emitted_images: image_count, independently_checked_swift_images: image_facts.length, failures: failures}
    manifest[:commands] = commands
    File.write(File.join(output, "manifest.json"), JSON.pretty_generate(manifest) + "\n")
    File.write(File.join(output, "summary.json"), JSON.pretty_generate(report) + "\n")
    failures.each { warn _1 }
    warn "Simulation acceptance: #{paths.length} selected; #{comparisons} transcript comparisons; #{image_facts.length} independent images; #{failures.length} failures"
    failures.empty? ? 0 : 1
  end
end
