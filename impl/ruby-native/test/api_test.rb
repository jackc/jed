# frozen_string_literal: true

require_relative '../lib/jed_native'

# Reusing host statement objects and mutating their owning database cannot be
# expressed by a sqllogictest transcript (each transcript record prepares anew).
module NativeApiTests
  module_function

  def assert(condition, message)
    raise message unless condition
  end

  def error(code)
    begin
      yield
    rescue JedNative::Error => exception
      assert(exception.sqlstate == code, "expected #{code}, got #{exception.sqlstate}")
      return
    end
    raise "expected #{code}, got success"
  end

  def run
    JedNative.memory do |db|
      db = db.session
      db.execute('CREATE TABLE items (id i16 PRIMARY KEY, v i32)')
      db.execute('INSERT INTO items VALUES (1,10)')
      statement = db.prepare('SELECT v FROM items WHERE id = $1')
      assert(db.query_prepared(statement, 1).rows == [[10]], 'initial lookup')
      assert(db.query_prepared(statement, 2).rows.empty?, 'bound miss')
      assert(db.query_prepared(statement, nil).rows.empty?, 'bound NULL')
      error('22003') { db.query_prepared(statement, 32768) }
      db.execute('BEGIN')
      db.execute('UPDATE items SET v = 20 WHERE id = 1')
      assert(db.query_prepared(statement, 1).rows == [[20]], 'pending write visibility')
      db.execute('ROLLBACK')
      assert(db.query_prepared(statement, 1).rows == [[10]], 'rollback invalidation')
      db.execute('DROP TABLE items')
      db.execute('CREATE TABLE items (id i16 PRIMARY KEY, v text)')
      db.execute("INSERT INTO items VALUES (1,'replacement')")
      result = db.query_prepared(statement, 1)
      assert(result.rows == [['replacement']] && result.column_types == ['text'], 'catalog replacement invalidation')
      error('23505') { db.execute_script("INSERT INTO items VALUES (2,'two'); INSERT INTO items VALUES (1,'duplicate')") }
      assert(db.query_prepared(statement, 2).rows.empty?, 'script failure is atomic')
      label = +'owned'
      db.execute('INSERT INTO items VALUES ($1, $2)', 3, label)
      label.replace('mutated binding')
      result = db.query_prepared(statement, 3)
      assert(result.rows == [['owned']], 'input string belongs to caller')
      result.rows[0][0].replace('mutated result')
      result.columns[0].replace('mutated name')
      assert(db.query_prepared(statement, 3).rows == [['owned']], 'result strings do not alias stored cells')
      JedNative.memory do |other|
        other = other.session
        other.execute('CREATE TABLE items (id i16 PRIMARY KEY, v text)')
        other.execute("INSERT INTO items VALUES (1,'other')")
        assert(other.query_prepared(statement, 1).rows == [['other']], 'statement binds to execution handle')
      end
      db.execute('CREATE TABLE empty_bindings (id i16, label text)')
      empty_cast = db.prepare('SELECT 1 FROM empty_bindings WHERE $1::i16 = 1')
      assert(db.query_prepared(empty_cast, 1).rows.empty?, 'valid empty-table binding')
      error('22003') { db.query_prepared(empty_cast, 32768) }
      assert(db.query_prepared(empty_cast, nil).rows.empty?, 'NULL reuse after invalid binding')
      error('42804') { db.query_prepared(empty_cast, '1') }
      error('42601') { db.query_prepared(empty_cast) }
      error('42601') { db.query_prepared(empty_cast, 1, 2) }
      conflict = db.prepare('SELECT 1 FROM empty_bindings WHERE id = $1 OR label = $1')
      error('42804') { db.query_prepared(conflict, nil) }
      declared_null = db.prepare('SELECT $1::i32')
      result = db.query_prepared(declared_null, nil)
      assert(result.rows == [[nil]] && result.column_types == ['i32'], 'NULL retains declared parameter type')
      assert(db.query_prepared(declared_null, 42).rows == [[42]], 'reuse NULL plan with integer value')
      error('42P18') { db.query('SELECT $1 FROM empty_bindings', 1) }
      error('42P18') { db.query('SELECT $2::i32', 1, 2) }
      no_nulls = db.prepare('UPDATE empty_bindings SET id = $1')
      error('22003') { db.query_prepared(no_nulls, 32768) }
    end
    puts 'ruby-native API: prepared reuse, binding, schema replacement, transaction and script rollback passed'
  end
end

NativeApiTests.run
