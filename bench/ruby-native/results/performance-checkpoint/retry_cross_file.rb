# frozen_string_literal: true

# Preserve the original three successful lanes and the failed Rust preparation.
# Build memory and measured-query memory are deliberately separate limits.
require 'json'
require 'time'
require 'optparse'
require_relative '../../source'

root = File.expand_path('../../../..', __dir__)
options = {}
OptionParser.new { |parser| parser.on('--final-source-sha256 HASH') { |value| options[:source] = value } }.parse!
abort 'Specify the frozen final source hash' unless ARGV.empty? && options[:source]&.match?(/\A[0-9a-f]{64}\z/)
journal = File.join(__dir__, 'run-cross-file-retry.json')
artifact = File.join(__dir__, 'cross-file-rust-retry.json')
build_report = File.join(__dir__, 'cross-file-rust-prebuild-bounded.json')
raise 'retry artifacts already exist; preserve them' if [journal, artifact, build_report].any? { |path| File.exist?(path) }
original = JSON.parse(File.read(File.join(__dir__, 'cross-file.json')))
failure = original.fetch('statuses').fetch('rust')
raise 'unexpected original preparation failure' unless failure.fetch('status') == 'unavailable' &&
  failure.fetch('detail').include?('worker memory limit exceeded (536870912 bytes)') &&
  original.fetch('results').length == 9 && original.fetch('results').all? { |row| row.fetch('status') == 'completed' }
source = File.join(root, 'impl/ruby-native')
commands = []
environment = { 'MISE_STATE_DIR' => '/tmp/jed-ruby-native-mise-state',
  'MISE_CACHE_DIR' => '/tmp/jed-ruby-native-mise-cache', 'MISE_TRUSTED_CONFIG_PATHS' => root,
  'RUBYOPT' => nil, 'RUBY_YJIT_ENABLE' => nil, 'RUBY_ZJIT_ENABLE' => nil,
  'RUBY_NATIVE_SOURCE' => source, 'TMPDIR' => '/tmp',
  'JED_NATIVE_RUN_REPORT' => nil }
run = lambda do |name, command, extra|
  before = NativeRubySource.digest(source)
  raise 'source changed' unless before == options[:source]
  env = environment.merge(extra)
  started = Time.now.utc.iso8601
  puts "Starting #{name}"
  $stdout.flush
  okay = system(env, *command, chdir: root)
  after = NativeRubySource.digest(source)
  commands << { name: name, started_at: started, finished_at: Time.now.utc.iso8601,
    source: source, source_sha256: before, final_source_sha256: after,
    command: command, environment: env, success: okay }
  File.write(journal, JSON.pretty_generate(
    phase: 'cross-file-retry', script_sha256: Digest::SHA256.file(__FILE__).hexdigest,
    reason: 'The original Rust adapter build exceeded the query-sized 512 MiB cap. Prebuild the unchanged adapter offline with 1 GiB RSS, then retain the original 512 MiB cap for all measured queries.',
    original_report: 'cross-file.json', original_journal: 'run-cross.json',
    commands: commands) + "\n")
  raise "failed #{name}/source changed" unless okay && before == after
end
run.call('cross-file-rust-prebuild', ['mise', 'run', 'ruby-native:bounded', '--',
  'cargo', 'build', '--release', '--offline', '--manifest-path',
  File.join(root, '.dev/ruby-native-file-compare/rust/Cargo.toml'), '--bin', 'bench-jed'],
  { 'CARGO_TARGET_DIR' => '/tmp/jed-ruby-native-compare/rust-target', 'CARGO_BUILD_JOBS' => '1',
    'JED_NATIVE_MEMORY_BYTES' => (1024**3).to_s, 'JED_NATIVE_ADDRESS_SPACE_BYTES' => (16 * 1024**3).to_s,
    'JED_NATIVE_TIMEOUT' => '600', 'JED_NATIVE_RUN_REPORT' => build_report })
# Run the unchanged orchestrator directly through its task, preserving /tmp as
# the adapter-cache parent. Each preparation/query still has its own supervisor.
run.call('cross-file-rust-retry', ['mise', 'run', 'ruby-native:file-compare', '--',
  '--run', '--lanes', 'rust', '--workloads', 'point_lookup_pk,cold_open_pk_small,insert_commit_durable',
  '--timeout', '600', '--report', artifact], {})
