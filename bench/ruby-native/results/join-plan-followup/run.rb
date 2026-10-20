# frozen_string_literal: true

# Explicit serial runner. Each measured worker has its own resource guard.
# Run directly, or give an outer guard at least 16 GiB AS: the cross-core Go/Rust
# workers need that address space and cannot raise an inherited hard limit.
require 'json'
require 'optparse'
require 'fileutils'
require 'time'
require_relative '../../source'

module JoinPlanRuns
  ROOT = File.expand_path('../../../..', __dir__)
  BASELINE = 'e430431f7e9264c3cc3b9a75436c46841004a707f9c9ec0b72744beeb4c27f88'
  RUNTIMES = {
    '4.0-none' => ['/home/jack/.local/share/mise/installs/ruby/4.0.7/bin/ruby', 'none'],
    '4.0-yjit' => ['/home/jack/.local/share/mise/installs/ruby/4.0.7/bin/ruby', 'yjit'],
    '4.1-zjit' => [File.join(ROOT, '.dev/ruby-native-runtimes/ruby-4.1.0-preview1/bin/ruby'), 'zjit']
  }.freeze
  PLAN = File.join(ROOT, 'bench/ruby-native/compare/join-plan.json')
  module_function

  def run(name, arguments, source:, extra_env: {}, artifact: nil)
    raise "output already exists: #{artifact}; use --replace to rerun" if artifact && File.exist?(artifact) && !@options[:replace]
    before = NativeRubySource.digest(source)
    expected = source == @options[:baseline_source] ? BASELINE : @options[:final_hash]
    raise 'selected source differs from frozen hash' unless before == expected
    environment = @environment.merge('RUBY_NATIVE_SOURCE' => source).merge(extra_env)
    started = Time.now.utc.iso8601
    puts "Starting #{name} (#{before})"
    $stdout.flush
    okay = system(environment, *arguments, chdir: ROOT)
    after = NativeRubySource.digest(source)
    @commands << { name: name, started_at: started, finished_at: Time.now.utc.iso8601,
      source: source, source_sha256: before, final_source_sha256: after,
      command: arguments, environment: environment, success: okay }
    save_commands
    raise "failed #{name}" unless okay && before == after
  end

  def save_commands
    suffix = @options[:runtime] ? "-#{@options[:runtime]}" : ''
    File.write(File.join(__dir__, "run-#{@options[:phase]}#{suffix}.json"), JSON.pretty_generate(
      phase: @options[:phase], options: @options, script_sha256: Digest::SHA256.file(__FILE__).hexdigest,
      plan_sha256: Digest::SHA256.file(PLAN).hexdigest, commands: @commands) + "\n")
  end

  def binding(name, runtime, source, lanes, verify: false)
    binary, jit = RUNTIMES.fetch(runtime)
    artifact = File.join(__dir__, "#{name}.json")
    command = ['mise', 'run', 'ruby-native:binding-compare', '--', '--ruby', binary, '--jit', jit,
      '--native-source', source, '--lanes', lanes.join(','), '--plan', PLAN, '--output', artifact]
    command << '--verify-only' if verify
    run(name, command, source: source, artifact: artifact)
  end

  def controls
    @runtimes.each do |runtime|
      %w[1-before 2-final 3-final 4-before].each do |state|
        source = state.end_with?('before') ? @options[:baseline_source] : @source
        binding("control-#{runtime}-#{state}", runtime, source, %w[native-prepared native-query native-eager])
      end
    end
  end

  def profiles
    %w[before final].each do |state|
      source = state == 'before' ? @options[:baseline_source] : @source
      name = "#{state}-profile"
      artifact = File.join(__dir__, "#{name}.json")
      command = ['mise', 'run', 'ruby-native:bounded', '--', RUNTIMES.fetch('4.0-yjit').first, '--yjit',
        'bench/ruby-native/query_profile.rb', '--api', 'execute', '--report', artifact]
      run(name, command, source: source, artifact: artifact,
        extra_env: { 'RUBY_NATIVE_COMPARE_PLAN' => PLAN, 'JED_NATIVE_RUN_REPORT' => File.join(__dir__, "#{name}-bounded.json") })
    end
  end

  def cross
    artifact = File.join(__dir__, 'cross-core.json')
    phase = 'join-plan-cross-core'
    run('cross-core', ['mise', 'run', 'ruby-native:compare'], source: @source, artifact: artifact,
      extra_env: { 'RUBY_NATIVE_LANES' => 'ruby-yjit,go,rust,ts', 'RUBY_NATIVE_PHASE' => phase,
        'RUBY_NATIVE_COMPARE_PLAN' => PLAN, 'RUBY_NATIVE_COMPARE_TIMEOUT' => '300',
        'RUBY_NATIVE_COMPARE_RUBY_ADDRESS_SPACE_BYTES' => (2 * 1024**3).to_s })
    FileUtils.mv(File.join(ROOT, "bench/ruby-native/results/#{phase}.json"), artifact)
  end

  def actual_binding
    binding('binding-4.0-yjit', '4.0-yjit', @source, %w[wrapper native-prepared native-query native-eager])
  end

  def verify
    %w[before final].each do |state|
      source = state == 'before' ? @options[:baseline_source] : @source
      binding("verify-#{state}", '4.0-yjit', source, %w[wrapper native-prepared native-query native-eager], verify: true)
    end
  end

  def diagnostics
    @runtimes.each do |runtime|
      binary, jit = RUNTIMES.fetch(runtime)
      flags = jit == 'none' ? %w[--disable-yjit --disable-zjit] : ["--#{jit}"]
      %w[before final].each do |state|
        source = state == 'before' ? @options[:baseline_source] : @source
        name = "diagnostic-#{state}-#{runtime}"
        artifact = File.join(__dir__, "#{name}.json")
        command = ['mise', 'run', 'ruby-native:bounded', '--', binary, *flags,
          'bench/ruby-native/join_plan_diagnostics.rb', '--jit', jit, '--report', artifact]
        run(name, command, source: source, artifact: artifact,
          extra_env: { 'JED_NATIVE_RUN_REPORT' => File.join(__dir__, "#{name}-bounded.json") })
      end
    end
  end

  def main
    @options = { baseline_source: '/tmp/jed-join-plan-before/impl/ruby-native' }
    OptionParser.new do |parser|
      parser.banner = 'Explicit serial measurements: run.rb --phase PHASE --final-source-sha256 HASH [--runtime NAME]'
      parser.on('--phase NAME', %w[verify diagnostics controls profiles cross binding]) { |value| @options[:phase] = value }
      parser.on('--final-source-sha256 HASH') { |value| @options[:final_hash] = value }
      parser.on('--baseline-source PATH') { |value| @options[:baseline_source] = File.expand_path(value) }
      parser.on('--runtime NAME', RUNTIMES.keys) { |value| @options[:runtime] = value }
      parser.on('--replace') { @options[:replace] = true }
    end.parse!
    abort 'Specify phase and final frozen source hash' unless ARGV.empty? && @options[:phase] && @options[:final_hash]&.match?(/\A[0-9a-f]{64}\z/)
    @source = File.join(ROOT, 'impl/ruby-native')
    raise 'baseline archive differs' unless NativeRubySource.digest(@options[:baseline_source]) == BASELINE
    raise 'current source differs' unless NativeRubySource.digest(@source) == @options[:final_hash]
    @runtimes = @options[:runtime] ? [@options[:runtime]] : RUNTIMES.keys
    @environment = { 'MISE_STATE_DIR' => '/tmp/jed-ruby-native-mise-state',
      'MISE_CACHE_DIR' => '/tmp/jed-ruby-native-mise-cache', 'MISE_TRUSTED_CONFIG_PATHS' => ROOT,
      'RUBYOPT' => nil, 'RUBY_YJIT_ENABLE' => nil, 'RUBY_ZJIT_ENABLE' => nil,
      'JED_NATIVE_MEMORY_BYTES' => (512 * 1024**2).to_s,
      'JED_NATIVE_ADDRESS_SPACE_BYTES' => (2 * 1024**3).to_s, 'JED_NATIVE_TIMEOUT' => '300',
      'JED_NATIVE_RUN_REPORT' => nil }
    @commands = []
    case @options[:phase]
    when 'controls' then controls
    when 'profiles' then profiles
    when 'cross' then cross
    when 'binding' then actual_binding
    when 'verify' then verify
    when 'diagnostics' then diagnostics
    end
  end
end

JoinPlanRuns.main if $PROGRAM_NAME == __FILE__
