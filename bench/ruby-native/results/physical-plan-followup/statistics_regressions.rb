# frozen_string_literal: true

# Explicit supported-core and PostgreSQL checks of the appended statistics
# regression. Existing engine binaries are reused only when their source and
# artifact identities equal the prior independently validated build evidence.
require 'json'
require 'digest'
require 'time'
require 'optparse'
require_relative '../../source'

module PhysicalStatisticsRegressions
  ROOT = File.expand_path('../../../..', __dir__)
  CORPUS = 'spec/conformance/suites/query/statistics_estimates_p9.test'
  PRIOR = 'bench/ruby-native/results/multiway-plan-followup/shared-regressions.json'
  BINARIES = {
    'rust' => '/tmp/jed-ruby-native-compare/rust-target/release/conformance',
    'go' => File.join(ROOT, '.dev/go-build/56/56739997b5a81efcea6614e7997ca562e50ee7fbbf2d74b9f463a829b0def834-d/conformance'),
    'ts' => '/home/jack/.local/share/mise/installs/node/26.10.0/bin/node'
  }.freeze
  module_function

  def sha(path) = Digest::SHA256.file(path).hexdigest

  def records
    lines, output, index, params, cost = File.readlines(File.join(ROOT, CORPUS)), [], 0, nil, nil
    while index < lines.length
      line = lines[index]
      params = line if line.start_with?('# params:')
      cost = line if line.start_with?('# cost:')
      unless line.match?(/\A(?:statement|query) /)
        index += 1
        next
      end
      header, start, sql, expected = line, index + 1, [], []
      index += 1
      while index < lines.length && !lines[index].strip.empty? && !lines[index].start_with?('----')
        sql << lines[index]
        index += 1
      end
      if header.start_with?('query ')
        raise 'query separator missing' unless lines[index]&.start_with?('----')
        index += 1
        while index < lines.length && !lines[index].strip.empty?
          expected << lines[index]
          index += 1
        end
      end
      output << { header: header, line: start, params: params, cost: cost, sql: sql.join, expected: expected.join }
      params = cost = nil
    end
    output
  end

  def oracle_projection
    all = records
    setup = all.select do |record|
      record[:header].start_with?('statement ') && record[:sql].match?(/\A(?:CREATE TABLE p9_(?:left|right)\b|INSERT INTO p9_(?:left|right)\b|ANALYZE p9_(?:left|right)\b|CREATE INDEX p9_left_k\b)/)
    end
    values = all.select do |record|
      record[:header] == "query II nosort\n" && record[:sql] == "SELECT l.id, r.id FROM p9_left l JOIN p9_right r ON l.k = r.k WHERE l.k = $1 ORDER BY l.id, r.id\n"
    end
    raise 'oracle projection coverage differs' unless setup.length == 7 && values.map { |record| record[:params] } == ["# params: 0\n", "# params: 1\n", "# params: 2\n"]
    body = "# Exact values-only projection of #{CORPUS}.\n# EXPLAIN estimates and costs are jed-owned; only SQL results are PostgreSQL-oracle checked.\n\n"
    setup.each { |record| body << record[:header] << record[:sql] << "\n" }
    values.each { |record| body << record[:params] << record[:cost] << record[:header] << record[:sql] << "----\n" << record[:expected] << "\n" }
    body
  end

  def inputs
    paths = Dir[File.join(ROOT, 'impl/rust/src/**/*.rs'), File.join(ROOT, 'impl/go/**/*.go'), File.join(ROOT, 'impl/ts/src/**/*.ts')].sort
    digest = Digest::SHA256.new
    paths.each { |path| digest << path.delete_prefix(ROOT + '/') << "\0" << File.binread(path) }
    prior = JSON.parse(File.read(File.join(ROOT, PRIOR))).fetch('inputs')
    binaries = BINARIES.to_h { |core, path| [core, { 'path' => path, 'sha256' => sha(path) }] }
    raise 'supported engine source differs from validated build' unless prior.fetch('core_sources_sha256') == digest.hexdigest
    raise 'supported engine binaries differ from validated build' unless prior.fetch('binaries') == binaries
    prior.fetch('rust_linked_artifacts').each { |path, hash| raise 'Rust linked artifact changed' unless sha(path) == hash }
    raise 'unexpected shared regression count' unless records.length == 34
    { 'ruby_source_sha256' => NativeRubySource.digest(File.join(ROOT, 'impl/ruby-native')),
      'corpus' => { 'path' => CORPUS, 'sha256' => sha(File.join(ROOT, CORPUS)), 'records' => records.length },
      'supported_core_sources_sha256' => digest.hexdigest, 'binaries' => binaries,
      'rust_linked_artifacts' => prior.fetch('rust_linked_artifacts'),
      'prior_build_evidence' => PRIOR, 'prior_build_evidence_sha256' => sha(File.join(ROOT, PRIOR)),
      'runner_sha256' => sha(__FILE__),
      'helpers' => %w[impl/ruby-native/tools/bounded_process.rb impl/ruby-native/tools/run_bounded.rb
        scripts/oracle_import.rb scripts/lib/pg_oracle.rb scripts/lib/oracle_overrides.rb spec/conformance/oracle_profile.toml].to_h { |path| [path, sha(File.join(ROOT, path))] } }
  end

  def run_command(name, child, extra, address_space:, timeout:)
    bound_path = File.join(__dir__, "#{name}-bounded.json")
    raise "existing report #{bound_path}" if File.exist?(bound_path)
    env = { 'MISE_STATE_DIR' => '/tmp/jed-ruby-native-mise-state', 'MISE_CACHE_DIR' => '/tmp/jed-ruby-native-mise-cache',
      'MISE_TRUSTED_CONFIG_PATHS' => ROOT, 'JED_NATIVE_MEMORY_BYTES' => '536870912',
      'JED_NATIVE_ADDRESS_SPACE_BYTES' => address_space.to_s, 'JED_NATIVE_TIMEOUT' => timeout.to_s,
      'JED_NATIVE_RUN_REPORT' => bound_path, 'RUBYOPT' => nil, 'RUBY_YJIT_ENABLE' => nil, 'RUBY_ZJIT_ENABLE' => nil }.merge(extra)
    command = ['mise', 'run', 'ruby-native:bounded', '--', *child]
    entry = { name: name, command: command, environment: env, started_at: Time.now.utc.iso8601(6) }
    @report[:commands] << entry
    save
    puts "Starting #{name}"
    $stdout.flush
    okay = system(env, *command, chdir: ROOT)
    entry.merge!(finished_at: Time.now.utc.iso8601(6), success: okay)
    save
    bound = JSON.parse(File.read(bound_path))
    raise "#{name} failed" unless okay && bound.fetch('success') && bound.fetch('reason').nil?
    raise 'bounded source changed' unless bound.values_at('source_sha256', 'final_source_sha256') == [@pinned.fetch('ruby_source_sha256')] * 2
    raise 'bounds or command differ' unless bound.values_at('memory_bytes', 'address_space_bytes', 'timeout_seconds', 'command') == [536870912, address_space, timeout.to_f, child]
    raise 'resource cap exceeded' unless bound.fetch('peak_rss_bytes').between?(0, 536870912)
    raise 'regression inputs changed' unless inputs == @pinned
    bound
  end

  def save = File.write(@output, JSON.pretty_generate(@report) + "\n")

  def main
    options = {}
    OptionParser.new do |parser|
      parser.on('--phase NAME', %w[prepare supported oracle]) { |value| options[:phase] = value }
      parser.on('--source-sha256 HASH') { |value| options[:source] = value }
    end.parse!
    raise 'specify phase and source hash' unless ARGV.empty? && options[:phase] && options[:source]&.match?(/\A[0-9a-f]{64}\z/)
    projection = File.join(__dir__, 'statistics-oracle-values.test')
    if options[:phase] == 'prepare'
      raise 'oracle projection already exists' if File.exist?(projection)
      File.write(projection, oracle_projection)
      return
    end
    @pinned = inputs
    raise 'Ruby source differs' unless @pinned.fetch('ruby_source_sha256') == options[:source]
    @output = File.join(__dir__, "statistics-#{options[:phase]}.json")
    raise 'existing regression journal' if File.exist?(@output)
    @report = { phase: options[:phase], inputs: @pinned, started_at: Time.now.utc.iso8601(6), commands: [] }
    save
    if options[:phase] == 'supported'
      BINARIES.each do |core, binary|
        %w[memory disk].each do |mode|
          child = [binary]
          child << File.join(ROOT, 'impl/ts/src/bin/conformance.ts') if core == 'ts'
          child << 'disk' if mode == 'disk'
          bound = run_command("statistics-#{core}-#{mode}", child, { 'JED_CONFORMANCE_FILTER' => 'query/statistics_estimates_p9.test' },
            address_space: (core == 'ts' && mode == 'disk' ? 64 : 16) * 1024**3, timeout: 90)
          raise 'supported corpus file did not pass' unless bound.fetch('stdout').include?("PASS query/statistics_estimates_p9.test\n") &&
            bound.fetch('stdout').include?("[#{mode}] 1 passed, 0 failed, 0 skipped")
        end
      end
    else
      raise 'oracle projection differs from shared corpus' unless File.binread(projection) == oracle_projection
      @report[:projection] = { path: projection.delete_prefix(ROOT + '/'), sha256: sha(projection), setup_records: 7, value_records: 3 }
      child = ['mise', 'run', 'corpus:check', projection.delete_prefix(ROOT + '/')]
      bound = run_command('statistics-oracle-values', child, {}, address_space: 2 * 1024**3, timeout: 180)
      expected = "OK: #{projection.delete_prefix(ROOT + '/')} regenerated byte-identically from the PostgreSQL oracle\n"
      raise 'oracle results differ' unless bound.fetch('stdout') == expected
    end
    @report.merge!(finished_at: Time.now.utc.iso8601(6), final_inputs: inputs, verified: true)
    save
    puts "Statistics #{options[:phase]} checks passed."
  end
end

PhysicalStatisticsRegressions.main if $PROGRAM_NAME == __FILE__
