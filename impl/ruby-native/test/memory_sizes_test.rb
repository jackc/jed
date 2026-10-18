# frozen_string_literal: true
require_relative '../../../scripts/lib/bundle_setup'
require 'toml-rb'
require_relative '../lib/jed_native'

vectors = TomlRB.load_file(File.expand_path('../../../spec/cost/memory_sizes.toml', __dir__)).fetch('vector')
vectors.each do |vector|
  JedNative.memory do |database|
    database = database.session
    (vector['setup'] || []).each { |sql| database.execute(sql) }
    result = database.query(vector.fetch('sql'))
    bytes = if vector['measure'] == 'row'
      JedNative::LogicalMemory.row_bytes(result.rows.fetch(0), result.column_types)
    else
      JedNative::LogicalMemory.value_bytes(result.rows.fetch(0).fetch(0), result.column_types.fetch(0))
    end
    raise "#{vector['sql']}: expected #{vector['bytes']} logical bytes, got #{bytes}" unless bytes == vector['bytes']
  end
end
puts "ruby-native logical memory: #{vectors.length} shared vectors passed"
