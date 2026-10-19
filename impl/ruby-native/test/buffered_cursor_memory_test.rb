# frozen_string_literal: true

require_relative '../lib/jed_native'
require 'tmpdir'

# The corpus pins the operator peaks. These probes cover the account while a
# buffered result is suspended, and its transfer to a host-owned collector.
module NativeBufferedCursorMemoryTests
  module_function

  def assert(value, message) = (raise(message) unless value)

  def error(code)
    yield
    raise "expected #{code}"
  rescue JedNative::Error => failure
    assert(failure.sqlstate == code, "expected #{code}, got #{failure.sqlstate}")
  end

  def decorated_owner_identity(engine)
    engine = engine.dup
    account = JedNative::QueryMemoryAccount.new(1024)
    engine.instance_variable_set(:@query_memory, account)
    engine.instance_variable_set(:@query_memory_contexts, [])
    columns = [{ name: 'id', table: 't', type: 'i32' },
               { name: 'v', table: 't', type: 'text', collation: 'C' },
               { name: 'unused', table: 't', type: 'text', qualified_only: true }]
    relation = JedNative::Relation.new(columns: columns, rows: [[7, 'ä', 'unselected payload']])
    mask = [false, true, false].freeze
    owner = engine.query_memory_owner
    engine.query_memory_attach(relation, owner, mask)
    engine.query_memory_add_row(owner, relation.rows.first, columns, mask)
    relation.instance_variable_set(:@execution_probe, Object.new)
    label = engine.collation_label('unicode', explicit: true)
    output = columns.map { |column| column[:type] == 'text' ? column.merge(collation: 'unicode', collation_label: label) : column }
    decorated = engine.decorate_collation_relation(relation, output)
    assert(decorated.instance_variable_get(:@native_memory_owner).equal?(owner), 'collation decoration replaced the owner token')
    assert(decorated.instance_variable_get(:@native_memory_mask).equal?(mask), 'collation decoration lost the payload mask')
    assert(decorated.columns == columns.each_with_index.map { |column, i| column.merge(collation: output[i][:collation], collation_label: output[i][:collation_label]) }, 'collation decoration changed other column metadata')
    assert(decorated.rows.first[1].is_a?(JedNative::CollatedText) && decorated.rows.first[1].collation_label.equal?(label), 'collation decoration lost the requested text label')
    assert(relation.rows.first[1].instance_of?(String) && !decorated.instance_variable_defined?(:@execution_probe), 'collation decoration mutated input values or copied execution state')
    assert(account.used == 130 && owner.bytes == 130, 'collation decoration re-reserved a masked buffer')
    engine.query_memory_release(owner, engine.query_memory_row_bytes(decorated.rows.first, decorated.columns, mask))
    assert(account.used.zero? && owner.bytes.zero?, 'decorated row released the wrong payload charge')
    engine.query_memory_release(relation)
    assert(account.used.zero?, 'aliased ownership was released twice')
  end

  def loaded_collation_ownership
    environment = JedNative::Environment.new.load_unicode_data(File.binread(File.expand_path('../../../spec/collation/fixtures/unicode.jucd', __dir__)))
    JedNative.memory(environment: environment) do |database|
      decorated_owner_identity(database.engine)
      session = database.session
      session.execute('CREATE TABLE labeled_t(id i32 PRIMARY KEY, v text COLLATE unicode, pad text)')
      session.execute("INSERT INTO labeled_t VALUES (1, 'a', 'xxxxxxxxxx'), (2, 'bb', 'yyyyyyyyyy'), (3, NULL, 'zzzzzzzzzz')")
      cases = [
        ["VALUES ('ä' COLLATE unicode), ('z' COLLATE unicode)", 131, [['ä'], ['z']], [65, 0], 0],
        ['WITH c AS MATERIALIZED (SELECT id, v FROM labeled_t) SELECT * FROM c', 873, [[1, 'a'], [2, 'bb'], [3, nil]], [194, 96, 0], 1]
      ]
      cases.each do |sql, allowance, expected, remaining, text_index|
        session.max_query_memory_bytes = allowance
        cursor = session.query(sql)
        execution = cursor.instance_variable_get(:@engine)
        account = nil
        expected.each_with_index do |row, index|
          actual = cursor.next
          account ||= execution.instance_variable_get(:@query_memory)
          assert(actual == row && account && account.used == remaining[index], 'collation decoration lost a suspended buffered owner')
          text = actual[text_index]
          assert(!text || (text.is_a?(JedNative::CollatedText) && text.collation_label.name == 'unicode' && text.collation_label.table.equal?(environment.collations['unicode'])), 'buffered transfer changed loaded collation metadata')
        end
        assert(account.peak == allowance, 'loaded collation changed the exact buffer peak')
        cursor.close
        assert(account.used.zero? && database.instance_variable_get(:@pins).empty?, 'decorated buffer retained an account or snapshot')
        assert(session.query(sql).rows == expected, 'loaded collation collector double-counted transferred rows')
        cursor = session.query(sql)
        execution = cursor.instance_variable_get(:@engine)
        cursor.next
        account = execution.instance_variable_get(:@query_memory)
        cursor.close
        assert(account.used.zero?, 'loaded collation early close retained unread rows')
        session.max_query_memory_bytes = allowance - 1
        error('54P05') { session.query(sql).each.to_a }
        assert(database.instance_variable_get(:@pins).empty?, 'decorated buffer admission failure retained a snapshot')
      end
      session.close
    end
  end

  def nested_preflight(database, calls)
    session = database.session
    session.execute_script(<<~SQL)
      CREATE TABLE a (id i32 PRIMARY KEY, k i32, v i32);
      CREATE TABLE b (id i32 PRIMARY KEY, k i32, w i32);
      CREATE TABLE c (id i32 PRIMARY KEY, k i32);
      INSERT INTO a VALUES (1, 1, 10), (2, 2, 20), (3, 3, NULL), (4, 9, 40);
      INSERT INTO b VALUES (11, 1, 5), (12, 2, NULL), (13, 2, 7), (14, 7, 0), (15, 8, 1);
      INSERT INTO c VALUES (21, 1), (22, 2), (23, 3);
    SQL
    cases = [
      ['SELECT a.id, b.id FROM a JOIN b ON a.k = b.k WHERE a.v >= 20 AND EXISTS (SELECT 1 FROM c WHERE c.id = 22) ORDER BY a.id, b.id', [], [[2, 12], [2, 13]], 31],
      ['SELECT a.id, b.id FROM a JOIN b ON a.k = b.k WHERE b.w > (SELECT max(k) FROM c) AND a.v < 100 ORDER BY a.id, b.id', [], [[1, 11], [2, 13]], 43],
      ['SELECT DISTINCT k FROM c WHERE k <= (SELECT max(k) FROM c) ORDER BY k', [], [[1], [2], [3]], nil],
      ['SELECT a.id, b.id FROM a JOIN b ON a.k = b.k WHERE b.w > (SELECT k FROM c WHERE id = $1) AND a.v < 100 ORDER BY a.id, b.id', [23], [[1, 11], [2, 13]], nil]
    ]
    [0, 65_536].each do |budget|
      session.max_query_memory_bytes = budget
      cases.each do |sql, params, expected, known_cost|
        eager = session.execute(sql, *params)
        assert(eager.rows == expected && (!known_cost || eager.cost == known_cost), 'nested preflight eager fixture differs from corpus')
        prepared = session.prepare(sql)
        [false, true].each do |prepared_api|
          [false, true].each do |collector|
            cursor = prepared_api ? session.query_prepared(prepared, *params) : session.query(sql, *params)
            assert(cursor.cost.zero?, 'spill preflight ran before the first pull')
            actual = collector ? cursor.rows : cursor.each.to_a
            assert(actual == expected, 'nested preflight emitted inner rows or lost outer rows')
            assert(cursor.columns == eager.columns && cursor.column_types == eager.column_types, 'nested preflight replaced outer result metadata')
            assert(cursor.cost == eager.cost, 'nested preflight changed query cost')
            assert(database.instance_variable_get(:@pins).empty? && cursor.instance_variable_get(:@engine).nil?, 'nested preflight retained a drained snapshot')
          end
        end
      end
    end

    session.max_query_memory_bytes = 0
    # Shape probing can fold an independent body. Its projection must execute
    # lazily inside the root cursor, without yielding the body's row to its host.
    sql = 'SELECT a.id, b.id FROM a JOIN b ON a.k = b.k WHERE a.v >= 20 AND EXISTS (SELECT cursor_nested_probe(k) FROM c WHERE c.id = 22) ORDER BY a.id, b.id'
    calls.clear
    cursor = session.query(sql)
    assert(calls.empty? && cursor.cost.zero?, 'nested host projection ran while opening the cursor')
    assert(cursor.next == [2, 12] && calls == [2], 'nested body escaped its root cursor or ran more than once')
    cost = cursor.cost
    cursor.close
    assert(cursor.cost == cost && calls == [2] && database.instance_variable_get(:@pins).empty?, 'closing nested preflight cursor performed more work or retained a pin')

    [
      ['SELECT a.id, b.id FROM a JOIN b ON a.k = b.k WHERE b.w > (SELECT k FROM c)', '21000'],
      ['SELECT a.id, b.id FROM a JOIN b ON a.k = b.k WHERE b.w > (SELECT max(k) / 0 FROM c)', '22012']
    ].each do |sql, state|
      error(state) { session.execute(sql) }
      expected_cost = session.engine.cost
      cursor = session.query(sql)
      assert(cursor.cost.zero?, 'nested error escaped before the first pull')
      error(state) { cursor.next }
      assert(cursor.cost == expected_cost && cursor.closed?, 'nested failure changed cost or left its cursor open')
      assert(database.instance_variable_get(:@pins).empty?, 'nested failure retained a snapshot pin')
    end
  ensure
    cursor&.close
    session&.close
  end

  def nested_preflight_storage_modes
    calls = []
    environment = JedNative::Environment.new.register_function(name: 'cursor_nested_probe', args: ['i32'], returns: 'i32') do |value|
      calls << value
      value
    end
    JedNative.memory(environment: environment) { |database| nested_preflight(database, calls) }
    Dir.mktmpdir('jed-cursor-preflight-') do |directory|
      JedNative.create(File.join(directory, 'data.jed'), environment: environment, locking: :exclusive) do |database|
        nested_preflight(database, calls)
      end
    end
  end

  def run
    JedNative.memory do |database|
      session = database.session
      session.execute('CREATE TABLE buffered_t(id i32 PRIMARY KEY, v text, pad text)')
      session.execute("INSERT INTO buffered_t VALUES (1, 'a', 'xxxxxxxxxx'), (2, 'bb', 'yyyyyyyyyy'), (3, NULL, 'zzzzzzzzzz')")
      sql = 'WITH c AS MATERIALIZED (SELECT id, v FROM buffered_t) SELECT * FROM c'
      expected = [[1, 'a'], [2, 'bb'], [3, nil]]

      session.max_query_memory_bytes = 873
      cursor = session.query(sql)
      execution = cursor.instance_variable_get(:@engine)
      assert(cursor.cost.zero? && execution.instance_variable_get(:@query_memory).nil?, 'buffered cursor executed at open')
      session.max_query_memory_bytes = 1
      assert(cursor.next == expected[0], 'buffered cursor lost its captured budget')
      account = execution.instance_variable_get(:@query_memory)
      assert(account && account.limit == 873 && account.peak == 873, 'WITH omitted its buffered result admission')
      assert(account.used == 194, 'first buffered row was not released before yield')
      assert(cursor.next == expected[1] && account.used == 96, 'second buffered row was not released')
      cursor.close
      assert(account.used.zero?, 'early close retained unread buffered rows')
      assert(cursor.instance_variable_get(:@engine).nil? && database.instance_variable_get(:@pins).empty?, 'early close retained the cursor snapshot')

      session.max_query_memory_bytes = 873
      assert(session.query(sql).each.to_a == expected, 'external WITH drain failed at exact allowance')
      assert(session.query(sql).rows == expected, 'owned collector double-counted transferred WITH rows')
      session.max_query_memory_bytes = 872
      error('54P05') { session.query(sql).each.to_a }
      error('54P05') { session.query(sql).rows }
      assert(database.instance_variable_get(:@pins).empty?, 'failed buffered read retained a pin')

      [['VALUES (1), (2)', 128], ['SELECT 1 UNION ALL SELECT 2', 160]].each do |query, allowance|
        session.max_query_memory_bytes = allowance
        cursor = session.query(query)
        execution = cursor.instance_variable_get(:@engine)
        assert(cursor.next == [1], 'buffered query yielded the wrong first row')
        account = execution.instance_variable_get(:@query_memory)
        assert(account && account.used == 64, 'buffered query lost its live account or failed to transfer a row')
        assert(cursor.next == [2] && account.used.zero?, 'buffered query did not release its final row')
        cursor.close
        assert(session.query(query).rows == [[1], [2]], 'collector double-counted a buffered result')
      end

      session.max_query_memory_bytes = 872
      session.begin
      error('54P05') { session.query(sql).each.to_a }
      error('25P02') { session.execute('SELECT 1') }
      session.rollback
      session.max_query_memory_bytes = 873
      assert(session.query(sql).rows == expected, 'buffered failure survived rollback')
      session.close
    end
    loaded_collation_ownership
    nested_preflight_storage_modes
    puts 'ruby-native buffered cursor memory: WITH admission, captured accounts, row transfers, loaded collations, nested preflight, collectors and early-close cleanup passed'
  end
end

NativeBufferedCursorMemoryTests.run
