# frozen_string_literal: true

require 'json'
require 'digest'
require_relative 'run_regressions'

module MultiwaySharedRegressionEvidence
  ROOT = MultiwaySharedRegressions::ROOT
  module_function

  def read(name) = JSON.parse(File.read(File.join(__dir__, "#{name}.json")))
  def sha(path) = Digest::SHA256.file(path).hexdigest

  def bounded(report, memory:, address_space:, timeout:, success: true)
    raise 'shared evidence resource policy differs' unless report.values_at('memory_bytes', 'address_space_bytes', 'timeout_seconds') == [memory, address_space, timeout]
    raise 'shared evidence worker source changed' unless report.fetch('source_sha256') == report.fetch('final_source_sha256')
    raise 'shared evidence worker status differs' unless !!report.fetch('success') == success
    if success
      raise 'shared evidence successful worker failed/exceeded RSS' unless report.fetch('reason').nil? && report.fetch('peak_rss_bytes').between?(0, memory)
    end
  end

  def node_guard(source)
    old, final = read('shared-ts-disk-probe-16g-bounded'), read('shared-ts-disk-probe-64g-bounded')
    [old, final].zip([16, 64]).each do |report, gib|
      bounded(report, memory: 512 * 1024**2, address_space: gib * 1024**3, timeout: 8.0, success: gib == 64)
      raise 'Node guard probe source differs' unless report.fetch('source_sha256') == source
      command = [MultiwaySharedRegressions::BINARIES.fetch('ts'), 'bench/ruby-native/results/multiway-plan-followup/shared_ts_disk_probe.ts',
        File.join(__dir__, "shared-ts-disk-probe-#{gib}g.jsonl")]
      raise 'Node guard probe command differs' unless report.fetch('command') == command
    end
    raise 'old Node guard did not time out' unless old.fetch('reason') == 'worker deadline exceeded'
    values = [16, 64].map { |gib| File.readlines(File.join(__dir__, "shared-ts-disk-probe-#{gib}g.jsonl")).map { |line| JSON.parse(line) } }
    raise 'old Node probe passed creation unexpectedly' unless values.first.map { |entry| entry.fetch('phase') } == ['before-create']
    expected = ['before-create', 'after-create', 'closed-0', 'opened-0', 'closed-1', 'opened-1', 'closed-2', 'opened-2', 'finished']
    raise 'new Node probe missed reopen coverage' unless values.last.map { |entry| entry.fetch('phase') } == expected
    status = lambda do |entry, field|
      Integer(entry.fetch('status').match(/^#{field}:\s+(\d+)/).captures.first)
    end
    before, after = values.last.first(2)
    virtual_bytes = status.call(after, 'VmSize') * 1024
    raise 'Node virtual-space explanation is unsupported' unless virtual_bytes > 16 * 1024**3 && virtual_bytes < 64 * 1024**3 &&
      status.call(before, 'Threads') < status.call(after, 'Threads') && status.call(after, 'VmRSS') * 1024 < 512 * 1024**2
    { before_threads: status.call(before, 'Threads'), after_threads: status.call(after, 'Threads'),
      virtual_bytes_after_owner_start: virtual_bytes, rss_bytes_after_owner_start: status.call(after, 'VmRSS') * 1024,
      note: 'The 16 GiB virtual ceiling prevented the Node owner worker from completing startup. The same auto-locking probe and all file regressions pass with 64 GiB virtual space; the resident-memory cap remains 512 MiB.' }
  end

  def rust_build
    proof = read('shared-rust-provenance')
    proof.fetch('files').each { |path, hash| raise 'Rust build artifact changed' unless sha(path) == hash }
    raise 'Rust compiler identity differs' unless proof.values_at('rustup_toolchain', 'rustup_home', 'cargo_manifest_dir') == ['1.99.0', '/home/jack/.rustup', File.join(ROOT, 'impl/rust')]
    paths = Dir[File.join(ROOT, 'impl/rust/src/**/*.rs')].sort
    digest = Digest::SHA256.new
    paths.each { |path| digest << path.delete_prefix(ROOT + '/') << "\0" << File.binread(path) }
    raise 'Rust linked engine source changed' unless digest.hexdigest == proof.fetch('rust_core_sha256')
    prior = File.join(ROOT, proof.fetch('prior_cross_report'))
    raise 'prior Rust comparison provenance changed' unless sha(prior) == proof.fetch('prior_cross_report_sha256') &&
      JSON.parse(File.read(prior)).fetch('source_sha256').fetch('rust_core') == digest.hexdigest
    freshness = read('shared-rust-artifact-check-bounded')
    bounded(freshness, memory: 512 * 1024**2, address_space: 16 * 1024**3, timeout: 30.0)
    raise 'Rust existing artifact was not fresh' unless freshness.fetch('stderr').include?('Finished `release`') && !freshness.fetch('stderr').include?('Compiling')
    expected = %w[cargo build --release --locked --offline --manifest-path bench/ruby-native/compare/rust/Cargo.toml --target-dir /tmp/jed-ruby-native-compare/rust-target -j1]
    raise 'Rust freshness command differs' unless freshness.fetch('command') == expected
    build = read('shared-rust-build-bounded')
    bounded(build, memory: 512 * 1024**2, address_space: 16 * 1024**3, timeout: 120.0)
    expected = ['rustc', '--edition=2024', '-C', 'opt-level=3', '-L', 'dependency=/tmp/jed-ruby-native-compare/rust-target/release/deps',
      '--extern', 'jed=/tmp/jed-ruby-native-compare/rust-target/release/deps/libjed-331b188b721c2993.rlib',
      'impl/rust/src/bin/conformance.rs', '-o', MultiwaySharedRegressions::BINARIES.fetch('rust')]
    raise 'Rust harness build command differs' unless build.fetch('command') == expected
    attempts = [['512m', 512 * 1024**2], ['1g', 1024**3]].map do |suffix, memory|
      report = read("shared-rust-build-attempt-#{suffix}-bounded")
      bounded(report, memory: memory, address_space: 16 * 1024**3, timeout: 180.0, success: false)
      raise 'Rust failed build reason differs' unless report.fetch('reason') == "worker memory limit exceeded (#{memory} bytes)"
      { report: "shared-rust-build-attempt-#{suffix}-bounded.json", reason: report.fetch('reason'), peak_rss_bytes: report.fetch('peak_rss_bytes') }
    end
    { provenance: proof, failed_root_package_builds: attempts, successful_harness_build_peak_rss_bytes: build.fetch('peak_rss_bytes'),
      note: 'The root-package Cargo attempts were stopped at their explicit RSS guards. The successful build compiled only the current conformance harness against the already validated, unchanged release engine artifacts.' }
  end

  def oracle(source)
    probe, bound = read('oracle-parameter-probe'), read('oracle-parameter-probe-bounded')
    bounded(bound, memory: 512 * 1024**2, address_space: 2 * 1024**3, timeout: 60.0)
    raise 'oracle probe source/command differs' unless bound.fetch('source_sha256') == source && bound.fetch('command') == ['ruby', 'bench/ruby-native/results/multiway-plan-followup/oracle-parameter-probe.rb']
    raise 'oracle probe stdout differs' unless JSON.parse(bound.fetch('stdout')) == probe
    raise 'oracle probe script changed' unless probe.fetch('harness_sha256') == sha(File.join(__dir__, 'oracle-parameter-probe.rb'))
    raise 'oracle probe sources changed' unless probe.fetch('source_sha256') == probe.fetch('source_sha256_after') &&
      probe.fetch('source_sha256').all? { |path, hash| sha(File.join(ROOT, path)) == hash }
    raise 'oracle live profile differs' unless probe.fetch('live_cluster') == {
      'pg_major' => 18, 'locale_provider' => 'b', 'locale' => 'C.UTF-8', 'datcollate' => 'C.UTF-8', 'datctype' => 'C.UTF-8',
      'encoding' => 'UTF8', 'database' => 'jed_oracle', 'tzdata_backward_links' => false }
    outcomes = probe.fetch('observations')
    cases = [
      ['SELECT id FROM narrow WHERE s = $1 AND $1 = $2', '40000,40000'],
      ['SELECT id FROM narrow WHERE s = $1 AND $1 = $2', '100,100'],
      ['SELECT id FROM narrow WHERE s = $1 OR id = $1', '40000'],
      ['SELECT id FROM narrow WHERE id = $1 OR $1 = 5', '2147483648'],
      ['SELECT id FROM narrow_key WHERE id = $1 AND $1 < 100000', '40000'],
      ['SELECT id FROM narrow_key WHERE id = $1 AND $1 < 100000', '-40000'],
      ['SELECT $1::i64 FROM narrow_key WHERE id = $1', '9223372036854775807'],
      ['SELECT s + $1, $1 + 1 FROM narrow WHERE id = $1', '1'],
      ['SELECT $1 + 1, s + $1 FROM narrow WHERE id = $1', '1'],
      ['SELECT $1::i16, $1::i32, $1::i64', '1'],
      ['SELECT s + $1, $1::i64 FROM narrow', '32700']
    ]
    raise 'oracle SQL/binding coverage differs' unless outcomes.map { |entry| entry.values_at('sql', 'params') } == cases
    states = [%w[22003], [], %w[22003], %w[22003], %w[22003], %w[22003], [], [], [], [], %w[22003]]
    raise 'oracle SQLSTATE coverage differs' unless outcomes.map { |entry| entry.fetch('sqlstates') } == states
    outputs = [[], ["1\n"], [], [], [], [], [], ["101|2\n"], ["2|101\n"], ["1|1|1\n"], []]
    raise 'oracle exact outputs differ' unless outcomes.map { |entry| entry.fetch('output') } == outputs
    %w[params casts].zip(MultiwaySharedRegressions::FILES.values).each do |name, relative|
      report = read("oracle-#{name}-bounded")
      bounded(report, memory: 512 * 1024**2, address_space: 2 * 1024**3, timeout: 180.0)
      path = "spec/conformance/suites/#{relative}"
      raise 'oracle final file/command/source differs' unless report.fetch('source_sha256') == source && report.fetch('command') == ['mise', 'run', 'corpus:check', path] &&
        report.fetch('stdout') == "OK: #{path} regenerated byte-identically from the PostgreSQL oracle\n"
    end
    before = read('oracle-params-before-bounded')
    bounded(before, memory: 512 * 1024**2, address_space: 2 * 1024**3, timeout: 180.0, success: false)
    raise 'oracle pre-ledger divergence changed' unless before.fetch('stdout').scan(/^\+ERROR: /).length == 5
    { reports: %w[oracle-params-bounded.json oracle-casts-bounded.json], live_cluster: probe.fetch('live_cluster'),
      observations: outcomes, ledger_path: 'spec/conformance/oracle_overrides.toml',
      ledger_sha256: sha(File.join(ROOT, 'spec/conformance/oracle_overrides.toml')),
      note: 'Both files regenerate byte-identically after recording the five binding-specific, independently observed PostgreSQL differences in the existing override ledger.' }
  end

  def build(source)
    raw = read('shared-regressions')
    current = MultiwaySharedRegressions.inputs
    pinned = raw.fetch('inputs')
    raise 'supported-core regression inputs changed' unless pinned.reject { |key, _| key == 'runner_sha256' } == current.reject { |key, _| key == 'runner_sha256' }
    complete = raw['verified']
    runner = complete ? 'run_regressions.rb' : 'run_regressions.initial'
    raise 'supported-core measured runner changed' unless pinned.fetch('runner_sha256') == sha(File.join(__dir__, runner))
    cells = MultiwaySharedRegressions::BINARIES.keys.product(%w[memory disk], MultiwaySharedRegressions::FILES.values)
    commands = raw.fetch('commands')
    expected_cells = complete ? cells : cells.first(11)
    raise 'supported-core command coverage differs' unless commands.map { |item| item.values_at('core', 'mode', 'file') } == expected_cells
    if complete
      raise 'supported-core final input guard differs' unless raw.fetch('final_inputs') == pinned
    else
      report = read('shared-ts-disk-binding-bounded')
      bounded(report, memory: 512 * 1024**2, address_space: 16 * 1024**3, timeout: 90.0, success: false)
      raise 'initial supported-core timeout differs' unless report.fetch('reason') == 'worker deadline exceeded' && !commands.last.fetch('success')
    end
    rows = cells.map do |core, mode, relative|
      retried = !complete && core == 'ts' && mode == 'disk'
      name = MultiwaySharedRegressions::FILES.key(relative)
      stem = "shared-#{core}-#{mode}-#{name}#{retried ? '-retry' : ''}-bounded"
      report = read(stem)
      address_space = core == 'ts' && mode == 'disk' ? 64 * 1024**3 : 16 * 1024**3
      bounded(report, memory: 512 * 1024**2, address_space: address_space, timeout: 90.0)
      raise 'shared regression final native guard differs' unless report.fetch('source_sha256') == source
      child = [MultiwaySharedRegressions::BINARIES.fetch(core)]
      child << File.join(ROOT, 'impl/ts/src/bin/conformance.ts') if core == 'ts'
      child << 'disk' if mode == 'disk'
      raise 'supported-core child command differs' unless report.fetch('command') == child
      raise 'supported-core file did not pass' unless report.fetch('stdout').include?("PASS #{relative}\n") &&
        report.fetch('stdout').include?("[#{mode}] 1 passed, 0 failed, 0 skipped")
      unless retried
        entry = commands.find { |item| item.values_at('core', 'mode', 'file') == [core, mode, relative] }
        raise 'supported-core journal command/filter differs' unless entry.fetch('success') && entry.fetch('command') == ['mise', 'run', 'ruby-native:bounded', '--', *child] &&
          entry.fetch('environment').fetch('JED_CONFORMANCE_FILTER') == relative
      end
      { core: core, storage: mode, corpus: relative, records: pinned.fetch('corpus').fetch(name).fetch('records'),
        report: "#{stem}.json", retried: retried, peak_rss_bytes: report.fetch('peak_rss_bytes'), elapsed_seconds: report.fetch('elapsed_seconds') }
    end
    { inputs: pinned, cases: rows, cases_passed: rows.length, record_executions: rows.sum { |row| row.fetch(:records) },
      rust_build: rust_build, node_guard: node_guard(source), oracle: oracle(source),
      rss_sampling_note: 'The enforced resident cap is512MiB. A zero sampled peak means the tiny worker exited before its first sample; it is not a measurement of zero memory use.' }
  end
end
