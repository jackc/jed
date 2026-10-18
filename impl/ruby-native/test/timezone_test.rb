# frozen_string_literal: true

require 'toml-rb'
require_relative '../lib/jed_native'

root = File.expand_path('../../..', __dir__)
image = File.binread(File.join(root, 'spec/tz/fixtures/tzdata.jtz'))
data = JedNative::TimeZoneData.new(image)
raise 'timezone bundle round-trip changed bytes' unless data.bytes == image
cases = TomlRB.load_file(File.join(root, 'spec/tz/vectors/tzif.toml')).fetch('case')
cases.each do |entry|
  zone = JedNative::TimeZoneData.fixed(entry.fetch('zone')) || data.zones.fetch(entry.fetch('zone'))
  actual = zone.offset_at(entry.fetch('instant_micros').div(1_000_000))
  expected = entry.values_at('utoff_secs', 'abbrev', 'is_dst')
  raise "timezone vector failed: #{entry.inspect}; got #{actual.inspect}" unless actual == expected
end
environment = JedNative::Environment.new
loaded = environment.load_time_zone_data(image)
raise 'environment was mutated' unless environment.timezones.empty?
raise 'loaded timezone data missing' unless loaded.timezones.key?('America/New_York')
altered = image.dup
altered.setbyte(-1, altered.getbyte(-1) ^ 1)
begin
  JedNative::TimeZoneData.new(altered)
  raise 'corrupted timezone bundle accepted'
rescue JedNative::Error => error
  raise unless error.sqlstate == 'XX001'
end
puts "Ruby native timezone: #{cases.length} shared vectors, immutable environment, bundle checksum and byte round-trip pass"
