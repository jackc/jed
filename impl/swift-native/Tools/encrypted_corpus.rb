# frozen_string_literal: true

require_relative "query_memory"

# Explicit encrypted-file acceptance. The unchanged shared runners retain their
# own rows/errors assertions; every encrypted cost/EXPLAIN output is compared.
module SwiftEncryptedCorpus
  module_function

  def flat(value) = value.gsub("\\") { "\\\\" }.gsub("\n", "\\n").gsub("\t", "\\t")

  def records(source)
    lines = source.lines(chomp: true).map { _1.delete_suffix("\r") }
    at = 0; output = []; cost = false
    while at < lines.length
      header = lines[at].strip; at += 1
      next if header.empty?
      if header.start_with?("#")
        cost = true if header.match?(/\A#\s*cost:/)
        next
      end
      query = header.match?(/\Aquery [IBTDR]+(?:\s|$)/)
      error = header.start_with?("statement error ")
      raise "unsupported corpus header #{header.inspect}" unless query || error || header == "statement ok"
      sql = []
      while at < lines.length && !lines[at].empty? && lines[at] != "----"
        sql << lines[at]; at += 1
      end
      raise "empty SQL record" if sql.empty?
      if query
        raise "query record has no expected-result delimiter" unless lines[at] == "----"
        at += 1
        at += 1 while at < lines.length && !lines[at].empty?
      end
      sql = sql.join("\n")
      kinds = []
      kinds << "rows" if query && sql.lstrip[0, 7].downcase == "explain"
      kinds << "cost" if cost && !error
      output << { sql: sql, kinds: kinds }
      cost = false
    end
    output
  end

  def expected(files)
    files.sort.flat_map do |name, source|
      records(source).flat_map { |record| record[:kinds].map { [name, _1, flat(record[:sql])] } }
    end
  end

  def read_transcript(path, core:, wanted:, extras:, failures:)
    unless File.file?(path)
      failures << "#{core}: missing encrypted transcript"; return []
    end
    source = File.binread(path).force_encoding(Encoding::UTF_8)
    unless source.valid_encoding?
      failures << "#{core}: transcript is not valid UTF-8"; return []
    end
    failures << "#{core}: transcript is not newline terminated" unless source.empty? || source.end_with?("\n")
    lines = source.split("\n", -1)
    lines.pop if lines.last == ""
    # Shared runners write a final newline even when no cost-bearing output was
    # produced. The required key list independently decides whether empty is valid.
    lines = [] if lines == [""]
    retained = []; outside = Set.new
    lines.each_with_index do |line, index|
      fields = line.split("\t", -1)
      if fields.length != 4 || !%w[cost rows].include?(fields[1]) ||
          (fields[1] == "cost" && !fields[3].match?(/\A[0-9]+\z/))
        failures << "#{core}: malformed transcript line #{index + 1}"; next
      end
      if core != "swift" && extras.include?(fields[0])
        outside << fields[0]; next
      end
      retained << fields
    end
    if retained.length != wanted.length
      failures << "#{core}: encrypted transcript has #{retained.length}/#{wanted.length} required outputs"
    end
    wanted.each_with_index do |key, index|
      next if retained[index]&.take(3) == key
      failures << "#{core}: missing/reordered/unexpected transcript output #{index + 1}: expected #{key.inspect}, got #{retained[index]&.take(3).inspect}"
    end
    outside.sort.each { puts "#{core}: postbaseline transcript comparison exclusion: #{_1}" }
    retained
  end

  def verify_files(path, core:, wanted:, extras:, failures:)
    unless File.file?(path)
      failures << "#{core}: missing file-completion evidence"; return []
    end
    prefix = core == "swift" ? "SQL file end: " : "PASS "
    actual = File.binread(path).lines.filter_map do |line|
      next unless line.start_with?(prefix)
      value = line.delete_prefix(prefix).strip
      core == "swift" ? value : value.split(/\s+/, 2).first
    end
    actual.reject! { core != "swift" && extras.include?(_1) }
    failures << "#{core}: duplicate completed file" unless actual.uniq == actual
    (wanted - actual).each { failures << "#{core}: file not completed: #{_1}" }
    (actual - wanted).each { failures << "#{core}: unexpected completed file: #{_1}" }
    actual
  end

  def compare(tables)
    return [] unless tables.key?("swift")
    swift = tables.fetch("swift")
    tables.reject { |core, _| core == "swift" }.flat_map do |core, rows|
      (0...[swift.length, rows.length].max).filter_map do |index|
        next if swift[index] == rows[index]
        "encrypted output #{index + 1}: swift=#{swift[index].inspect}; #{core}=#{rows[index].inspect}"
      end
    end
  end

  def check_adapter
    files = { "a.test" => "# requires: query.select\n# cost: 99\nquery T\nEXPLAIN SELECT 1\n----\nplan\n\n# cost: 2\nstatement error 22012\nSELECT 1 / 0\n\n# cost: 1\nstatement ok\nSELECT 2\n\n" }
    wanted = expected(files)
    raise "encrypted expected-output grammar mismatch" unless wanted == [["a.test", "rows", "EXPLAIN SELECT 1"], ["a.test", "cost", "EXPLAIN SELECT 1"], ["a.test", "cost", "SELECT 2"]]
    raise "transcript escaping mismatch" unless flat("a\\b\nc\td") == "a\\\\b\\nc\\td"
    Dir.mktmpdir("jed-encrypted-adapter-") do |directory|
      path = File.join(directory, "transcript.tsv")
      valid = wanted.each_with_index.map { |key, index| (key + [index.zero? ? "Result (rows=1 cost=1)" : "1"]).join("\t") }
      File.write(path, valid.join("\n") + "\n")
      failures = []
      table = read_transcript(path, core: "swift", wanted: wanted, extras: Set.new, failures: failures)
      raise "valid transcript rejected" unless failures.empty? && table.length == 3
      raise "matching transcript rejected" unless compare("swift" => table, "go" => table).empty?
      changed = table.map(&:dup); changed[1][3] = "2"
      raise "different encrypted cost accepted" if compare("swift" => table, "go" => changed).empty?
      unicode = [["a.test", "cost", "SELECT 'aäñé🙂'"]]
      File.write(path, (unicode.first + ["1"]).join("\t") + "\n")
      failures = []
      read_transcript(path, core: "swift", wanted: unicode, extras: Set.new, failures: failures)
      raise "UTF-8 transcript rejected" unless failures.empty?
      File.binwrite(path, "a.test\tcost\tSELECT '\xFF'\t1\n".b)
      failures = []
      read_transcript(path, core: "swift", wanted: unicode, extras: Set.new, failures: failures)
      raise "invalid UTF-8 transcript accepted" unless failures == ["swift: transcript is not valid UTF-8"]
      [valid.take(2), [valid[0], valid[0], *valid.drop(1)], valid.reverse, ["malformed"]].each do |lines|
        File.write(path, lines.join("\n") + "\n"); failures = []
        read_transcript(path, core: "swift", wanted: wanted, extras: Set.new, failures: failures)
        raise "missing/duplicate/reordered/malformed transcript accepted" if failures.empty?
      end
      File.write(path, "PASS a.test\n")
      failures = []; verify_files(path, core: "go", wanted: files.keys, extras: Set.new, failures: failures)
      raise "valid completed file rejected" unless failures.empty?
      verify_files(path, core: "go", wanted: ["missing.test"], extras: Set.new, failures: failures)
      raise "missing completed file accepted" if failures.empty?
    end
    puts "encrypted corpus adapter checks passed"
    0
  end

  # Reparse retained complete execution evidence after an adapter correction.
  # Preserve the original report, require successful original processes, and
  # repeat every membership check as well as the cross-core output comparison.
  def recheck(output)
    output = File.expand_path(output)
    original_path = File.join(output, "report.json")
    original = JSON.parse(File.read(original_path))
    cores = original.fetch("cores")
    unless original.fetch("baseline") == SwiftQueryMemory::BASELINE && original.fetch("complete_matrix") &&
        cores.uniq == cores && cores.to_set == SwiftQueryMemory::CORES.to_set &&
        original.fetch("runs").map { _1.fetch("core") } == cores && original.fetch("runs").all? { _1.fetch("passed") == true }
      raise "recheck requires complete successful original core executions at the frozen baseline"
    end
    actor = original.fetch("swift_actor").transform_keys(&:to_sym)
    SwiftQueryMemory.verify_actor(actor)
    files = SwiftQueryMemory.inventory
    included = files.reject { |_, source| SwiftQueryMemory.concurrency?(source) || SwiftQueryMemory.skip?(source, "disk") || SwiftQueryMemory.skip?(source, "encrypted") }
    wanted = expected(included)
    record_keys = included.flat_map { |name, source| (1..records(source).length).map { [name, _1] } }.to_set
    unless original.fetch("expected_files") == included.keys.sort && original.fetch("expected_records") == record_keys.length && original.fetch("expected_outputs") == wanted.length
      raise "original encrypted inventory differs from frozen inventory"
    end
    extras = (Dir.glob("**/*.test", base: SwiftQueryMemory::LIVE) - Dir.glob("**/*.test", base: SwiftQueryMemory::CORPUS)).to_set
    failures = []; tables = {}; runs = []; hashes = {}
    cores.each do |core|
      paths = ["#{core}.tsv", "#{core}-records.tsv", core == "swift" ? "swift-progress.log" : "#{core}.log"]
      paths.each { |name| hashes[name] = Digest::SHA256.file(File.join(output, name)).hexdigest }
      measured = SwiftQueryMemory.read_table(File.join(output, paths[1]), core: core, expected: record_keys,
        frozen: files.keys.to_set, extras: extras, failures: failures)
      completed = verify_files(File.join(output, paths[2]), core: core, wanted: included.keys, extras: extras, failures: failures)
      tables[core] = read_transcript(File.join(output, paths[0]), core: core, wanted: wanted, extras: extras, failures: failures)
      runs << { core: core, files: completed.length, records: measured.length, outputs: tables[core].length }
    end
    failures.concat(compare(tables))
    report = { original_report_sha256: Digest::SHA256.file(original_path).hexdigest, evidence_sha256: hashes,
      baseline: SwiftQueryMemory::BASELINE, swift_actor: actor, runs: runs, failures: failures, complete_matrix: true }
    File.write(File.join(output, "recheck.json"), JSON.pretty_generate(report) + "\n")
    warn failures.first(80).join("\n") unless failures.empty?
    puts "encrypted corpus recheck: #{runs.sum { _1[:records] }} records, #{failures.length} failures; full retained matrix"
    failures.empty? ? 0 : 1
  end

  def run
    return check_adapter if ENV["usage_check_adapter"] == "true"
    return recheck(ENV.fetch("usage_recheck")) if ENV["usage_recheck"]
    cores = ENV.fetch("usage_cores", SwiftQueryMemory::CORES.join(",")).split(",", -1)
    raise "--cores must contain swift and unique supported core names" unless cores.include?("swift") && cores.uniq == cores && (cores - SwiftQueryMemory::CORES).empty?
    configuration = ENV.fetch("usage_configuration", "debug")
    files = SwiftQueryMemory.inventory(ENV["usage_filter"])
    included = files.reject { |_, source| SwiftQueryMemory.concurrency?(source) || SwiftQueryMemory.skip?(source, "disk") || SwiftQueryMemory.skip?(source, "encrypted") }
    raise "selection contains no encrypted disk SQL files" if included.empty?
    wanted = expected(included)
    record_keys = included.flat_map { |name, source| (1..records(source).length).map { [name, _1] } }.to_set
    extras = (Dir.glob("**/*.test", base: SwiftQueryMemory::LIVE) - Dir.glob("**/*.test", base: SwiftQueryMemory::CORPUS)).to_set
    output = ENV["usage_output"] ? File.expand_path(ENV["usage_output"]) : Dir.mktmpdir("jed-swift-encrypted-corpus-")
    raise "output directory must be absent or empty" if File.exist?(output) && !Dir.empty?(output)
    FileUtils.mkdir_p(output)
    puts "encrypted corpus evidence: #{output}"
    unless ENV["usage_no_build"] == "true"
      env, command = SwiftNativeTasks.command("build", configuration: configuration, extra: ["--build-tests"])
      raise "Swift test build failed; see #{output}/build.log" unless SwiftQueryMemory.launch(command, env, File.join(output, "build.log"), SwiftNativeTasks::PACKAGE)
    end
    actor = SwiftQueryMemory.capture_actor(configuration, output)
    failures = []; runs = []; tables = {}
    cores.each do |core|
      transcript = File.join(output, "#{core}.tsv"); File.write(transcript, "")
      ledger = File.join(output, "#{core}-records.tsv"); File.write(ledger, "")
      log = File.join(output, "#{core}.log")
      environment = { "JED_CONFORMANCE_ENCRYPTED" => "1", "JED_CONFORMANCE_TRANSCRIPT" => transcript,
        "JED_CONFORMANCE_QUERY_MEMORY_PEAKS" => ledger, "JED_CONFORMANCE_FILTER" => ENV["usage_filter"],
        "JED_CONFORMANCE_QUERY_MEMORY" => nil, "JED_CONFORMANCE_QUERY_MEMORY_PROBE" => nil,
        "JED_CONFORMANCE_WORK_MEM" => nil, "JED_CONFORMANCE_TEMP_BUFFERS" => nil,
        "JED_SERVICE_OWNER" => nil, "JED_SWIFT_SERVICE_OWNER_COMMAND" => nil, "JED_SWIFT_SERVICE_ROOT" => nil }
      completion = log
      case core
      when "swift"
        SwiftQueryMemory.verify_actor(actor)
        command = [actor.fetch(:executable), actor.fetch(:selector)]; directory = SwiftNativeTasks::PACKAGE
        completion = File.join(output, "swift-progress.log"); File.write(completion, "")
        environment.merge!({"JED_SWIFT_SQL_FILES" => files.keys.sort.join(","), "JED_SWIFT_SQL_FILE" => nil,
          "JED_SWIFT_ALL_SQL" => nil, "JED_SWIFT_SQL_MODE" => "disk", "JED_SWIFT_TRACE_SQL" => "1",
          "JED_SWIFT_SQL_TRACE_PATH" => completion})
      when "rust"
        command = ["cargo", "run", "--release", "--quiet", "--bin", "conformance", "--manifest-path", File.join(SwiftNativeTasks::ROOT, "impl/rust/Cargo.toml"), "--", "disk"]
        directory = SwiftNativeTasks::ROOT
      when "go"
        command = ["go", "run", "./cmd/conformance", "disk"]; directory = File.join(SwiftNativeTasks::ROOT, "impl/go")
      when "ts"
        command = ["node", "src/bin/conformance.ts", "disk"]; directory = File.join(SwiftNativeTasks::ROOT, "impl/ts")
      end
      passed = SwiftQueryMemory.launch(command, environment, log, directory)
      failures << "#{core}: encrypted conformance process failed (#{log})" unless passed
      SwiftQueryMemory.verify_actor(actor) if core == "swift"
      measured = SwiftQueryMemory.read_table(ledger, core: core, expected: record_keys,
        frozen: files.keys.to_set, extras: extras, failures: failures)
      completed = verify_files(completion, core: core, wanted: included.keys, extras: extras, failures: failures)
      tables[core] = read_transcript(transcript, core: core, wanted: wanted, extras: extras, failures: failures)
      runs << { core: core, passed: !!passed, files: completed.length, records: measured.length, outputs: tables[core].length }
    end
    failures.concat(compare(tables))
    report = { baseline: SwiftQueryMemory::BASELINE, swift_actor: actor, cores: cores,
      expected_files: included.keys.sort, expected_records: record_keys.length, expected_outputs: wanted.length,
      exclusions: (files.keys - included.keys).sort, runs: runs, failures: failures,
      complete_matrix: cores.to_set == SwiftQueryMemory::CORES.to_set && !ENV["usage_filter"] }
    File.write(File.join(output, "report.json"), JSON.pretty_generate(report) + "\n")
    warn failures.first(80).join("\n") unless failures.empty?
    puts "encrypted corpus: #{runs.sum { _1[:records] }} records, #{failures.length} failures; #{report[:complete_matrix] ? 'full' : 'diagnostic subset'} matrix"
    failures.empty? ? 0 : 1
  rescue ArgumentError, RuntimeError, KeyError, SystemCallError => error
    warn error.message; 1
  end
end
