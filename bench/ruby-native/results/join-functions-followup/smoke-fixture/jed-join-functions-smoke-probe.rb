require '/home/jack/.codex/worktrees/b8e6/jed/bench/ruby-native/join_functions_diagnostics'
d = JoinFunctionsDiagnostics
source = ENV.fetch('RUBY_NATIVE_SOURCE')
require File.join(source, 'lib/jed_native')
d.instance_variable_set(:@source, source)
d.instance_variable_set(:@available, true)
sql = d.jobs.find { |job| job[:name] == 'join_text_projection' }.fetch(:sql)
directory = Dir.mktmpdir('jed-functions-probe-', '/home/jack/.codex/worktrees/b8e6/jed/.dev')
reports = []
begin
  %w[memory disk].each do |mode|
    database = mode == 'memory' ? JedNative.memory : JedNative.create(File.join(directory, 'db.jed'), locking: :exclusive)
    d.fixture(database)
    [false, true].each do |physical_disabled|
      sessions, statements = d.pair(database, sql, physical_disabled: physical_disabled)
      original = nil
      values = d.bindings.each_with_index.map do |parameter, index|
        answer = d.compare(sessions, statements, sql, [parameter], :execute_prepared, trace: true)
        original ||= d.cache(sessions.first).values.first
        traces = d.instance_variable_get(:@last_pair_trace)
        {
          state: d::STATES[index], answer: answer,
          engine: sessions.first.engine.object_id,
          original_entry: original&.object_id,
          cache_entries: d.cache(sessions.first).values.map(&:object_id),
          cache: d.cache_state(sessions.first),
          roots: d.cache(sessions.first).values.map { |entry| entry.roots.map { |key, saved| [key, saved[0].object_id, d.builtin_expression?(saved[0])] } },
          calls: traces.transform_values { |trace| trace[:calls] }
        }
      end
      reports << { mode: mode, physical_disabled: physical_disabled, observations: values }
      sessions.each(&:close)
    end
    database.close
  end
  puts JSON.generate(reports)
ensure
  FileUtils.remove_entry(directory)
end
