# frozen_string_literal: true

# File-only permission retry. The first report and journal remain untouched.
require 'json'
require 'time'
require 'optparse'
require_relative '../../source'

root = File.expand_path('../../../..', __dir__)
options = {}
OptionParser.new do |parser|
  parser.on('--final-source-sha256 HASH') { |value| options[:source] = value }
end.parse!
abort 'Specify the frozen final source hash' unless ARGV.empty? && options[:source]&.match?(/\A[0-9a-f]{64}\z/)
name = 'binding-file-4.0-yjit-retry'
artifact = File.join(__dir__, "#{name}.json")
journal = File.join(__dir__, 'run-binding-file-retry.json')
raise 'retry artifacts already exist; preserve them' if File.exist?(artifact) || File.exist?(journal)
original = JSON.parse(File.read(File.join(__dir__, 'binding-file-4.0-yjit.json')))
raise 'original was not the expected socket failure' unless original.fetch('lanes').length == 4 && original.fetch('lanes').values.all? do |lane|
  !lane.fetch('success') && lane.fetch('reason') == 'worker exited 1' && lane.fetch('stderr').include?('Operation not permitted')
end
source = File.join(root, 'impl/ruby-native')
before = NativeRubySource.digest(source)
raise 'source changed' unless before == options[:source]
command = ['mise', 'run', 'ruby-native:binding-compare', '--', '--ruby',
  '/home/jack/.local/share/mise/installs/ruby/4.0.7/bin/ruby', '--jit', 'yjit', '--native-source', source,
  '--lanes', 'wrapper,native-prepared,native-query,native-eager', '--output', artifact,
  '--file', File.join(root, '.dev/ruby-native-bench-data/large.jed')]
environment = { 'MISE_STATE_DIR' => '/tmp/jed-ruby-native-mise-state',
  'MISE_CACHE_DIR' => '/tmp/jed-ruby-native-mise-cache', 'MISE_TRUSTED_CONFIG_PATHS' => root,
  'RUBYOPT' => nil, 'RUBY_YJIT_ENABLE' => nil, 'RUBY_ZJIT_ENABLE' => nil,
  'RUBY_NATIVE_SOURCE' => source, 'JED_NATIVE_MEMORY_BYTES' => (512 * 1024**2).to_s,
  'JED_NATIVE_ADDRESS_SPACE_BYTES' => (2 * 1024**3).to_s, 'JED_NATIVE_TIMEOUT' => '300',
  'JED_NATIVE_RUN_REPORT' => nil }
started = Time.now.utc.iso8601
okay = system(environment, *command, chdir: root)
after = NativeRubySource.digest(source)
File.write(journal, JSON.pretty_generate(
  phase: 'binding-file-retry', script_sha256: Digest::SHA256.file(__FILE__).hexdigest,
  reason: 'The default sandbox denied local page-service Unix socket creation; this unchanged file-only worker was rerun with explicit escalation.',
  original_report: 'binding-file-4.0-yjit.json', original_journal: 'run-binding.json',
  commands: [{ name: name, started_at: started, finished_at: Time.now.utc.iso8601,
    source: source, source_sha256: before, final_source_sha256: after,
    command: command, environment: environment, success: okay }]) + "\n")
raise 'file-only permission retry failed/source changed' unless okay && before == after
