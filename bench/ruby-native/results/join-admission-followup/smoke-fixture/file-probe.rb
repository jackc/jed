require '/home/jack/.codex/worktrees/b8e6/jed/bench/ruby-native/join_admission_diagnostics'
source = '/home/jack/.codex/worktrees/b8e6/jed/impl/ruby-native'
require File.join(source, 'lib/jed_native')
diagnostic = JoinAdmissionDiagnostics
diagnostic.instance_variable_set(:@source, source)
diagnostic.instance_variable_set(:@available, true)
diagnostic.instance_variable_set(:@admission_available, true)
Dir.mktmpdir('input-probe-', '/home/jack/.codex/worktrees/b8e6/jed/.dev/ruby-native-join-admission-diagnostics') do |directory|
  database = JedNative.create(File.join(directory, 'fixture.jed'), locking: :exclusive)
  database.work_mem = 1024
  diagnostic.fixture(database)
  result = diagnostic::INPUT_CASES.map do |job|
    sessions, statements = diagnostic.pair(database, job[:sql])
    values = 3.times.map do
      value = diagnostic.compare(sessions, statements, job[:sql], job[:params], :query_prepared, trace: true)
      trace = diagnostic.instance_variable_get(:@last_pair_trace)
      { cost: value[:cost], rows: value[:rows].length, calls: trace[:enabled][:calls] }
    end
    sessions.each(&:close)
    { job: job, values: values }
  end
  database.close
  puts JSON.generate(result)
end
