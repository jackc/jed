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
lines.concat(["    }.freeze", "  end", "end", ""])
File.write(File.join(root, 'impl/ruby-native/lib/jed_native/function_catalog.rb'), lines.join("\n"))
formatting = TomlRB.load_file(File.join(root, 'spec/types/formatting.toml'))
lines = ["# frozen_string_literal: true", "# Generated from spec/types/formatting.toml; processors are handwritten.", "module JedNative", "  module FormattingTables"]
%w[num dch suffixes words].each { |name| lines << "    #{name.upcase} = #{formatting.fetch(name).inspect}.freeze" }
lines.concat(["  end", "end", ""])
File.write(File.join(root, 'impl/ruby-native/lib/jed_native/formatting_tables.rb'), lines.join("\n"))
units = TomlRB.load_file(File.join(root, 'spec/types/datetime_units.toml'))
lines = ["# frozen_string_literal: true", "# Generated from spec/types/datetime_units.toml; processors are handwritten.", "module JedNative", "  module DateTimeUnits", "    TOKEN_MAX_LEN = #{units.fetch('token_max_len')}"]
%w[delta date].each { |name| lines << "    #{name.upcase} = #{units.fetch(name).to_h { |entry| [entry.fetch('token'), entry.values_at('type', 'value')] }.inspect}.freeze" }
lines.concat(["  end", "end", ""])
File.write(File.join(root, 'impl/ruby-native/lib/jed_native/datetime_units.rb'), lines.join("\n"))
