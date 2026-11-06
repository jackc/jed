# frozen_string_literal: true
require_relative "swift_task"
require "digest"
require "json"
require "tmpdir"
require "timeout"

module SwiftIntegrityCLI
  module_function
  def run
    configuration = ENV.fetch("usage_configuration", "debug")
    raise "configuration must be debug or release" unless %w[debug release].include?(configuration)
    unless ENV["usage_no_build"] == "true"
      environment, command = SwiftNativeTasks.command("build", configuration: configuration, extra: ["--product", "jed-check"])
      return 1 unless system(environment, *command, chdir: SwiftNativeTasks::PACKAGE)
    end
    source_actor = File.join(SwiftNativeTasks::BUILD, configuration, "jed-check")
    raise "missing jed-check executable; build it first" unless File.file?(source_actor) && File.executable?(source_actor)
    output = ENV["usage_output"] ? File.expand_path(ENV["usage_output"]) : Dir.mktmpdir("jed-swift-integrity-cli-")
    FileUtils.mkdir_p(output)
    raise "evidence directory must be empty" unless Dir.empty?(output)
    snapshot = JSON.parse(File.read(File.join(SwiftNativeTasks::PACKAGE, "Tests/JedTests/Fixtures/SpecSnapshot.json")))
    actor = File.join(output, "jed-check")
    actor_hash = Digest::SHA256.file(source_actor).hexdigest
    FileUtils.copy_file(source_actor, actor); File.chmod(0o500, actor)
    raise "actor changed during capture" unless [source_actor, actor].all? { Digest::SHA256.file(_1).hexdigest == actor_hash }
    images = File.join(output, "images"); FileUtils.mkdir(images)
    sources = snapshot.fetch("source_hashes")
    selected = sources.keys.select { _1.start_with?("spec/fileformat/integrity/") }
    raise "expected24 integrity image/report pairs" unless selected.length == 48
    selected += %w[empty_db.jed encrypted_empty_db.jed malformed_interior_cycle.jed].map { "spec/fileformat/fixtures/" + _1 }
    selected.each do |relative|
      source = File.join(SwiftNativeTasks::ROOT, relative)
      raise "frozen input changed: #{relative}" unless Digest::SHA256.file(source).hexdigest == sources.fetch(relative)
      FileUtils.copy_file(source, File.join(images, File.basename(relative)))
    end
    raw_key = File.join(output, "test-key.raw"); hex_key = File.join(output, "test-key.hex")
    wrong_key = File.join(output, "wrong-key.raw"); passphrase = File.join(output, "test-passphrase.txt")
    File.binwrite(raw_key, "J" * 32); File.write(hex_key, "4a" * 32 + "\n")
    File.binwrite(wrong_key, "\0" * 32); File.write(passphrase, "incorrect test passphrase\n")
    failures = []; results = []
    probe = lambda do |name, arguments, expected_exit, &assertion|
      ordinal = results.length
      stdout = File.join(output, "%02d-stdout.txt" % ordinal)
      stderr = File.join(output, "%02d-stderr.txt" % ordinal)
      command = [actor, *arguments]
      File.write(File.join(output, "%02d-command.json" % ordinal), JSON.pretty_generate(command) + "\n")
      pid = Process.spawn(*command, out: stdout, err: stderr, pgroup: true)
      begin
        _, status = Timeout.timeout(30) { Process.waitpid2(pid) }
        raise "exit #{status.exitstatus.inspect}, expected #{expected_exit}: #{File.read(stderr)}" unless status.exitstatus == expected_exit
        assertion.call(File.read(stdout), File.read(stderr)) if assertion
        results << { name: name, passed: true, exit: status.exitstatus }
      rescue => error
        if error.is_a?(Timeout::Error)
          Process.kill("KILL", -pid) rescue Errno::ESRCH
          Process.waitpid(pid) rescue Errno::ECHILD
        end
        failures << "#{name}: #{error}"
        results << { name: name, passed: false, error: error.to_s }
      end
    end
    reports = Dir.glob(File.join(images, "*.json")).sort
    count = 0
    reports.each do |path|
      expected = JSON.parse(File.read(path))
      expected.fetch("runs").each do |run|
        count += 1
        arguments = ["--format", "json", "--max-findings", run.fetch("max_findings").to_s]
        arguments << "--quick" if run.fetch("level") == "quick"
        arguments << "--constraints" if run["constraints"]
        arguments << File.join(images, expected.fetch("fixture"))
        report = run.fetch("report")
        exit_code = report.fetch("findings").empty? && !report.fetch("truncated") ? 0 : 2
        probe.call("#{expected.fetch('fixture')}/#{run.fetch('level')}/#{run.fetch('max_findings')}", arguments, exit_code) do |out, _|
          actual = JSON.parse(out)
          actual.fetch("findings").each { _1.delete("detail") }
          raise "report differs: #{actual.inspect}" unless actual == report
        end
      end
    end
    raise "expected50 frozen report runs, got#{count}" unless count == 50
    empty = File.join(images, "empty_db.jed"); encrypted = File.join(images, "encrypted_empty_db.jed")
    probe.call("clean-text", [empty], 0) { |out, _| raise "missing clean summary" unless out.match?(/\Aok: \d+ pages, 0 rows checked\n\z/) }
    probe.call("open-failed", ["--format", "json", File.join(images, "malformed_interior_cycle.jed")], 2) do |out, _|
      value = JSON.parse(out)
      raise "invalid open-failed report" unless value.keys == ["open_failed"] && value.fetch("open_failed").fetch("code") == "XX001"
    end
    [raw_key, hex_key].each do |key|
      probe.call("encrypted-#{File.basename(key)}", ["--format", "json", "--key-file", key, encrypted], 0) do |out, _|
        report = JSON.parse(out)
        raise "encrypted image not clean" unless report.fetch("findings").empty? && !report.fetch("truncated") && report.fetch("rows") == 0
      end
    end
    [["missing-credential", [], encrypted, "28P02"], ["wrong-key", ["--key-file", wrong_key], encrypted, "28P01"],
      ["wrong-passphrase", ["--passphrase-file", passphrase], encrypted, "28P01"],
      ["plaintext-credential", ["--key-file", raw_key], empty, "28P03"]].each do |name, flags, file, code|
      probe.call(name, [*flags, file], 1) { |_, error| raise "missing #{code}: #{error}" unless error.include?(code) }
    end
    probe.call("negative-limit", ["--max-findings", "-1", empty], 1)
    probe.call("unknown-format", ["--format", "yaml", empty], 1)
    probe.call("conflicting-credentials", ["--key-file", raw_key, "--key-file", hex_key, encrypted], 1)
    selected.each do |relative|
      failures << "input mutated: #{relative}" unless Digest::SHA256.file(File.join(images, File.basename(relative))).hexdigest == sources.fetch(relative)
    end
    failures << "captured actor changed" unless Digest::SHA256.file(actor).hexdigest == actor_hash
    report = { baseline: snapshot.fetch("baseline"), fixture_fingerprint: snapshot.fetch("fingerprint"),
      actor_sha256: actor_hash, fixture_reports: count, cli_probes: results.length - count, results: results, failures: failures }
    File.write(File.join(output, "report.json"), JSON.pretty_generate(report) + "\n")
    puts "Integrity CLI: #{count} frozen reports, #{results.length - count} boundary probes, #{failures.length} failures; #{output}"
    failures.each { warn _1 }
    failures.empty? ? 0 : 1
  rescue => error
    warn "integrity CLI adapter: #{error}"
    1
  end
end
