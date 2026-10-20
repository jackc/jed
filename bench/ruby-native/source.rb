# frozen_string_literal: true

require 'digest'
# Historical evidence lives on the original experiment branch (ARCHIVE.md).
# Never substitute current source when a historical baseline was requested.
module NativeRubySource
  ARCHIVED_SOURCES = {
    'baseline' => '140356387cf9d419505ab7282050d94bb0ec5a46541f87fd866313a084c7a032',
    'working-baseline' => 'd14527fb00cb90489d6eae07fc59fbf7d0a5309ce2e7bae03c7b34f6713008fd'
  }.freeze
  ARCHIVED_PHASES = ARCHIVED_SOURCES.keys.freeze
  module_function

  def digest(path)
    Digest::SHA256.hexdigest(Dir[File.join(path, 'lib/**/*.rb')].sort.map { |file| File.binread(file) }.join)
  end

  def verify_baseline(path, phase = 'baseline')
    expected = ARCHIVED_SOURCES.fetch(phase)
    actual = digest(path)
    raise "baseline source checksum differs: #{actual}, expected #{expected}" unless actual == expected
  end

  def resolve(root, phase:, source: ENV['RUBY_NATIVE_SOURCE'])
    if source
      source = File.expand_path(source)
    elsif ARCHIVED_PHASES.include?(phase)
      raise "Historical phase #{phase} requires RUBY_NATIVE_SOURCE; see bench/ruby-native/ARCHIVE.md"
    else
      source = File.join(root, 'impl/ruby-native')
    end
    raise "Ruby source is missing: #{source}" unless File.file?(File.join(source, 'lib/jed_native.rb'))
    verify_baseline(source, phase) if ARCHIVED_PHASES.include?(phase)
    source
  end
end

if $PROGRAM_NAME == __FILE__
  root = File.expand_path('../..', __dir__)
  phase = ENV.fetch('RUBY_NATIVE_PHASE', 'current')
  source = NativeRubySource.resolve(root, phase: phase)
  puts "Ruby source (#{phase}): #{source}"
  puts "SHA-256: #{NativeRubySource.digest(source)}"
end
