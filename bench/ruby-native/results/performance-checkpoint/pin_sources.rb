# frozen_string_literal: true

# Capture before timing; the verifier later rejects any change to these files.
require 'json'
require 'digest'
require 'time'
require_relative '../../source'

module CheckpointSourcePins
  ROOT = File.expand_path('../../../..', __dir__)
  FILES = %w[
    bench/ruby-native/source.rb
    bench/ruby-native/binding_compare.rb
    bench/ruby-native/binding_compare_lane.rb
    bench/ruby-native/compare.rb
    bench/ruby-native/file_compare.rb
    bench/ruby-native/bench_jed.rb
    bench/ruby-native/blocking_diagnostics.rb
    bench/ruby-native/query_profile.rb
    bench/ruby/lib/bench.rb
    impl/ruby-native/tools/bounded_process.rb
    impl/ruby-native/tools/run_bounded.rb
    bench/ruby-native/compare/checkpoint-plan.json
    bench/ruby-native/compare/checkpoint-host-plan.json
    bench/ruby-native/compare/blocking-plan.json
    bench/ruby-native/results/performance-checkpoint/run.rb
  ].freeze
  module_function

  def current
    { 'source_sha256' => NativeRubySource.digest(File.join(ROOT, 'impl/ruby-native')),
      'baseline_source_sha256' => NativeRubySource.digest('/tmp/jed-method-sharing-before/impl/ruby-native'),
      'files' => FILES.to_h { |path| [path, Digest::SHA256.file(File.join(ROOT, path)).hexdigest] } }
  end

  def main
    path = File.join(__dir__, 'source-pins.json')
    raise 'source pins already exist; preserve them' if File.exist?(path)
    File.write(path, JSON.pretty_generate(current.merge('captured_at' => Time.now.utc.iso8601)) + "\n")
  end
end

CheckpointSourcePins.main if $PROGRAM_NAME == __FILE__
