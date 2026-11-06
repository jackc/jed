# frozen_string_literal: true

require_relative "swift_task"
require "digest"
require "json"
require "set"
require "tmpdir"

# An opt-in acceptance adapter. No engine or shared default task depends on it.
module SwiftQueryMemory
  BASELINE = "362e0798ac3a8bd04bd2da556044b6ec73bf468a"
  CORPUS = File.join(SwiftNativeTasks::PACKAGE, "Tests/JedTests/Fixtures/Corpus")
  LIVE = File.join(SwiftNativeTasks::ROOT, "spec/conformance/suites")
  MODES = %w[memory disk disk-spill].freeze
  CORES = %w[swift rust go ts].freeze
  DEFAULT_BUDGET = 1 << 40
  TEST_EXECUTABLE = "JedSwiftExperimentPackageTests.xctest"
  TEST_SELECTOR = "JedTests.SQLCorpusBoundaryTests/testWholeFrozenSQLFilesThroughDatabaseSession"
  RESOURCE_BUNDLES = %w[JedSwiftExperiment_JedTests.resources JedSwiftExperiment_JedCryptoTests.resources].freeze
  module_function

  def inventory(filter = nil)
    snapshot = JSON.parse(File.read(File.join(CORPUS, "../SpecSnapshot.json")))
    raise "unexpected frozen baseline" unless snapshot.fetch("baseline") == BASELINE
    hashes = snapshot.fetch("source_hashes").filter_map do |path, hash|
      [path.delete_prefix("spec/conformance/suites/"), hash] if path.start_with?("spec/conformance/suites/")
    end.to_h
    actual = Dir.glob("**/*.test", base: CORPUS).sort
    raise "frozen corpus inventory differs from snapshot" unless actual == hashes.keys.sort
    hashes.to_h do |name, hash|
      bytes = File.binread(File.join(CORPUS, name))
      raise "frozen corpus hash differs: #{name}" unless Digest::SHA256.hexdigest(bytes) == hash
      # The supported runners locate the live shared corpus. Never compare a
      # modified expectation or SQL record with the frozen Swift snapshot.
      raise "live corpus differs from baseline: #{name}" unless File.binread(File.join(LIVE, name)) == bytes
      [name, bytes.force_encoding(Encoding::UTF_8)]
    end.select { |name, _| !filter || name.include?(filter) }
  end

  def concurrency?(source) = source.match?(/^\s*#\s*format:\s*concurrency\s*$/)
  def fixture?(source) = source.match?(/^\s*#\s*fixture:/)
  def skip?(source, mode) = source.match?(/^\s*#\s*skip:\s*#{Regexp.escape(mode)}(?:\s|$)/)

  # Mechanical record grammar only. Expected cells and SQL text are skipped as
  # blocks so text resembling a header cannot invent a record ordinal.
  def record_count(source)
    lines = source.lines(chomp: true).map { _1.delete_suffix("\r") }
    at = 0; count = 0
    while at < lines.length
      header = lines[at].strip; at += 1
      next if header.empty? || header.start_with?("#")
      query = header.match?(/\Aquery [IBTDR]+(?:\s|$)/)
      unless query || header.match?(/\Astatement (?:ok|error)(?:\s|$)/)
        raise "unsupported corpus header #{header.inspect}"
      end
      count += 1
      at += 1 while at < lines.length && !lines[at].empty? && lines[at] != "----"
      if query
        raise "query record has no expected-result delimiter" unless lines[at] == "----"
        at += 1
        at += 1 while at < lines.length && !lines[at].empty?
      end
    end
    count
  end

  def expected(files, mode)
    storage = mode == "memory" ? "memory" : "disk"
    files.reject { |_, source| concurrency?(source) || skip?(source, storage) }
      .flat_map { |name, source| (1..record_count(source)).map { [name, _1] } }.to_set
  end

  def read_table(path, core:, expected:, frozen:, extras:, failures:)
    table = {}; seen = Set.new; outside = Set.new
    unless File.file?(path)
      failures << "#{core}: missing peak output #{path}"; return table
    end
    File.foreach(path).with_index(1) do |line, number|
      fields = line.chomp.split("\t", -1)
      unless fields.length == 4 && fields[1].match?(/\A[1-9][0-9]*\z/) &&
             fields.drop(2).all? { _1.match?(/\A[0-9]+\z/) }
        failures << "#{core}: malformed peak line #{number}"; next
      end
      file, ordinal, peak, storage = fields
      key = [file, Integer(ordinal)]
      if seen.include?(key)
        failures << "#{core}: duplicate #{file} record #{ordinal}"; next
      end
      seen << key
      if core != "swift" && extras.include?(file)
        outside << file; next
      end
      unless frozen.include?(file) && expected.include?(key)
        failures << "#{core}: unexpected #{file} record #{ordinal}"; next
      end
      table[key] = [Integer(peak), Integer(storage)]
    end
    (expected - table.keys.to_set).sort.each { |file, ordinal| failures << "#{core}: missing #{file} record #{ordinal}" }
    puts "#{core}: #{table.length}/#{expected.length} frozen record measurements; #{outside.length} explicitly excluded postbaseline files"
    outside.sort.each { puts "  postbaseline comparison exclusion: #{_1}" }
    table
  end

  def compare(mode, tables, files)
    tables.values.flat_map(&:keys).uniq.sort.filter_map do |key|
      # Shared query_memory_peaks.rb exception: Go opens #fixture as a file,
      # while Rust/TS/Swift deserialize it; compare peak only for these files.
      measured = tables.transform_values do |table|
        value = table[key]
        value && (fixture?(files.fetch(key[0])) ? value.take(1) : value)
      end.compact
      next if measured.values.uniq.length <= 1
      "#{mode} #{key[0]} record #{key[1]}: " + measured.map { |core, value| "#{core}=#{value.join('/')}" }.join("; ")
    end
  end

  def launch(arguments, environment, log, directory)
    puts "running #{File.basename(log)}"; $stdout.flush
    File.open(log, "w") do |output|
      system(environment, *arguments, chdir: directory, out: output, err: [:child, :out])
    end
  end

  def resource_hashes(directory)
    raise "missing XCTest resource directory: #{directory}" unless File.directory?(directory)
    Dir.glob("**/*", File::FNM_DOTMATCH, base: directory).sort.filter_map do |name|
      next if name.split("/").any? { _1 == "." || _1 == ".." }
      path = File.join(directory, name)
      raise "XCTest resource must not be a symlink: #{path}" if File.symlink?(path)
      next if File.directory?(path)
      raise "XCTest resource must be a regular file: #{path}" unless File.file?(path)
      [name, Digest::SHA256.file(path).hexdigest]
    end.to_h
  end

  # Capture once before any mode runs. Bundle.module prefers the sibling bundle
  # over its compiled build-path fallback, so copy both bundles beside the actor.
  # No SwiftPM command is used after this point, even with --no-build.
  def capture_actor(configuration, output, build: SwiftNativeTasks::BUILD, package: SwiftNativeTasks::PACKAGE)
    raise "configuration must be debug or release" unless %w[debug release].include?(configuration)
    source = File.join(build, configuration, TEST_EXECUTABLE)
    raise "standalone Swift XCTest executable is not built: #{source}" unless File.file?(source) && File.executable?(source)
    executable_hash = Digest::SHA256.file(source).hexdigest
    resources = RESOURCE_BUNDLES.to_h do |name|
      path = File.join(build, configuration, name)
      hashes = resource_hashes(path)
      target = name.delete_prefix("JedSwiftExperiment_").delete_suffix(".resources")
      fixtures = resource_hashes(File.join(package, "Tests", target, "Fixtures"))
        .transform_keys { "Fixtures/" + _1 }
      unless hashes.select { |key, _| key.start_with?("Fixtures/") } == fixtures
        raise "built XCTest fixtures differ from source: #{name}; rebuild tests before capture"
      end
      [name, hashes]
    end
    directory = File.join(output, "swift-actor")
    FileUtils.mkdir(directory)
    executable = File.join(directory, TEST_EXECUTABLE)
    FileUtils.copy_file(source, executable); File.chmod(0o500, executable)
    RESOURCE_BUNDLES.each do |name|
      FileUtils.cp_r(File.join(build, configuration, name), File.join(directory, name))
    end
    actor = { executable: executable, executable_sha256: executable_hash, resources: resources,
      selector: TEST_SELECTOR, configuration: configuration }
    verify_actor(actor)
    # A concurrent rebuild during capture is rejected instead of producing a
    # mixed executable/resource snapshot. Later rebuilds cannot touch this copy.
    unless Digest::SHA256.file(source).hexdigest == executable_hash && RESOURCE_BUNDLES.all? { |name|
      resource_hashes(File.join(build, configuration, name)) == resources.fetch(name)
    }
      raise "Swift XCTest build changed while capturing actor; retry after the build completes"
    end
    actor[:resource_sha256] = Digest::SHA256.hexdigest(JSON.generate(resources))
    File.write(File.join(output, "swift-actor.json"), JSON.pretty_generate(actor) + "\n")
    puts "Swift XCTest immutable actor: #{executable_hash}; resources: #{actor[:resource_sha256]}"
    actor
  end

  def verify_actor(actor)
    unless Digest::SHA256.file(actor.fetch(:executable)).hexdigest == actor.fetch(:executable_sha256)
      raise "captured Swift XCTest executable changed"
    end
    directory = File.dirname(actor.fetch(:executable))
    actor.fetch(:resources).each do |name, hashes|
      raise "captured Swift XCTest resources changed: #{name}" unless resource_hashes(File.join(directory, name)) == hashes
    end
  end

  def check_adapter
    files = { "ordinary.test" => "statement ok\nSELECT 1\n\nquery T\nSELECT 'query I'\n----\nquery I\n\n",
      "fixture.test" => "# fixture: example.jed\nstatement ok\nSELECT 1\n\n",
      "disk.test" => "# skip: memory file required\nstatement ok\nSELECT 1\n\n",
      "concurrent.test" => "# format: concurrency\n" }
    raise "record parser counted SQL/output headers" unless record_count(files.fetch("ordinary.test")) == 2
    wanted = expected(files, "memory")
    raise "mode eligibility mismatch" unless wanted == Set[["ordinary.test", 1], ["ordinary.test", 2], ["fixture.test", 1]]
    Dir.mktmpdir("jed-peak-adapter-check-") do |dir|
      path = File.join(dir, "measurements.tsv")
      body = "ordinary.test\t1\t32\t8192\nordinary.test\t2\t64\t8192\nfixture.test\t1\t0\t999\n"
      File.write(path, body)
      failures = []
      table = read_table(path, core: "swift", expected: wanted, frozen: files.keys.to_set, extras: Set.new, failures: failures)
      raise "valid table rejected" unless failures.empty? && table.length == 3
      other = table.merge(["fixture.test", 1] => [0, 1000])
      raise "fixture storage exception lost" unless compare("memory", { "swift" => table, "go" => other }, files).empty?
      other = table.merge(["ordinary.test", 2] => [65, 8192])
      raise "peak mismatch missed" unless compare("memory", { "swift" => table, "go" => other }, files).length == 1
      File.write(path, "ordinary.test\t1\t32\t8192\nordinary.test\t1\t32\t8192\nunknown.test\t1\t0\t1\nbad\n")
      failures = []
      read_table(path, core: "swift", expected: wanted, frozen: files.keys.to_set, extras: Set.new, failures: failures)
      %w[duplicate unexpected malformed missing].each do |kind|
        raise "#{kind} input did not fail closed" unless failures.any? { _1.include?(kind) }
      end
    end
    Dir.mktmpdir("jed-peak-actor-check-") do |dir|
      build = File.join(dir, "build"); package = File.join(dir, "package")
      FileUtils.mkdir_p(File.join(build, "debug"))
      source = File.join(build, "debug", TEST_EXECUTABLE)
      File.write(source, "actor before capture"); File.chmod(0o700, source)
      RESOURCE_BUNDLES.each do |name|
        target = name.delete_prefix("JedSwiftExperiment_").delete_suffix(".resources")
        [File.join(build, "debug", name, "Fixtures"), File.join(package, "Tests", target, "Fixtures")].each do |fixtures|
          FileUtils.mkdir_p(fixtures); File.binwrite(File.join(fixtures, "fixture"), "resource before capture")
        end
      end
      actor = capture_actor("debug", dir, build: build, package: package)
      File.write(source, "actor after capture")
      RESOURCE_BUNDLES.each { |name| File.write(File.join(build, "debug", name, "Fixtures/fixture"), "resource after capture") }
      verify_actor(actor)
      raise "actor was not captured" unless File.read(actor.fetch(:executable)) == "actor before capture"
      File.write(File.join(File.dirname(actor.fetch(:executable)), RESOURCE_BUNDLES.first, "Fixtures/fixture"), "modified capture")
      rejected = false
      begin
        verify_actor(actor)
      rescue RuntimeError => error
        rejected = error.message.include?("resources changed")
      end
      raise "changed captured resource was accepted" unless rejected
    end
    puts "query-memory adapter checks passed"
    0
  end

  def run
    return check_adapter if ENV["usage_check_adapter"] == "true"
    bytes = Integer(ENV.fetch("usage_bytes", DEFAULT_BUDGET.to_s), 10)
    raise "--bytes must be positive Int64 bytes" unless (1..(2**63 - 1)).cover?(bytes)
    modes = ENV.fetch("usage_mode", "all") == "all" ? MODES : [ENV.fetch("usage_mode")]
    raise "--mode must be all, memory, disk, or disk-spill" unless (modes - MODES).empty?
    cores = ENV.fetch("usage_cores", CORES.join(",")).split(",", -1)
    raise "--cores must contain swift and unique supported core names" unless cores.include?("swift") && cores.uniq == cores && (cores - CORES).empty?
    configuration = ENV.fetch("usage_configuration", "debug")
    files = inventory(ENV["usage_filter"])
    sql = files.reject { |_, source| concurrency?(source) }
    raise "filter matched no sequential frozen SQL files" if sql.empty?
    extras = (Dir.glob("**/*.test", base: LIVE) - Dir.glob("**/*.test", base: CORPUS)).to_set
    output = ENV["usage_output"] ? File.expand_path(ENV["usage_output"]) : Dir.mktmpdir("jed-swift-query-memory-")
    raise "output directory must be absent or empty" if File.exist?(output) && !Dir.empty?(output)
    FileUtils.mkdir_p(output)
    puts "query-memory evidence: #{output}"
    unless ENV["usage_no_build"] == "true"
      env, args = SwiftNativeTasks.command("build", configuration: configuration, extra: ["--build-tests"])
      raise "Swift test build failed; see #{output}/build.log" unless launch(args, env, File.join(output, "build.log"), SwiftNativeTasks::PACKAGE)
    end
    actor = capture_actor(configuration, output)
    failures = []; runs = []
    modes.each do |mode|
      wanted = expected(files, mode)
      raise "mode #{mode} excludes every selected SQL record" if wanted.empty?
      tables = {}
      cores.each do |core|
        peak = File.join(output, "#{mode}-#{core}.tsv")
        File.write(peak, "")
        env = {
          "JED_CONFORMANCE_QUERY_MEMORY" => bytes.to_s,
          "JED_CONFORMANCE_QUERY_MEMORY_PEAKS" => peak,
          "JED_CONFORMANCE_WORK_MEM" => mode == "disk-spill" ? "256" : nil,
          "JED_CONFORMANCE_FILTER" => ENV["usage_filter"],
          "JED_CONFORMANCE_ENCRYPTED" => nil, "JED_CONFORMANCE_TEMP_BUFFERS" => nil,
          "JED_CONFORMANCE_QUERY_MEMORY_PROBE" => nil, "JED_CONFORMANCE_TRANSCRIPT" => nil,
          "JED_SWIFT_SERVICE_OWNER_COMMAND" => nil, "JED_SWIFT_SERVICE_ROOT" => nil
        }
        disk = mode == "memory" ? [] : ["disk"]
        case core
        when "swift"
          verify_actor(actor)
          args = [actor.fetch(:executable), actor.fetch(:selector)]
          env.merge!({"JED_SWIFT_SQL_FILES" => sql.keys.sort.join(","), "JED_SWIFT_SQL_FILE" => nil,
            "JED_SWIFT_ALL_SQL" => nil, "JED_SWIFT_SQL_MODE" => mode == "memory" ? "memory" : "disk",
            "JED_SWIFT_TRACE_SQL" => "1", "JED_SWIFT_SQL_TRACE_PATH" => File.join(output, "#{mode}-swift-progress.log")})
          File.write(env.fetch("JED_SWIFT_SQL_TRACE_PATH"), "")
          directory = SwiftNativeTasks::PACKAGE
        when "rust"
          args = ["cargo", "run", "--release", "--quiet", "--bin", "conformance", "--manifest-path",
                  File.join(SwiftNativeTasks::ROOT, "impl/rust/Cargo.toml"), "--", *disk]
          directory = SwiftNativeTasks::ROOT
        when "go"
          args = ["go", "run", "./cmd/conformance", *disk]; directory = File.join(SwiftNativeTasks::ROOT, "impl/go")
        when "ts"
          args = ["node", "src/bin/conformance.ts", *disk]; directory = File.join(SwiftNativeTasks::ROOT, "impl/ts")
        end
        log = File.join(output, "#{mode}-#{core}.log")
        passed = launch(args, env, log, directory)
        failures << "#{mode} #{core}: conformance process failed (#{log})" unless passed
        verify_actor(actor) if core == "swift"
        local = []
        tables[core] = read_table(peak, core: core, expected: wanted, frozen: files.keys.to_set, extras: extras, failures: local)
        failures.concat(local.map { "#{mode} #{_1}" })
        runs << { mode: mode, core: core, passed: !!passed, records: tables[core].length, expected: wanted.length }
      end
      failures.concat(compare(mode, tables, files))
    end
    report = { baseline: BASELINE, budget: bytes, modes: modes, cores: cores, runs: runs, failures: failures,
      swift_actor: actor,
      complete_matrix: modes == MODES && cores.to_set == CORES.to_set && !ENV["usage_filter"] }
    File.write(File.join(output, "report.json"), JSON.pretty_generate(report) + "\n")
    warn failures.first(80).join("\n") unless failures.empty?
    puts "query-memory: #{runs.sum { _1[:records] }} measurements, #{failures.length} failures; #{report[:complete_matrix] ? 'full' : 'diagnostic subset'} matrix"
    failures.empty? ? 0 : 1
  rescue ArgumentError, RuntimeError, KeyError, SystemCallError => error
    warn error.message; 1
  end
end
