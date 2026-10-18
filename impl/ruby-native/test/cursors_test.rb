# frozen_string_literal: true

require_relative '../lib/jed_native'
module NativeCursorTests
  module_function
  def assert(value, message) = (raise(message) unless value)
  def run
    JedNative.memory do |core|
      s = core.session
      s.execute_script('create table t(id i32 primary key, v i32); insert into t values(1,3),(2,2),(3,1)')
      queries = [
        'select * from t', 'select id+1 from t where v>1', 'select id from t limit 2 offset 1',
        'select id from t order by id desc', 'select id from t order by v',
        'select id+3 from t order by v limit 1', 'select count(*),sum(v) from t',
        'select v, sum(id)+1 from t group by v order by v', 'select distinct v from t',
        'select row_number() over(order by v),id from t order by id',
        'select a.id,b.id from t a join t b on a.id=b.id order by a.id',
        'select id from t union select v from t',
        'with x as(select * from t) select id from x', 'values(1),(2)',
        'select (select count(*) from t),id from t'
      ]
      queries.each do |sql|
        eager = s.execute(sql)
        cursor = s.query(sql)
        assert(cursor.cost.zero?, "cursor executed before pull: #{sql}")
        assert(cursor.rows == eager.rows, "cursor rows differ: #{sql}")
        assert(cursor.cost == eager.cost, "cursor cost #{cursor.cost} differs from #{eager.cost}: #{sql}")
        assert(cursor.column_types == eager.column_types, "cursor metadata differs: #{sql}")
      end
      cursor = s.query('select 10/(id-2) from t')
      assert(cursor.next == [-10], 'first pull evaluates one projection')
      cost = cursor.cost
      cursor.close
      assert(cursor.cost == cost, 'early close does no additional work')
      sorted = s.query('select 10/(id-2) from t order by v desc')
      assert(sorted.next == [-10], 'blocking sort defers final projection')
      sorted.close
      cursor = s.query('select id from t')
      version = core.version
      s.execute('insert into t values(4,0)')
      assert(core.oldest_live_txid == version, 'cursor owns reader pin')
      assert(cursor.rows == [[1],[2],[3]], 'cursor freezes open-time root')
      assert(core.oldest_live_txid == core.version, 'drain releases cursor pin')
      s.begin
      s.execute('insert into t values(5,5)')
      cursor = s.query('select id from t')
      s.execute('insert into t values(6,6)')
      assert(cursor.rows == [[1],[2],[3],[4],[5]], 'pending cursor excludes later writes')
      cursor = s.query('select id from t')
      s.rollback
      assert(cursor.closed?, 'block end closes unread cursor')
      cursor = s.query('select 10/(id-2) from t')
      cursor.next
      begin
        cursor.next
        raise 'expected arithmetic failure during pull'
      rescue JedNative::Error => error
        assert(error.sqlstate == '22012', 'cursor arithmetic error')
      end
      assert(cursor.closed?, 'failed cursor releases pin')
      assert(core.query('select count(*) from t').rows == [[4]], 'bare database cursor owns its temporary session')
    end
    puts 'ruby-native cursors: lazy projections, full-drain costs, errors and frozen snapshot lifecycle passed'
  end
end
NativeCursorTests.run
