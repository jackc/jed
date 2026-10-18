# frozen_string_literal: true

require 'digest'
require 'fileutils'
require 'json'
require 'open3'
require 'tmpdir'

# Keep the initial implementation reproducible after the ignored .dev directory
# is removed. The tracked source archive contains Ruby source, not build output.
module NativeRubySource
  module_function

  def digest(path)
    Digest::SHA256.hexdigest(Dir[File.join(path, 'lib/**/*.rb')].sort.map { |file| File.binread(file) }.join)
  end

  def verify_baseline(path)
    report = File.join(__dir__, 'results/baseline.json')
    return unless File.file?(report)

    expected = JSON.parse(File.read(report)).fetch('ruby_source_sha256')
    actual = digest(path)
    raise "baseline source checksum differs: #{actual}, expected #{expected}" unless actual == expected
  end

  def resolve(root, phase:)
    source = ENV['RUBY_NATIVE_SOURCE']
    if source
      source = File.expand_path(source)
    elsif phase == 'baseline'
      source = File.join(root, '.dev/ruby-native-baseline')
      unless File.directory?(source)
        FileUtils.mkdir_p(File.dirname(source))
        archive = File.join(__dir__, 'results/baseline-source.tar.gz')
        if File.file?(archive)
          temporary = Dir.mktmpdir('ruby-native-restore-', File.dirname(source))
          begin
            output, status = Open3.capture2e('tar', '-xzf', archive, '-C', temporary)
            raise "cannot restore baseline source: #{output}" unless status.success?

            verify_baseline(temporary)
            FileUtils.mv(temporary, source)
          ensure
            FileUtils.remove_entry(temporary) if File.exist?(temporary)
          end
        else
          # The first experiment run has no published baseline yet.
          raise 'baseline source archive is missing' if File.file?(File.join(__dir__, 'results/baseline.json'))

          FileUtils.cp_r(File.join(root, 'impl/ruby-native'), source)
        end
      end
    else
      source = File.join(root, 'impl/ruby-native')
    end
    verify_baseline(source) if phase == 'baseline'
    source
  end
end

if $PROGRAM_NAME == __FILE__
  Dir.mktmpdir('jed-ruby-source-check') do |root|
    source = NativeRubySource.resolve(root, phase: 'baseline')
    puts "restored baseline source verified: #{NativeRubySource.digest(source)}"
  end
end
