# frozen_string_literal: true

require 'toml-rb'
require_relative '../lib/jed_native'

root = File.expand_path('../../..', __dir__)
bytes = File.binread(File.join(root, 'spec/collation/fixtures/unicode.jucd'))
data = JedNative::UnicodeData.new(bytes)
raise 'Unicode bundle round-trip changed bytes' unless data.bytes == bytes
dev_hex = TomlRB.load_file(File.join(root, 'spec/collation/vectors/bundle.toml')).fetch('bundle').first.fetch('bundle_hex')
dev = JedNative::UnicodeData.new([dev_hex].pack('H*'))
tables = data.collations.merge(dev.collations)
cases = TomlRB.load_file(File.join(root, 'spec/collation/vectors/sortkey.toml')).fetch('sortkey')
cases.each do |entry|
  got = tables.fetch(entry.fetch('coll_name')).sort_key(entry.fetch('string')).unpack1('H*')
  raise "Unicode sort key mismatch: #{entry.inspect}" unless got == entry.fetch('sortkey_hex')
end
raise 'full Unicode case expansion differs' unless data.properties.fold('caféß𐐨', :upper) == 'CAFÉSS𐐀'
raise 'simple Unicode fold changes code point count' unless data.properties.fold('İ𐐀', :lower, simple: true) == 'i𐐨'
environment = JedNative::Environment.new
loaded = environment.load_unicode_data(bytes)
raise 'loading Unicode mutated environment' unless environment.collations.empty? && loaded.collations.keys.sort == %w[es unicode]
puts "Ruby native Unicode: #{cases.length} shared sort-key vectors, byte round-trip, immutable environment and casing pass"
