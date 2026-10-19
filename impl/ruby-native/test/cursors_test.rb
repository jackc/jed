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
        assert(cursor.instance_variable_get(:@engine).nil? && cursor.instance_variable_get(:@session).nil?, 'drained result retained a catalog generation')
        begin
          cursor.next
          raise 'closed cursor yielded another row'
        rescue StopIteration
          assert(cursor.cost == eager.cost, 'closed cursor lost its final cost')
        end
      end
      cursor = s.query('select 10/(id-2) from t')
      assert(cursor.next == [-10], 'first pull evaluates one projection')
      cost = cursor.cost
      cursor.close
      assert(cursor.cost == cost, 'early close does no additional work')
      assert(cursor.instance_variable_get(:@engine).nil? && cursor.instance_variable_get(:@fiber).nil?, 'early close retained its execution snapshot')
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
      cursor = s.query('select id from t order by id')
      first = cursor.next
      cursor.columns.first.replace('changed')
      second = cursor.next
      assert(first['id'] == 1 && second['id'] == 2, 'mutable public headers changed row lookup names')
      cursor.close
      assert(core.query('select count(*) from t').rows == [[4]], 'bare database cursor owns its temporary session')
      bindings = [
        ['i16', 12], ['i32', nil], ['i64', 2**40], ['boolean', true],
        ['text', 'bound text'], ['bytea', "\x00\xff".b],
        ['uuid', '00000000-0000-0000-0000-000000000001'],
        ['decimal', JedNative::Decimal.parse('1.25')], ['f32', 1.25], ['f64', 1.25],
        ['timestamptz', JedNative::TimestampValue.new(0, zoned: false)],
        ['timestamptz[]', JedNative::ArrayValue.new([JedNative::TimestampValue.new(0, zoned: false)], element_type: 'timestamptz')]
      ]
      bindings.each do |type, value|
        statement = s.prepare("select $1::#{type}")
        eager = s.execute_prepared(statement, value)
        cursor = s.query_prepared(statement, value)
        assert(cursor.cost.zero?, "binding ran query before pull: #{type}")
        assert(cursor.rows == eager.rows && cursor.cost == eager.cost, "cursor binding changed value or cost: #{type}")
      end
      label = +'original'
      cursor = s.query('select $1::text', label)
      label.replace('changed')
      assert(cursor.rows == [['original']], 'prebound cursor retained mutable host parameter')
    end
    puts 'ruby-native cursors: lazy projections, full-drain costs, errors and frozen snapshot lifecycle passed'
  end
end
NativeCursorTests.run
