# frozen_string_literal: true

require_relative '../lib/jed_native'

# Implicit tree identity, cache bounds and host ownership are host contracts.
# Query semantics remain corpus-owned.
module NativeStatementCacheTests
  module_function
  def assert(value, message) = (raise message unless value)
  def error(code)
    yield
    raise "expected #{code}"
  rescue JedNative::Error => caught
    assert(caught.sqlstate == code, "expected #{code}, got #{caught.sqlstate}: #{caught.message}")
  end

  def cache(database) = database.instance_variable_get(:@native_statement_cache)
  def entries(database) = cache(database).instance_variable_get(:@entries)
  def tree(database, sql) = entries(database).fetch([sql.encoding, sql])[1]

  def ownership_and_limits
    JedNative.memory do |database|
      session = database.session
      sql = +'SELECT $1::i32'
      assert(session.query(sql, 1).rows == [[1]], 'initial query')
      statement = tree(database, sql)
      assert(session.execute(sql.dup, 2).rows == [[2]], 'eager reuse')
      assert(tree(database, sql).equal?(statement), 'equal SQL did not reuse tree')
      sql.replace('SELECT $1::text')
      assert(session.query(sql, 'owned').rows == [['owned']], 'caller SQL mutation')
      assert(session.query('SELECT $1::i32', 3).rows == [[3]], 'mutable key corrupted old entry')
      assert(entries(database).keys.all? { |key| key.frozen? && key[1].frozen? }, 'cache keys retain caller ownership')

      first = session.prepare('SELECT $1::i32')
      second = session.prepare('SELECT $1::i32')
      assert(!first.equal?(second) && !first.equal?(statement), 'public prepare aliases internal statement')
      first.ast[:columns].clear
      assert(session.query('SELECT $1::i32', 4).rows == [[4]], 'public AST mutation changed implicit tree')
      error('42804') { session.query('SELECT $1::i32', '4') }
      error('42601') { session.query('SELECT $1::i32') }
      error('42601') { session.query('SELECT $1::i32', 1, 2) }
      assert(session.query('SELECT $1::i32', nil).rows == [[nil]], 'error poisoned cached binding')

      session.max_sql_length = 4
      error('54000') { session.query('SELECT $1::i32', 1) }
      error('54000') { session.execute('SELECT $1::i32', 1) }
      session.max_sql_length = 0
      [Encoding::UTF_8, Encoding::BINARY, Encoding::US_ASCII].each do |encoding|
        literal = "SELECT 'encoding'".dup.force_encoding(encoding)
        cached = session.query(literal).rows.first.first
        fresh = session.query_prepared(session.prepare(literal)).rows.first.first
        assert(cached.encoding == fresh.encoding, 'equal SQL reused a different source encoding')
      end
      binary = "SELECT '\xff'".b
      session.execute(binary)
      error('22021') { session.execute(binary.dup.force_encoding(Encoding::UTF_8)) }
      before = entries(database).length
      2.times { error('42601') { session.query('SELECT (') } }
      assert(entries(database).length == before, 'parse failures were retained')

      80.times { |i| session.query("SELECT #{i}").close }
      assert(entries(database).length <= JedNative::StatementCache::MAX_ENTRIES, 'entry count is unbounded')
      hot = 'SELECT 123456'
      session.query(hot).close
      hot_tree = tree(database, hot)
      80.times do |i|
        session.query(hot).close
        session.query("SELECT #{i + 1000}").close
      end
      assert(tree(database, hot).equal?(hot_tree), 'frequently used SQL was evicted')
      20.times { |i| session.query("SELECT #{i} /*#{'x' * 8192}*/").close }
      retained = entries(database).keys.sum { |key| key[1].bytesize }
      assert(retained <= JedNative::StatementCache::MAX_SQL_BYTES, 'SQL bytes are unbounded')
      assert(retained == cache(database).instance_variable_get(:@sql_bytes), 'byte accounting drifted')
      oversized = "SELECT 7 /*#{'x' * JedNative::StatementCache::MAX_STATEMENT_BYTES}*/"
      assert(session.query(oversized).rows == [[7]], 'oversized cache bypass changed execution')
      assert(!entries(database).key?([oversized.encoding, oversized]), 'oversized SQL retained')
      session.close
      database.close
      assert(entries(database).empty?, 'database close retains implicit trees')
      error('55000') { session.query('SELECT 1').rows }
      assert(entries(database).empty?, 'closed query repopulated the cache')
    end
  end

  def execution_context
    JedNative.memory do |database|
      reader, writer = database.session, database.session
      writer.execute('CREATE TABLE t(id i32 PRIMARY KEY, v i32)')
      writer.execute('INSERT INTO t VALUES(1,10)')
      explanation = 'EXPLAIN SELECT * FROM t'
      reader.execute(explanation)
      assert(!entries(database).key?([explanation.encoding, explanation]), 'EXPLAIN retains execution annotations')
      sql = 'SELECT v FROM t WHERE id=$1'
      original = reader.query(sql, 1)
      scan_sql = 'SELECT v + $1::i32 AS result FROM t ORDER BY id'
      original_scan = reader.query(scan_sql, 1)
      assert(original.cost.zero? && original_scan.cost.zero?, 'cursor ran before its first pull')
      initial_tree = tree(database, sql)
      writer.execute('UPDATE t SET v=20 WHERE id=1')
      assert(writer.query(sql, 1).rows == [[20]], 'other session reused stale point tree')
      assert(writer.query(scan_sql, 2).rows == [[22]], 'other session reused stale scan tree')
      assert(reader.execute(sql, 1).rows == [[20]], 'implicit cache retained old publication')
      assert(original.rows == [[10]], 'unpulled point cursor lost its old publication')
      assert(original_scan.rows == [[11]], 'unpulled scan cursor lost its old publication or parameters')
      reader.begin
      reader.execute('UPDATE t SET v=30 WHERE id=1')
      assert(reader.query(sql, 1).rows == [[30]], 'pending write')
      reader.execute('SAVEPOINT s')
      reader.execute('UPDATE t SET v=40 WHERE id=1')
      assert(reader.query(sql, 1).rows == [[40]], 'savepoint write')
      reader.execute('ROLLBACK TO s')
      assert(reader.query(sql, 1).rows == [[30]], 'savepoint rollback')
      reader.rollback
      assert(reader.query(sql, 1).rows == [[20]], 'transaction rollback')
      reader.revoke(:select, 't')
      error('42501') { reader.query(sql, 1) }
      reader.reset_privileges
      old_schema = reader.query(sql, 1)
      writer.execute('DROP TABLE t')
      writer.execute('CREATE TABLE t(id i16 PRIMARY KEY, v text)')
      writer.execute("INSERT INTO t VALUES(1,'replacement')")
      result = reader.query(sql, 1)
      assert(result.rows == [['replacement']] && result.column_types == ['text'], 'replacement schema')
      assert(old_schema.rows == [[20]] && old_schema.column_types == ['i32'], 'unpulled cursor lost its old schema')
      error('22003') { reader.query(sql, 32768) }
      assert(tree(database, sql).equal?(initial_tree), 'catalog change required replacing parsed tree')
      prepared = reader.prepare(sql)
      eager, fresh = reader.execute(sql, 1), reader.execute_prepared(prepared, 1)
      assert(eager.rows == fresh.rows && eager.cost == fresh.cost, 'cached execution changed cost')

      setting_sql = "SELECT current_setting('app.value')"
      reader.set_var('app.value', 'reader')
      writer.set_var('app.value', 'writer')
      assert(reader.query(setting_sql).rows == [['reader']], 'reader setting')
      assert(writer.query(setting_sql).rows == [['writer']], 'cross-session setting leak')
      assert(reader.query(setting_sql).rows == [['reader']], 'shared tree retained other context')

      database.execute('CREATE TEMP TABLE local_only(x i32)')
      error('42P01') { database.query('SELECT x FROM local_only').rows }
      assert(database.query(sql, 1).rows == [['replacement']], 'database query helper')
      assert(tree(database, sql).equal?(initial_tree), 'fresh helper did not reuse parsed tree')
      reader.begin
      error('42601') { reader.query('SELECT (') }
      error('25P02') { reader.query(sql, 1).rows }
      reader.rollback
      assert(reader.query(sql, 1).rows == [['replacement']], 'rollback did not recover cached query')
      reader.close
      writer.close
    end
  end

  def host_environments
    first_calls, other_calls = [], []
    first_environment = JedNative::Environment.new.register_function(name: 'cache_host', args: ['i64'], returns: 'i64') do |value|
      first_calls << value
      value + 1
    end
    other_environment = JedNative::Environment.new.register_function(name: 'cache_host', args: ['i64'], returns: 'text') do |value|
      other_calls << value
      "other #{value}"
    end
    sql = 'SELECT cache_host($1::i64)'
    JedNative.memory(environment: first_environment) do |database|
      first = database.session
      other = database.session(environment: other_environment)
      pending = first.query(sql, 7)
      assert(first_calls.empty?, 'implicit host query evaluated during construction')
      shared_tree = tree(database, sql)
      result = other.query(sql, 8)
      assert(result.rows == [['other 8']] && result.column_types == ['text'], 'same SQL leaked another session environment')
      assert(tree(database, sql).equal?(shared_tree), 'session environment replaced the shared parsed tree')
      assert(pending.rows == [[8]] && pending.column_types == ['i64'], 'unpulled host cursor lost its captured environment')
      assert(first.execute(sql, 9).rows == [[10]], 'eager host query reused another environment')
      first.revoke(:execute, 'cache_host')
      error('42501') { first.query(sql, 10) }
      assert(first_calls == [7, 9], 'revoked cached host query invoked its callback')
      first.reset_privileges
      assert(first.query(sql, 11).rows == [[12]], 'host privilege reset did not recover the cached query')

      JedNative.memory(environment: other_environment) do |other_database|
        result = other_database.query(sql, 12)
        assert(result.rows == [['other 12']] && result.column_types == ['text'], 'same SQL leaked another database environment')
        assert(!tree(other_database, sql).equal?(shared_tree), 'implicit trees leaked across database lifetimes')
      end
      missing = database.session(environment: JedNative::Environment.new)
      error('42883') { missing.query(sql, 13).rows }
      assert(first_calls == [7, 9, 11] && other_calls == [8, 12], 'cached host result or wrong callback was reused')
      missing.close
      first.close
      other.close
    end
  end

  def namespaces
    JedNative.memory do |database|
      first, other = database.session, database.session
      first.execute('CREATE TEMP TABLE local_value(v i32)')
      first.execute('INSERT INTO local_value VALUES(7)')
      other.execute('CREATE TEMP TABLE local_value(v text)')
      other.execute("INSERT INTO local_value VALUES('private')")
      ['SELECT v FROM local_value', 'SELECT v FROM temp.local_value'].each do |sql|
        pending = first.query(sql)
        result = other.query(sql)
        assert(result.rows == [['private']] && result.column_types == ['text'], 'shared tree leaked another temp domain')
        assert(pending.rows == [[7]] && pending.column_types == ['i32'], 'unpulled cursor lost its temp domain')
        first.revoke(:select, 'local_value')
        error('42501') { first.query(sql).rows }
        assert(other.query(sql).rows == [['private']], 'privilege change leaked across sessions')
        first.reset_privileges
      end
      error('42P01') { first.query('SELECT v FROM main.local_value').rows }
      error('42P01') { database.query('SELECT v FROM local_value').rows }
      database.execute('CREATE TABLE published(id i32)')
      assert(first.query('SELECT v FROM local_value').rows == [[7]], 'publication replaced a session temp domain')
      assert(other.query('SELECT v FROM local_value').rows == [['private']], 'publication replaced another temp domain')

      database.attach('ref')
      first.execute('CREATE TABLE ref.t(id i32 PRIMARY KEY, v i32)')
      first.execute('INSERT INTO ref.t VALUES(1,10)')
      sql = 'SELECT v FROM ref.t WHERE id=$1'
      assert(first.query(sql, 1).rows == [[10]], 'initial attached query')
      shared_tree = tree(database, sql)
      assert(other.query(sql, 1).rows == [[10]], 'other session did not capture the attachment')
      database.detach('ref')
      [first, other].each { |session| error('42P01') { session.query(sql, 1).rows } }
      database.attach('ref')
      other.execute('CREATE TABLE ref.t(id text PRIMARY KEY, v text)')
      other.execute("INSERT INTO ref.t VALUES('one','replacement')")
      [first, other].each do |session|
        result = session.query(sql, 'one')
        assert(result.rows == [['replacement']] && result.column_types == ['text'], 'reattached query retained old metadata')
        error('42804') { session.query(sql, 1).rows }
      end
      assert(tree(database, sql).equal?(shared_tree), 'namespace change replaced the parsed tree')
      first.close
      other.close
    end
  end

  def reentrant_context_changes
    reader = writer = nil
    pending, calls = [], []
    sql = "SELECT replace_context($1::i64), v, '2000-01-01 00:00:00'::timestamptz FROM context_t WHERE id=1"
    environment = JedNative::Environment.new.register_function(name: 'replace_context', args: ['i64'], returns: 'i64') do |value|
      calls << value
      if calls.length == 1
        reader.timezone = '+02:00'
        writer.execute('DROP TABLE context_t')
        writer.execute('CREATE TABLE context_t(id i32 PRIMARY KEY, v text)')
        writer.execute("INSERT INTO context_t VALUES(1,'replacement')")
        pending << reader.query(sql, value + 1)
      end
      value
    end
    JedNative.memory(environment: environment) do |database|
      reader, writer = database.session, database.session
      writer.execute('CREATE TABLE context_t(id i32 PRIMARY KEY, v i32)')
      writer.execute('INSERT INTO context_t VALUES(1,10)')
      zone_sql = "SELECT '2000-01-01 00:00:00'::timestamptz"
      original_zone = reader.query(zone_sql).rows.first.first
      outer = reader.query(sql, 7)
      assert(calls.empty?, 'context-changing callback ran before the first pull')
      shared_tree = tree(database, sql)
      assert(outer.rows == [[7, 10, original_zone]] && outer.column_types == %w[i64 i32 timestamptz],
             'callback changed its executing cursor catalog or timezone')
      assert(pending.length == 1 && pending.first.cost.zero?, 'nested cached query ran during callback construction')
      shifted_zone = reader.query(zone_sql).rows.first.first
      assert(shifted_zone != original_zone, 'callback did not change the live session timezone')
      assert(pending.first.rows == [[8, 'replacement', shifted_zone]] && pending.first.column_types == %w[i64 text timestamptz],
             'nested cached query did not capture the new catalog and timezone')
      assert(reader.execute(sql, 9).rows == [[9, 'replacement', shifted_zone]], 'eager cached query retained pre-callback context')
      assert(tree(database, sql).equal?(shared_tree) && calls == [7, 8, 9], 'reentrant query replaced the tree or reused a host result')
      reader.close
      writer.close
    end
  end

  ownership_and_limits
  execution_context
  host_environments
  namespaces
  reentrant_context_changes
  puts 'ruby-native implicit statements: bounded ownership, bindings, snapshots, host contexts, namespaces and reentrant capture passed'
end
