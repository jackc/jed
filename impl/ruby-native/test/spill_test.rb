# frozen_string_literal: true

require_relative '../lib/jed_native'

# SQL result/cost cases are shared corpus entries. These checks cover the host
# scratch lifetime, accounting ownership, and bounded run/partition invariants.
module NativeSpillTests
  module_function
  def run
    engine = JedNative::Engine.new(work_mem: 96)
    account = JedNative::QueryMemoryAccount.new(96)
    engine.instance_variable_set(:@query_memory, account)
    input = 513.times.map { |index| [index % 17, index] }
    spool = JedNative::RowSpool.new(engine)
    input.each { |row| spool.push(row) }
    raise 'row spool did not spill' unless spool.spilled?
    raise 'spool replay changed rows' unless spool.to_a == input && spool.to_a == input
    raise 'spilled rows retained a memory charge' unless account.used.zero?
    spool.close

    sorter = JedNative::ExternalSorter.new(engine) { |left, right| left[0] <=> right[0] }
    input.each { |row| sorter.push(row) }
    raise 'run inventory grew with row count' unless sorter.instance_variable_get(:@runs).compact.length <= input.length.bit_length
    raise 'external merge was not stable' unless sorter.to_a == input.sort_by { |row| [row[0], row[1]] }
    sorter.close
    raise 'sort charge leaked' unless account.used.zero?

    state = JedNative::SpillStateMap.new(engine)
    input.each { |key, ordinal| state.put([key], [ordinal]) }
    17.times do |key|
      found, value = state.get([key])
      raise 'disk map did not return newest state' unless found && value == [input.reverse.find { |row| row[0] == key }[1]]
    end
    state.close
    rows = JedNative::SpillHashRows.new(engine)
    input.each { |key, ordinal| rows.push(key, [ordinal]) }
    17.times do |key|
      raise 'disk hash replay changed fold order' unless rows.each(key).to_a == input.select { |row| row[0] == key }.map { |row| [row[1]] }
    end
    rows.close
    raise 'scratch state charge leaked' unless account.used.zero?
    puts 'ruby-native spill: bounded runs, stable merge, repeatable spools, disk state and memory release passed'
  end
end

NativeSpillTests.run
