# frozen_string_literal: true
require 'tmpdir'
require_relative '../lib/jed_native'

module RubyStorageRefreshTest
  module_function
  def assert(value, message) = (raise message unless value)
  def error(code)
    yield
    raise "expected #{code}"
  rescue JedNative::Error => exception
    raise "expected #{code}, got #{exception.sqlstate}" unless exception.sqlstate == code
  end

  # The shared fixture specifies these ordered SQL edits, including two
  # three-way leaf splits and prefix-loss splits that propagate to the root.
  def multiway_bytes
    JedNative.memory(page_size: 256) do |db|
      db.execute('CREATE TABLE a (id i32 PRIMARY KEY, v i64)')
      insert = db.prepare('INSERT INTO a VALUES ($1, 7)')
      (1..600).each { |id| db.execute_prepared(insert, id) unless id == 400 }
      db.execute('UPDATE a SET v = 4000000000000 WHERE id = 60')
      db.execute('INSERT INTO a VALUES (400, -4000000000000)')
      db.execute('CREATE TABLE b (k text PRIMARY KEY, n i32)')
      insert = db.prepare('INSERT INTO b VALUES ($1, 1)')
      64.times { |id| db.execute_prepared(insert, format('customer/acct/%03d', id)) }
      [['a', 2], ['z', 3], ['customer/b', 5], ['customer/a', 4]].each do |key, value|
        db.execute('INSERT INTO b VALUES ($1, $2)', key, value)
      end
      expected = File.binread(File.expand_path('../../../spec/fileformat/fixtures/multiway_split.jed', __dir__))
      assert(db.to_image(page_size: 256, txid: 1) == expected, 'ordered edits differ from the shared multi-way byte fixture')
    end
  end

  def recovery_floor
    Dir.mktmpdir('ruby-meta-floor') do |dir|
      [1, 2].each do |txid|
        image = JedNative.memory { |db| db.to_image(page_size: 256, txid: txid) }
        codec = JedNative::FileFormat.new
        bad_slot = txid & 1
        good_slot = bad_slot ^ 1
        bad = image.byteslice(bad_slot * 256, 256)
        root = bad.byteslice(20, 4).unpack1('N')
        bad[40, 4] = [1].pack('N')
        bad[64, 4] = [root].pack('N')
        bad[68, 8] = [codec.crc64(image.byteslice(root * 256, 256)) ^ 1].pack('Q>')
        bad[32, 4] = [codec.meta_crc(bad)].pack('N')
        image[bad_slot * 256, 256] = bad
        good = image.byteslice(good_slot * 256, 256)
        path = File.join(dir, "db-#{txid}")
        File.binwrite(path, image)
        JedNative.open(path, locking: :exclusive) { |db| db.execute('CREATE TABLE recovered (id i32 PRIMARY KEY)') }
        assert(File.binread(path).byteslice(good_slot * 256, 256) == good, 'commit overwrote the only complete meta generation')
      end
    end
  end

  def committed_gist_memory
    Dir.mktmpdir('ruby-gist-load') do |dir|
      path = File.join(dir, 'db')
      image = JedNative.create(path, locking: :exclusive) do |db|
        db.execute('CREATE TABLE t (id i32 PRIMARY KEY, r i32range)')
        db.execute('CREATE INDEX t_r ON t USING gist(r)')
        db.execute("INSERT INTO t VALUES (1, '[1,5)')")
        db.serialize
      end
      [:deserialize, :attach].each do |mode|
        db = mode == :deserialize ? JedNative.deserialize(image) : JedNative.memory
        db.attach('aux', path, locking: :exclusive) if mode == :attach
        name = mode == :attach ? 'aux.t' : 't'
        session = db.session(max_query_memory_bytes: 1)
        session.execute("DELETE FROM #{name} WHERE id = 99")
        error('54P05') { session.execute("INSERT INTO #{name} VALUES (2, '[10,20)')") }
        session.max_query_memory_bytes = 4096
        session.execute("INSERT INTO #{name} VALUES (2, '[10,20)')")
        session.max_query_memory_bytes = 1
        session.execute("DELETE FROM #{name} WHERE id = 99")
        session.close
        db.close
      end
    end
  end

  def run
    multiway_bytes
    recovery_floor
    committed_gist_memory
    puts 'ruby-native storage refresh: exact multi-way fixture, equal-txid recovery floor and committed GiST memory passed'
  end
end
RubyStorageRefreshTest.run
