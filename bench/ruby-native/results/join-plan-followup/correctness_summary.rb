# frozen_string_literal: true

# Run only after all four corpus audits and the final host suite have exited.
# This independently verifies retained evidence before generating its manifest.
require 'json'
require 'digest'
require 'optparse'
require_relative '../../source'

module JoinPlanCorrectnessEvidence
  ROOT = File.expand_path('../../../..', __dir__)
  SOURCE = File.join(ROOT, 'impl/ruby-native')
  RUNTIMES = {
    'memory' => { executable: File.join(ROOT, '.dev/ruby-native-runtimes/ruby-4.1.0-preview1/bin/ruby'),
      jit: 'zjit', version: 'ruby 4.1.0preview1 ', records: 20_267, sql_calls: 20_087 },
    'disk' => { executable: '/home/jack/.local/share/mise/installs/ruby/4.0.7/bin/ruby',
      jit: 'yjit', version: 'ruby 4.0.7 ', records: 18_250, sql_calls: 18_099 }
  }.freeze
  HARNESS_PATHS = %w[impl/ruby-native/bin/conformance.rb impl/ruby-native/bin/concurrency.rb
    impl/ruby-native/tools/bounded_process.rb impl/ruby-native/tools/run_bounded.rb spec/conformance/params.rb].freeze
  module_function

  def read(name) = JSON.parse(File.read(File.join(__dir__, name)))

  def files_hash(paths)
    digest = Digest::SHA256.new
    paths.sort.each { |path| digest << path.delete_prefix(ROOT + '/') << "\0" << File.binread(path) }
    digest.hexdigest
  end

  def bounded!(report, source, timeout: nil)
    raise 'bounded audit did not succeed' unless report['success'] && report['reason'].nil?
    raise 'bounded source changed or differs' unless report.values_at('source_sha256', 'final_source_sha256') == [source, source]
    raise 'bounded memory limits differ' unless report.values_at('memory_bytes', 'address_space_bytes') == [536870912, 2147483648]
    raise 'bounded timeout differs' if timeout && report['timeout_seconds'] != timeout
    raise 'bounded peak RSS exceeds allowance' unless report.fetch('peak_rss_bytes').positive? && report.fetch('peak_rss_bytes') <= 536870912
  end

  def focused_reports(source)
    commands = {
      'interpreter' => ['ruby', '--disable-yjit', '--disable-zjit'],
      'yjit' => ['ruby', '--yjit'],
      'zjit' => ['.dev/ruby-native-runtimes/ruby-4.1.0-preview1/bin/ruby', '--zjit']
    }
    paths = commands.keys.map { |jit| File.join(__dir__, "focused-#{jit}-bounded.json") }
    return {} if paths.none? { |path| File.exist?(path) }
    raise 'incomplete focused runtime reports' unless paths.all? { |path| File.exist?(path) }
    commands.to_h do |jit, command|
      report = read("focused-#{jit}-bounded.json")
      bounded!(report, source, timeout: 180.0)
      expected = command + ['impl/ruby-native/test/prepared_join_plan_test.rb']
      raise 'focused test command differs' unless report.fetch('command') == expected
      raise 'focused test did not finish' unless report.fetch('stdout').include?('ruby-native prepared joins:') &&
        report.fetch('stdout').include?('exact budget prefixes passed')
      # Bounded reports identify the supervising Ruby. The child runtime is
      # selected by the exact executable and flags above, not this field.
      raise 'focused supervisor differs' unless report.fetch('runtime').start_with?('ruby 4.0.7 ')
      [jit, report.slice('command', 'peak_rss_bytes', 'elapsed_seconds').merge(
        'supervisor_runtime' => report.fetch('runtime'), 'requested_jit' => jit)]
    end
  end

  def build(frozen_source, frozen_harness)
    options = { source: frozen_source, harness: frozen_harness }
    source = NativeRubySource.digest(SOURCE)
    raise 'current source differs from frozen source' unless source == options.fetch(:source)
    harness_hashes = HARNESS_PATHS.to_h { |path| [path, Digest::SHA256.file(File.join(ROOT, path)).hexdigest] }
    raise 'current conformance harness differs from frozen harness' unless harness_hashes.fetch(HARNESS_PATHS.first) == options.fetch(:harness)
    corpus = Dir[File.join(ROOT, 'spec/conformance/suites/**/*.test')].sort
    tests = Dir[File.join(SOURCE, 'test/*_test.rb')].sort
    raise 'unexpected corpus or host-test count' unless corpus.length == 679 && tests.length == 62
    skip_reasons = %w[memory disk].to_h do |mode|
      reasons = { 'explicit_mode' => [], 'memory_only_concurrency' => [] }
      corpus.each do |path|
        contents = File.read(path)
        reason = if contents.match?(/^#\s*skip:\s*#{mode}\b/)
                   'explicit_mode'
                 elsif mode == 'disk' && contents.match?(/^#\s*format:\s*concurrency/) &&
                       contents.match?(/^#\s*requires:.*resource\.storage_memory\b/)
                   'memory_only_concurrency'
                 end
        reasons.fetch(reason) << path.delete_prefix(ROOT + '/') if reason
      end
      [mode, reasons]
    end
    skipped_paths = skip_reasons.transform_values { |reasons| reasons.values.flatten.sort }
    authored_skips = skipped_paths.transform_values(&:length)
    available_records = corpus.sum { |path| File.read(path).scan(/^(?:statement|query) /).length }
    provenance = { 'source_sha256' => source, 'conformance_harness_sha256' => options.fetch(:harness),
      'harness_files_sha256' => harness_hashes, 'corpus_files' => corpus.length, 'corpus_sha256' => files_hash(corpus),
      'authored_skip_files' => authored_skips, 'authored_skip_paths' => skipped_paths, 'authored_skips_by_reason' => skip_reasons }
    runner_hash = Digest::SHA256.file(File.join(__dir__, 'run_audits.rb')).hexdigest
    summary = { source_sha256: source, conformance_harness_sha256: options.fetch(:harness), audits: {},
      policy: 'Every SQL record uses the selected API. Query collection is external for every record. No failure allowlist, mixed API routing or un-authored skips are accepted.' }
    RUNTIMES.each do |mode, runtime|
      %w[query execute].each do |api|
        name = "#{mode}-#{api == 'query' ? 'pure-query' : api}-#{runtime.fetch(:jit)}"
        report, bound, journal = ["#{name}.json", "#{name}-bounded.json", "#{name}-run.json"].map { |file| read(file) }
        bounded!(bound, source, timeout: 900.0)
        raise 'corpus source differs' unless report.values_at('source_sha256', 'final_source_sha256') == [source, source]
        raise 'corpus runtime identity differs' unless report.fetch('ruby').start_with?(runtime.fetch(:version)) &&
          report.values_at('ruby_executable', 'jit') == runtime.values_at(:executable, :jit) &&
          report.fetch('ruby').include?("+#{runtime.fetch(:jit).upcase}")
        command = [runtime.fetch(:executable), "--#{runtime.fetch(:jit)}", 'impl/ruby-native/bin/conformance.rb']
        raise 'bounded command differs' unless bound.fetch('command') == command
        raise 'journal command differs' unless journal.fetch('command') == ['mise', 'run', 'ruby-native:bounded', '--', *command]
        raise 'journal did not finish verification' unless journal['finished_at'] && journal['command_success'] && journal['verified']
        raise 'journal source/harness/corpus changed' unless journal.values_at('source_before', 'source_after') == [provenance, provenance]
        raise 'journal runner or executable changed' unless journal.fetch('runner_sha256') == runner_hash &&
          journal.fetch('ruby_executable_sha256') == Digest::SHA256.file(runtime.fetch(:executable)).hexdigest
        environment = journal.fetch('environment')
        expected = { 'RUBY_NATIVE_SOURCE' => SOURCE, 'RUBYOPT' => nil, 'RUBY_YJIT_ENABLE' => nil, 'RUBY_ZJIT_ENABLE' => nil,
          'JED_NATIVE_COMPILATION' => 'immediate', 'JED_NATIVE_QUERY_API' => api, 'JED_NATIVE_QUERY_MEMORY_API' => nil,
          'JED_NATIVE_STORAGE' => mode, 'JED_NATIVE_LOCKING' => mode == 'disk' ? 'exclusive' : 'auto',
          'JED_NATIVE_CORPUS_ROOT' => File.join(ROOT, 'spec/conformance/suites'), 'JED_CONFORMANCE_FILTER' => nil,
          'JED_NATIVE_MEMORY_BYTES' => '536870912', 'JED_NATIVE_ADDRESS_SPACE_BYTES' => '2147483648',
          'JED_NATIVE_FILE_TIMEOUT' => '10', 'JED_NATIVE_TIMEOUT' => '900' }
        raise 'journal environment differs' unless environment.slice(*expected.keys) == expected
        raise 'audit API/coverage settings differ' unless report.values_at('mode', 'filter', 'query_api', 'query_memory_api', 'compilation') ==
          [mode, '', api, api, 'immediate']
        raise 'query collector was not external' if api == 'query' && report['query_collection'] != 'external'
        raise 'audit missed files or records' unless report['finished_at'] && report.values_at('files', 'records', 'passed', 'available_records') ==
          [679, runtime.fetch(:records), runtime.fetch(:records), available_records]
        raise 'audit skipped un-authored files' unless report.fetch('skipped_files') == authored_skips.fetch(mode)
        raise 'audit contains failure details' unless report.fetch('failures').empty?
        %w[failed unsupported_files timed_out_files memory_limited_files omitted_failure_details materialized_memory_records].each do |key|
          raise "audit #{key} is nonzero" unless report.fetch(key).zero?
        end
        other = api == 'query' ? 'execute' : 'query'
        raise 'audit mixed APIs or dropped SQL calls' unless report.values_at("#{api}_api_records", "#{other}_api_records") == [runtime.fetch(:sql_calls), 0]
        raise 'per-file resource guard differs' unless report.values_at('worker_memory_bytes', 'worker_address_space_bytes', 'worker_timeout_seconds') == [536870912, 2147483648, 10.0]
        raise 'worker peak RSS exceeds allowance' unless report.fetch('peak_worker_rss_bytes').positive? && report.fetch('peak_worker_rss_bytes') <= 536870912
        raise 'compiler was not exercised' unless report.fetch('compiled_text_snapshot_kernels').positive? && report.fetch('compiled_like_kernels').positive?
        printed = JSON.parse(bound.fetch('stdout').lines.last)
        raise 'bounded stdout and corpus report differ' unless printed.all? { |key, value| report[key] == value }
        raise 'journal final observations differ' unless journal.values_at('records', 'peak_worker_rss_bytes') == report.values_at('records', 'peak_worker_rss_bytes')
        summary[:audits][name] = report.slice('mode', 'locking', 'ruby', 'jit', 'query_api', 'query_collection', 'files', 'records', 'passed', 'failed',
          'skipped_files', 'query_api_records', 'execute_api_records', 'peak_worker_rss_bytes', 'compiled_kernels', 'compiled_text_snapshot_kernels', 'compiled_like_kernels').merge(
            'elapsed_seconds' => bound.fetch('elapsed_seconds'), 'outer_peak_rss_bytes' => bound.fetch('peak_rss_bytes'))
      end
    end
    host = read('host-tests-bounded.json')
    bounded!(host, source)
    command = host.fetch('command')
    raise 'host suite did not load every test file' unless command.any? { |part| part.include?('test/*_test.rb') && part.include?('require_relative') }
    raise 'host suite Ruby runtime differs' unless host.fetch('runtime').start_with?('ruby 4.0.7 ')
    raise 'host suite interpreter audit unexpectedly enabled a JIT' if command.include?('--yjit') || command.include?('--zjit') ||
      host.fetch('runtime').include?('+YJIT') || host.fetch('runtime').include?('+ZJIT')
    test_hashes = tests.to_h { |path| [path.delete_prefix(ROOT + '/'), Digest::SHA256.file(path).hexdigest] }
    summary[:host_tests] = { files: tests.length, named_files_sha256: files_hash(tests), file_sha256: test_hashes,
      command: command, runtime: host.fetch('runtime'), jit: 'interpreter',
      peak_rss_bytes: host.fetch('peak_rss_bytes'), elapsed_seconds: host.fetch('elapsed_seconds') }
    summary[:focused_tests] = focused_reports(source)
    source_record = provenance.merge('host_test_files' => tests.length, 'host_tests_sha256' => files_hash(tests),
      'host_test_file_sha256' => test_hashes, 'runner_sha256' => runner_hash,
      'limits' => { 'worker_rss_bytes' => 536870912, 'worker_address_space_bytes' => 2147483648,
        'per_file_timeout_seconds' => 10, 'outer_timeout_seconds' => 900 })
    { provenance: source_record, summary: summary }
  end
end

if $PROGRAM_NAME == __FILE__
  options = {}
  OptionParser.new do |parser|
    parser.on('--source-sha256 HASH') { |value| options[:source] = value }
    parser.on('--harness-sha256 HASH') { |value| options[:harness] = value }
  end.parse!
  evidence = JoinPlanCorrectnessEvidence.build(options.fetch(:source), options.fetch(:harness))
  evidence.each { |name, value| File.write(File.join(__dir__, "correctness-#{name}.json"), JSON.pretty_generate(value) + "\n") }
  puts JSON.pretty_generate(evidence.fetch(:summary))
end
