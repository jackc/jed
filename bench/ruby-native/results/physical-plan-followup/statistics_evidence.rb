# frozen_string_literal: true

require 'json'
require 'digest'
require 'time'
require_relative 'statistics_regressions'

module PhysicalPlanStatisticsEvidence
  ROOT = PhysicalStatisticsRegressions::ROOT
  BEFORE = '40ceac58a86ce19bba2ed2497adfb26b5ca7eafcc31e39def71312c1a9d56382'
  CORRECTED = '2f8c524be36d8c03d689cc37d6bd0fd5f4cd4be5c5945cbf7347a6e217d64413'
  HARNESS = '9935e8bfaf7d1780b050a77f652291d3e2c1c1ac74669ea35e29d1852ddb53ab'
  CORPUS = '7a1d297b72ac7f80645d78d8beb855dc2faa07c5279a9f1fa0761355a9631be1'
  module_function

  def read(name) = JSON.parse(File.read(File.join(__dir__, "#{name}.json")))
  def sha(path) = Digest::SHA256.file(path).hexdigest

  def bound!(report, source:, command:, address_space:, timeout:, success: true)
    raise 'statistics bounded source differs' unless report.values_at('source_sha256', 'final_source_sha256') == [source] * 2
    raise 'statistics bounded command/limits differ' unless report.values_at('command', 'memory_bytes', 'address_space_bytes', 'timeout_seconds') == [command, 536870912, address_space, timeout.to_f]
    raise 'statistics bounded result differs' unless report.fetch('success') == success && report.fetch('reason') == (success ? nil : 'worker exited 1')
    raise 'statistics RSS exceeded its limit' unless report.fetch('peak_rss_bytes').between?(0, 536870912)
  end

  def ruby_audit(name, source:, success:)
    report, bound = read(name), read("#{name}-bounded")
    command = ['ruby', '--yjit', 'impl/ruby-native/bin/conformance.rb']
    command << 'query/statistics_estimates_p9.test' unless success
    bound!(bound, source: source, command: command, address_space: 2147483648, timeout: success ? 60 : 90, success: success)
    raise 'statistics Ruby audit source differs' unless report.values_at('source_sha256', 'final_source_sha256') == [source] * 2
    api = success ? 'query' : 'execute'
    raise 'statistics Ruby audit mode differs' unless report.values_at('mode', 'filter', 'query_api', 'query_memory_api', 'compilation', 'jit') ==
      ['memory', 'query/statistics_estimates_p9.test', api, api, 'immediate', 'yjit']
    raise 'statistics final query collector differs' if success && report.fetch('query_collection') != 'external'
    raise 'statistics Ruby audit runtime differs' unless report.fetch('ruby').start_with?('ruby 4.0.7 ') && report.fetch('ruby_executable') == '/home/jack/.local/share/mise/installs/ruby/4.0.7/bin/ruby'
    raise 'statistics Ruby audit coverage differs' unless report.values_at('files', 'records', "#{api}_api_records", "#{api == 'query' ? 'execute' : 'query'}_api_records", 'available_records') == [1, 34, 34, 0, 34]
    raise 'statistics Ruby worker bounds differ' unless report.values_at('worker_memory_bytes', 'worker_address_space_bytes', 'worker_timeout_seconds') == [536870912, 2147483648, 10.0]
    %w[unsupported_files skipped_files timed_out_files memory_limited_files omitted_failure_details materialized_memory_records].each do |key|
      raise "statistics Ruby #{key} differs" unless report.fetch(key).zero?
    end
    printed = JSON.parse(bound.fetch('stdout').lines.last)
    raise 'statistics Ruby supervisor stdout differs' unless printed.all? { |key, value| report[key] == value }
    if success
      raise 'final statistics Ruby audit failed' unless report.values_at('passed', 'failed', 'failures') == [34, 0, []]
    else
      raise 'baseline statistics failure count differs' unless report.values_at('passed', 'failed') == [31, 3]
      failures = report.fetch('failures')
      sql = 'EXPLAIN SELECT l.id, r.id FROM p9_left l JOIN p9_right r ON l.k = r.k WHERE l.k = $1'
      raise 'baseline statistics failure cases differ' unless failures.map { |entry| entry.values_at('file', 'line', 'sql') } == [
        [PhysicalStatisticsRegressions::CORPUS, 242, sql],
        [PhysicalStatisticsRegressions::CORPUS, 263, sql.delete_prefix('EXPLAIN ') + ' ORDER BY l.id, r.id'],
        [PhysicalStatisticsRegressions::CORPUS, 330, sql]
      ]
      raise 'baseline actual-cost failure differs' unless failures[1].fetch('error') == 'RuntimeError: cost expected 92, got 96'
      expected_old = '["0", "Nested Loop", "inner; on:conjuncts=1", "2", "19", "1", "Scan p9_left", "Index bound: using p9_left_k; touched=2; filter:conjuncts=1", "1", "4", "1", "Scan p9_right", "Full scan; touched=2", "6", "7"]'
      raise 'baseline stale estimates/winner differ' unless [failures[0], failures[2]].all? { |entry| entry.fetch('error').end_with?("; got #{expected_old}") }
    end
    { report: "#{name}.json", source_sha256: source, api: api, passed: report.fetch('passed'), failed: report.fetch('failed'),
      failures: report.fetch('failures'), peak_worker_rss_bytes: report.fetch('peak_worker_rss_bytes') }
  end

  def journal!(name, final_source)
    report = read(name)
    pinned = PhysicalStatisticsRegressions.inputs
    raise 'statistics final Ruby source differs' unless pinned.fetch('ruby_source_sha256') == final_source
    # The supported engines and PostgreSQL ran before the final Ruby-only cast
    # validation refinement. Their bounded supervisor recorded that earlier
    # Ruby library too; retain it exactly while checking every consumed engine,
    # binary, shared corpus, runner and oracle helper against today's files.
    observed = pinned.merge('ruby_source_sha256' => CORRECTED)
    raise 'statistics regression provenance changed' unless report.fetch('inputs') == observed && report.fetch('final_inputs') == observed && report.fetch('verified')
    commands = report.fetch('commands')
    commands.each do |entry|
      raise 'statistics command failed' unless entry.fetch('success') && Time.iso8601(entry.fetch('started_at')) <= Time.iso8601(entry.fetch('finished_at'))
      env = entry.fetch('environment')
      raise 'statistics task environment differs' unless env.fetch('MISE_TRUSTED_CONFIG_PATHS') == ROOT && env.fetch('JED_NATIVE_MEMORY_BYTES') == '536870912'
      raise 'statistics JIT environment inherited' unless %w[RUBYOPT RUBY_YJIT_ENABLE RUBY_ZJIT_ENABLE].all? { |key| env.key?(key) && env[key].nil? }
    end
    commands.each_cons(2) do |left, right|
      raise 'statistics command intervals overlap' unless Time.iso8601(left.fetch('finished_at')) <= Time.iso8601(right.fetch('started_at'))
    end
    report
  end

  def build(source)
    corpus = File.join(ROOT, PhysicalStatisticsRegressions::CORPUS)
    raise 'shared statistics regression changed' unless sha(corpus) == CORPUS
    raise 'conformance harness changed' unless sha(File.join(ROOT, 'impl/ruby-native/bin/conformance.rb')) == HARNESS
    before = ruby_audit('statistics-before-memory-yjit', source: BEFORE, success: false)
    corrected = ruby_audit('jed-physical-statistics-final', source: CORRECTED, success: true)
    final = ruby_audit('statistics-final-memory-yjit', source: source, success: true)
    supported = journal!('statistics-supported', source)
    cells = PhysicalStatisticsRegressions::BINARIES.keys.product(%w[memory disk])
    raise 'supported statistics coverage differs' unless supported.fetch('commands').map { |entry| entry.fetch('name') } == cells.map { |core, mode| "statistics-#{core}-#{mode}" }
    checks = cells.zip(supported.fetch('commands')).map do |(core, mode), entry|
      child = [PhysicalStatisticsRegressions::BINARIES.fetch(core)]
      child << File.join(ROOT, 'impl/ts/src/bin/conformance.ts') if core == 'ts'
      child << 'disk' if mode == 'disk'
      address_space = (core == 'ts' && mode == 'disk' ? 64 : 16) * 1024**3
      bound = read("statistics-#{core}-#{mode}-bounded")
      bound!(bound, source: CORRECTED, command: child, address_space: address_space, timeout: 90)
      raise 'supported statistics journal command/filter differs' unless entry.fetch('command') == ['mise', 'run', 'ruby-native:bounded', '--', *child] &&
        entry.fetch('environment').fetch('JED_CONFORMANCE_FILTER') == 'query/statistics_estimates_p9.test'
      raise 'supported statistics did not pass' unless bound.fetch('stdout').include?("PASS query/statistics_estimates_p9.test\n") && bound.fetch('stdout').include?("[#{mode}] 1 passed, 0 failed, 0 skipped")
      { core: core, mode: mode, records: 34, report: "statistics-#{core}-#{mode}-bounded.json",
        peak_rss_bytes: bound.fetch('peak_rss_bytes'), elapsed_seconds: bound.fetch('elapsed_seconds') }
    end
    oracle = journal!('statistics-oracle', source)
    raise 'oracle command coverage differs' unless oracle.fetch('commands').length == 1
    projection = File.join(__dir__, 'statistics-oracle-values.test')
    raise 'oracle projection changed' unless File.binread(projection) == PhysicalStatisticsRegressions.oracle_projection && oracle.fetch('projection') == {
      'path' => projection.delete_prefix(ROOT + '/'), 'sha256' => sha(projection), 'setup_records' => 7, 'value_records' => 3 }
    child = ['mise', 'run', 'corpus:check', projection.delete_prefix(ROOT + '/')]
    bound = read('statistics-oracle-values-bounded')
    bound!(bound, source: CORRECTED, command: child, address_space: 2147483648, timeout: 180)
    raise 'oracle journal command differs' unless oracle.fetch('commands').first.fetch('command') == ['mise', 'run', 'ruby-native:bounded', '--', *child]
    raise 'oracle results differ' unless bound.fetch('stdout') == "OK: #{projection.delete_prefix(ROOT + '/')} regenerated byte-identically from the PostgreSQL oracle\n"
    %w[supported oracle].zip([68719476736, 2147483648], [600, 240]).each do |phase, address_space, timeout|
      command = ['ruby', 'bench/ruby-native/results/physical-plan-followup/statistics_regressions.rb', '--phase', phase, '--source-sha256', CORRECTED]
      bound!(read("statistics-#{phase}-run-bounded"), source: CORRECTED, command: command, address_space: address_space, timeout: timeout)
    end
    { corpus: PhysicalStatisticsRegressions::CORPUS, corpus_sha256: CORPUS, new_records: 7,
      baseline: before, initial_corrected_ruby: corrected, final_ruby: final,
      supported_checks: checks, supported_record_executions: 204,
      supported_oracle_provenance: {
        recorded_ruby_supervisor_source_sha256: CORRECTED, final_ruby_source_sha256: source,
        consumed_inputs_match_current: true,
        note: 'The recorded Ruby hash belongs to the resource supervisor, not the supported engines or PostgreSQL. Their engine sources, binaries, shared corpus, runner and oracle helpers still match exactly; the final Ruby library is revalidated separately.'
      },
      oracle: oracle.fetch('projection').merge('report' => 'statistics-oracle-values-bounded.json'),
      expected: { bindings: [0, 1, 2], output_rows: [16, 2, 0], actual_costs: [92, 19, 8],
        estimated_join_rows: [12, 2, 0], estimated_join_costs: [88, 19, 7],
        left_accesses: %w[full secondary secondary], left_output_rows: [8, 1, 0] },
      note: 'The new shared expectations pass every supported core in both storage modes. PostgreSQL independently verifies the three exact result sets; its planner/costs are not used as an oracle for jed EXPLAIN.' }
  end
end
