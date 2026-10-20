# frozen_string_literal: true

# Explicit supported-core checks for the two shared regression files. No
# downloads/builds are performed, and every file/mode/core runs sequentially.
require 'json'
require 'digest'
require 'time'
require 'optparse'

module MultiwaySharedRegressions
  ROOT = File.expand_path('../../../..', __dir__)
  FILES = { 'binding' => 'params/binding.test', 'cast' => 'joins/safe_conjunct_pushdown.test' }.freeze
  BINARIES = {
    'rust' => '/tmp/jed-ruby-native-compare/rust-target/release/conformance',
    'go' => File.join(ROOT, '.dev/go-build/56/56739997b5a81efcea6614e7997ca562e50ee7fbbf2d74b9f463a829b0def834-d/conformance'),
    'ts' => '/home/jack/.local/share/mise/installs/node/26.10.0/bin/node'
  }.freeze
  module_function

  def sha(path) = Digest::SHA256.file(path).hexdigest

  def inputs
    paths = Dir[File.join(ROOT, 'impl/rust/src/**/*.rs'), File.join(ROOT, 'impl/go/**/*.go'), File.join(ROOT, 'impl/ts/src/**/*.ts')].sort
    digest = Digest::SHA256.new
    paths.each { |path| digest << path.delete_prefix(ROOT + '/') << "\0" << File.binread(path) }
    linked = '/tmp/jed-ruby-native-compare/rust-target/release/deps/libjed-331b188b721c2993'
    { 'core_sources_sha256' => digest.hexdigest,
      'rust_linked_artifacts' => %w[rlib rmeta].to_h { |extension| path = "#{linked}.#{extension}"; [path, sha(path)] },
      'rust_build_manifest_dir' => File.join(ROOT, 'impl/rust'),
      'binaries' => BINARIES.to_h { |core, path| [core, { 'path' => path, 'sha256' => sha(path) }] },
      'corpus' => FILES.to_h do |name, relative|
        path = File.join(ROOT, 'spec/conformance/suites', relative)
        [name, { 'path' => path.delete_prefix(ROOT + '/'), 'sha256' => sha(path),
          'records' => File.read(path).scan(/^(?:query|statement) /).length }]
      end,
      'runner_sha256' => sha(__FILE__),
      'bounded_helpers' => %w[bounded_process.rb run_bounded.rb].to_h do |name|
        path = File.join(ROOT, 'impl/ruby-native/tools', name)
        [path.delete_prefix(ROOT + '/'), sha(path)]
      end }
  end

  def main
    options = {}
    OptionParser.new { |parser| parser.on('--replace') { options[:replace] = true } }.parse!
    raise 'unexpected arguments' unless ARGV.empty?
    raise 'the outer process needs at least 64 GiB virtual address space for Node owner workers' if Process.getrlimit(Process::RLIMIT_AS).last < 64 * 1024**3
    output = File.join(__dir__, 'shared-regressions.json')
    raise 'existing shared regression evidence; use --replace to rerun' if File.exist?(output) && !options[:replace]
    pinned = inputs
    raise 'unexpected shared regression counts' unless pinned.fetch('corpus').transform_values { |item| item.fetch('records') } == { 'binding' => 72, 'cast' => 30 }
    report = { 'purpose' => 'Supported-core validation of the two changed shared regression files, separate from Ruby performance measurements.',
      'started_at' => Time.now.utc.iso8601, 'inputs' => pinned, 'commands' => [] }
    File.write(output, JSON.pretty_generate(report) + "\n")
    BINARIES.each do |core, binary|
      %w[memory disk].each do |mode|
        FILES.each do |name, relative|
          stem = "shared-#{core}-#{mode}-#{name}"
          bound_path = File.join(__dir__, "#{stem}-bounded.json")
          raise "existing #{stem}; use --replace to rerun" if File.exist?(bound_path) && !options[:replace]
          child = [binary]
          child << File.join(ROOT, 'impl/ts/src/bin/conformance.ts') if core == 'ts'
          child << 'disk' if mode == 'disk'
          command = ['mise', 'run', 'ruby-native:bounded', '--', *child]
          # A Node owner worker adds a V8 reservation: the measured process uses
          # about 18 GiB virtual space while remaining far below the RSS cap.
          address_space = core == 'ts' && mode == 'disk' ? 64 * 1024**3 : 16 * 1024**3
          environment = { 'MISE_STATE_DIR' => '/tmp/jed-ruby-native-mise-state', 'MISE_CACHE_DIR' => '/tmp/jed-ruby-native-mise-cache',
            'MISE_TRUSTED_CONFIG_PATHS' => ROOT, 'JED_CONFORMANCE_FILTER' => relative,
            'JED_NATIVE_MEMORY_BYTES' => '536870912', 'JED_NATIVE_ADDRESS_SPACE_BYTES' => address_space.to_s,
            'JED_NATIVE_TIMEOUT' => '90', 'JED_NATIVE_RUN_REPORT' => bound_path,
            'RUBYOPT' => nil, 'RUBY_YJIT_ENABLE' => nil, 'RUBY_ZJIT_ENABLE' => nil }
          entry = { 'core' => core, 'mode' => mode, 'file' => relative, 'command' => command,
            'environment' => environment, 'report' => File.basename(bound_path), 'started_at' => Time.now.utc.iso8601 }
          report.fetch('commands') << entry
          File.write(output, JSON.pretty_generate(report) + "\n")
          puts "Starting #{stem}"
          $stdout.flush
          okay = system(environment, *command, chdir: ROOT)
          bound = JSON.parse(File.read(bound_path))
          entry.merge!('finished_at' => Time.now.utc.iso8601, 'success' => okay && bound.fetch('success'))
          File.write(output, JSON.pretty_generate(report) + "\n")
          raise "shared regression failed #{stem}" unless entry.fetch('success') && bound.fetch('reason').nil?
          raise 'shared regression bounds differ' unless bound.values_at('memory_bytes', 'address_space_bytes', 'timeout_seconds') == [536870912, address_space, 90.0] &&
            bound.fetch('peak_rss_bytes').between?(0, 536870912)
          raise 'shared regression command differs' unless bound.fetch('command') == child
          raise 'shared regression file did not pass' unless bound.fetch('stdout').include?("PASS #{relative}\n") &&
            bound.fetch('stdout').include?("[#{mode}] 1 passed, 0 failed, 0 skipped")
          raise 'shared regression inputs changed' unless inputs == pinned
        end
      end
    end
    report.merge!('finished_at' => Time.now.utc.iso8601, 'final_inputs' => inputs, 'verified' => true)
    File.write(output, JSON.pretty_generate(report) + "\n")
    puts 'All 12 supported-core regression file/mode checks passed.'
  end
end

MultiwaySharedRegressions.main if $PROGRAM_NAME == __FILE__
