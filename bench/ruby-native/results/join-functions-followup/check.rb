# frozen_string_literal: true

# Explicit correctness checks for this frozen experiment only. Every child is
# run through mise with enforced process-group limits. Existing reports are
# never replaced; a failed attempt remains reviewable.
require 'json'
require 'digest'
require 'optparse'
require 'time'
require_relative '../../source'

module JoinFunctionsChecks
  ROOT = File.expand_path('../../../..', __dir__)
  SOURCE = File.join(ROOT, 'impl/ruby-native')
  CORPUS = File.join(ROOT, 'spec/conformance/suites')
  STABLE = '/home/jack/.local/share/mise/installs/ruby/4.0.7/bin/ruby'
  PREVIEW = File.join(ROOT, '.dev/ruby-native-runtimes/ruby-4.1.0-preview1/bin/ruby')
  FOCUS = 'impl/ruby-native/test/prepared_join_functions_test.rb'
  FOCUS_FILES = %w[impl/ruby-native/test/prepared_join_plan_test.rb
    impl/ruby-native/test/prepared_multiway_plan_test.rb
    impl/ruby-native/test/prepared_physical_plan_test.rb
    impl/ruby-native/test/prepared_outer_joins_test.rb].push(FOCUS).freeze
  FOCUS_PROGRAM = FOCUS_FILES.map { |path| "require_relative #{path.inspect}" }.join('; ').freeze
  HARNESS = %w[impl/ruby-native/bin/conformance.rb impl/ruby-native/bin/concurrency.rb
    impl/ruby-native/tools/bounded_process.rb impl/ruby-native/tools/run_bounded.rb spec/conformance/params.rb].freeze
  RECORDS = { 'memory' => 20_297, 'disk' => 18_280 }.freeze
  SQL_CALLS = { 'memory' => 20_117, 'disk' => 18_129 }.freeze
  RSS = 536_870_912
  module_function

  def read(path) = JSON.parse(File.read(path))
  def digest(path) = Digest::SHA256.file(path).hexdigest

  def named_digest(paths)
    value = Digest::SHA256.new
    paths.sort.each { |path| value << path.delete_prefix(ROOT + '/') << "\0" << File.binread(path) }
    value.hexdigest
  end

  def provenance
    corpus = Dir[File.join(CORPUS, '**/*.test')].sort
    tests = Dir[File.join(SOURCE, 'test/*_test.rb')].sort
    raise 'unexpected corpus/host file count' unless [corpus.length, tests.length] == [679, 69]
    skipped = %w[memory disk].to_h do |mode|
      paths = corpus.select do |path|
        contents = File.read(path)
        contents.match?(/^#\s*skip:\s*#{mode}\b/) ||
          mode == 'disk' && contents.match?(/^#\s*format:\s*concurrency/) &&
            contents.match?(/^#\s*requires:.*resource\.storage_memory\b/)
      end
      [mode, paths.map { |path| path.delete_prefix(ROOT + '/') }]
    end
    { 'source_sha256' => NativeRubySource.digest(SOURCE), 'runner_sha256' => digest(__FILE__),
      'harness_sha256' => HARNESS.to_h { |path| [path, digest(File.join(ROOT, path))] },
      'corpus_files' => corpus.length, 'corpus_sha256' => named_digest(corpus),
      'available_records' => corpus.sum { |path| File.read(path).scan(/^(?:statement|query) /).length },
      'authored_skip_paths' => skipped, 'host_files' => tests.length, 'host_sha256' => named_digest(tests),
      'focused_sha256' => FOCUS_FILES.to_h { |path| [path, digest(File.join(ROOT, path))] } }
  end

  def specifications(options)
    case options.fetch(:phase)
    when 'focus'
      [['interpreter', STABLE, %w[--disable-yjit --disable-zjit]], ['yjit', STABLE, ['--yjit']],
       ['zjit', PREVIEW, ['--zjit']]].map do |name, binary, flags|
        { name: "focused-#{name}", phase: 'focus', binary: binary,
          command: ['mise', 'run', 'ruby-native:bounded', '--', binary, *flags, '-e', FOCUS_PROGRAM],
          child: [binary, *flags, '-e', FOCUS_PROGRAM], timeout: 180, address_space: 1_073_741_824 }
      end
    when 'host'
      # The existing eight-thread sharing test reserves roughly 512 MiB of
      # inaccessible allocator mappings. See ../expression-walk-followup/host-address-space/probe-bounded.json.
      # Keep the resident-memory guard; allow the same virtual reserve as corpus.
      [{ name: 'host-tests', phase: 'host', binary: STABLE, command: ['mise', 'run', 'ruby-native:test'],
         timeout: 180, address_space: 2_147_483_648 }]
    when 'corpus'
      modes = options[:mode] ? [options[:mode]] : %w[memory disk]
      apis = options[:api] ? [options[:api]] : %w[query execute]
      modes.flat_map do |mode|
        binary, jit = mode == 'memory' ? [PREVIEW, 'zjit'] : [STABLE, 'yjit']
        apis.map do |api|
          child = [binary, "--#{jit}", 'impl/ruby-native/bin/conformance.rb']
          { name: "#{mode}-#{api == 'query' ? 'pure-query' : api}-#{jit}", phase: 'corpus', binary: binary,
            jit: jit, mode: mode, api: api, child: child,
            command: ['mise', 'run', 'ruby-native:bounded', '--', *child],
            timeout: 900, address_space: 2_147_483_648 }
        end
      end
    end
  end

  def environment(specification, bound_path, report_path)
    {
      'MISE_STATE_DIR' => '/tmp/jed-ruby-native-mise-state', 'MISE_CACHE_DIR' => '/tmp/jed-ruby-native-mise-cache',
      'MISE_TRUSTED_CONFIG_PATHS' => ROOT, 'RUBY_NATIVE_SOURCE' => SOURCE, 'RUBYOPT' => nil,
      'RUBY_YJIT_ENABLE' => nil, 'RUBY_ZJIT_ENABLE' => nil,
      'JED_NATIVE_MEMORY_BYTES' => RSS.to_s,
      'JED_NATIVE_ADDRESS_SPACE_BYTES' => specification.fetch(:address_space).to_s,
      'JED_NATIVE_TIMEOUT' => specification.fetch(:timeout).to_s, 'JED_NATIVE_FILE_TIMEOUT' => '10',
      'JED_NATIVE_RUN_REPORT' => bound_path,
      'JED_NATIVE_REPORT' => specification[:phase] == 'corpus' ? report_path : nil,
      'JED_NATIVE_COMPILATION' => specification[:phase] == 'corpus' ? 'immediate' : nil,
      'JED_NATIVE_QUERY_API' => specification[:api], 'JED_NATIVE_QUERY_MEMORY_API' => nil,
      'JED_NATIVE_STORAGE' => specification[:mode],
      'JED_NATIVE_LOCKING' => specification[:mode] == 'disk' ? 'exclusive' : 'auto',
      'JED_NATIVE_CORPUS_ROOT' => CORPUS, 'JED_CONFORMANCE_FILTER' => nil,
      'JED_PROCESS_CORES' => nil, 'JED_PROCESS_JOBS' => nil, 'JED_PROCESS_FILTER' => nil
    }
  end

  def verify!(specification, bound, report, pinned)
    raise 'bounded check failed' unless bound['success'] && bound['reason'].nil?
    raise 'bounded source changed' unless bound.values_at('source_sha256', 'final_source_sha256') == [pinned.fetch('source_sha256')] * 2
    raise 'bounded limits differ' unless bound.values_at('memory_bytes', 'address_space_bytes', 'timeout_seconds') ==
      [RSS, specification.fetch(:address_space), specification.fetch(:timeout).to_f]
    raise 'bounded RSS exceeds allowance' unless bound.fetch('peak_rss_bytes').between?(1, RSS)
    raise 'bounded supervising runtime differs' unless bound.fetch('runtime').start_with?('ruby 4.0.7 ')
    raise 'child command differs' if specification[:child] && bound.fetch('command') != specification.fetch(:child)
    case specification.fetch(:phase)
    when 'focus'
      raise 'focused assertions did not finish' unless bound.fetch('stdout').include?('ruby-native prepared join functions:') &&
        bound.fetch('stdout').include?('costs, mutation and ownership passed')
    when 'host'
      expected = ['ruby', '-Iimpl/ruby-native/lib', '-e', 'Dir["impl/ruby-native/test/*_test.rb"].sort.each { |f| require_relative f }']
      raise 'host check did not load every test' unless bound.fetch('command') == expected
      raise 'host supervisor unexpectedly used a JIT' if bound.fetch('runtime').match?(/\+(?:YJIT|ZJIT)/)
      raise 'host suite omitted focused assertions' unless bound.fetch('stdout').include?('ruby-native prepared join functions:')
    when 'corpus'
      mode, api = specification.values_at(:mode, :api)
      raise 'corpus source changed' unless report.values_at('source_sha256', 'final_source_sha256') == [pinned.fetch('source_sha256')] * 2
      raise 'corpus runtime differs' unless report.values_at('ruby_executable', 'jit') == specification.values_at(:binary, :jit)
      version = mode == 'memory' ? 'ruby 4.1.0preview1 ' : 'ruby 4.0.7 '
      raise 'corpus runtime version/flags differ' unless report.fetch('ruby').start_with?(version) &&
        report.fetch('ruby').include?("+#{specification.fetch(:jit).upcase}")
      raise 'corpus API/settings differ' unless report.values_at('mode', 'filter', 'query_api', 'query_memory_api', 'compilation') ==
        [mode, '', api, api, 'immediate']
      raise 'query collection is not external' if api == 'query' && report['query_collection'] != 'external'
      raise 'corpus check lost records/files' unless report['finished_at'] &&
        report.values_at('files', 'passed', 'records', 'available_records') ==
          [679, RECORDS.fetch(mode), RECORDS.fetch(mode), pinned.fetch('available_records')]
      raise 'corpus skipped unauthored files' unless report.fetch('skipped_files') == pinned.fetch('authored_skip_paths').fetch(mode).length
      raise 'corpus contains failures' unless report.fetch('failures').empty?
      %w[failed unsupported_files timed_out_files memory_limited_files omitted_failure_details materialized_memory_records].each do |key|
        raise "corpus #{key} is nonzero" unless report.fetch(key).zero?
      end
      other = api == 'query' ? 'execute' : 'query'
      raise 'corpus mixed APIs or dropped SQL calls' unless report.values_at("#{api}_api_records", "#{other}_api_records") == [SQL_CALLS.fetch(mode), 0]
      raise 'per-file limits differ' unless report.values_at('worker_memory_bytes', 'worker_address_space_bytes', 'worker_timeout_seconds') ==
        [RSS, 2_147_483_648, 10.0]
      raise 'worker RSS exceeds allowance' unless report.fetch('peak_worker_rss_bytes').between?(1, RSS)
      raise 'compiler coverage missing' unless report.fetch('compiled_text_snapshot_kernels').positive? && report.fetch('compiled_like_kernels').positive?
      printed = JSON.parse(bound.fetch('stdout').lines.last)
      raise 'corpus stdout and report differ' unless printed.all? { |key, value| report[key] == value }
    end
  end

  def run(specification, pinned)
    name = specification.fetch(:name)
    report_path, bound_path, journal_path = ["#{name}.json", "#{name}-bounded.json", "#{name}-run.json"].map { |file| File.join(__dir__, file) }
    raise "existing check outputs: #{name}" if [report_path, bound_path, journal_path].any? { |path| File.exist?(path) }
    raise 'source, tests or corpus changed before check' unless provenance == pinned
    env = environment(specification, bound_path, report_path)
    journal = { name: name, specification: specification, command: specification.fetch(:command), environment: env,
      started_at: Time.now.utc.iso8601(6), source_before: pinned,
      ruby_executable_sha256: digest(specification.fetch(:binary)) }
    File.write(journal_path, JSON.pretty_generate(journal) + "\n")
    puts "Starting #{name}"
    $stdout.flush
    okay = system(env, *specification.fetch(:command), chdir: ROOT)
    journal.merge!(finished_at: Time.now.utc.iso8601(6), command_success: okay, source_after: provenance)
    File.write(journal_path, JSON.pretty_generate(journal) + "\n")
    raise "check process failed: #{name}" unless okay
    raise 'source, tests or corpus changed during check' unless journal.fetch(:source_after) == pinned
    raise 'runtime executable changed during check' unless journal.fetch(:ruby_executable_sha256) == digest(specification.fetch(:binary))
    bound = read(bound_path)
    report = specification[:phase] == 'corpus' ? read(report_path) : nil
    verify!(specification, bound, report, pinned)
    journal[:verified] = true
    File.write(journal_path, JSON.pretty_generate(journal) + "\n")
    puts "Verified #{name}"
  end

  def main
    options = {}
    OptionParser.new do |parser|
      parser.on('--phase NAME', %w[focus host corpus]) { |value| options[:phase] = value }
      parser.on('--mode NAME', %w[memory disk]) { |value| options[:mode] = value }
      parser.on('--api NAME', %w[query execute]) { |value| options[:api] = value }
      parser.on('--source-sha256 HASH') { |value| options[:source] = value }
    end.parse!
    abort 'usage: check.rb --phase focus|host|corpus --source-sha256 HASH [--mode memory|disk --api query|execute]' unless
      ARGV.empty? && options[:phase] && options[:source]&.match?(/\A[0-9a-f]{64}\z/) &&
      (options[:phase] == 'corpus' || !options[:mode] && !options[:api])
    pinned = provenance
    raise 'current source differs from requested source' unless pinned.fetch('source_sha256') == options.fetch(:source)
    specifications(options).each { |specification| run(specification, pinned) }
  end
end

JoinFunctionsChecks.main if $PROGRAM_NAME == __FILE__
