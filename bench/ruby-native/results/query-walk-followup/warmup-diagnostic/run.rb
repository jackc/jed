# frozen_string_literal: true

# Adaptive, separate queue. Never alters or replaces the 33 primary commands.
require 'optparse'
require 'time'
require_relative '../../../source'

module QueryWalkWarmupRuns
  ROOT = File.expand_path('../../../../..', __dir__)
  BASELINE_SOURCE = '/tmp/jed-query-shape-before/impl/ruby-native'
  BASELINE_HASH = 'd21f4edda6dc0a007fce8499c2200c334e5e1e242f0d1397617910a21cda7396'
  FINAL_HASH = 'bb56f9a0b9592def93f4e0fb24f23098439b1d657e77f2d5fdbfdd14eeafc17d'
  RUNTIME = File.join(ROOT, '.dev/ruby-native-runtimes/ruby-4.1.0-preview1/bin/ruby')
  OWN_FILES = %w[run.rb stats.rb evidence.rb short-plan.json long-plan.json].freeze
  module_function

  def write_json(path, value)
    File.write(path, JSON.generate(value) + "\n")
  end

  def validate_sources
    raise 'Baseline differs' unless NativeRubySource.digest(BASELINE_SOURCE) == BASELINE_HASH
    raise 'Final differs' unless NativeRubySource.digest(File.join(ROOT, 'impl/ruby-native')) == FINAL_HASH
  end

  def validate_plans
    original = File.binread(File.join(ROOT, 'bench/ruby-native/compare/multiway-plan.json'))
    raise 'Short plan must be an exact copy' unless File.binread(File.join(__dir__, 'short-plan.json')) == original
    expected = JSON.parse(original)
    target = expected.fetch('workloads').find { |job| job.fetch('name') == 'three_table_join' }
    raise 'Original target schedule differs' unless target.values_at('warmup', 'iterations') == [100, 20]
    target['warmup'] = 1000
    raise 'Long plan differs beyond target warmup' unless JSON.parse(File.read(File.join(__dir__, 'long-plan.json'))) == expected
  end

  def validate_pins
    validate_sources
    validate_plans
    main = JSON.parse(File.read(File.join(__dir__, '../source-pins.json')))
    main.fetch('files').each do |path, hash|
      raise "Primary helper changed: #{path}" unless Digest::SHA256.file(File.join(ROOT, path)).hexdigest == hash
    end
    raise 'Preview runtime changed' unless Digest::SHA256.file(RUNTIME).hexdigest == main.fetch('runtime_files').fetch(RUNTIME)
    pins = JSON.parse(File.read(File.join(__dir__, 'pins.json')))
    OWN_FILES.each do |name|
      raise "Adaptive helper changed: #{name}" unless Digest::SHA256.file(File.join(__dir__, name)).hexdigest == pins.fetch('files').fetch(name)
    end
  end

  def pins
    path = File.join(__dir__, 'pins.json')
    raise 'Pins already exist' if File.exist?(path)
    validate_sources
    validate_plans
    main = JSON.parse(File.read(File.join(__dir__, '../source-pins.json')))
    write_json(path, {
      schema: 1, captured_at: Time.now.utc.iso8601(6), purpose: 'adaptive_warmup_followup',
      baseline_source: BASELINE_SOURCE, baseline_source_sha256: BASELINE_HASH,
      final_source: File.join(ROOT, 'impl/ruby-native'), final_source_sha256: FINAL_HASH,
      runtime: RUNTIME, runtime_sha256: Digest::SHA256.file(RUNTIME).hexdigest,
      primary_source_pins: main,
      files: OWN_FILES.to_h { |name| [name, Digest::SHA256.file(File.join(__dir__, name)).hexdigest] }
    })
    validate_pins
  end

  def run(name, command, source, artifact, extra_env = {})
    validate_pins
    raise "Output exists: #{artifact}" if File.exist?(artifact)
    environment = @environment.merge('RUBY_NATIVE_SOURCE' => source).merge(extra_env)
    before = NativeRubySource.digest(source)
    started = Time.now.utc.iso8601(6)
    puts "Starting adaptive #{name}"
    $stdout.flush
    success = system(environment, *command, chdir: ROOT)
    after = NativeRubySource.digest(source)
    @commands << {
      name: name, started_at: started, finished_at: Time.now.utc.iso8601(6),
      command: command, environment: environment, source: source,
      source_sha256: before, final_source_sha256: after, success: success,
      artifact: artifact
    }
    write_json(@journal, { schema: 1, phase: 'adaptive_warmup', script_sha256: Digest::SHA256.file(__FILE__).hexdigest,
      pins_sha256: Digest::SHA256.file(File.join(__dir__, 'pins.json')).hexdigest, commands: @commands })
    raise "Failed #{name}" unless success && before == after
    validate_pins
  end

  def all
    @journal = File.join(__dir__, 'run-all.json')
    raise 'Journal already exists' if File.exist?(@journal)
    @commands = []
    %w[short long].each do |condition|
      %w[1-before 2-final 3-final 4-before].each do |state|
        source = state.end_with?('before') ? BASELINE_SOURCE : File.join(ROOT, 'impl/ruby-native')
        name = "#{condition}-#{state}"
        artifact = File.join(__dir__, "#{name}.json")
        command = ['mise', 'run', 'ruby-native:binding-compare', '--', '--ruby', RUNTIME, '--jit', 'zjit',
          '--native-source', source, '--lanes', 'native-prepared',
          '--plan', File.join(__dir__, "#{condition}-plan.json"), '--output', artifact]
        run(name, command, source, artifact)
      end
    end
    %w[short long].each do |condition|
      %w[before final].each do |state|
        source = state == 'before' ? BASELINE_SOURCE : File.join(ROOT, 'impl/ruby-native')
        name = "stats-#{condition}-#{state}"
        artifact = File.join(__dir__, "#{name}.json")
        bounded = File.join(__dir__, "#{name}-bounded.json")
        raise "Bounded report exists: #{bounded}" if File.exist?(bounded)
        command = ['mise', 'run', 'ruby-native:bounded', '--', RUNTIME, '--zjit', '--zjit-stats-quiet',
          File.join(__dir__, 'stats.rb'), '--plan', File.join(__dir__, "#{condition}-plan.json"), '--report', artifact]
        run(name, command, source, artifact, 'JED_NATIVE_RUN_REPORT' => bounded)
      end
    end
  end

  def main
    phase = nil
    OptionParser.new { |parser| parser.on('--phase NAME', %w[pins all]) { |value| phase = value } }.parse!
    abort 'Specify --phase pins|all' unless phase && ARGV.empty?
    @environment = {
      'MISE_STATE_DIR' => '/tmp/jed-ruby-native-mise-state',
      'MISE_CACHE_DIR' => '/tmp/jed-ruby-native-mise-cache', 'MISE_TRUSTED_CONFIG_PATHS' => ROOT,
      'RUBYOPT' => nil, 'RUBY_YJIT_ENABLE' => nil, 'RUBY_ZJIT_ENABLE' => nil,
      'JED_NATIVE_MEMORY_BYTES' => (512 * 1024**2).to_s,
      'JED_NATIVE_ADDRESS_SPACE_BYTES' => (2 * 1024**3).to_s,
      'JED_NATIVE_TIMEOUT' => '300', 'JED_NATIVE_RUN_REPORT' => nil
    }
    public_send(phase)
  end
end

QueryWalkWarmupRuns.main if $PROGRAM_NAME == __FILE__
