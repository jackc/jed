# frozen_string_literal: true

require 'tmpdir'
require_relative '../lib/jed_native'

# SQL/cost conformance lives in the shared corpus; these cover the lazy host
# seam, ownership, guards, and equivalence to the ordinary cursor executor.
module NativePointCursorTests
  module_function
  def assert(condition, message) = (raise(message) unless condition)
  def error(code)
    yield
    raise "expected SQLSTATE #{code}"
  rescue JedNative::Error => exception
    assert(exception.sqlstate == code, "expected #{code}, got #{exception.sqlstate}: #{exception.message}")
  end

  def outcome(session, statement, *params, ordinary: false)
    previous = session.max_cost
    session.max_cost = 1_000_000 if ordinary
    cursor = session.query_prepared(statement, *params)
    [cursor.rows, cursor.cost, cursor.columns, cursor.column_types]
  ensure
    session.max_cost = previous
    cursor&.close
  end

  def check(database)
    session = database.session
    session.execute_script("CREATE TABLE t(id i16 PRIMARY KEY, note text, items text[], bytes bytea); INSERT INTO t VALUES(-32768,'minimum',ARRAY['a','b'],'\\x00ff'::bytea),(1,'one',ARRAY['x'],NULL),(32767,'maximum',NULL,NULL)")
    statement = session.prepare('SELECT id, note AS label, items, bytes, note AS label FROM t WHERE id = $1')
    [1, -32768, 32767, 2, nil].each do |value|
      assert(outcome(session, statement, value) == outcome(session, statement, value, ordinary: true), "point cursor differs for #{value.inspect}")
    end
    reversed = session.prepare('SELECT * FROM t WHERE $1 = id')
    assert(outcome(session, reversed, -32768) == outcome(session, reversed, -32768, ordinary: true), 'reversed predicate differs')
    wide = session.prepare('SELECT id FROM t WHERE id = $1::i64')
    [-32769, 32768, 2**50, nil].each do |value|
      assert(outcome(session, wide, value) == outcome(session, wide, value, ordinary: true), 'wider integer bound differs')
    end
    error('42601') { session.query_prepared(statement) }
    error('42601') { session.query_prepared(statement, 1, 2) }
    error('42804') { session.query_prepared(statement, '1') }
    error('22003') { session.query_prepared(statement, 32768) }

    cursor = session.query_prepared(statement, 1)
    assert(cursor.is_a?(JedNative::PointRows), 'eligible query did not use point cursor')
    assert(cursor.cost.zero?, 'point cursor executed before first pull')
    pin = cursor.object_id
    assert(database.instance_variable_get(:@pins).key?(pin), 'point cursor has no snapshot pin')
    cursor.close
    assert(cursor.cost.zero? && cursor.closed?, 'closing unread point cursor performed work')
    assert(!database.instance_variable_get(:@pins).key?(pin), 'closing point cursor retained its pin')
    assert(cursor.instance_variable_get(:@plan).nil?, 'closed point cursor retained its table root')
    error_free_eof = begin cursor.next; false; rescue StopIteration; true; end
    assert(error_free_eof, 'closed point cursor did not signal EOF')

    expected = outcome(session, statement, -32768)
    cursor = session.query_prepared(statement, -32768)
    cursor.columns[1].replace('changed')
    cursor.column_types[1].replace('changed')
    row = cursor.next
    assert(row['label'] == 'minimum' && row['id'] == -32768, 'public metadata changed row-name lookup')
    assert(row[1].is_a?(JedNative::CollatedText), 'direct text projection lost its collation label')
    assert(row[2][0].is_a?(JedNative::CollatedText), 'direct text-array projection lost element collation labels')
    assert(cursor.cost == expected[1], 'early close would lose point cost')
    row[1].replace('mutated')
    row[2][0].replace('mutated')
    row[3].replace('mutated')
    cursor.close
    assert(outcome(session, statement, -32768) == expected, 'point row aliases stored values or plan metadata')

    first = session.query_prepared(statement, 1)
    second = session.query_prepared(statement, -32768)
    version = database.version
    session.execute("UPDATE t SET note = 'new' WHERE id = 1")
    assert(database.oldest_live_txid == version, 'unread point failed to retain the old generation')
    lifetime = session.lifetime_cost
    assert(second.rows[0][1] == 'minimum' && first.rows[0][1] == 'one', 'interleaved point cursors lost their captured table')
    assert(session.lifetime_cost - lifetime == first.cost + second.cost, 'point cursors overwrote the shared lifetime counter')
    assert(database.oldest_live_txid == database.version, 'point drain failed to release the old generation')
    assert(outcome(session, statement, 1)[0][0][1] == 'new', 'point plan survived table replacement')

    cursor = session.query_prepared(statement, 1)
    session.begin
    session.execute("UPDATE t SET note = 'transaction' WHERE id = 1")
    lifetime = session.lifetime_cost
    assert(cursor.rows[0][1] == 'new', 'idle cursor adopted a later transaction snapshot')
    assert(session.lifetime_cost - lifetime == cursor.cost, 'old cursor lost work in the live transaction meter')
    session.rollback
    assert(session.lifetime_cost == lifetime + cursor.cost, 'rollback refunded cursor work')

    cursor = session.query_prepared(statement, 1)
    session.max_cost = 1
    session.lifetime_max_cost = 1
    session.max_query_memory_bytes = 1
    assert(cursor.rows[0][1] == 'new', 'settings changed an already-open cursor allowance')
    session.max_cost = session.lifetime_max_cost = session.max_query_memory_bytes = 0

    session.begin
    session.execute("UPDATE t SET note = 'pending' WHERE id = 1")
    cursor = session.query_prepared(statement, 1)
    assert(!cursor.is_a?(JedNative::PointRows), 'transaction must retain the ordinary cursor')
    session.execute("UPDATE t SET note = 'later' WHERE id = 1")
    assert(cursor.rows[0][1] == 'pending', 'fallback cursor lost its pending snapshot')
    cursor = session.query_prepared(statement, 1)
    session.rollback
    assert(cursor.closed?, 'rollback did not close the fallback cursor')

    %i[max_cost lifetime_max_cost max_query_memory_bytes].each do |setting|
      session.public_send("#{setting}=", 1_000_000)
      cursor = session.query_prepared(statement, 1)
      assert(!cursor.is_a?(JedNative::PointRows), "#{setting} bypassed the ordinary resource accounting")
      cursor.close
      session.public_send("#{setting}=", 0)
    end
    [
      'SELECT id + 1 FROM t WHERE id = $1',
      'SELECT id FROM t WHERE id = $1 LIMIT 1',
      'SELECT id FROM t WHERE id = $1 ORDER BY id',
      'SELECT DISTINCT id FROM t WHERE id = $1',
      'SELECT id INTO copied FROM t WHERE id = $1',
      'SELECT id FROM t WHERE id = $1 AND note = \'new\''
    ].each do |sql|
      cursor = session.query(sql, 1)
      assert(!cursor.is_a?(JedNative::PointRows), "general expression used point cursor: #{sql}")
      cursor.close
    end
    session.execute('CREATE INDEX same_key ON t(id)')
    cursor = session.query_prepared(statement, 1)
    assert(!cursor.is_a?(JedNative::PointRows), 'competing index bypassed access planning')
    cursor.close
    session.execute('DROP INDEX same_key')

    session.revoke(:select, 't')
    error('42804') { session.query_prepared(statement, 'bad') }
    error('42501') { session.query_prepared(statement, 1) }
    session.reset_privileges
    assert(database.query('SELECT id FROM t WHERE id = 1').rows == [[1]], 'database convenience cursor lost its owning session')
    session.close

    reader = database.read_session
    cursor = reader.query('SELECT id FROM t WHERE id = 1')
    assert(cursor.is_a?(JedNative::PointRows) && cursor.rows == [[1]], 'authorized read-only session did not use point cursor')
    reader.close
  end

  def storage_guards(database)
    session = database.session
    statement = session.prepare('SELECT id, note FROM t WHERE id = $1')
    outcome(session, statement, 1) # Ensure the leaf is already resident.
    storage = session.engine.instance_variable_get(:@storage)
    [[:@service_epoch, '08006'], [:@poisoned, '58030'], [:@closed, '55000']].each do |field, state|
      cursor = session.query_prepared(statement, 1)
      original = storage.instance_variable_get(field)
      begin
        storage.instance_variable_set(field, field == :@service_epoch ? original + 1 : true)
        error(state) { cursor.next }
        assert(cursor.closed? && cursor.cost.zero?, "#{state} guard performed work or retained its pin")
        assert(!database.instance_variable_get(:@pins).key?(cursor.object_id), "#{state} guard leaked a pin")
      ensure
        storage.instance_variable_set(field, original)
      end
    end
    session.close
  end

  def filler(length)
    alphabet = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/'
    state = 0x4a454442
    Array.new(length) do
      state ^= (state << 13) & 0xffff_ffff
      state ^= state >> 17
      state ^= (state << 5) & 0xffff_ffff
      alphabet[state & 63]
    end.join
  end

  def failed_pull(session, statement, key, ordinary: false)
    session.max_cost = 1_000_000 if ordinary
    lifetime = session.lifetime_cost
    cursor = session.query_prepared(statement, key)
    assert(cursor.is_a?(JedNative::PointRows) != ordinary, 'corruption check used the wrong cursor lane')
    assert(cursor.cost.zero?, 'corrupt selected value was read during cursor construction')
    error('XX001') { cursor.rows }
    assert(cursor.closed?, 'decode failure retained the cursor')
    assert(!session.database.instance_variable_get(:@pins).key?(cursor.object_id), 'decode failure retained its pin')
    [cursor.cost, session.lifetime_cost - lifetime]
  ensure
    cursor&.close
    session.max_cost = 0
  end

  def file_payloads(path)
    values = [filler(600), filler(200) + 'y' * 200, 'x' * 600]
    healthy_costs, overflow_pages = [], []
    JedNative.create(path, page_size: 256, locking: :exclusive) do |database|
      session = database.session
      session.execute('CREATE TABLE payloads(id i32 PRIMARY KEY, body text)')
      insert = session.prepare('INSERT INTO payloads VALUES($1, $2)')
      values.each_with_index { |value, index| session.execute_prepared(insert, index + 1, value) }
      session.execute('CREATE TABLE scalar_slots(id i64 PRIMARY KEY, b boolean, f f64, d date, u uuid)')
      session.execute("INSERT INTO scalar_slots VALUES(-9223372036854775808, true, 1.25, '2001-02-03'::date, '00000000-0000-0000-0000-000000000001'::uuid),(9223372036854775807,NULL,NULL,NULL,NULL)")
      session.close
    end
    JedNative.open(path, locking: :exclusive) do |database|
      session = database.session
      full = session.prepare('SELECT body FROM payloads WHERE id = $1')
      key_only = session.prepare('SELECT id FROM payloads WHERE id = $1')
      forms = %i[external external_comp inline_comp]
      values.each_with_index do |value, index|
        key = index + 1
        cursor = session.query_prepared(full, key)
        assert(cursor.is_a?(JedNative::PointRows), 'large selected value missed the point cursor')
        assert(cursor.rows == [[value]], "large selected value changed for #{forms[index]}")
        healthy_costs << cursor.cost
        assert([cursor.rows, cursor.cost, cursor.columns, cursor.column_types] == outcome(session, full, key, ordinary: true), "#{forms[index]} differs from ordinary cursor")
        row = session.engine.tables['payloads'][:native_primary_index][key]
        reference = row.storage_reference(1)
        assert(reference.form == forms[index], "fixture failed to produce #{forms[index]}")
        if reference.external?
          page = reference.first_page
          until page.zero?
            overflow_pages << page
            page = reference.pool.page(page)[:next_page]
          end
        end
        small = outcome(session, key_only, key)
        assert(small == outcome(session, key_only, key, ordinary: true), 'untouched large value changed cost')
        assert(cursor.cost - small[1] == 3, "#{forms[index]} omitted overflow/decompression work")
      end
      scalar = session.prepare('SELECT * FROM scalar_slots WHERE id = $1')
      [-(2**63), 2**63 - 1].each do |key|
        cursor = session.query_prepared(scalar, key)
        assert(cursor.is_a?(JedNative::PointRows), 'fixed-width selected values missed the point cursor')
        assert([cursor.rows, cursor.cost, cursor.columns, cursor.column_types] == outcome(session, scalar, key, ordinary: true), 'nullable fixed-width projection or i64 boundary changed')
      end
      session.close
    end

    # Reseal deliberately invalid overflow contents, retaining valid tree and
    # value descriptors. A manifest-free checkpoint keeps recovery from choosing
    # a preceding commit, so the failure occurs lazily when the body is decoded.
    codec = JedNative::FileFormat.new
    bytes = File.binread(path)
    state = codec.decode_image(bytes, lazy: true)
    JedNative::PhysicalImage.meta_pages(codec, 256, state[:meta]).each do |page, raw|
      bytes[page * 256, 256] = raw
    end
    overflow_pages.each do |page|
      raw = bytes.byteslice(page * 256, 256)
      assert(raw.getbyte(0) == JedNative::FileFormat::PAGE_OVERFLOW, 'selected payload did not retain its overflow page')
      raw[16, 240] = "\xff".b * 240
      raw[12, 4] = [codec.page_crc(raw)].pack('N')
      bytes[page * 256, 256] = raw
    end
    File.binwrite(path, bytes)
    JedNative.open(path, locking: :exclusive) do |database|
      session = database.session
      full = session.prepare('SELECT body FROM payloads WHERE id = $1')
      [1, 2].each do |key|
        fast = failed_pull(session, full, key)
        assert(fast == failed_pull(session, full, key, ordinary: true), 'decode failure changed the ordinary partial cost or lifetime counter')
        assert(fast == [healthy_costs[key - 1] - 1] * 2, 'decode failure charged row_produced or lost its earlier work')
        assert(session.query('SELECT id FROM payloads WHERE id = $1', key).rows == [[key]], 'unselected corrupt body was decoded')
      end
      assert(session.query_prepared(full, 3).rows == [[values[2]]], 'corrupt external values affected inline compression')
      session.close
    end
  end

  def run
    JedNative.memory { |database| check(database) }
    Dir.mktmpdir('jed-ruby-point-cursor') do |directory|
      JedNative.create(File.join(directory, 'db'), page_size: 512, locking: :exclusive) do |database|
        check(database)
        storage_guards(database)
      end
      file_payloads(File.join(directory, 'payloads'))
    end
    puts 'ruby-native point cursor: lazy execution, values/costs, metadata ownership, snapshot pins, guards, and fallbacks passed'
  end
end
NativePointCursorTests.run
