# frozen_string_literal: true

require 'tmpdir'
require_relative '../lib/jed_native'

# Mutable public trees, internal traversal boundaries and caller-owned scope
# hashes cannot be expressed by a shared SQL transcript.
module NativeQueryWalkTests
  module_function

  APIS = %i[query_prepared execute_prepared query execute].freeze
  def assert(value, message) = (raise message unless value)
  def error(code)
    yield
    raise "expected #{code}"
  rescue JedNative::Error => caught
    assert(caught.sqlstate == code, "expected #{code}, got #{caught.sqlstate}: #{caught.message}")
  end

  def configure(current, disabled)
    current.engine.instance_variable_set(:@native_query_walks_disabled, disabled)
    current
  end

  def shape(engine, node)
    [engine.send(:local_aggregate?, node), engine.send(:contains_aggregate?, node),
     engine.send(:window_nodes, node).map(&:object_id)]
  end

  def ordinary_calls(owner, method)
    count = 0
    trace = TracePoint.new(:call) { |event| count += 1 if event.defined_class == owner && event.method_id == method }
    result = trace.enable { yield }
    [result, count]
  end

  def helper_semantics
    JedNative.memory do |database|
      enabled = configure(database.session, false)
      disabled = configure(database.session, true)
      aggregate = { kind: :call, name: 'sum', args: [{ kind: :literal, type: 'i64', value: 1 }] }
      window = { kind: :call, name: 'row_number', args: [], over: {} }
      cases = [nil, false, 7, 'sum', {}, [], aggregate, window,
               [aggregate], [[aggregate]], { parts: [[aggregate]] },
               { kind: :subquery, query: { columns: [{ expr: aggregate }, { expr: window }] } },
               { kind: :exists, query: { columns: [{ expr: aggregate }, { expr: window }] } },
               { kind: :array_query, query: { columns: [{ expr: aggregate }, { expr: window }] } },
               { kind: :other, query: aggregate, with: [window] },
               { kind: :case, branches: [[{ kind: :literal, value: true }, aggregate]], else: window },
               { kind: :call, name: 'sum', args: [window], over: {} },
               { kind: :call, name: 'sum', args: [], over: false },
               { kind: :other, over: {}, child: [window, window] }]
      cases.each do |node|
        expected = shape(disabled.engine, node)
        3.times { assert(shape(enabled.engine, node) == expected, "query walk changed traversal of #{node.inspect}") }
      end
      assert(shape(enabled.engine, [[aggregate]])[0, 2] == [true, false], 'aggregate helpers lost their distinct Array-root semantics')
      assert(shape(enabled.engine, { parts: [[aggregate]] })[0, 2] == [true, false], 'contains_aggregate expanded nested arrays')
      assert(shape(enabled.engine, { kind: :subquery, query: aggregate })[0, 2] == [false, true], 'local aggregate crossed its subquery boundary')
      assert(shape(enabled.engine, { kind: :other, query: aggregate, with: [window] }) == [true, true, []], 'query/with exclusions were unified incorrectly')

      [window, [window, window], { left: window, right: [window] }].each do |node|
        sentinel = { existing: true }
        actual, expected = [sentinel], [sentinel]
        returned = enabled.engine.send(:window_nodes, node, actual)
        disabled.engine.send(:window_nodes, node, expected)
        assert(returned.equal?(actual) && actual.map(&:object_id) == expected.map(&:object_id), 'window collection changed accumulator identity, order or duplicates')
      end
      first = { kind: :call, name: 'row_number', args: [], over: {} }
      second = Marshal.load(Marshal.dump(first))
      2.times { enabled.engine.send(:window_nodes, first) }
      assert(enabled.engine.send(:window_nodes, second).first.equal?(second), 'equal syntax returned another tree\'s window node')
      node = { kind: :call, name: +'abs', args: [] }
      mutations = [-> { node[:name].replace('sum') }, -> { node[:over] = {} },
                   -> { node[:kind] = :other }, -> { node[:children] = [[aggregate, window]] },
                   -> { node[:kind] = :subquery }, -> { node[:kind] = :call; node[:over] = nil }]
      mutations.each do |change|
        2.times { shape(enabled.engine, node) }
        change.call
        assert(shape(enabled.engine, node) == shape(disabled.engine, node), 'query walk missed a tree mutation')
      end
      nested_default = { child: {} }
      2.times { assert(!enabled.engine.send(:local_aggregate?, nested_default), 'default-mutation fixture already contained an aggregate') }
      nested_default[:child].default_proc = ->(_hash, key) { key == :kind ? :call : key == :name ? 'sum' : nil }
      assert(shape(enabled.engine, nested_default) == shape(disabled.engine, nested_default) &&
        enabled.engine.send(:local_aggregate?, nested_default), 'query walk changed nested Hash default semantics')
      [:default, :identity, :subclass].each do |change|
        child = { kind: :call, name: 'sum', args: [] }
        outer = { parts: [child] }
        2.times { shape(enabled.engine, outer) }
        case change
        when :default then child.default = {}
        when :identity then child.compare_by_identity
        when :subclass then outer[:parts][0] = Class.new(Hash).new.update(child)
        end
        assert(shape(enabled.engine, outer) == shape(disabled.engine, outer), "Hash-inside-Array #{change} mutation changed traversal")
      end

      plain = { kind: :binary, op: '+', left: { kind: :column, name: 'id' }, right: { kind: :literal, value: 1 } }
      enabled.engine.send(:local_aggregate?, plain)
      actual, skipped = ordinary_calls(JedNative::AggregateExecution, :local_aggregate?) { enabled.engine.send(:local_aggregate?, plain) }
      expected, ordinary = ordinary_calls(JedNative::AggregateExecution, :local_aggregate?) { disabled.engine.send(:local_aggregate?, plain) }
      assert(actual == expected && skipped.zero? && ordinary.positive?, 'helper comparisons did not exercise the independent query walkers')
      enabled.engine.instance_variable_set(:@native_query_walks_disabled, true)
      _, ordinary = ordinary_calls(JedNative::AggregateExecution, :local_aggregate?) { enabled.engine.send(:local_aggregate?, plain) }
      assert(ordinary.positive? && shape(enabled.engine, aggregate) == shape(disabled.engine, aggregate), 'disable switch did not restore the original walker')
    ensure
      enabled&.close
      disabled&.close
    end
  end

  def outcome(current, statement, sql, params, api)
    lifetime, phase, rows = current.lifetime_cost, :admission, []
    result = api.to_s.end_with?('_prepared') ? current.public_send(api, statement, *params) : current.public_send(api, sql, *params)
    phase = :pull
    result.is_a?(JedNative::Rows) ? result.each { |row| rows << row.to_a } : rows = result.rows
    [:rows, rows, result.columns, result.column_types, result.cost, current.lifetime_cost - lifetime]
  rescue JedNative::Error => caught
    [:error, phase, caught.sqlstate, caught.message, rows, result&.cost || current.engine.cost, current.lifetime_cost - lifetime]
  ensure
    result&.close
  end

  def compare(database, sql, params = [], api: :query_prepared, setting: nil, limit: 0)
    enabled = configure(database.session, false)
    disabled = configure(database.session, true)
    statements = [enabled, disabled].map { |current| current.prepare(sql) }
    2.times do
      actual = outcome(enabled, statements[0], sql, params, api)
      expected = outcome(disabled, statements[1], sql, params, api)
      assert(actual == expected, "warm query walk changed #{sql}: #{actual.inspect} != #{expected.inspect}")
    end
    [enabled, disabled].each do |current|
      current.public_send("#{setting}=", setting == :lifetime_max_cost ? current.lifetime_cost + limit : limit) if setting
    end
    actual = outcome(enabled, statements[0], sql, params, api)
    expected = outcome(disabled, statements[1], sql, params, api)
    assert(actual == expected, "query walk changed #{api}/#{setting}=#{limit}: #{actual.inspect} != #{expected.inspect}")
    actual
  ensure
    enabled&.close
    disabled&.close
  end

  def setup(database)
    database.execute_script(<<~SQL)
      CREATE TABLE qs_a(id i32 PRIMARY KEY, k i32, label text);
      CREATE TABLE qs_b(id i32 PRIMARY KEY, v i32);
      CREATE TABLE qs_c(id i32 PRIMARY KEY, v i32);
      INSERT INTO qs_a VALUES(1,2,'a'),(2,3,'b'),(3,9,NULL);
      INSERT INTO qs_b VALUES(2,20),(3,30);
      INSERT INTO qs_c VALUES(1,100),(2,200),(3,300);
    SQL
  end

  def public_queries(database)
    cases = [
      ['SELECT a.id,b.v,c.v FROM qs_a a JOIN qs_b b ON b.id=a.k JOIN qs_c c ON c.id=a.id WHERE a.id=$1 ORDER BY a.id', [1]],
      ['SELECT a.label,b.v FROM qs_a a LEFT JOIN qs_b b ON b.id=a.k ORDER BY a.id', []],
      ['SELECT count(*),sum(b.v) FROM qs_a a JOIN qs_b b ON b.id=a.k', []],
      ['SELECT a.id, row_number() OVER(ORDER BY a.id) FROM qs_a a ORDER BY a.id', []],
      ['WITH q AS(SELECT id FROM qs_a WHERE id=$1) SELECT id FROM q', [2]],
      ['SELECT a.id,(SELECT max(v) FROM qs_b) FROM qs_a a ORDER BY a.id', []],
      ['SELECT a.id,10/(a.id-$1) FROM qs_a a ORDER BY a.id', [2]],
      ['SELECT a.id FROM qs_a a WHERE sum(a.id)>0', []],
      ['SELECT a.id FROM qs_a a WHERE row_number() OVER()>0', []]
    ]
    APIS.each { |api| cases.each { |sql, params| compare(database, sql, params, api: api) } }
    sql, params = cases.first
    base = compare(database, sql, params)
    assert(base.first == :rows, 'cost-prefix fixture did not return rows')
    (1..base[4] + 1).each do |limit|
      %i[max_cost lifetime_max_cost].each { |setting| compare(database, sql, params, setting: setting, limit: limit) }
    end
    [1, 64, 192, 512, 4096].each { |limit| compare(database, sql, params, setting: :max_query_memory_bytes, limit: limit) }
  end

  def long_membership(database)
    database.execute_script('CREATE TABLE qs_in(id i32 PRIMARY KEY, a i32); INSERT INTO qs_in VALUES(1,0),(2,239),(3,999),(4,NULL)')
    values = (0...240).to_a.join(',')
    nullable = ((0...238).to_a + [239, 'NULL']).join(',')
    cases = [
      ["SELECT id FROM qs_in WHERE a IN(#{values}) ORDER BY id", [[1], [2]]],
      ["SELECT id FROM qs_in WHERE a NOT IN(#{values}) ORDER BY id", [[3]]],
      ["SELECT id,a IN(#{values}),a NOT IN(#{values}) FROM qs_in ORDER BY id",
       [[1, true, false], [2, true, false], [3, false, true], [4, nil, nil]]],
      ["SELECT id,a IN(#{nullable}),a NOT IN(#{nullable}) FROM qs_in ORDER BY id",
       [[1, true, false], [2, true, false], [3, nil, nil], [4, nil, nil]]]
    ]
    failure = "SELECT id FROM qs_in WHERE a IN(#{(0...239).to_a.join(',')},10/(a-a)) ORDER BY id"
    APIS.each do |api|
      enabled = configure(database.session, false)
      disabled = configure(database.session, true)
      cases.each do |sql, rows|
        left, right = [enabled, disabled].map { |current| current.prepare(sql) }
        actual = outcome(enabled, left, sql, [], api)
        expected = outcome(disabled, right, sql, [], api)
        assert(actual == expected && actual[0, 2] == [:rows, rows], "long membership changed #{api} values, metadata or cost")
      end
      left, right = [enabled, disabled].map { |current| current.prepare(failure) }
      actual = outcome(enabled, left, failure, [], api)
      expected = outcome(disabled, right, failure, [], api)
      assert(actual == expected && actual[0] == :error && actual[2] == '22012', 'long membership changed lazy error order or costs')
      sql, rows = cases.first
      left, right = [enabled, disabled].map { |current| current.prepare(sql) }
      actual = outcome(enabled, left, sql, [], api)
      expected = outcome(disabled, right, sql, [], api)
      assert(actual == expected && actual[0, 2] == [:rows, rows], 'long estimator/evaluator state did not recover after an error')
    ensure
      enabled&.close
      disabled&.close
    end
  end

  def opaque_guards
    fixture = File.binread(File.expand_path('../../../spec/fileformat/fixtures/unparsable_definitions.jed', __dir__))
    JedNative.from_image(fixture) do |database|
      # Removing views here isolates the no-view guard from catalog-dependent
      # view validation. Opaque table definitions remain the original fixture.
      database.engine.instance_variable_get(:@views).clear
      sql = 'WITH q AS(UPDATE c SET v=1 WHERE id=999 RETURNING id) SELECT id FROM q'
      APIS.each do |api|
        result = compare(database, sql, [], api: api)
        assert(result.first == :error && result[2] == 'XX003' && result[-2, 2] == [0, 0], 'no-view read guard skipped a writable CTE opaque definition')
      end
      current = configure(database.session, false)
      engine = current.engine
      read = current.prepare('SELECT id FROM t').ast
      2.times { engine.send(:validate_opaque_statement, read) }
      engine.instance_variable_set(:@native_query_walks_disabled, true)
      engine.send(:validate_opaque_statement, read)
      engine.instance_variable_set(:@native_query_walks_disabled, false)
      nested = { kind: :select, nested: [[{ kind: :update, table: 'c', assignments: [{ column: 'v' }] }]] }
      error('XX003') { engine.send(:validate_opaque_statement, nested) }
      # The same read tree must not keep the no-view result after the catalog
      # acquires a definition which this parser cannot interpret.
      engine.instance_variable_get(:@views)['t'] = { name: 't', query: { kind: :opaque_definition, detail: 'new opaque t' } }
      error('XX003') { engine.send(:validate_opaque_statement, read) }
    ensure
      current&.close
    end
    JedNative.from_image(fixture) do |database|
      ['SELECT * FROM r', 'SELECT * FROM jed_columns WHERE table_name=\'r\''].each do |sql|
        result = compare(database, sql)
        assert(result.first == :error && result[2] == 'XX003', 'query walk skipped opaque view validation')
      end
      result = compare(database, 'WITH r(id) AS(VALUES(7)) SELECT id FROM r')
      assert(result[0, 2] == [:rows, [[7]]], 'opaque view guard ignored CTE shadowing')
      enabled = configure(database.session, false)
      disabled = configure(database.session, true)
      plain = { kind: :table, name: 'r' }
      shadow = { kind: :select, with: [{ name: 'r', query: { kind: :values, rows: [] } }], from: [plain] }
      [enabled, disabled].each do |current|
        scope = {}
        current.engine.send(:validate_opaque_query_tree, shadow, scope)
        assert(scope.empty?, 'nested CTE validation mutated its caller scope')
        error('XX003') { current.engine.send(:validate_opaque_query_tree, [shadow, plain], scope) }
        assert(scope.empty?, 'nested CTE shadowing leaked into a sibling scope')
        [Hash.new(true), Hash.new { |_hash, key| key == 'r' }, { 'r' => true }].each do |seeded|
          before = seeded.dup
          default, default_proc = seeded.default, seeded.default_proc
          current.engine.send(:validate_opaque_query_tree, [shadow, plain], seeded)
          assert(seeded == before && seeded.default == default && seeded.default_proc.equal?(default_proc),
            'opaque validation changed a seeded or default CTE scope')
        end
        # A default proc can mutate its receiver on lookup. The original
        # validator copies each local scope, so these writes never escape to
        # the caller's hash even when this node introduces no WITH bindings.
        seeded = Hash.new { |scope_hash, name| scope_hash[name] = true }
        original_default = seeded.default_proc
        current.engine.send(:validate_opaque_query_tree, plain, seeded)
        assert(seeded.empty? && seeded.default_proc.equal?(original_default),
          'no-WITH scope reuse exposed default-proc writes to the caller')
      end
    ensure
      enabled&.close
      disabled&.close
    end
  end

  def public_write_mutation
    JedNative.memory do |database|
      database.execute_script('CREATE TABLE qs_write(id i32 PRIMARY KEY); INSERT INTO qs_write VALUES(1)')
      current = configure(database.session(read_only: true), false)
      statement = current.prepare('SELECT id FROM qs_write')
      2.times { assert(current.query_prepared(statement).rows == [[1]], 'readonly mutation fixture did not warm a read') }
      statement.ast.replace(kind: :delete, table: 'qs_write')
      error('25006') { current.query_prepared(statement) }
      assert(database.query('SELECT id FROM qs_write').rows == [[1]], 'stale read classification admitted a write')
    ensure
      current&.close
    end
  end

  def source_metadata
    JedNative.memory do |database|
      setup(database)
      current = configure(database.session, false)
      control = configure(database.session, true)
      sql = 'SELECT b.id,c.v FROM qs_b b JOIN qs_c c ON c.id=b.id WHERE b.id=$1'
      statement, ordinary = [current, control].map { |session| session.prepare(sql) }
      3.times do
        assert(outcome(current, statement, sql, [2], :query_prepared) ==
          outcome(control, ordinary, sql, [2], :query_prepared), 'source metadata changed a prepared result')
      end
      actual, hits = ordinary_calls(JedNative::QueryColumns, :query_source_columns) { outcome(current, statement, sql, [3], :query_prepared) }
      expected, misses = ordinary_calls(JedNative::QueryColumns, :query_source_columns) { outcome(control, ordinary, sql, [3], :query_prepared) }
      assert(actual == expected && hits < misses, 'prepared source metadata never replaced original descriptor construction')
      cache = current.engine.instance_variable_get(:@native_join_resolution_plans)
      entry = cache.values.find { |item| !item.sources.empty? }
      assert(entry, 'prepared query did not retain source metadata')
      assert(control.engine.instance_variable_get(:@native_join_resolution_plans).values.all? { |item| item.sources.empty? },
        'disabled control retained source metadata')
      saved = entry.sources.values.first
      assert(saved.frozen? && saved.all?(&:frozen?), 'retained source metadata is mutable')
      engine = current.engine
      frame = JedNative::PreparedJoins::Frame.new(entry: entry, depth: 0, plans: {})
      engine.instance_variable_set(:@native_join_resolution_frame, frame)
      source = { kind: :table, name: +'qs_b', alias: +'fresh_alias' }
      first = engine.send(:query_source_columns, source, {}, [], nil)
      assert(engine.send(:query_source_columns, source, {}, [], nil).equal?(first), 'equal source metadata did not reuse its snapshot')
      source[:alias].replace('mutated_alias')
      second = engine.send(:query_source_columns, source, {}, [], nil)
      assert(first.all? { |column| column[:table] == 'fresh_alias' } &&
        second.all? { |column| column[:table] == 'mutated_alias' }, 'mutable alias changed old metadata or reused the old label')
      source[:name].replace('qs_c')
      assert(engine.send(:query_source_columns, source, {}, [], nil).any? { |column| column[:name] == 'v' },
        'mutable table name did not re-resolve source columns')
      12.times do |index|
        engine.send(:query_source_columns, { kind: :table, name: 'qs_b', alias: "alias_#{index}" }, {}, [], nil)
      end
      assert(entry.sources.length == JedNative::PreparedJoins::MAX_INPUTS &&
        entry.source_units <= JedNative::PreparedJoins::KEY_UNITS &&
        entry.source_bytes <= JedNative::PreparedJoins::KEY_BYTES, 'source metadata exceeded its entry/unit/byte bounds')
      engine.instance_variable_set(:@native_query_walks_disabled, true)
      fresh, calls = ordinary_calls(JedNative::QueryColumns, :query_source_columns) do
        engine.send(:query_source_columns, { kind: :table, name: 'qs_b', alias: 'fresh_alias' }, {}, [], nil)
      end
      assert(calls == 1 && fresh == first && !fresh.equal?(first), 'disable switch failed to bypass retained source metadata')
      engine.instance_variable_set(:@native_query_walks_disabled, false)
      tables = engine.instance_variable_get(:@tables)
      original = tables.fetch('qs_b')
      tables['qs_b'] = original.merge(columns: original[:columns].map { |column| column[:name] == 'v' ? column.merge(name: 'changed') : column })
      changed, calls = ordinary_calls(JedNative::QueryColumns, :query_source_columns) do
        engine.send(:query_source_columns, { kind: :table, name: 'qs_b', alias: 'fresh_alias' }, {}, [], nil)
      end
      assert(calls == 1 && changed.any? { |column| column[:name] == 'changed' } && first.none? { |column| column[:name] == 'changed' },
        'replaced table descriptor reused old source metadata')
      tables['qs_b'] = original
      engine.instance_variable_set(:@native_join_resolution_frame, nil)
      empty = JedNative::PreparedJoins::Entry.new(source_units: 0, source_bytes: 0)
      owned = { values: [+'owned'], columns: [{ name: +'id', type: +'i32' }] }
      snapshot, units, bytes = engine.send(:prepared_join_source_snapshot, owned, empty)
      owned[:values].first.replace('changed')
      assert(snapshot.frozen? && snapshot[:values].frozen? && snapshot[:values].first.frozen? &&
        snapshot[:values] == ['owned'] && units.positive? && bytes.positive?, 'source snapshot borrowed caller metadata')
      maximum = JedNative::PreparedJoins::KEY_UNITS
      assert(engine.send(:prepared_join_source_snapshot, Array.new(maximum - 1, 0), empty) &&
        engine.send(:prepared_join_source_snapshot, Array.new(maximum, 0), empty).nil?, 'source metadata unit boundary changed')
      maximum = JedNative::PreparedJoins::KEY_BYTES
      assert(engine.send(:prepared_join_source_snapshot, 'x' * maximum, empty) &&
        engine.send(:prepared_join_source_snapshot, ['x' * maximum, 'y'], empty).nil?, 'source metadata cumulative byte boundary changed')
      empty.source_units = JedNative::PreparedJoins::KEY_UNITS
      assert(engine.send(:prepared_join_source_snapshot, nil, empty).nil?, 'source metadata ignored prior retained units')
      empty.source_units = 0
      empty.source_bytes = maximum
      assert(engine.send(:prepared_join_source_snapshot, 'x', empty).nil?, 'source metadata ignored prior retained bytes')
      [current, current.engine, statement, JedNative::Relation.new(columns: [], rows: []), Object.new].each do |value|
        assert(engine.send(:prepared_join_source_snapshot, value, empty).nil?, 'source metadata retained an execution context')
      end
    ensure
      tables['qs_b'] = original if tables && original
      engine&.instance_variable_set(:@native_join_resolution_frame, nil)
      current&.close
      control&.close
    end
  end

  def run
    helper_semantics
    source_metadata
    JedNative.memory { |database| setup(database); public_queries(database); long_membership(database) }
    Dir.mktmpdir('jed-ruby-query-walk') do |directory|
      JedNative.create(File.join(directory, 'fixture.jed'), locking: :exclusive) do |database|
        setup(database)
        public_queries(database)
        long_membership(database)
      end
    end
    opaque_guards
    public_write_mutation
    puts 'ruby-native query walks: mutable trees, independent traversal semantics, CTE scope isolation, opaque definitions and exact cost prefixes passed'
  end
end

NativeQueryWalkTests.run
