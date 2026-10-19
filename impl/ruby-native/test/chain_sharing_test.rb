# frozen_string_literal: true

# Physical large-value invariants from large-values.md §15. These host checks
# mirror the native cores' chain_sharing tests; SQL corpus pages are too large
# to exercise all four stored forms with these small deterministic payloads.
require 'tmpdir'
require_relative '../lib/jed_native'

module RubyChainSharingTest
  PAGE_SIZE = 256
  PHASES = [
    ['BEGIN', 'SAVEPOINT a', 'UPDATE t SET n = n + 1000',
     'UPDATE t SET id = id + 100 WHERE id <= 3', 'ROLLBACK TO SAVEPOINT a',
     'UPDATE t SET n = n + 1 WHERE id <= 3', 'SAVEPOINT b',
     'UPDATE t SET id = id + 200 WHERE id = 1', 'RELEASE SAVEPOINT b',
     'SAVEPOINT c', 'UPDATE t SET id = id + 400 WHERE id = 2',
     'ROLLBACK TO SAVEPOINT c', 'COMMIT'],
    ["INSERT INTO t VALUES (2, 'ignored', 0) ON CONFLICT (id) DO UPDATE SET id = 300",
     'INSERT INTO t SELECT id + 1000, body, n FROM t WHERE id IN (201, 300)'],
    ['DELETE FROM t WHERE id IN (201, 300)', "INSERT INTO t VALUES (13, 'FILLER', 13)",
     'UPDATE t SET n = n + 1']
  ].freeze

  def self.assert(value, message)
    raise message unless value
  end

  def self.filler(size)
    alphabet = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/'
    state = 0x4a454442
    Array.new(size) do
      state ^= (state << 13) & 0xffff_ffff
      state ^= state >> 17
      state ^= (state << 5) & 0xffff_ffff
      alphabet[state & 63]
    end.join
  end

  def self.seed(session)
    session.execute('CREATE TABLE t (id i32 PRIMARY KEY, body text, n i32)')
    tuples = [[1, filler(600)], [2, filler(200) + 'y' * 200], [3, 'x' * 600]]
    tuples.concat((4..12).map { |id| [id, "small #{id}"] })
    session.execute('INSERT INTO t VALUES ' + tuples.map { |id, text| "(#{id}, '#{text}', #{id})" }.join(', '))
  end

  def self.create(path)
    database = JedNative.create(path, page_size: PAGE_SIZE, locking: :exclusive)
    session = database.session
    seed(session)
    session.close
    database.close
  end

  def self.open(path) = JedNative.open(path, locking: :exclusive)

  def self.overflow_pages(path)
    bytes = File.binread(path)
    (2...bytes.bytesize / PAGE_SIZE).filter_map do |page|
      raw = bytes.byteslice(page * PAGE_SIZE, PAGE_SIZE)
      [page, raw] if raw.getbyte(0) == JedNative::FileFormat::PAGE_OVERFLOW
    end.to_h
  end

  def self.verify_image(path)
    codec = JedNative::FileFormat.new
    bytes = File.binread(path)
    state = codec.decode_image(bytes, lazy: true)
    ids = state[:live_pages].to_a
    assert(ids.length == ids.uniq.length, 'two current records share an overflow page')
    assert(ids.length == state[:meta][:live], 'persisted live page count is wrong')
  end

  def self.corrupt_kept_values
    Dir.mktmpdir('ruby-kept-chain') do |directory|
      path = File.join(directory, 'db')
      create(path)
      codec = JedNative::FileFormat.new
      bytes = File.binread(path)
      state = codec.decode_image(bytes, lazy: true)
      # Install a manifest-free checkpoint, so this deliberate, resealed body
      # corruption cannot select a preceding commit during recovery.
      JedNative::PhysicalImage.meta_pages(codec, PAGE_SIZE, state[:meta]).each do |page, raw|
        bytes[page * PAGE_SIZE, PAGE_SIZE] = raw
      end
      overflow_pages(path).each do |page, raw|
        raw[16, PAGE_SIZE - 16] = "\xff".b * (PAGE_SIZE - 16)
        raw[12, 4] = [codec.page_crc(raw)].pack('N')
        bytes[page * PAGE_SIZE, PAGE_SIZE] = raw
      end
      File.binwrite(path, bytes)
      database = open(path)
      session = database.session
      session.execute('UPDATE t SET n = n + 100')
      session.execute("INSERT INTO t VALUES (13, 'new', 13)")
      session.execute('UPDATE t SET id = 0 WHERE id = 2')
      assert(session.execute('SELECT sum(n) FROM t').rows == [[1291]], 'untouched-chain update lost rows')
      session.close
      database.close
      verify_image(path)
      database = open(path)
      [0, 1].each do |id|
        begin
          database.execute("SELECT body FROM t WHERE id = #{id}")
        rescue JedNative::Error => error
          assert(error.sqlstate == 'XX001', "expected corrupt chain, got #{error}")
        else
          raise "corrupt chain #{id} was silently rewritten or accepted"
        end
      end
      assert(database.execute('SELECT body FROM t WHERE id = 3').rows == [['x' * 600]], 'inline compressed value changed')
      database.close
    end
  end

  def self.read_unassigned_and_compact
    Dir.mktmpdir('ruby-read-kept-chain') do |directory|
      path = File.join(directory, 'db')
      create(path)
      before = overflow_pages(path)
      assert(before.length == 4, "seed has #{before.length} overflow pages instead of four")
      database = open(path)
      session = database.session
      session.execute('UPDATE t SET n = -n WHERE length(body) > 3')
      session.execute('UPDATE t SET id = id + 100 WHERE id <= 2')
      wanted = session.execute('SELECT id, body, n FROM t ORDER BY id').rows
      session.close
      database.close
      assert(overflow_pages(path) == before, 'an unchanged chain was rewritten after being read')
      verify_image(path)
      database = open(path)
      queries = ['SELECT * FROM t', 'SELECT body FROM t WHERE id = 101']
      costs = queries.map { |sql| database.execute(sql).cost }
      database.compact
      assert(queries.map { |sql| database.execute(sql).cost } == costs, 'compaction changed logical costs')
      assert(database.execute('SELECT id, body, n FROM t ORDER BY id').rows == wanted, 'compaction changed values')
      database.close
    end
  end

  def self.cost_and_ownership
    Dir.mktmpdir('ruby-chain-ownership') do |directory|
      path = File.join(directory, 'db')
      create(path)
      memory = JedNative.memory(page_size: PAGE_SIZE)
      resident = memory.session
      seed(resident)
      PHASES.each_with_index do |phase, index|
        database = open(path)
        paged = database.session
        phase.each do |sql|
          sql = sql.gsub('FILLER', filler(900))
          a, b = resident.execute(sql), paged.execute(sql)
          assert(a.cost == b.cost, "phase #{index} #{sql}: resident cost #{a.cost}, paged #{b.cost}")
        end
        sql = 'SELECT id, body, n FROM t ORDER BY id'
        assert(resident.execute(sql).rows == paged.execute(sql).rows, "phase #{index}: rows differ")
        paged.close
        database.close
        verify_image(path)
      end
      resident.close
      memory.close
    end
  end

  def self.run
    %i[corrupt_kept_values read_unassigned_and_compact cost_and_ownership].each do |name|
      public_send(name)
      puts "ruby-native chain sharing: #{name} passed"
    end
  end
end

RubyChainSharingTest.run
