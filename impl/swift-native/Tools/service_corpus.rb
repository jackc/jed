# frozen_string_literal: true

require_relative "query_memory"
require "timeout"

# SQL still runs in the selected client's ordinary corpus runner. The external
# owner transports pages only; each runner asserts its normal rows/errors/costs.
module SwiftServiceCorpus
  CORES = %w[swift rust go ts].freeze
  PAIRS = %w[swift:rust swift:go swift:ts rust:swift go:swift ts:swift].freeze
  FIXTURES = %w[collation/collation_upgrade.test collation/skew.test collation/skew_full.test storage/unparsable_definitions.test].freeze
  ROOT = SwiftNativeTasks::ROOT
  module_function

  def pairs(value)
    selected = value.split(",", -1)
    raise "--pairs requires unique client:owner tokens from #{PAIRS.join(',')}" if selected.empty? || selected.uniq != selected || !(selected - PAIRS).empty?
    selected
  end

  def included(files)
    files.reject { |_, source| SwiftQueryMemory.concurrency?(source) || SwiftQueryMemory.skip?(source, "disk") }
  end

  def role_counts(files)
    files.each_with_object({ client: 0, fixture: 0 }) do |(_, source), counts|
      counts[SwiftQueryMemory.fixture?(source) ? :fixture : :client] += SwiftQueryMemory.record_count(source)
    end
  end

  def completed(path, client:, wanted:, extras:, failures:)
    prefix = client == "swift" ? "SQL file end: " : "PASS "
    actual = File.readlines(path).filter_map do |line|
      next unless line.start_with?(prefix)
      value = line.delete_prefix(prefix).strip
      client == "swift" ? value : value.split(/\s+/, 2).first
    end
    actual.reject! { client != "swift" && extras.include?(_1) }
    failures << "duplicate completed file" unless actual.uniq == actual
    (wanted - actual).each { failures << "file not completed: #{_1}" }
    (actual - wanted).each { failures << "unexpected completed file: #{_1}" }
    actual
  end

  def check_roles(log, client:, counts:, extra_counts:, failures:)
    source = File.read(log)
    if client == "swift"
      found = source.scan(/^Service corpus: (\d+) records run on a page client; (\d+) canonical fixture-local records$/)
      expected = [counts[:client], counts[:fixture]]
    else
      found = source.scan(/^\[service, owner swift-native\] (\d+) records run on a page client$/)
      expected = [counts[:client] + extra_counts[:client]]
    end
    actual = found.length == 1 ? found[0].map { Integer(_1) } : nil
    failures << "service role accounting differs: expected #{expected.inspect}, got #{found.inspect}" unless actual == expected
    actual
  end

  def launch(command, environment, log, directory, seconds: 3600)
    File.write(log + ".command.json", JSON.pretty_generate(command: command, directory: directory, environment: environment, timeout_seconds: seconds) + "\n")
    puts "running #{File.basename(log)}"; $stdout.flush
    File.open(log, "w") do |output|
      pid = Process.spawn(environment, *command, chdir: directory, out: output, err: [:child, :out], pgroup: true)
      begin
        _, status = Timeout.timeout(seconds) { Process.waitpid2(pid) }
        status.success?
      rescue Timeout::Error
        # The child is still ours to reap. Kill its process group as service
        # runners may be blocked waiting for their external owner response.
        Process.kill("KILL", -pid) rescue Errno::ESRCH
        Process.waitpid(pid) rescue Errno::ECHILD
        output.puts("service corpus process exceeded #{seconds}s"); false
      end
    end
  end

  def copy_executable(source, directory, name)
    raise "missing executable #{source}; run without --no-build" unless File.file?(source) && File.executable?(source)
    before = Digest::SHA256.file(source).hexdigest
    target = File.join(directory, name)
    FileUtils.copy_file(source, target); File.chmod(0o500, target)
    raise "executable changed during capture: #{source}" unless Digest::SHA256.file(source).hexdigest == before && Digest::SHA256.file(target).hexdigest == before
    { path: target, sha256: before }
  end

  def source_hashes
    patterns = %w[impl/go/**/*.go impl/go/go.mod impl/go/go.sum impl/rust/src/**/*.rs impl/rust/Cargo.toml impl/rust/Cargo.lock impl/ts/src/**/* impl/ts/package.json impl/ts/package-lock.json impl/ts/jed_lock.node impl/ts/node_modules/@noble/hashes/**/*]
    patterns.flat_map { Dir.glob(File.join(ROOT, _1)) }.uniq.sort.filter_map do |path|
      [path.delete_prefix(ROOT + "/"), Digest::SHA256.file(path).hexdigest] if File.file?(path)
    end.to_h
  end

  def actors(output, configuration, selected, no_build, no_swift_build)
    needed = selected.flat_map { _1.split(":") }.uniq
    directory = File.join(output, "actors"); FileUtils.mkdir(directory)
    builds = []
    unless no_build
      unless no_swift_build
        environment, command = SwiftNativeTasks.command("build", configuration: configuration, extra: ["--build-tests"])
        builds << [command, environment, SwiftNativeTasks::PACKAGE, "swift-tests"]
        environment, command = SwiftNativeTasks.command("build", configuration: configuration, extra: ["--product", "jed-process"])
        builds << [command, environment, SwiftNativeTasks::PACKAGE, "swift-owner"]
      end
      if needed.include?("go")
        %w[conformance process_actor].each do |product|
          builds << [["go", "build", "-o", File.join(SwiftNativeTasks::BUILD, "service-go-#{product}"), "./cmd/#{product}"], {}, File.join(ROOT, "impl/go"), "go-#{product}"]
        end
      end
      if needed.include?("rust")
        builds << [["cargo", "build", "--release", "--quiet", "--manifest-path", File.join(ROOT, "impl/rust/Cargo.toml"), "--bin", "conformance", "--bin", "process_actor"], {}, ROOT, "rust"]
      end
    end
    builds.each do |command, environment, cwd, label|
      raise "#{label} build failed; see evidence log" unless launch(command, environment, File.join(output, "build-#{label}.log"), cwd)
    end
    snapshot = SwiftQueryMemory.capture_actor(configuration, output)
    captures = { "swift-owner" => copy_executable(File.join(SwiftNativeTasks::BUILD, configuration, "jed-process"), directory, "jed-process") }
    owners = { "swift" => [captures.fetch("swift-owner").fetch(:path)] }
    clients = { "swift" => [snapshot.fetch(:executable), snapshot.fetch(:selector)] }
    if needed.include?("go")
      { "client" => "conformance", "owner" => "process_actor" }.each do |role, product|
        captures["go-#{role}"] = copy_executable(File.join(SwiftNativeTasks::BUILD, "service-go-#{product}"), directory, "go-#{product}")
      end
      clients["go"] = [captures.fetch("go-client").fetch(:path), "service"]
      owners["go"] = [captures.fetch("go-owner").fetch(:path)]
    end
    if needed.include?("rust")
      { "client" => "conformance", "owner" => "process_actor" }.each do |role, product|
        captures["rust-#{role}"] = copy_executable(File.join(ROOT, "impl/rust/target/release", product), directory, "rust-#{product}")
      end
      clients["rust"] = [captures.fetch("rust-client").fetch(:path), "service"]
      owners["rust"] = [captures.fetch("rust-owner").fetch(:path)]
    end
    if needed.include?("ts")
      raise "existing native TS lock addon required; run mise run ts:lock_build" unless File.file?(File.join(ROOT, "impl/ts/jed_lock.node"))
      node, error, status = Open3.capture3("node", "-p", "process.execPath")
      raise "cannot locate node: #{error}" unless status.success? && node.strip.start_with?("/")
      captures["node-runtime"] = { path: node.strip, sha256: Digest::SHA256.file(node.strip).hexdigest }
      clients["ts"] = [node.strip, File.join(ROOT, "impl/ts/src/bin/conformance.ts"), "service"]
      owners["ts"] = [node.strip, File.join(ROOT, "impl/ts/src/bin/process_actor.ts")]
    end
    File.write(File.join(output, "actors.json"), JSON.pretty_generate(captures) + "\n")
    [snapshot, captures, clients, owners]
  end

  def verify_captures(snapshot, captures, sources)
    SwiftQueryMemory.verify_actor(snapshot)
    captures.each { |name, facts| raise "actor changed: #{name}" unless Digest::SHA256.file(facts.fetch(:path)).hexdigest == facts.fetch(:sha256) }
    raise "supported source/runtime inputs changed during service comparison" unless source_hashes == sources
  end

  def check_adapter
    raise "default matrix changed" unless pairs(PAIRS.join(",")) == PAIRS
    ["", "go:rust", "swift:go,", "swift:go,swift:go", "swift:unknown"].each do |bad|
      begin
        pairs(bad)
      rescue RuntimeError
        next
      end
      raise "invalid pairing accepted: #{bad.inspect}"
    end
    files = { "ordinary.test" => "statement ok\nSELECT 1\n\n", "fixture.test" => "# fixture: example.jed\nstatement ok\nSELECT 1\n\n", "skip.test" => "# skip: disk reason\nstatement ok\nSELECT 1\n\n" }
    counts = role_counts(included(files))
    raise "fixture exception misclassified" unless counts == { client: 1, fixture: 1 }
    Dir.mktmpdir("jed-service-adapter-") do |directory|
      log = File.join(directory, "client.log")
      File.write(log, "Service corpus: 1 records run on a page client; 1 canonical fixture-local records\nSQL file end: ordinary.test\nSQL file end: fixture.test\n")
      failures = []
      check_roles(log, client: "swift", counts: counts, extra_counts: {}, failures: failures)
      completed(log, client: "swift", wanted: included(files).keys, extras: Set.new, failures: failures)
      raise "valid service evidence rejected" unless failures.empty?
      File.write(log, "Service corpus: 0 records run on a page client; 2 canonical fixture-local records\nSQL file end: ordinary.test\nSQL file end: ordinary.test\n")
      failures = []
      check_roles(log, client: "swift", counts: counts, extra_counts: {}, failures: failures)
      completed(log, client: "swift", wanted: included(files).keys, extras: Set.new, failures: failures)
      %w[role duplicate completed].each { |kind| raise "#{kind} evidence defect missed" unless failures.any? { _1.include?(kind) } }
    end
    puts "service corpus adapter checks passed"; 0
  end

  def run
    return check_adapter if ENV["usage_check_adapter"] == "true"
    selected = pairs(ENV.fetch("usage_pairs", PAIRS.join(",")))
    configuration = ENV.fetch("usage_configuration", "debug")
    raise "configuration must be debug or release" unless %w[debug release].include?(configuration)
    all = SwiftQueryMemory.inventory
    actual_fixtures = included(all).select { |_, source| SwiftQueryMemory.fixture?(source) }.keys.sort
    raise "canonical service fixture exception inventory changed" unless actual_fixtures == FIXTURES.sort
    filter = ENV["usage_filter"]
    raise "--filter must be nonempty when present" if filter == ""
    files = all.select { |name, _| filter.nil? || name.include?(filter) }
    eligible = included(files)
    raise "selection contains no service disk SQL records" if eligible.empty? || role_counts(eligible).values.sum.zero?
    counts = role_counts(eligible)
    wanted = SwiftQueryMemory.expected(files, "disk")
    extras = (Dir.glob("**/*.test", base: SwiftQueryMemory::LIVE) - all.keys).to_set
    extra_sources = extras.filter_map do |name|
      [name, File.read(File.join(SwiftQueryMemory::LIVE, name))] if filter.nil? || name.include?(filter)
    end.to_h
    extra_counts = role_counts(included(extra_sources))
    output = ENV["usage_output"] ? File.expand_path(ENV["usage_output"]) : Dir.mktmpdir("jed-swift-service-corpus-")
    raise "output directory must be absent or empty" if File.exist?(output) && !Dir.empty?(output)
    FileUtils.mkdir_p(output)
    puts "service corpus evidence: #{output}"; $stdout.flush
    sources = source_hashes
    snapshot, captures, clients, owners = actors(output, configuration, selected, ENV["usage_no_build"] == "true", ENV["usage_no_swift_build"] == "true")
    raise "supported source/runtime inputs changed while building actors" unless source_hashes == sources
    File.write(File.join(output, "source-hashes.json"), JSON.pretty_generate(sources) + "\n")
    failures = []; runs = []
    selected.each do |pair|
      verify_captures(snapshot, captures, sources)
      client, owner = pair.split(":")
      stem = pair.tr(":", "-"); log = File.join(output, stem + ".log")
      ledger = File.join(output, stem + "-records.tsv"); File.write(ledger, "")
      environment = { "JED_CONFORMANCE_FILTER" => filter, "JED_CONFORMANCE_QUERY_MEMORY_PEAKS" => ledger,
        "JED_CONFORMANCE_QUERY_MEMORY" => nil, "JED_CONFORMANCE_QUERY_MEMORY_PROBE" => nil,
        "JED_CONFORMANCE_WORK_MEM" => nil, "JED_CONFORMANCE_TEMP_BUFFERS" => nil,
        "JED_CONFORMANCE_ENCRYPTED" => nil, "JED_CONFORMANCE_TRANSCRIPT" => nil,
        "JED_SERVICE_OWNER" => nil, "JED_SWIFT_SERVICE_OWNER_COMMAND" => nil, "JED_SWIFT_SERVICE_ROOT" => nil,
        "JED_SWIFT_PROCESS_ACTOR" => captures.fetch("swift-owner").fetch(:path) }
      completion = log
      if client == "swift"
        completion = File.join(output, stem + "-progress.log"); File.write(completion, "")
        environment.merge!({ "JED_SWIFT_SQL_FILES" => files.keys.sort.join(","), "JED_SWIFT_SQL_FILE" => nil,
          "JED_SWIFT_ALL_SQL" => nil, "JED_SWIFT_SQL_MODE" => "disk", "JED_SWIFT_TRACE_SQL" => "1",
          "JED_SWIFT_SQL_TRACE_PATH" => completion, "JED_SWIFT_SERVICE_OWNER_COMMAND" => JSON.generate(owners.fetch(owner)),
          "JED_SWIFT_SERVICE_ROOT" => ROOT })
        directory = SwiftNativeTasks::PACKAGE
      else
        environment["JED_SERVICE_OWNER"] = "swift-native"
        directory = File.join(ROOT, "impl", client == "ts" ? "ts" : client)
      end
      passed = launch(clients.fetch(client), environment, log, directory)
      verify_captures(snapshot, captures, sources)
      local = []
      local << "conformance process failed: #{log}" unless passed
      measured = SwiftQueryMemory.read_table(ledger, core: client, expected: wanted, frozen: files.keys.to_set, extras: extras, failures: local)
      done = completed(completion, client: client, wanted: eligible.keys, extras: extras, failures: local)
      roles = check_roles(log, client: client, counts: counts, extra_counts: extra_counts, failures: local)
      failures.concat(local.map { "#{pair}: #{_1}" })
      runs << { pair: pair, passed: !!passed, completed_files: done.length, records: measured.length, roles: roles, failures: local }
    end
    report = { baseline: SwiftQueryMemory::BASELINE, swift_actor: snapshot, actors: captures, pairs: selected,
      files: eligible.keys.sort, records: wanted.length, expected_roles: counts,
      fixture_local_files: eligible.select { |_, source| SwiftQueryMemory.fixture?(source) }.keys.sort,
      exclusions: (files.keys - eligible.keys).sort, postbaseline_exclusions: extra_sources.keys.sort,
      postbaseline_roles: extra_counts, runs: runs, failures: failures,
      complete_matrix: selected.to_set == PAIRS.to_set && filter.nil? }
    File.write(File.join(output, "report.json"), JSON.pretty_generate(report) + "\n")
    warn failures.first(80).join("\n") unless failures.empty?
    puts "service corpus: #{runs.sum { _1[:records] }} records, #{failures.length} failures; #{report[:complete_matrix] ? 'full' : 'diagnostic subset'} matrix"
    failures.empty? ? 0 : 1
  rescue ArgumentError, RuntimeError, KeyError, SystemCallError => error
    warn error.message; 1
  end
end
