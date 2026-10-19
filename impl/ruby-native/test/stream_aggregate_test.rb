# frozen_string_literal: true

require 'tmpdir'
require_relative '../lib/jed_native'

# SQL values/costs are covered by the aggregate corpus. These checks cover the
# file execution lane's retention invariant and its contextual budget gate.
module NativeStreamAggregateTests
  module_function

  def assert(value, message)
    raise message unless value
  end

  def spools
    count = 0
    trace = TracePoint.new(:call) do |event|
      count += 1 if event.defined_class == JedNative::RowSpool && event.method_id == :initialize
    end
    result = nil
    trace.enable { result = yield }
    [result, count]
  end

  def run
    Dir.mktmpdir('jed-ruby-stream-aggregate') do |directory|
      JedNative.create(File.join(directory, 'db'), locking: :exclusive) do |database|
        session = database.session
        session.execute('CREATE TABLE t(id i32 PRIMARY KEY, v i32, f f64)')
        session.execute('INSERT INTO t VALUES (1, 10, 1.5), (2, 20, 2.5), (3, NULL, NULL), (4, -4, -1.0)')
        [
          'SELECT count(*), count(v), sum(v), min(v), max(v), sum(f), avg(f) FROM t',
          'SELECT count(*)+1, sum(v) FROM t WHERE v > 0',
          'SELECT count(*), min(v) FROM t WHERE false',
          'SELECT count(*) FROM t LIMIT 0',
          'SELECT sum(v) FROM t OFFSET 1'
        ].each do |sql|
          fast, retained = spools { session.execute(sql) }
          assert(retained.zero?, "plain aggregate retained row spools: #{sql}")
          session.max_cost = 1_000_000
          ordinary, retained = spools { session.execute(sql) }
          assert(retained.positive?, 'active cost budget bypassed ordinary aggregate lane')
          assert(fast.rows == ordinary.rows && fast.cost == ordinary.cost, 'streaming aggregate changed values or successful cost')
          session.max_cost = 0
        end
        prepared = session.prepare('SELECT count(*) FROM t')
        session.max_query_memory_bytes = 4096
        result, retained = spools { session.query_prepared(prepared).rows }
        assert(result == [[4]] && retained.positive?, 'query-memory budget bypassed ordinary accounting')
        session.max_query_memory_bytes = 0
        result, retained = spools { session.query_prepared(prepared).rows }
        assert(result == [[4]] && retained.zero?, 'restored unlimited budget did not stream retained statement')
        begin
          session.execute('SELECT count(*) FROM t WHERE 10 / (id - 2) > 0')
          raise 'expected division by zero'
        rescue JedNative::Error => error
          assert(error.sqlstate == '22012', 'streamed predicate error changed')
          assert(session.engine.cost == 6, 'failed walk charged deferred scan/aggregate blocks')
        end
        session.close
      end
    end
    puts 'ruby-native stream aggregates: no retained input spools, stable results/costs, contextual budgets and predicate errors passed'
  end
end

NativeStreamAggregateTests.run
