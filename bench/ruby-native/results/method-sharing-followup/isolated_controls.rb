# frozen_string_literal: true

# Follow-up to selected YJIT shifts in the main seven-workload experiment.
# Retain the original; run the selected three jobs in fresh processes with identical guards.
require_relative 'run'

module MethodSharingRuns
  def self.isolated_controls_main
    @options = { phase: 'isolated-controls', baseline_source: '/tmp/jed-method-sharing-before/impl/ruby-native',
      isolation_driver_sha256: Digest::SHA256.file(__FILE__).hexdigest }
    OptionParser.new do |parser|
      parser.banner = 'Explicit isolated control: isolated_controls.rb --final-source-sha256 HASH'
      parser.on('--final-source-sha256 HASH') { |value| @options[:final_hash] = value }
      parser.on('--baseline-source PATH') { |value| @options[:baseline_source] = File.expand_path(value) }
      parser.on('--replace') { @options[:replace] = true }
    end.parse!
    abort 'Specify the final frozen source hash' unless ARGV.empty? && @options[:final_hash]&.match?(/\A[0-9a-f]{64}\z/)
    @source = File.join(ROOT, 'impl/ruby-native')
    raise 'baseline archive differs' unless NativeRubySource.digest(@options[:baseline_source]) == BASELINE
    raise 'current source differs' unless NativeRubySource.digest(@source) == @options[:final_hash]
    @workloads = 'scalar,arithmetic_projection,text_like_fallback'
    @environment = { 'MISE_STATE_DIR' => '/tmp/jed-ruby-native-mise-state',
      'MISE_CACHE_DIR' => '/tmp/jed-ruby-native-mise-cache', 'MISE_TRUSTED_CONFIG_PATHS' => ROOT,
      'RUBYOPT' => nil, 'RUBY_YJIT_ENABLE' => nil, 'RUBY_ZJIT_ENABLE' => nil,
      'JED_NATIVE_MEMORY_BYTES' => (512 * 1024**2).to_s,
      'JED_NATIVE_ADDRESS_SPACE_BYTES' => (2 * 1024**3).to_s, 'JED_NATIVE_TIMEOUT' => '300',
      'JED_NATIVE_RUN_REPORT' => nil }
    @commands = []
    %w[4.0-yjit].each do |runtime|
      %w[1-before 2-final 3-final 4-before].each do |state|
        source = state.end_with?('before') ? @options[:baseline_source] : @source
        name = "isolated-control-#{runtime}-#{state}"
        artifact = File.join(__dir__, "#{name}.json")
        binary, jit = RUNTIMES.fetch(runtime)
        command = ['mise', 'run', 'ruby-native:binding-compare', '--', '--ruby', binary, '--jit', jit,
          '--native-source', source, '--lanes', 'native-prepared,native-query,native-eager',
          '--plan', PLAN, '--workloads', @workloads, '--output', artifact]
        run(name, command, source: source, artifact: artifact)
      end
    end
  end
end

MethodSharingRuns.isolated_controls_main if $PROGRAM_NAME == __FILE__
