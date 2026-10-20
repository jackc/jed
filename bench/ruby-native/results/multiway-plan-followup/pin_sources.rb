# frozen_string_literal: true

# Capture before timing; the verifier later rejects any change to these files.
require 'json'
require 'digest'
require 'time'
require 'open3'
require_relative '../../source'

module MultiwayPlanSourcePins
  ROOT = File.expand_path('../../../..', __dir__)
  FILES = %w[
    bench/ruby-native/source.rb
    bench/ruby-native/binding_compare.rb
    bench/ruby-native/binding_compare_lane.rb
    bench/ruby-native/compare.rb
    bench/ruby-native/query_profile.rb
    bench/ruby-native/multiway_plan_diagnostics.rb
    impl/ruby-native/tools/bounded_process.rb
    impl/ruby-native/tools/run_bounded.rb
    bench/ruby-native/compare/multiway-plan.json
    bench/ruby-native/results/multiway-plan-followup/run.rb
  ].freeze
  module_function

  def node_executable
    output, status = Open3.capture2e({
      'MISE_STATE_DIR' => '/tmp/jed-ruby-native-mise-state',
      'MISE_CACHE_DIR' => '/tmp/jed-ruby-native-mise-cache',
      'MISE_TRUSTED_CONFIG_PATHS' => ROOT
    }, 'mise', 'which', 'node', chdir: ROOT)
    raise "unable to resolve Node executable: #{output}" unless status.success?
    File.realpath(output.strip)
  end

  def current
    { 'source_sha256' => NativeRubySource.digest(File.join(ROOT, 'impl/ruby-native')),
      'baseline_source_sha256' => NativeRubySource.digest('/tmp/jed-multiway-plan-before/impl/ruby-native'),
      'files' => (FILES + Dir['bench/ruby-native/compare/go/*', 'bench/ruby-native/compare/rust/src/**/*.rs',
        'bench/ruby-native/compare/rust/Cargo.*', 'bench/ruby-native/compare/lane.rb', 'bench/ruby-native/compare/lane.ts']).uniq.sort.to_h do |path|
          [path, Digest::SHA256.file(File.join(ROOT, path)).hexdigest]
        end,
      'runtime_files' => ['/home/jack/.local/share/mise/installs/ruby/4.0.7/bin/ruby',
        File.join(ROOT, '.dev/ruby-native-runtimes/ruby-4.1.0-preview1/bin/ruby'), node_executable].to_h do |path|
          [path, Digest::SHA256.file(path).hexdigest]
        end }
  end

  def main
    path = File.join(__dir__, 'source-pins.json')
    raise 'source pins already exist; preserve them' if File.exist?(path)
    File.write(path, JSON.pretty_generate(current.merge('captured_at' => Time.now.utc.iso8601)) + "\n")
  end
end

MultiwayPlanSourcePins.main if $PROGRAM_NAME == __FILE__
