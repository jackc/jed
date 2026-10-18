# frozen_string_literal: true

require_relative '../lib/jed_native'
require 'timeout'

module NativeSessionTests
  module_function
  def assert(value, message) = (raise(message) unless value)
  def error(code)
    yield
    raise "expected #{code}"
  rescue JedNative::Error => caught
    assert(caught.sqlstate == code, "expected #{code}, got #{caught.sqlstate}")
  end
  def run
    JedNative.memory do |core|
      s, other = core.session, core.session
      core.execute('create table t(id i32 primary key)')
      core.execute('begin')
      core.execute('insert into t values(1)')
      core.execute('rollback')
      assert(core.query('select * from t').rows == [[1]], 'bare database calls have independent sessions')
      s.set_var('App.Tenant', 'one')
      assert(s.var('app.tenant') == 'one' && other.var('APP.TENANT').nil?, 'variables are private and folded')
      error('42704') { s.set_var('bogus', 'x') }
      s.begin
      s.set_var('app.tenant', 'two')
      s.rollback
      assert(s.var('app.tenant') == 'two', 'settings survive rollback')
      summary = s.execute_script("insert into t values(2); select 1; insert into t values(3);")
      assert(summary.statements_run == 3 && summary.rows_affected_total == 2, 'script summary')
      s.begin
      s.execute_script('insert into t values(4)')
      assert(s.status == :open, 'script joins open transaction')
      error('25001') { s.transaction { raise 'must not enter nested block' } }
      assert(s.status == :open, 'nested scoped begin preserves outer transaction')
      s.rollback
      error('0A000') { s.execute_script('insert into t values(5); COMMIT;') }
      assert(s.query('select id from t order by id').rows == [[1], [2], [3]], 'script rejection rolls back')
      s.max_sql_length = 10
      summary = s.execute_script('SELECT 1; SELECT 2; SELECT 3')
      assert(summary.statements_run == 3, 'SQL length applies to each statement')
      s.max_sql_length = 1_048_576

      held = core.write_session
      waiting, acquired, release = Queue.new, Queue.new, Queue.new
      thread = Thread.new do
        waiting << true
        child = core.write_session
        acquired << child.query('select count(*) from t').rows
        release.pop
        child.rollback
        child.close
      end
      waiting.pop
      Thread.pass until thread.status == 'sleep'
      assert(acquired.empty?, 'real second writer blocks')
      read = core.read_session
      assert(read.query('select count(*) from t').rows == [[3]], 'reader proceeds while writer blocked')
      held.execute('insert into t values(4)')
      held.commit
      assert(Timeout.timeout(2) { acquired.pop } == [[4]], 'writer handoff captures newly committed rows')
      release << true
      thread.join
      assert(read.query('select count(*) from t').rows == [[3]], 'held reader snapshot stays fixed')
      read.close
      held.close
      held = core.write_session
      impatient = core.session(lock_timeout_ms: 1)
      error('55P03') { impatient.execute('insert into t values(5)') }
      held.rollback
      held.close
      impatient.close
    end
    spans = JedNative.split_statements("-- ;\nSELECT ';'; /* ; /* ; */ */ SELECT $$a;b$$; SELECT E'a\\\';b'; -- final").to_a
    assert(spans.length == 3 && spans[1].text.include?('$$a;b$$'), 'splitter respects quoted and commented semicolons')
    assert(spans.all? { |span| span.offset.is_a?(Integer) }, 'split spans expose byte offsets')
    puts 'ruby-native sessions: fresh helpers, settings, scripts, thread gate and pinned snapshots passed'
  end
end
NativeSessionTests.run
