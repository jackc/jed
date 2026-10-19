# frozen_string_literal: true

require_relative 'file_compare'

module NativeRubyFileCompareTest
  extend self

  def assert(value, message)
    raise message unless value
  end

  filesystem = NativeRubyFileCompare.workspace_filesystem
  assert(!%w[tmpfs ramfs devtmpfs].include?(filesystem.fetch(:type)), 'file benchmark workspace is memory-backed')
  original_tmpdir = ENV['TMPDIR']
  result = NativeRubyFileCompare.run_on_disk(timeout: 5, memory_bytes: 128 * 1024**2, address_space_bytes: 1024**3) do |outer|
    nested = JedNativeTools::BoundedProcess.run(timeout: 2, memory_bytes: 128 * 1024**2, address_space_bytes: 1024**3) do |inner|
      scratch = Dir.mktmpdir('scratch-location-check-')
      { inner: inner, scratch: scratch, tmpdir: ENV['TMPDIR'], device: File.stat(scratch).dev }
    end
    raise nested.reason unless nested.success?
    { outer: outer, nested: nested.value }
  end
  assert(result.success?, "filesystem probe failed: #{result.reason} #{result.stderr}")
  outer = result.value.fetch('outer')
  nested = result.value.fetch('nested')
  assert(outer.start_with?(NativeRubyFileCompare::WORKSPACES + '/'), 'outer dataset escaped the disk workspace')
  assert(nested.fetch('inner').start_with?(outer + '/'), 'nested Ruby worker escaped the disk workspace')
  assert(nested.fetch('scratch').start_with?(nested.fetch('inner') + '/'), 'Ruby scratch database would use a different filesystem')
  assert(nested.fetch('device') == filesystem.fetch(:device), 'scratch filesystem differs from the reported filesystem')
  assert(ENV['TMPDIR'] == original_tmpdir, 'supervisor TMPDIR was not restored')
  assert(!File.exist?(outer), 'completed filesystem probe retained its workspace')

  begin
    NativeRubyFileCompare.run_on_disk(timeout: -1) { raise 'must not run' }
    raise 'invalid timeout was accepted'
  rescue ArgumentError
    assert(ENV['TMPDIR'] == original_tmpdir, 'failed worker setup did not restore TMPDIR')
  end
end

puts 'file comparison filesystem tests passed'
