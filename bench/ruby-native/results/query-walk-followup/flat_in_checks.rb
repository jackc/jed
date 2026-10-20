# frozen_string_literal: true

# Opt-in regression evidence for the planner's deep flat-IN expansion.
# Each child has its own process-group guard; supported binaries are checked
# against the prior independently verified build, without rebuilding them.
require 'json'
require 'digest'
require 'time'
require 'optparse'
require_relative '../../source'

module FlatInChecks
  ROOT = File.expand_path('../../../..', __dir__)
  CORPUS = 'spec/conformance/suites/expr/in_list.test'
  PRIOR = 'bench/ruby-native/results/physical-plan-followup/statistics-supported.json'
  BASELINE = 'd21f4edda6dc0a007fce8499c2200c334e5e1e242f0d1397617910a21cda7396'
  SOURCE = File.join(ROOT, 'impl/ruby-native')
  STABLE = '/home/jack/.local/share/mise/installs/ruby/4.0.7/bin/ruby'
  PREVIEW = File.join(ROOT, '.dev/ruby-native-runtimes/ruby-4.1.0-preview1/bin/ruby')
  RUNTIMES = { 'interpreter' => [STABLE, '--disable-yjit', '--disable-zjit'],
    'yjit' => [STABLE, '--yjit'], 'zjit' => [PREVIEW, '--zjit'] }.freeze
  module_function

  def sha(path) = Digest::SHA256.file(path).hexdigest
  def read(path) = JSON.parse(File.read(path))

  def inputs
    prior = read(File.join(ROOT, PRIOR)).fetch('inputs')
    digest = Digest::SHA256.new
    Dir[File.join(ROOT, 'impl/rust/src/**/*.rs'), File.join(ROOT, 'impl/go/**/*.go'),
      File.join(ROOT, 'impl/ts/src/**/*.ts')].sort.each do |path|
      digest << path.delete_prefix(ROOT + '/') << "\0" << File.binread(path)
    end
    raise 'supported engine source changed' unless digest.hexdigest == prior.fetch('supported_core_sources_sha256')
    prior.fetch('binaries').each_value { |data| raise 'supported binary changed' unless sha(data.fetch('path')) == data.fetch('sha256') }
    prior.fetch('rust_linked_artifacts').each { |path, hash| raise 'Rust linked artifact changed' unless sha(path) == hash }
    records = File.read(File.join(ROOT, CORPUS)).scan(/^(?:statement|query) /).length
    raise 'shared regression record count changed' unless records == 26
    { source_sha256: NativeRubySource.digest(SOURCE), corpus: CORPUS, corpus_sha256: sha(File.join(ROOT, CORPUS)),
      records: records, script_sha256: sha(__FILE__), supported_core_sources_sha256: digest.hexdigest,
      binaries: prior.fetch('binaries'), rust_linked_artifacts: prior.fetch('rust_linked_artifacts'),
      prior_build_evidence: PRIOR, prior_build_evidence_sha256: sha(File.join(ROOT, PRIOR)),
      runtimes: RUNTIMES.values.map(&:first).uniq.to_h { |path| [path, sha(path)] },
      files: %w[impl/ruby-native/bin/conformance.rb impl/ruby-native/test/query_walks_test.rb
        impl/ruby-native/tools/bounded_process.rb impl/ruby-native/tools/run_bounded.rb
        spec/conformance/params.rb scripts/oracle_import.rb scripts/lib/pg_oracle.rb
        scripts/lib/oracle_overrides.rb spec/conformance/oracle_profile.toml].to_h { |path| [path, sha(File.join(ROOT, path))] } }
  end

  def run(name, child, extra = {}, address_space: 2 * 1024**3, timeout: 180)
    bound_path = File.join(__dir__, "#{name}-bounded.json")
    raise "existing evidence #{bound_path}" if File.exist?(bound_path)
    environment = {
      'MISE_STATE_DIR' => '/tmp/jed-ruby-native-mise-state',
      'MISE_CACHE_DIR' => '/tmp/jed-ruby-native-mise-cache', 'MISE_TRUSTED_CONFIG_PATHS' => ROOT,
      'RUBY_NATIVE_SOURCE' => SOURCE, 'RUBYOPT' => nil, 'RUBY_YJIT_ENABLE' => nil, 'RUBY_ZJIT_ENABLE' => nil,
      'JED_NATIVE_MEMORY_BYTES' => '536870912', 'JED_NATIVE_ADDRESS_SPACE_BYTES' => address_space.to_s,
      'JED_NATIVE_TIMEOUT' => timeout.to_s, 'JED_NATIVE_RUN_REPORT' => bound_path,
      'JED_CONFORMANCE_FILTER' => nil, 'JED_NATIVE_REPORT' => nil, 'JED_NATIVE_QUERY_MEMORY_API' => nil
    }.merge(extra)
    entry = { name: name, command: ['mise', 'run', 'ruby-native:bounded', '--', *child],
      environment: environment, started_at: Time.now.utc.iso8601(6) }
    @journal[:commands] << entry
    save
    puts "Starting #{name}"
    $stdout.flush
    okay = system(environment, *entry[:command], chdir: ROOT)
    entry.merge!(finished_at: Time.now.utc.iso8601(6), success: okay)
    save
    bound = read(bound_path)
    raise "#{name} failed" unless okay && bound.fetch('success') && bound.fetch('reason').nil?
    raise 'worker command/bounds changed' unless bound.values_at('command', 'memory_bytes', 'address_space_bytes', 'timeout_seconds') ==
      [child, 536870912, address_space, timeout.to_f]
    raise 'worker source changed' unless bound.values_at('source_sha256', 'final_source_sha256') == [@pinned[:source_sha256]] * 2
    raise 'worker exceeded RSS allowance' unless bound.fetch('peak_rss_bytes').between?(0, 536870912)
    raise 'regression inputs changed' unless inputs == @pinned
    bound
  end

  def save = File.write(@output, JSON.generate(@journal) + "\n")

  def main
    options = {}
    OptionParser.new do |parser|
      parser.on('--phase NAME', %w[native supported oracle]) { |value| options[:phase] = value }
      parser.on('--source-sha256 HASH') { |value| options[:source] = value }
    end.parse!
    raise 'specify phase and frozen source hash' unless ARGV.empty? && options[:phase] && options[:source]
    @pinned = inputs
    raise 'Ruby source differs' unless @pinned[:source_sha256] == options[:source]
    @output = File.join(__dir__, "flat-in-#{options[:phase]}.json")
    raise 'existing journal' if File.exist?(@output)
    @journal = { phase: options[:phase], inputs: @pinned, commands: [] }
    save
    case options[:phase]
    when 'native'
      RUNTIMES.each do |name, runtime|
        %w[memory disk].each do |mode|
          artifact = File.join(__dir__, "flat-in-ruby-#{name}-#{mode}.json")
          raise 'existing corpus artifact' if File.exist?(artifact)
          run("flat-in-ruby-#{name}-#{mode}", [*runtime, 'impl/ruby-native/bin/conformance.rb', 'expr/in_list.test'], {
            'JED_NATIVE_STORAGE' => mode, 'JED_NATIVE_QUERY_API' => 'query',
            'JED_NATIVE_LOCKING' => 'exclusive', 'JED_NATIVE_COMPILATION' => 'immediate',
            'JED_NATIVE_REPORT' => artifact, 'JED_NATIVE_FILE_TIMEOUT' => '10' })
          result = read(artifact)
          raise 'Ruby regression failed' unless result.values_at('files', 'passed', 'records', 'query_api_records', 'execute_api_records') ==
            [1, 26, 26, 26, 0] && result.fetch('failures').empty?
        end
      end
    when 'supported'
      @pinned[:binaries].each do |core, data|
        %w[memory disk].each do |mode|
          child = [data.fetch('path')]
          child << 'impl/ts/src/bin/conformance.ts' if core == 'ts'
          child << 'disk' if mode == 'disk'
          result = run("flat-in-#{core}-#{mode}", child, { 'JED_CONFORMANCE_FILTER' => 'expr/in_list.test' },
            address_space: (core == 'ts' && mode == 'disk' ? 64 : 16) * 1024**3, timeout: 90)
          raise 'supported corpus did not pass' unless result.fetch('stdout').include?("PASS expr/in_list.test\n") &&
            result.fetch('stdout').include?("[#{mode}] 1 passed, 0 failed, 0 skipped")
        end
      end
    when 'oracle'
      result = run('flat-in-oracle', ['mise', 'run', 'corpus:check', CORPUS])
      raise 'oracle output differs' unless result.fetch('stdout') ==
        "OK: #{CORPUS} regenerated byte-identically from the PostgreSQL oracle\n"
    end
    @journal.merge!(final_inputs: inputs, verified: true)
    save
    puts "Flat-IN #{options[:phase]} checks passed."
  end
end

FlatInChecks.main if $PROGRAM_NAME == __FILE__
