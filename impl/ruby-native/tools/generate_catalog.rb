#!/usr/bin/env ruby
# frozen_string_literal: true
# Only shared mechanical metadata is generated; every Ruby kernel is handwritten.
require_relative '../../../scripts/lib/bundle_setup'
require 'toml-rb'
root = File.expand_path('../../..', __dir__)
catalog = TomlRB.load_file(File.join(root, 'spec/functions/catalog.toml'))
functions = catalog.fetch('operator').select { |entry| entry['kind'] == 'function' }.group_by { |entry| entry.fetch('name') }
lines = ["# frozen_string_literal: true", "# Generated from spec/functions/catalog.toml by tools/generate_catalog.rb.", "module JedNative", "  module FunctionCatalog", "    SIGNATURES = {"]
functions.each do |name, entries|
  values = entries.map do |entry|
    [entry.fetch('arg_families'), entry.fetch('result'), entry.fetch('arg_resolution') == 'promote', !!entry['variadic'], entry.fetch('arg_defaults', []).length]
  end
  lines << "      #{name.inspect} => #{values.inspect},"
end
lines.concat(["    }.freeze", "", "    # Keep overload metadata: immutable and stable overloads may share a name.", "    VOLATILITY = {"])
functions.each do |name, entries|
  values = entries.map { |entry| [entry.fetch('arg_families'), entry.fetch('volatility')] }
  lines << "      #{name.inspect} => #{values.inspect},"
end
lines.concat(["    }.freeze", "", "    ARGUMENTS = {"])
functions.each do |name, entries|
  named = entries.select { |entry| entry['arg_names'] }
  next if named.empty?
  values = named.map { |entry| [entry.fetch('arg_names'), entry.fetch('arg_families'), entry.fetch('arg_defaults', [])] }
  lines << "      #{name.inspect} => #{values.inspect},"
end
lines.concat(["    }.freeze", "", "    SAFE_FUNCTIONS = {"])
functions.each do |name, entries|
  families = entries.select { |entry| entry['traps'] == false }.map { |entry| entry.fetch('arg_families') }
  lines << "      #{name.inspect} => #{families.inspect}," unless families.empty?
end
casts = TomlRB.load_file(File.join(root, 'spec/types/casts.toml')).fetch('cast')
safe_casts = casts.select { |entry| entry['traps'] == false }.map { |entry| entry.values_at('from', 'to') }
scalars = TomlRB.load_file(File.join(root, 'spec/types/scalars.toml')).fetch('type')
opclasses = scalars.filter_map { |entry| [entry.fetch('id'), entry['btree_opclass']] if entry['btree_opclass'] }.to_h
lines.concat(["    }.freeze", "    SAFE_CASTS = #{safe_casts.inspect}.freeze", "    CAST_MODES = #{casts.to_h { |entry| [entry.values_at('from', 'to'), entry.fetch('mode')] }.inspect}.freeze", "    BTREE_OPCLASSES = #{opclasses.inspect}.freeze", "  end", "end", ""])
File.write(File.join(root, 'impl/ruby-native/lib/jed_native/function_catalog.rb'), lines.join("\n"))
formatting = TomlRB.load_file(File.join(root, 'spec/types/formatting.toml'))
lines = ["# frozen_string_literal: true", "# Generated from spec/types/formatting.toml; processors are handwritten.", "module JedNative", "  module FormattingTables"]
%w[num dch suffixes words].each { |name| lines << "    #{name.upcase} = #{formatting.fetch(name).inspect}.freeze" }
lines.concat(["  end", "end", ""])
File.write(File.join(root, 'impl/ruby-native/lib/jed_native/formatting_tables.rb'), lines.join("\n"))
units = TomlRB.load_file(File.join(root, 'spec/types/datetime_units.toml'))
lines = ["# frozen_string_literal: true", "# Generated from spec/types/datetime_units.toml; processors are handwritten.", "module JedNative", "  module DateTimeUnits", "    TOKEN_MAX_LEN = #{units.fetch('token_max_len')}"]
%w[delta date].each { |name| lines << "    #{name.upcase} = #{units.fetch(name).to_h { |entry| [entry.fetch('token'), entry.values_at('type', 'value')] }.inspect}.freeze" }
lines << "    INTERVAL_QUALIFIERS = #{units.fetch('interval_qualifier').to_h { |entry| [entry.fetch('name'), [entry.fetch('least'), !!entry['mm_ss']]] }.inspect}.freeze"
lines.concat(["  end", "end", ""])
File.write(File.join(root, 'impl/ruby-native/lib/jed_native/datetime_units.rb'), lines.join("\n"))
