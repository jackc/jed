require '/home/jack/.codex/worktrees/b8e6/jed/bench/ruby-native/join_functions_diagnostics'
d = JoinFunctionsDiagnostics
source = ENV.fetch('RUBY_NATIVE_SOURCE')
require File.join(source, 'lib/jed_native')
d.instance_variable_set(:@source, source)
d.instance_variable_set(:@available, true)
database = JedNative.memory
d.fixture(database)
sql = d.jobs.find { |job| job[:name] == 'join_text_projection' }.fetch(:sql)
sessions, statements = d.pair(database, sql)
previous = nil
values = 3.times.map do
  answer = d.compare(sessions, statements, sql, [0], :execute_prepared, trace: true)
  entry = d.cache(sessions.first).values.first
  result = {
    cost: answer[:cost],
    entry_id: entry.object_id,
    context: entry.context.each_with_index.map do |item, index|
      { slot: index, class: item.class.to_s, id: item.object_id, inspect: item.inspect[0, 1000],
        equals_previous: previous && previous.context[index] == item }
    end,
    tables: entry.tables.map do |name, table|
      old = previous&.tables&.find { |pair| pair[0] == name }&.last
      { name: name, id: table.object_id, equals_previous: old && old.equal?(table),
        table_keys: table.keys, previous_keys: old&.keys }
    end
  }
  previous = entry
  result
end
puts JSON.generate(values)
sessions.each(&:close)
database.close
