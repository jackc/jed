# frozen_string_literal: true

# Explicit full-corpus correctness audits. Each corpus file and the outer
# process are bounded; every record uses the selected public API. Query audits
# collect externally. No record-level failure allowance or API routing exists.
require 'json'
require 'digest'
require 'optparse'
require 'time'
require_relative '../../source'

module CursorConformanceAudits
  ROOT = File.expand_path('../../../..', __dir__)
  SOURCE = File.join(ROOT, 'impl/ruby-native')
  HARNESS = File.join(SOURCE, 'bin/conformance.rb')
  CORPUS = File.join(ROOT, 'spec/conformance/suites')
  RUNTIMES = {
    'memory' => [File.join(ROOT, '.dev/ruby-native-runtimes/ruby-4.1.0-preview1/bin/ruby'), 'zjit'],
    'disk' => ['/home/jack/.local/share/mise/installs/ruby/4.0.7/bin/ruby', 'yjit']
  }.freeze
  EXPECTED_RECORDS = { 'memory' => 20_267, 'disk' => 18_250 }.freeze
  EXPECTED_SQL_CALLS = { 'memory' => 20_087, 'disk' => 18_099 }.freeze
  HARNESS_PATHS = %w[impl/ruby-native/bin/conformance.rb impl/ruby-native/bin/concurrency.rb
    impl/ruby-native/tools/bounded_process.rb impl/ruby-native/tools/run_bounded.rb spec/conformance/params.rb].freeze
  module_function

  def named_digest(paths)
    digest = Digest::SHA256.new
    paths.sort.each { |path| digest << path.delete_prefix(ROOT + '/') << "\0" << File.binread(path) }
    digest.hexdigest
  end

  def provenance
    paths = Dir[File.join(CORPUS, '**/*.test')].sort
    { source_sha256: NativeRubySource.digest(SOURCE),
      conformance_harness_sha256: Digest::SHA256.file(HARNESS).hexdigest,
      harness_files_sha256: HARNESS_PATHS.to_h { |path| [path, Digest::SHA256.file(File.join(ROOT, path)).hexdigest] },
      corpus_files: paths.length, corpus_sha256: named_digest(paths),
      authored_skip_files: %w[memory disk].to_h { |mode| [mode, paths.count { |path| File.read(path).match?(/^#\s*skip:\s*#{mode}\b/) }] } }
  end

  def verify!(report, bound, mode, api, jit, binary, pinned)
    [report, bound].each do |value|
      raise 'source changed during corpus audit' unless value.values_at('source_sha256', 'final_source_sha256') == [pinned.fetch(:source_sha256)] * 2
    end
    raise 'bounded corpus audit failed' unless bound['success'] && bound['reason'].nil?
    raise 'outer audit limits differ' unless bound.values_at('memory_bytes', 'address_space_bytes', 'timeout_seconds') == [536870912, 2147483648, 900.0]
    raise 'corpus audit runtime differs' unless report.values_at('ruby_executable', 'jit') == [binary, jit]
    raise 'corpus audit was not uniform and complete' unless report.values_at('mode', 'filter', 'query_api', 'query_memory_api', 'compilation') ==
      [mode, '', api, api, 'immediate']
    raise 'query collection is not external' if api == 'query' && report['query_collection'] != 'external'
    raise 'corpus audit did not finish every corpus file' unless report['finished_at'] && report.fetch('files') == pinned.fetch(:corpus_files)
    raise 'corpus audit skipped extra files' unless report.fetch('skipped_files') == pinned.fetch(:authored_skip_files).fetch(mode)
    raise 'corpus audit retained a failure or lost records' unless report.fetch('failures').empty? &&
      report.values_at('passed', 'records') == [EXPECTED_RECORDS.fetch(mode)] * 2
    %w[failed unsupported_files timed_out_files memory_limited_files omitted_failure_details materialized_memory_records].each do |key|
      raise "corpus audit #{key} is nonzero" unless report.fetch(key).zero?
    end
    other_api = api == 'query' ? 'execute' : 'query'
    raise 'corpus audit mixed APIs' unless report.fetch("#{other_api}_api_records").zero?
    raise 'corpus audit did not send every SQL record through selected API' unless report.fetch("#{api}_api_records") == EXPECTED_SQL_CALLS.fetch(mode)
    raise 'per-file audit limits differ' unless report.values_at('worker_memory_bytes', 'worker_address_space_bytes', 'worker_timeout_seconds') == [536870912, 2147483648, 10.0]
    printed = JSON.parse(bound.fetch('stdout').lines.last)
    raise 'outer audit stdout differs from corpus report' unless printed.all? { |key, value| report[key] == value }
    raise 'compiler audit did not cover text snapshots' unless report.fetch('compiled_text_snapshot_kernels').positive?
  end

  def run(mode, options, pinned)
    binary, jit = RUNTIMES.fetch(mode)
    api = options.fetch(:api)
    name = "#{mode}-#{api == 'query' ? 'pure-query' : api}-#{jit}"
    report_path = File.join(__dir__, "#{name}.json")
    bound_path = File.join(__dir__, "#{name}-bounded.json")
    journal_path = File.join(__dir__, "#{name}-run.json")
    raise "existing audit outputs: #{name}; specify --replace to rerun" if !options[:replace] && [report_path, bound_path, journal_path].any? { |path| File.exist?(path) }
    raise 'source or harness changed before corpus audit' unless provenance == pinned
    environment = {
      'MISE_STATE_DIR' => '/tmp/jed-ruby-native-mise-state', 'MISE_CACHE_DIR' => '/tmp/jed-ruby-native-mise-cache',
      'MISE_TRUSTED_CONFIG_PATHS' => ROOT, 'RUBY_NATIVE_SOURCE' => SOURCE, 'RUBYOPT' => nil,
      'RUBY_YJIT_ENABLE' => nil, 'RUBY_ZJIT_ENABLE' => nil, 'JED_NATIVE_COMPILATION' => 'immediate',
      'JED_NATIVE_QUERY_API' => api, 'JED_NATIVE_QUERY_MEMORY_API' => nil,
      'JED_NATIVE_STORAGE' => mode, 'JED_NATIVE_LOCKING' => mode == 'disk' ? 'exclusive' : 'auto',
      'JED_NATIVE_CORPUS_ROOT' => CORPUS, 'JED_CONFORMANCE_FILTER' => nil,
      'JED_NATIVE_MEMORY_BYTES' => '536870912', 'JED_NATIVE_ADDRESS_SPACE_BYTES' => '2147483648',
      'JED_NATIVE_FILE_TIMEOUT' => '10', 'JED_NATIVE_TIMEOUT' => '900',
      'JED_NATIVE_REPORT' => report_path, 'JED_NATIVE_RUN_REPORT' => bound_path
    }
    command = ['mise', 'run', 'ruby-native:bounded', '--', binary, "--#{jit}", 'impl/ruby-native/bin/conformance.rb']
    journal = { name: name, api: api, started_at: Time.now.utc.iso8601, command: command, environment: environment,
      source_before: pinned, runner_sha256: Digest::SHA256.file(__FILE__).hexdigest,
      ruby_executable_sha256: Digest::SHA256.file(binary).hexdigest }
    File.write(journal_path, JSON.pretty_generate(journal) + "\n")
    puts "Starting #{name}"
    $stdout.flush
    okay = system(environment, *command, chdir: ROOT)
    journal.merge!(finished_at: Time.now.utc.iso8601, command_success: okay, source_after: provenance)
    File.write(journal_path, JSON.pretty_generate(journal) + "\n")
    raise "corpus audit process failed: #{name}" unless okay
    raise 'source or harness changed during corpus audit' unless journal.fetch(:source_after) == pinned
    report = JSON.parse(File.read(report_path))
    bound = JSON.parse(File.read(bound_path))
    verify!(report, bound, mode, api, jit, binary, pinned)
    journal[:verified] = true
    journal[:records] = report.fetch('records')
    journal[:peak_worker_rss_bytes] = report.fetch('peak_worker_rss_bytes')
    File.write(journal_path, JSON.pretty_generate(journal) + "\n")
    puts "Verified #{name}: #{report.fetch('passed')}/#{report.fetch('records')}, zero failures"
  end

  def main
    options = { mode: 'all', api: 'query' }
    OptionParser.new do |parser|
      parser.on('--source-sha256 HASH') { |value| options[:source_sha256] = value }
      parser.on('--harness-sha256 HASH') { |value| options[:harness_sha256] = value }
      parser.on('--mode NAME', %w[memory disk all]) { |value| options[:mode] = value }
      parser.on('--api NAME', %w[query execute]) { |value| options[:api] = value }
      parser.on('--replace') { options[:replace] = true }
    end.parse!
    abort 'Specify pinned --source-sha256 and --harness-sha256' unless ARGV.empty? && %i[source_sha256 harness_sha256].all? { |key| options[key]&.match?(/\A[0-9a-f]{64}\z/) }
    pinned = provenance
    raise 'frozen source or harness does not match' unless pinned.values_at(:source_sha256, :conformance_harness_sha256) == options.values_at(:source_sha256, :harness_sha256)
    raise 'unexpected corpus file count' unless pinned.fetch(:corpus_files) == 679
    (options[:mode] == 'all' ? RUNTIMES.keys : [options[:mode]]).each { |mode| run(mode, options, pinned) }
  end
end

CursorConformanceAudits.main if $PROGRAM_NAME == __FILE__
