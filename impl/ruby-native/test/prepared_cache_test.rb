# frozen_string_literal: true

require 'tmpdir'
require_relative '../lib/jed_native'

# SQL transcripts prepare each record anew. These probes retain host statements
# and exercise the catalog/session transitions that invalidate a resolved plan.
module NativePreparedCacheTests
  module_function

  def assert(value, message) = (raise(message) unless value)

  def error(code)
    yield
    raise "expected #{code}"
  rescue JedNative::Error => caught
    assert(caught.sqlstate == code, "expected #{code}, got #{caught.sqlstate}: #{caught.message}")
  end

  def plan(session, statement)
    routed = session.engine.instance_variable_get(:@native_domain_routes).fetch(statement.ast).first
    session.engine.instance_variable_get(:@native_read_plans).fetch(routed)
  end

  def publication(database)
    session, writer = database.session, database.session
    session.execute('CREATE TABLE t(id i32 PRIMARY KEY, v i32)')
    session.execute('INSERT INTO t VALUES(1, 10)')
    statement = session.prepare('SELECT v FROM t WHERE id = $1')
    first = session.query_prepared(statement, 1)
    assert(first.rows == [[10]], 'initial lookup')
    resolved = plan(session, statement)
    3.times do
      result = session.query_prepared(statement, 1)
      assert(result.rows == [[10]] && result.cost == first.cost, 'reused plan changes rows or cost')
      assert(plan(session, statement).equal?(resolved), 'same publication did not reuse its plan')
    end
    assert(session.query_prepared(statement, nil).rows.empty?, 'NULL binding does not match')
    assert(plan(session, statement).equal?(resolved), 'declared parameter type did not survive NULL binding')

    writer.execute('UPDATE t SET v=20 WHERE id=1')
    assert(session.query_prepared(statement, 1).rows == [[20]], 'new publication retained old row')
    assert(!plan(session, statement).equal?(resolved), 'new publication retained old plan')
    session.begin
    session.execute('UPDATE t SET v=30 WHERE id=1')
    assert(session.query_prepared(statement, 1).rows == [[30]], 'pending rows were not resolved')
    session.execute('SAVEPOINT before_change')
    session.execute('UPDATE t SET v=40 WHERE id=1')
    assert(session.query_prepared(statement, 1).rows == [[40]], 'savepoint rows were not resolved')
    session.execute('ROLLBACK TO before_change')
    assert(session.query_prepared(statement, 1).rows == [[30]], 'savepoint rollback retained stale plan')
    session.rollback
    assert(session.query_prepared(statement, 1).rows == [[20]], 'transaction rollback retained stale plan')

    writer.execute('DROP TABLE t')
    writer.execute('CREATE TABLE t(id i32 PRIMARY KEY, v text)')
    writer.execute("INSERT INTO t VALUES(1, 'replacement')")
    result = session.query_prepared(statement, 1)
    assert(result.rows == [['replacement']] && result.column_types == ['text'], 'replacement schema retained old plan')
    session.revoke(:select, 't')
    error('42501') { session.query_prepared(statement, 1) }
    session.reset_privileges
    assert(session.query_prepared(statement, 1).rows == [['replacement']], 'permission change damaged reusable plan')
  ensure
    session&.close
    writer&.close
  end

  def namespaces
    JedNative.memory do |database|
      session = database.session
      main = session.prepare('SELECT x FROM main.t')
      temp = session.prepare('SELECT x FROM temp.t')
      session.execute('CREATE TABLE t(x i32)')
      session.execute('INSERT INTO t VALUES(1)')
      assert(session.query_prepared(main).rows == [[1]], 'qualified main lookup')
      error('42P01') { session.query_prepared(temp) }
      session.execute('DROP TABLE t')
      session.execute('CREATE TEMP TABLE t(x text)')
      session.execute("INSERT INTO t VALUES('temporary')")
      error('42P01') { session.query_prepared(main) }
      assert(session.query_prepared(temp).rows == [['temporary']], 'qualified temp lookup after replacement')
      database.execute('CREATE TABLE other(x i32)')
      assert(session.query_prepared(temp).rows == [['temporary']], 'publication lost session temporary table')

      attached = session.prepare('SELECT x FROM ref.t')
      database.attach('ref')
      session.execute('CREATE TABLE ref.t(x i32)')
      session.execute('INSERT INTO ref.t VALUES(7)')
      assert(session.query_prepared(attached).rows == [[7]], 'database attachment was not captured')
      database.detach('ref')
      error('42P01') { session.query_prepared(attached) }
      database.attach('ref')
      session.execute('CREATE TABLE ref.t(x text)')
      session.execute("INSERT INTO ref.t VALUES('reattached')")
      assert(session.query_prepared(attached).rows == [['reattached']], 'reattachment retained old binding')
      session.close
    end
  end

  def execution_context
    calls = 0
    environment = JedNative::Environment.new.register_function(name: 'counter', args: [], returns: 'i64') { calls += 1 }
    other_environment = JedNative::Environment.new.register_function(name: 'counter', args: [], returns: 'i64') { 99 }
    JedNative.memory(environment: environment) do |database|
      session = database.session
      statement = session.prepare('SELECT counter()')
      assert(session.query_prepared(statement).rows == [[1]], 'first host call')
      assert(session.query_prepared(statement).rows == [[2]], 'cached plan retained host result')
      session.execute('CREATE TABLE empty_calls(id i32)')
      empty_call = session.prepare('SELECT counter() FROM empty_calls WHERE false')
      assert(session.query_prepared(empty_call).rows.empty?, 'empty host projection')
      session.revoke(:execute, 'counter')
      error('42501') { session.query_prepared(empty_call) }
      session.reset_privileges
      arithmetic = session.prepare('SELECT abs($1::i64) + 7')
      assert(session.query_prepared(arithmetic, -5).rows == [[12]], 'first scalar cursor')
      scalar = session.engine.instance_variable_get(:@native_select_plans).values.first
      assert(scalar, 'scalar cursor did not retain a resolved plan')
      assert(session.query_prepared(arithmetic, 10).rows == [[17]], 'reused scalar cursor')
      assert(session.engine.instance_variable_get(:@native_select_plans).values.first.equal?(scalar), 'scalar cursor did not reuse plan')
      other = database.session(environment: other_environment)
      assert(other.query_prepared(statement).rows == [[99]], 'statement leaked another engine host environment')
      other.close
      missing = database.session(environment: JedNative::Environment.new)
      error('42883') { missing.query_prepared(statement).rows }
      missing.close

      zoned = session.prepare("SELECT '2000-01-01 00:00:00'::timestamptz")
      utc = session.query_prepared(zoned).rows[0][0].micros
      session.timezone = '+02:00'
      shifted = session.query_prepared(zoned).rows[0][0].micros
      assert(shifted == utc + 7_200_000_000, 'cached zoned literal ignored session timezone')

      session.execute('CREATE TYPE pair AS (x i32)')
      composite = session.prepare("SELECT '(5)'::pair")
      assert(session.query_prepared(composite).rows == [[[5]]], 'initial composite literal')
      session.execute('DROP TYPE pair')
      error('42704') { session.query_prepared(composite).rows }
      session.execute('CREATE TYPE pair AS (x text)')
      assert(session.query_prepared(composite).rows == [[['5']]], 'replacement composite type retained old literal')
      80.times { |index| session.query_prepared(session.prepare("SELECT #{index}")).rows }
      assert(session.engine.instance_variable_get(:@native_domain_routes).length <= 64, 'routed syntax cache is unbounded')
      assert(session.engine.instance_variable_get(:@native_cursor_metadata).length <= 64, 'cursor metadata cache is unbounded')
      assert(session.engine.instance_variable_get(:@native_select_plans).length <= 64, 'resolved scalar cache is unbounded')
      assert(session.engine.instance_variable_get(:@native_cte_validation).length <= 64, 'CTE validation cache is unbounded')
      session.close
    end
  end

  def custom_access
    JedNative.memory do |database|
      session = database.session
      session.execute('CREATE TABLE access(id i32 PRIMARY KEY, bucket i32)')
      session.execute("INSERT INTO access VALUES #{40.times.map { |index| "(#{index},#{index < 35 ? 0 : index})" }.join(',')}")
      session.execute('CREATE INDEX access_bucket ON access(bucket)')
      session.execute('ANALYZE access')
      sql = 'SELECT id FROM access WHERE bucket = $1 ORDER BY id LIMIT 10'
      prepared = session.prepare(sql)
      [0, 35, 90, nil, 0].each do |value|
        fresh = session.execute(sql, value)
        reused = session.query_prepared(prepared, value)
        assert(reused.rows == fresh.rows && reused.cost == fresh.cost, 'cached expressions changed custom access rows or cost')
      end
      window = session.prepare('SELECT row_number() OVER (ORDER BY id), id FROM access ORDER BY id LIMIT 2')
      2.times { assert(session.query_prepared(window).rows == [[1, 0], [2, 1]], 'cached window shape changed execution') }
      recursive = session.prepare('WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM r WHERE n<3) SELECT n FROM r ORDER BY n')
      2.times { assert(session.query_prepared(recursive).rows == [[1], [2], [3]], 'cached CTE validation changed execution') }
      invalid = session.prepare('WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT sum(n)::i64 FROM r) SELECT * FROM r')
      2.times { error('42P19') { session.query_prepared(invalid).rows } }
      session.close
    end
  end

  def run
    JedNative.memory { |database| publication(database) }
    Dir.mktmpdir('jed-ruby-prepared') do |directory|
      JedNative.create(File.join(directory, 'db'), locking: :exclusive) { |database| publication(database) }
    end
    namespaces
    execution_context
    custom_access
    puts 'ruby-native prepared cache: reusable memory/file plans, bindings, publications, rollback, namespaces and host context passed'
  end
end

NativePreparedCacheTests.run
