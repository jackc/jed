# frozen_string_literal: true

# Explicit serial queue. Each worker owns its resource guard. Invoke this driver
# directly, or with at least 16 GiB AS; supported-core workers need that reserve.
require 'json'
require 'optparse'
require 'fileutils'
require 'time'
require_relative '../../source'

module JoinBoundsRuns
  ROOT = File.expand_path('../../../..', __dir__)
  BASELINE_COMMIT = '25a1a7cce500266ac6f665ea920496e746382102'
  BASELINE = '405f5536969b71c80fa30bd45d962bda33af642b0185039d76e6443005c35336'
  PLAN = File.join(ROOT, 'bench/ruby-native/compare/join-functions-plan.json')
  PLAN_SHA256 = '559d1c076c6b143b209000d04688fd23238b8c15f6165339d19bcaf763b10fc6'
  ORIGINAL_PLAN = File.join(ROOT, 'bench/ruby-native/compare/join-predicate-plan.json')
  ORIGINAL_PLAN_SHA256 = '75b2215366db0658b8fc22f38ebe205f80e3d67caa744e8d0f59f76a94d159cb'
  RUNTIMES = {
    '4.0-none' => ['/home/jack/.local/share/mise/installs/ruby/4.0.7/bin/ruby', 'none'],
    '4.0-yjit' => ['/home/jack/.local/share/mise/installs/ruby/4.0.7/bin/ruby', 'yjit'],
    '4.1-zjit' => [File.join(ROOT, '.dev/ruby-native-runtimes/ruby-4.1.0-preview1/bin/ruby'), 'zjit']
  }.freeze
  PINNED_FILES = %w[
    bench/ruby-native/source.rb
    bench/ruby-native/binding_compare.rb
    bench/ruby-native/binding_compare_lane.rb
    bench/ruby-native/compare.rb
    bench/ruby-native/query_profile.rb
    bench/ruby-native/join_bounds_diagnostics.rb
    bench/ruby-native/join_functions_diagnostics.rb
    impl/ruby-native/tools/bounded_process.rb
    impl/ruby-native/tools/run_bounded.rb
    bench/ruby-native/compare/join-predicate-plan.json
    bench/ruby-native/compare/join-functions-plan.json
    bench/ruby-native/results/join-bounds-followup/run.rb
  ].freeze
  module_function

  def write_json(path, value)
    File.write(path, JSON.generate(value) + "\n")
  end

  def flags(jit)
    jit == 'none' ? %w[--disable-yjit --disable-zjit] : ["--#{jit}"]
  end

  def pins
    path = File.join(__dir__, 'source-pins.json')
    raise 'source pins already exist; preserve prior evidence' if File.exist?(path)
    output, status = Open3.capture2e(@environment, 'mise', 'which', 'node', chdir: ROOT)
    raise "could not resolve Node executable: #{output}" unless status.success?
    files = PINNED_FILES + Dir.chdir(ROOT) do
      Dir['bench/ruby-native/compare/go/*', 'bench/ruby-native/compare/rust/src/**/*.rs',
        'bench/ruby-native/compare/rust/Cargo.*', 'bench/ruby-native/compare/lane.rb',
        'bench/ruby-native/compare/lane.ts']
    end
    write_json(path, {
      captured_at: Time.now.utc.iso8601, source_sha256: NativeRubySource.digest(@source),
      baseline_commit: BASELINE_COMMIT, baseline_source: @options[:baseline_source],
      baseline_source_sha256: NativeRubySource.digest(@options[:baseline_source]),
      files: files.uniq.sort.to_h { |file| [file, Digest::SHA256.file(File.join(ROOT, file)).hexdigest] },
      runtime_files: (RUNTIMES.values.map(&:first) + [File.realpath(output.strip)]).uniq.to_h do |file|
        [file, Digest::SHA256.file(file).hexdigest]
      end
    })
  end

  def run(name, command, source:, artifact:, extra_env: {})
    raise "output exists: #{artifact}; preserve it before an intentional rerun" if File.exist?(artifact)
    before = NativeRubySource.digest(source)
    expected = source == @options[:baseline_source] ? BASELINE : @options[:final_hash]
    raise 'source differs from frozen revision' unless before == expected
    environment = @environment.merge('RUBY_NATIVE_SOURCE' => source).merge(extra_env)
    started = Time.now.utc.iso8601
    puts "Starting #{name} (#{before})"
    $stdout.flush
    success = system(environment, *command, chdir: ROOT)
    after = NativeRubySource.digest(source)
    @commands << { name: name, started_at: started, finished_at: Time.now.utc.iso8601,
      source: source, source_sha256: before, final_source_sha256: after,
      command: command, environment: environment, success: success }
    write_json(@journal, { phase: @options[:phase], options: @options,
      script_sha256: Digest::SHA256.file(__FILE__).hexdigest,
      plan_sha256: Digest::SHA256.file(PLAN).hexdigest, commands: @commands })
    raise "failed #{name}" unless success && before == after
  end

  def binding(name, runtime, source, lanes, verify: false)
    binary, jit = RUNTIMES.fetch(runtime)
    artifact = File.join(__dir__, "#{name}.json")
    command = ['mise', 'run', 'ruby-native:binding-compare', '--', '--ruby', binary, '--jit', jit,
      '--native-source', source, '--lanes', lanes.join(','), '--plan', PLAN, '--output', artifact]
    command << '--verify-only' if verify
    run(name, command, source: source, artifact: artifact)
  end

  def verify
    %w[before final].each do |state|
      source = state == 'before' ? @options[:baseline_source] : @source
      binding("verify-#{state}", '4.0-yjit', source,
        %w[wrapper native-prepared native-query native-eager], verify: true)
    end
  end

  def diagnostics
    @runtimes.each do |runtime|
      binary, jit = RUNTIMES.fetch(runtime)
      %w[before final].each do |state|
        source = state == 'before' ? @options[:baseline_source] : @source
        name = "diagnostic-#{state}-#{runtime}"
        artifact = File.join(__dir__, "#{name}.json")
        run(name, ['mise', 'run', 'ruby-native:bounded', '--', binary, *flags(jit),
          'bench/ruby-native/join_bounds_diagnostics.rb', '--jit', jit, '--report', artifact],
          source: source, artifact: artifact,
          extra_env: { 'JED_NATIVE_RUN_REPORT' => File.join(__dir__, "#{name}-bounded.json") })
      end
    end
  end

  def controls
    @runtimes.each do |runtime|
      %w[1-before 2-final 3-final 4-before].each do |state|
        source = state.end_with?('before') ? @options[:baseline_source] : @source
        binding("control-#{runtime}-#{state}", runtime, source,
          %w[native-prepared native-query native-eager])
      end
    end
  end

  def cross
    phase = 'join-bounds-cross-core'
    artifact = File.join(__dir__, 'cross-core.json')
    run('cross-core', ['mise', 'run', 'ruby-native:compare'], source: @source, artifact: artifact,
      extra_env: { 'RUBY_NATIVE_LANES' => 'ruby-yjit,go,rust,ts', 'RUBY_NATIVE_PHASE' => phase,
        'RUBY_NATIVE_COMPARE_PLAN' => PLAN, 'RUBY_NATIVE_COMPARE_TIMEOUT' => '300',
        'RUBY_NATIVE_COMPARE_RUBY_ADDRESS_SPACE_BYTES' => (2 * 1024**3).to_s })
    FileUtils.mv(File.join(ROOT, "bench/ruby-native/results/#{phase}.json"), artifact)
  end

  def actual_binding
    binding('binding-4.0-yjit', '4.0-yjit', @source,
      %w[wrapper native-prepared native-query native-eager])
  end

  def profiles
    %w[before final].each do |state|
      source = state == 'before' ? @options[:baseline_source] : @source
      name = "#{state}-profile"
      artifact = File.join(__dir__, "#{name}.json")
      run(name, ['mise', 'run', 'ruby-native:bounded', '--', RUNTIMES.fetch('4.0-yjit').first,
        '--yjit', 'bench/ruby-native/query_profile.rb', '--api', 'execute', '--report', artifact],
        source: source, artifact: artifact,
        extra_env: { 'RUBY_NATIVE_COMPARE_PLAN' => PLAN,
          'JED_NATIVE_RUN_REPORT' => File.join(__dir__, "#{name}-bounded.json") })
    end
  end

  def main
    @options = { baseline_source: '/tmp/jed-join-bounds-before/impl/ruby-native' }
    OptionParser.new do |parser|
      parser.banner = 'Opt-in serial queue: run.rb --phase PHASE --final-source-sha256 HASH'
      parser.on('--phase NAME', %w[pins verify diagnostics controls cross binding profiles]) { |value| @options[:phase] = value }
      parser.on('--final-source-sha256 HASH') { |value| @options[:final_hash] = value }
      parser.on('--baseline-source PATH') { |value| @options[:baseline_source] = File.expand_path(value) }
      parser.on('--runtime NAME', RUNTIMES.keys) { |value| @options[:runtime] = value }
    end.parse!
    abort 'Specify a phase and frozen source hash' unless ARGV.empty? && @options[:phase] &&
      @options[:final_hash]&.match?(/\A[0-9a-f]{64}\z/)
    @source = File.join(ROOT, 'impl/ruby-native')
    raise 'baseline differs' unless NativeRubySource.digest(@options[:baseline_source]) == BASELINE
    raise 'the selected plan changed' unless Digest::SHA256.file(PLAN).hexdigest == PLAN_SHA256
    original = JSON.parse(File.read(ORIGINAL_PLAN))
    current = JSON.parse(File.read(PLAN))
    raise 'baseline workloads changed' unless Digest::SHA256.file(ORIGINAL_PLAN).hexdigest == ORIGINAL_PLAN_SHA256 &&
      current.fetch('workloads').first(10) == original.fetch('workloads') &&
      current.values_at('rows', 'repetitions', 'validation_iterations') == original.values_at('rows', 'repetitions', 'validation_iterations')
    raise 'current source differs' unless NativeRubySource.digest(@source) == @options[:final_hash]
    @runtimes = @options[:runtime] ? [@options[:runtime]] : RUNTIMES.keys
    @environment = { 'MISE_STATE_DIR' => '/tmp/jed-ruby-native-mise-state',
      'MISE_CACHE_DIR' => '/tmp/jed-ruby-native-mise-cache', 'MISE_TRUSTED_CONFIG_PATHS' => ROOT,
      'RUBYOPT' => nil, 'RUBY_YJIT_ENABLE' => nil, 'RUBY_ZJIT_ENABLE' => nil,
      'JED_NATIVE_MEMORY_BYTES' => (512 * 1024**2).to_s,
      'JED_NATIVE_ADDRESS_SPACE_BYTES' => (2 * 1024**3).to_s,
      'JED_NATIVE_TIMEOUT' => '300', 'JED_NATIVE_RUN_REPORT' => nil }
    return pins if @options[:phase] == 'pins'
    suffix = @options[:runtime] ? "-#{@options[:runtime]}" : ''
    @journal = File.join(__dir__, "run-#{@options[:phase]}#{suffix}.json")
    raise "journal exists: #{@journal}; preserve prior evidence" if File.exist?(@journal)
    @commands = []
    public_send(@options[:phase] == 'binding' ? :actual_binding : @options[:phase])
  end
end

JoinBoundsRuns.main if $PROGRAM_NAME == __FILE__
