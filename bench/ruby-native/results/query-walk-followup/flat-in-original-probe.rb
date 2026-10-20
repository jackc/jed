require 'json'
require 'digest'
require_relative '/home/jack/.codex/worktrees/b8e6/jed/bench/ruby-native/source'
root = '/home/jack/.codex/worktrees/b8e6/jed'
source = ENV.fetch('RUBY_NATIVE_SOURCE')
require File.join(source, 'lib/jed_native')
plan = JSON.parse(File.read(File.join(root, 'bench/ruby-native/compare/query-walk-controls.json')))
sql = plan.fetch('workloads').find { |job| job.fetch('name') == 'oversized_syntax' }.fetch('sql')
hash = NativeRubySource.digest(source)
database = JedNative.memory
writer = database.session
writer.execute('CREATE TABLE items(id i32 PRIMARY KEY, bucket i32, score i32, label text)')
writer.execute("INSERT INTO items VALUES #{1000.times.map { |i| "(#{i},#{i % 10},#{i * 37 % 10000},'row#{i}')" }.join(',')}")
writer.close
cases = [false, true].map do |traced|
  session = database.session
  prepared = session.prepare(sql)
  count = 0
  tracker = TracePoint.new(:call) { |event| count += 1 if event.path.start_with?(File.join(source, 'lib/')) } if traced
  begin
    tracker&.enable
    result = session.query_prepared(prepared, 0)
    rows = []
    result.each { |row| rows << row.to_a }
    { traced: traced, status: 'rows', rows: rows, columns: result.columns,
      types: result.column_types, cost: result.cost, calls: count }
  rescue Exception => error
    { traced: traced, status: 'error', class: error.class.name, message: error.message,
      backtrace: error.backtrace, calls: count }
  ensure
    tracker&.disable
    result&.close
    session.close
  end
end
database.close
raise 'source changed' unless NativeRubySource.digest(source) == hash
puts JSON.generate(source: source, source_sha256: hash, final_source_sha256: hash,
  script_sha256: Digest::SHA256.file(__FILE__).hexdigest, sql: sql, cases: cases)
