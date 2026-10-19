# frozen_string_literal: true

require_relative '../lib/jed_native'

# The shared corpus pins SQL thresholds. These tests pin the host cursor's
# ownership, first-pull execution, snapshots and early-close contract.
module NativeCursorColumnarTests
  module_function

  def assert(value, message) = (raise(message) unless value)

  def error(code)
    yield
    raise "expected #{code}"
  rescue JedNative::Error => failure
    assert(failure.sqlstate == code, "expected #{code}, got #{failure.sqlstate}")
  end

  def drain(session, sql, prepared: false, collect: false)
    cursor = prepared ? session.query_prepared(session.prepare(sql)) : session.query(sql)
    values = collect ? cursor.rows : cursor.each.to_a
    [values.map(&:to_a), cursor.columns, cursor.column_types, cursor.cost]
  ensure
    cursor&.close
  end

  class ObservedRow < Array
    def initialize(values, index, events, fail_at)
      super(values)
      @index, @events, @fail_at = index, events, fail_at
    end

    def [](column, *rest)
      @events << [@index, column]
      raise JedNative::Error.new('XX001', 'deferred row fault') if @fail_at == [@index, column]
      super
    end
  end

  def observed_cursor(session, sql, events, fail_at: nil)
    cursor = session.query(sql)
    engine = cursor.instance_variable_get(:@engine)
    tables = engine.instance_variable_get(:@tables)
    stored = tables.fetch('numbers')
    rows = stored[:rows].each_with_index.map { |row, index| ObservedRow.new(row, index, events, fail_at) }
    engine.instance_variable_set(:@tables, tables.merge('numbers' => stored.merge(rows: rows)))
    cursor
  end

  def metadata(value)
    case value
    when JedNative::ArrayValue
      [value.class, value.element_type, value.dimensions, value.lower_bounds, value.map { |item| metadata(item) }]
    when Array then value.map { |item| metadata(item) }
    when JedNative::CollatedText then [value.class, String.new(value), value.collation_label.to_h]
    when String then [value.class, String.new(value)]
    else value
    end
  end

  def metered_eager_gather(session)
    %i[statement lifetime profile].each do |mode|
      [[1, [5, 0], [[0, 0]]], [192, nil, (0...6).map { |i| [i, 0] } + [[0, 0]]]].each do |budget, fault, expected_reads|
        engine = session.engine.dup
        engine.max_query_memory_bytes = budget
        engine.max_cost = 1000 if mode == :statement
        engine.lifetime_max_cost = engine.lifetime_cost + 1000 if mode == :lifetime
        engine.instance_variable_set(:@explain_profile_frames, []) if mode == :profile
        events = []
        tables = engine.instance_variable_get(:@tables)
        stored = tables.fetch('numbers')
        rows = stored[:rows].each_with_index.map { |row, i| ObservedRow.new(row, i, events, fault) }
        engine.instance_variable_set(:@tables, tables.merge('numbers' => stored.merge(rows: rows)))
        sql = budget == 1 ? 'SELECT id FROM numbers' : 'SELECT id, id FROM numbers'
        error('54P05') { engine.run(session.prepare(sql).ast) }
        assert(events == expected_reads, "#{mode} eager gather changed reservation/read order")
        assert(engine.instance_variable_get(:@query_memory).nil?, "#{mode} eager rejection retained its account")
      end
    end
  end

  def text_metadata(database)
    setup = database.session
    setup.execute('CREATE TABLE labels (id i32 PRIMARY KEY, c text COLLATE "C", u text COLLATE unicode, a text[] COLLATE unicode)')
    setup.execute("INSERT INTO labels VALUES (1, 'z', 'ä', ARRAY['z', NULL]), (2, NULL, NULL, NULL)")
    setup.close
    sql = 'SELECT u, c, a, u FROM labels'
    [0, 4096].each do |budget|
      actual, ordinary = [false, true].map do |old_lane|
        session = database.session
        session.max_query_memory_bytes = budget
        session.engine.instance_variable_set(:@native_compilation_disabled, true)
        cursor = session.query(sql)
        engine = cursor.instance_variable_get(:@engine)
        engine.define_singleton_method(:columnar_cursor?) { |_| false } if old_lane
        tables = engine.instance_variable_get(:@tables)
        stored = tables.fetch('labels')
        wrong = JedNative::CollationLabel.new(conflict: true).freeze
        values = [1, JedNative::CollatedText.new('z', wrong), 'ä',
                  JedNative::ArrayValue.new(['z', nil], element_type: 'text')]
        engine.instance_variable_set(:@tables, tables.merge('labels' => stored.merge(rows: [values, [2, nil, nil, nil]])))
        [metadata(cursor.each.to_a), cursor.columns, cursor.column_types, cursor.cost]
      ensure
        cursor&.close
        session&.close
      end
      assert(actual == ordinary, 'gathered text lost declared C/unicode metadata or NULL shape')
      assert(actual[0][0][0][2][:name] == 'unicode' && actual[0][0][1][2][:name] == 'C', 'ordinary label reference was not declared-column metadata')
    end
  end

  def run
    calls = []
    environment = JedNative::Environment.new.load_unicode_data(File.binread(File.expand_path('../../../spec/collation/fixtures/unicode.jucd', __dir__)))
    environment = environment.register_function(name: 'columnar_probe', args: ['i32'], returns: 'i32', cost: 1) do |value|
      calls << value
      value
    end
    JedNative.memory(environment: environment) do |database|
      text_metadata(database)
      setup = database.session
      setup.execute('CREATE TABLE numbers (id i32 PRIMARY KEY, g i32)')
      setup.execute('INSERT INTO numbers SELECT x, x % 2 FROM generate_series(1, 6) x')
      setup.execute('CREATE TABLE empty_numbers (id i32 PRIMARY KEY)')
      setup.close
      session = database.session
      metered_eager_gather(session)
      sql = 'SELECT id FROM numbers'
      expected = (1..6).map { |id| [id] }
      baseline = drain(session, sql)
      assert(baseline.take(3) == [expected, ['id'], ['i32']], 'plain cursor metadata or values changed')

      [false, true].each do |prepared|
        session.max_query_memory_bytes = 192
        assert(drain(session, sql, prepared: prepared) == baseline, 'raw cursor rejected exact gathered allowance')
        session.max_query_memory_bytes = 191
        error('54P05') { drain(session, sql, prepared: prepared) }
        session.max_query_memory_bytes = 576
        assert(drain(session, sql, prepared: prepared, collect: true) == baseline, 'owned collector lost its gathered lane')
        session.max_query_memory_bytes = 575
        error('54P05') { drain(session, sql, prepared: prepared, collect: true) }
      end
      session.max_query_memory_bytes = 192
      assert(session.execute(sql).rows == expected, 'eager lane disagrees at exact allowance')
      assert(session.execute_prepared(session.prepare(sql)).rows == expected, 'prepared eager lane disagrees')
      session.max_query_memory_bytes = 191
      error('54P05') { session.execute(sql) }

      # One gathered lane per source ordinal, even when the projection repeats
      # a column. Output collection still measures both copies in every row.
      duplicate = 'SELECT id AS a, id AS b FROM numbers'
      session.max_query_memory_bytes = 192
      assert(drain(session, duplicate)[0] == expected.map { |row| row * 2 }, 'duplicate projection charged a second input lane')
      assert(session.execute(duplicate).rows == expected.map { |row| row * 2 }, 'eager duplicate projection charged twice')
      session.max_query_memory_bytes = 768
      assert(drain(session, duplicate, collect: true)[0].length == 6, 'duplicate collector overcharged')
      session.max_query_memory_bytes = 767
      error('54P05') { drain(session, duplicate, collect: true) }
      session.max_query_memory_bytes = 384
      assert(drain(session, 'SELECT * FROM numbers')[0].length == 6, 'star did not gather both columns')
      session.max_query_memory_bytes = 383
      error('54P05') { drain(session, 'SELECT * FROM numbers') }
      session.max_query_memory_bytes = 1
      assert(drain(session, 'SELECT id FROM empty_numbers')[0].empty?, 'empty gather reserved bytes')
      assert(drain(session, 'SELECT id FROM numbers LIMIT 100')[0] == expected, 'LIMIT scan incorrectly gathered columns')

      # The lane is independent of the account. It gathers on first pull in
      # source-row/source-column order, before yielding its first projection.
      [0, 384].each do |budget|
        session.max_query_memory_bytes = budget
        events = []
        cursor = observed_cursor(session, 'SELECT g, id, g FROM numbers', events)
        engine = cursor.instance_variable_get(:@engine)
        assert(events.empty? && cursor.cost.zero?, 'cursor gathered before its first pull')
        assert(cursor.next == [1, 1, 1], 'gather projection order changed')
        assert(events == (0...6).flat_map { |i| [[i, 0], [i, 1]] }, 'gather duplicated or reordered source reads')
        account = engine.instance_variable_get(:@query_memory)
        assert(!account || account.used == 384, 'gather charge did not survive suspension')
        first_cost = cursor.cost
        cursor.close
        assert(cursor.cost == first_cost && first_cost < baseline[3], 'close charged unread output')
        assert(!account || account.used.zero?, 'close retained gathered ownership')
      end
      [0, 1].each do |budget|
        session.max_query_memory_bytes = budget
        events = []
        cursor = observed_cursor(session, sql, events, fail_at: [5, 0])
        assert(events.empty?, 'deferred storage fault was observed at open')
        error('XX001') { cursor.next }
        assert(events == (0...6).map { |i| [i, 0] }, 'gather memory rejection hid a later storage fault')
        assert(cursor.closed?, 'faulted columnar cursor stayed open')
      end

      # Metered queries retain the preceding cursor lane's checkpoints. Compare
      # against that path by explicitly declining columnar admission on the
      # captured engine, independently of the new admission implementation.
      %i[max_cost lifetime_max_cost].each do |setting|
        (1..baseline[3] + 1).each do |ceiling|
          observations = [[0, false], [192, false], [192, true]].map do |budget, old_lane|
            probe = database.session
            probe.max_query_memory_bytes = budget
            probe.public_send("#{setting}=", ceiling)
            cursor = probe.query(sql)
            cursor.instance_variable_get(:@engine).define_singleton_method(:columnar_cursor?) { |_| false } if old_lane
            begin
              [:ok, cursor.each.to_a, cursor.cost]
            rescue JedNative::Error => failure
              [failure.sqlstate, cursor.cost]
            ensure
              cursor.close
              probe.close
            end
          end
          assert(observations.uniq.length == 1, "columnar admission changed #{setting} prefix #{ceiling}")
          assert(observations[0] == [setting == :max_cost ? '54P01' : '54P02', 4], 'metered scan bulk-charged unread rows') if ceiling == 4
        end
      end
      admission = session.query(sql)
      execution = admission.instance_variable_get(:@engine)
      execution.instance_variable_set(:@cursor_select_depth, 1)
      statement = session.prepare(sql).ast
      assert(execution.send(:columnar_cursor?, statement), 'plain unmetered cursor no longer admits its gathered lane')
      execution.instance_variable_set(:@explain_profile_frames, [])
      assert(!execution.send(:columnar_cursor?, statement), 'profiling cursor changed its row checkpoints')
      admission.close

      session.max_query_memory_bytes = 192
      old = session.query(sql)
      session.max_query_memory_bytes = 191
      error('54P05') { drain(session, sql) }
      assert(old.each.to_a == expected, 'live setting replaced captured gather allowance')
      session.max_query_memory_bytes = 384
      old = session.query('SELECT id, g FROM numbers')
      writer = database.session
      writer.execute('UPDATE numbers SET g = 9 WHERE id = 1')
      writer.close
      assert(old.next == [1, 1], 'gather did not retain its captured table generation')
      old.close

      # An arbitrary expression keeps its per-row projection. Closing after
      # one output must never call a host function for unread rows.
      session.max_query_memory_bytes = 0
      cursor = session.query('SELECT columnar_probe(id) FROM numbers')
      assert(calls.empty?, 'host projection ran at open')
      assert(cursor.next == [1] && calls == [1], 'host projection was gathered eagerly')
      cursor.close
      assert(calls == [1], 'close evaluated unread host projections')
      assert(database.instance_variable_get(:@pins).empty?, 'columnar cursor retained snapshot pins')
      assert((session.instance_variable_get(:@cursors) || {}).empty?, 'columnar cursor remained registered')
      session.close
    end
    puts 'ruby-native cursor columnar: shared lane/collector thresholds, unique lanes, deferred gather faults, cost prefixes and cleanup passed'
  end
end

NativeCursorColumnarTests.run
