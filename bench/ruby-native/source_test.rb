# frozen_string_literal: true

require 'tmpdir'
require 'fileutils'
require_relative 'source'
require_relative 'file_compare'

def assert_source_error(fragment)
  begin
    yield
  rescue RuntimeError => error
    raise "wrong source error: #{error.message}" unless error.message.include?(fragment)
    return
  end
  raise "expected source error: #{fragment}"
end

original_source = ENV.delete('RUBY_NATIVE_SOURCE')
original_baseline = ENV.delete('RUBY_NATIVE_BASELINE_SOURCE')
begin
  Dir.mktmpdir('jed-ruby-source-test-') do |root|
    current = File.join(root, 'impl/ruby-native')
    FileUtils.mkdir_p(File.join(current, 'lib'))
    File.write(File.join(current, 'lib/jed_native.rb'), "# test source\n")
    raise 'current source default changed' unless NativeRubySource.resolve(root, phase: 'current') == current
    NativeRubySource::ARCHIVED_PHASES.each do |phase|
      # Even an existing local directory is not evidence of the archived source.
      FileUtils.mkdir_p(File.join(root, ".dev/ruby-native-#{phase}"))
      assert_source_error('requires RUBY_NATIVE_SOURCE') { NativeRubySource.resolve(root, phase: phase) }
      assert_source_error('baseline source checksum differs') do
        NativeRubySource.resolve(root, phase: phase, source: current)
      end
    end
    ENV['RUBY_NATIVE_SOURCE'] = current
    raise 'explicit current source ignored' unless NativeRubySource.resolve('/missing', phase: 'current') == current
    assert_source_error('RUBY_NATIVE_BASELINE_SOURCE') { NativeRubyFileCompare.baseline_source }
    ENV['RUBY_NATIVE_BASELINE_SOURCE'] = current
    assert_source_error('baseline source checksum differs') { NativeRubyFileCompare.baseline_source }
    raise 'baseline lookup changed current source' unless ENV['RUBY_NATIVE_SOURCE'] == current
    assert_source_error('Ruby source is missing') do
      NativeRubySource.resolve(root, phase: 'current', source: File.join(root, 'absent'))
    end
  end
  raise 'historical file lanes selected by default' if NativeRubyFileCompare::DEFAULT_LANES.any? { |lane| lane.start_with?('baseline-') }
  raise 'historical file lanes unavailable' unless %w[baseline-ruby baseline-yjit baseline-zjit].all? { |lane| NativeRubyFileCompare::ALLOWED_LANES.include?(lane) }
ensure
  original_source ? ENV['RUBY_NATIVE_SOURCE'] = original_source : ENV.delete('RUBY_NATIVE_SOURCE')
  original_baseline ? ENV['RUBY_NATIVE_BASELINE_SOURCE'] = original_baseline : ENV.delete('RUBY_NATIVE_BASELINE_SOURCE')
end

puts 'Ruby benchmark source selection: current defaults and explicit historical provenance passed'
