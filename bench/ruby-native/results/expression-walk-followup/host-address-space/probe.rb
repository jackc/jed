# frozen_string_literal: true

# Diagnostic only: the existing host-test prefix, with Linux process reservation
# observations. No engine or test behavior is changed.
require 'json'

def reservation_snapshot(label)
  status = File.read('/proc/self/status').lines.grep(/\A(?:VmPeak|VmSize|VmRSS|VmData|VmStk|Threads):/).to_h do |line|
    key, value = line.split(':', 2)
    [key, value.strip]
  end
  maps = File.read('/proc/self/maps').lines.filter_map do |line|
    range, permissions, _offset, _device, _inode, path = line.split(/\s+/, 6)
    next if path && !path.strip.empty?
    first, last = range.split('-').map { |address| address.to_i(16) }
    { bytes: last - first, permissions: permissions }
  end
  puts JSON.generate(label: label, pid: Process.pid, status: status,
    anonymous_bytes: maps.sum { |mapping| mapping[:bytes] },
    anonymous_no_access_bytes: maps.sum { |mapping| mapping[:permissions].start_with?('---') ? mapping[:bytes] : 0 },
    largest_anonymous: maps.sort_by { |mapping| -mapping[:bytes] }.first(16))
  $stdout.flush
end

source = File.expand_path(ENV.fetch('RUBY_NATIVE_SOURCE', 'impl/ruby-native'))
target = 'compiled_method_sharing_test.rb'
paths = Dir[File.join(source, 'test/*_test.rb')].sort
last = paths.index { |path| File.basename(path) == target }
raise 'host prefix target missing' unless last
reservation_snapshot('start')
paths[0..last].each do |path|
  reservation_snapshot("before #{File.basename(path)}")
  trace = if File.basename(path) == target
    TracePoint.new(:call, :return) do |event|
      if event.path == path && %i[compiler_families cache_bounds_and_source concurrent_admission
        instance_admission fork_reset lifetime warmed_apis].include?(event.method_id)
        reservation_snapshot("#{event.event} #{event.method_id}")
      end
    end
  end
  trace&.enable
  require path
  trace&.disable
  reservation_snapshot("after #{File.basename(path)}")
end
puts 'host address-space diagnostic: existing test prefix passed'
