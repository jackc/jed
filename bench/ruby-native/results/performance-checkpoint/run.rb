# frozen_string_literal: true

# Explicit experiment only. Every worker is bounded and every phase is serial.
# Do not put this driver under a 2 GiB address-space hard limit: file/cross-core
# orchestration needs 16 GiB, while each Ruby engine worker keeps its 2 GiB cap.
require 'json'
require 'optparse'
require 'fileutils'
require 'time'
require_relative '../../source'

module PerformanceCheckpointRuns
  ROOT = File.expand_path('../../../..', __dir__)
  BASELINE = '3e749562c6b764681d6917604cf02aa73b3194cfdbb8c5e756616588de91dc88'
  RUNTIMES = {
    '4.0-none' => ['/home/jack/.local/share/mise/installs/ruby/4.0.7/bin/ruby', 'none'],
    '4.0-yjit' => ['/home/jack/.local/share/mise/installs/ruby/4.0.7/bin/ruby', 'yjit'],
    '4.1-zjit' => [File.join(ROOT, '.dev/ruby-native-runtimes/ruby-4.1.0-preview1/bin/ruby'), 'zjit']
  }.freeze
  PLANS = { 'memory' => 'checkpoint-plan.json', 'host' => 'checkpoint-host-plan.json',
    'blocking' => 'blocking-plan.json' }.transform_values { |name| File.join(ROOT, 'bench/ruby-native/compare', name) }.freeze
  NATIVE = %w[native-prepared native-query native-eager].freeze
  STATES = %w[1-before 2-final 3-final 4-before].freeze
  FILE_JOBS = %w[point_lookup_pk cold_open_pk_small insert_commit_durable].freeze
  module_function

  def path(name) = File.join(__dir__, "#{name}.json")

  def run(name, command, source:, extra_env: {}, artifact: path(name))
    raise "output already exists: #{artifact}; use --replace to rerun" if File.exist?(artifact) && !@options[:replace]
    before = NativeRubySource.digest(source)
    expected = source == @options[:baseline_source] ? BASELINE : @options[:final_hash]
    raise 'source differs from frozen hash' unless before == expected
    environment = @environment.merge('RUBY_NATIVE_SOURCE' => source).merge(extra_env)
    started = Time.now.utc.iso8601
    puts "Starting #{name} (#{before})"
    $stdout.flush
    okay = system(environment, *command, chdir: ROOT)
    after = NativeRubySource.digest(source)
    @commands << { name: name, started_at: started, finished_at: Time.now.utc.iso8601,
      source: source, source_sha256: before, final_source_sha256: after, command: command,
      environment: environment, success: okay }
    save_commands
    raise "failed #{name}" unless okay && before == after
  end

  def save_commands
    File.write(path("run-#{@options[:phase]}"), JSON.pretty_generate(
      phase: @options[:phase], options: @options, script_sha256: Digest::SHA256.file(__FILE__).hexdigest,
      plans: PLANS.transform_values { |plan| Digest::SHA256.file(plan).hexdigest }, commands: @commands) + "\n")
  end

  def binding(name, runtime, source, plan: nil, lanes: NATIVE, file: false, verify: false)
    binary, jit = RUNTIMES.fetch(runtime)
    command = ['mise', 'run', 'ruby-native:binding-compare', '--', '--ruby', binary, '--jit', jit,
      '--native-source', source, '--lanes', lanes.join(','), '--output', path(name)]
    command += ['--plan', PLANS.fetch(plan)] if plan
    command += ['--file', File.join(ROOT, '.dev/ruby-native-bench-data/large.jed')] if file
    command << '--verify-only' if verify
    run(name, command, source: source)
  end

  def blocking(name, runtime, source, verify: false)
    binary, jit = RUNTIMES.fetch(runtime)
    flags = jit == 'none' ? %w[--disable-yjit --disable-zjit] : ["--#{jit}"]
    command = ['mise', 'run', 'ruby-native:bounded', '--', binary, *flags,
      'bench/ruby-native/blocking_diagnostics.rb', '--jit', jit, '--api', 'all', '--report', path(name)]
    command << '--verify-only' if verify
    run(name, command, source: source, extra_env: {
      'JED_NATIVE_TIMEOUT' => '600', 'JED_NATIVE_RUN_REPORT' => path("#{name}-bounded") })
  end

  def files(name, runtime, source, cross: false)
    binary, jit = RUNTIMES.fetch(runtime)
    lane = jit == 'none' ? 'ruby' : "ruby-#{jit}"
    lanes = cross ? "#{lane},go,rust,ts" : lane
    # Runtime selection comes from the orchestration Ruby (RbConfig.ruby).
    # Its children still apply the original file harness's per-engine limits.
    command = ['mise', 'run', 'ruby-native:bounded', '--', binary, '--disable-yjit', '--disable-zjit',
      'bench/ruby-native/file_compare.rb', '--run', '--lanes', lanes,
      '--workloads', FILE_JOBS.join(','), '--timeout', '600', '--report', path(name)]
    run(name, command, source: source, extra_env: {
      'JED_NATIVE_ADDRESS_SPACE_BYTES' => (16 * 1024**3).to_s,
      'JED_NATIVE_TIMEOUT' => cross ? '1800' : '900', 'JED_NATIVE_RUN_REPORT' => path("#{name}-bounded") })
  end

  def controls(kind)
    STATES.each do |state|
      source = state.end_with?('before') ? @options[:baseline_source] : @source
      name = "#{kind}-4.0-yjit-#{state}"
      case kind
      when 'memory', 'host' then binding(name, '4.0-yjit', source, plan: kind)
      when 'file' then files(name, '4.0-yjit', source)
      when 'blocking' then blocking(name, '4.0-yjit', source)
      end
    end
  end

  def characterize
    %w[4.0-none 4.1-zjit].each do |runtime|
      %w[memory host].each { |kind| binding("#{kind}-#{runtime}-final", runtime, @source, plan: kind) }
      files("file-#{runtime}-final", runtime, @source)
      blocking("blocking-#{runtime}-final", runtime, @source)
    end
  end

  def actual_binding
    %w[memory host].each do |kind|
      binding("binding-#{kind}-4.0-yjit", '4.0-yjit', @source, plan: kind, lanes: ['wrapper', *NATIVE])
    end
    binding('binding-file-4.0-yjit', '4.0-yjit', @source, lanes: ['wrapper', *NATIVE], file: true)
  end

  def cross
    phase = 'performance-checkpoint-cross-core'
    run('cross-memory', ['mise', 'run', 'ruby-native:compare'], source: @source,
      extra_env: { 'RUBY_NATIVE_LANES' => 'ruby-yjit,go,rust,ts', 'RUBY_NATIVE_PHASE' => phase,
        'RUBY_NATIVE_COMPARE_PLAN' => PLANS.fetch('memory'), 'RUBY_NATIVE_COMPARE_TIMEOUT' => '600',
        'RUBY_NATIVE_COMPARE_RUBY_ADDRESS_SPACE_BYTES' => (2 * 1024**3).to_s })
    FileUtils.mv(File.join(ROOT, "bench/ruby-native/results/#{phase}.json"), path('cross-memory'))
    files('cross-file', '4.0-yjit', @source, cross: true)
  end

  def profile
    run('final-profile', ['mise', 'run', 'ruby-native:bounded', '--', RUNTIMES.fetch('4.0-yjit').first,
      '--yjit', 'bench/ruby-native/query_profile.rb', '--api', 'execute', '--report', path('final-profile')],
      source: @source, extra_env: { 'RUBY_NATIVE_COMPARE_PLAN' => PLANS.fetch('memory'),
        'JED_NATIVE_RUN_REPORT' => path('final-profile-bounded') })
  end

  def verify
    %w[before final].each do |state|
      source = state == 'before' ? @options[:baseline_source] : @source
      %w[memory host].each do |kind|
        lanes = state == 'before' ? ['wrapper', *NATIVE] : NATIVE
        binding("verify-#{kind}-#{state}", '4.0-yjit', source, plan: kind, lanes: lanes, verify: true)
      end
    end
    blocking('verify-blocking-final', '4.0-yjit', @source, verify: true)
  end

  def main
    @options = { baseline_source: '/tmp/jed-method-sharing-before/impl/ruby-native' }
    phases = %w[verify memory-controls host-controls file-controls blocking-controls characterize binding cross profile]
    OptionParser.new do |parser|
      parser.banner = 'Explicit serial checkpoint: run.rb --phase PHASE --final-source-sha256 HASH'
      parser.on('--phase NAME', phases) { |value| @options[:phase] = value }
      parser.on('--final-source-sha256 HASH') { |value| @options[:final_hash] = value }
      parser.on('--baseline-source PATH') { |value| @options[:baseline_source] = File.expand_path(value) }
      parser.on('--replace') { @options[:replace] = true }
    end.parse!
    abort 'Specify phase and final frozen source hash' unless ARGV.empty? && @options[:phase] && @options[:final_hash]&.match?(/\A[0-9a-f]{64}\z/)
    @source = File.join(ROOT, 'impl/ruby-native')
    raise 'baseline archive differs' unless NativeRubySource.digest(@options[:baseline_source]) == BASELINE
    raise 'current source differs' unless NativeRubySource.digest(@source) == @options[:final_hash]
    @environment = { 'MISE_STATE_DIR' => '/tmp/jed-ruby-native-mise-state',
      'MISE_CACHE_DIR' => '/tmp/jed-ruby-native-mise-cache', 'MISE_TRUSTED_CONFIG_PATHS' => ROOT,
      'RUBYOPT' => nil, 'RUBY_YJIT_ENABLE' => nil, 'RUBY_ZJIT_ENABLE' => nil,
      'JED_NATIVE_MEMORY_BYTES' => (512 * 1024**2).to_s,
      'JED_NATIVE_ADDRESS_SPACE_BYTES' => (2 * 1024**3).to_s, 'JED_NATIVE_TIMEOUT' => '300',
      'JED_NATIVE_RUN_REPORT' => nil }
    @commands = []
    phase = @options.fetch(:phase)
    case phase
    when /\A(memory|host|file|blocking)-controls\z/ then controls(Regexp.last_match(1))
    when 'characterize' then characterize
    when 'binding' then actual_binding
    when 'cross' then cross
    when 'profile' then profile
    when 'verify' then verify
    end
  end
end

PerformanceCheckpointRuns.main if $PROGRAM_NAME == __FILE__
