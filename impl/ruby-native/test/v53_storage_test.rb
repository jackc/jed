# frozen_string_literal: true
require 'tmpdir'
require 'fileutils'
require_relative '../lib/jed_native'

module RubyV53StorageTest
  module_function
  def assert(value, message) = (raise message unless value)

  def exercise(encrypted)
    Dir.mktmpdir('ruby-v53') do |dir|
      path = File.join(dir, 'db')
      options = { locking: :exclusive, page_size: encrypted ? 1024 : 256 }
      credential = encrypted ? { credential: JedNative::Credential.key("\x4A".b * 32) } : {}
      db = JedNative.create(path, **options, **credential)
      db.execute('CREATE TABLE t (id i32 PRIMARY KEY, v i32)')
      db.execute('INSERT INTO t SELECT g, g FROM generate_series(1, 7200) g')
      storage = db.engine.instance_variable_get(:@storage)
      retained_root = storage.load[:tables].find { |table| table[:name] == 't' }[:root_data_page]
      # Simulate a hold surviving an ownership change after its page was reused.
      storage.instance_variable_set(:@held_orphans, [retained_root])
      db.execute('CREATE TABLE held_probe (id i32 PRIMARY KEY)')
      assert(!storage.load[:free_pages].include?(retained_root), 'stale hold freed a live tree')
      expected = 7200 * 7201 / 2
      saw_remap = saw_fold = false
      90.times do |i|
        id = i * 71 + 1
        db.execute('UPDATE t SET v = v + 1 WHERE id = $1', id)
        expected += 1
        storage = db.engine.instance_variable_get(:@storage)
        entries = storage.meta.fetch(:remap)
        saw_fold ||= saw_remap && entries.empty?
        saw_remap ||= !entries.empty?
        assert(db.execute('SELECT sum(v) FROM t').rows == [[expected]], 'deferred publication lost a row update')
        next unless i == 8 || i == 45
        copy = File.join(dir, "backup-#{i}")
        db.backup(copy)
        JedNative.open(copy, locking: :exclusive, **credential) do |backup|
          assert(backup.execute('SELECT sum(v) FROM t').rows == [[expected]], 'bootstrap backup lost remap')
        end
        db.close
        db = JedNative.open(path, locking: :exclusive, **credential)
      end
      assert(saw_remap, 'writer never deferred an interior')
      assert(saw_fold, 'bounded remap list never folded')
      db.close
      JedNative.open(path, locking: :exclusive, **credential) do |reopened|
        assert(reopened.execute('SELECT sum(v) FROM t').rows == [[expected]], 'recovery lost deferred updates')
        assert(reopened.integrity_check.findings.empty?, 'deferred image failed integrity')
      end
    ensure
      db&.close unless db&.instance_variable_get(:@closed)
    end
  end

  def stamp_and_remap_validation
    Dir.mktmpdir('ruby-v53-validation') do |dir|
      path = File.join(dir, 'db')
      JedNative.create(path, locking: :exclusive, page_size: 256) do |db|
        db.execute('CREATE TABLE t (id i32 PRIMARY KEY)')
        db.execute('INSERT INTO t VALUES (1)')
      end
      image = File.binread(path)
      codec = JedNative::FileFormat.new
      meta = codec.select_meta(image, 256)
      slot = meta[:slot_index]
      floor = codec.read_meta(image, 256, slot ^ 1, manifest: false)[:txid]
      page = meta[:manifest].first.first
      block = image.byteslice(page * 256, 256)
      block[2, 2] = [floor & 0xFFFF].pack('n')
      block[12, 4] = [codec.page_crc(block)].pack('N')
      image[page * 256, 256] = block
      # Match the CRC identity too: the stamp must independently reject it.
      image[slot * 256 + 68, 4] = [codec.listed_crc(block)].pack('N')
      raw = image.byteslice(slot * 256, 256)
      raw[32, 4] = [codec.meta_crc(raw)].pack('N')
      image[slot * 256, 256] = raw
      assert(codec.valid_manifest?(image, 256, raw, meta, floor: nil), 'CRC identity setup invalid')
      assert(codec.read_meta(image, 256, slot).nil?, 'stale stamp passed a complete CRC identity')

      image = File.binread(path)
      raw = image.byteslice(slot * 256, 256)
      raw[60, 4] = [codec.remap_capacity(256) + 1].pack('N')
      raw[32, 4] = [codec.meta_crc(raw)].pack('N')
      image[slot * 256, 256] = raw
      assert(codec.read_meta(image, 256, slot, manifest: false).nil?, 'oversized remap accepted')
      assert(codec.select_meta(image, 256)[:slot_index] != slot, 'invalid remap did not fall back')
    end
  end

  def golden_histories
    root = File.expand_path('../../../spec/fileformat/fixtures', __dir__)
    Dir[File.join(root, '*cow_deferred*.jed')].sort.each do |source|
      encrypted = File.basename(source).start_with?('encrypted_')
      options = encrypted ? { credential: JedNative::Credential.key("\x4A".b * 32) } : {}
      Dir.mktmpdir('ruby-v53-history') do |dir|
        path = File.join(dir, 'db')
        FileUtils.cp(source, path)
        JedNative.open(path, locking: :exclusive, **options) do |db|
          assert(db.integrity_check.findings.empty?, "#{source}: integrity")
          db.execute('CREATE TABLE added (id i32 PRIMARY KEY)')
          db.execute('INSERT INTO added VALUES (7)')
        end
        JedNative.open(path, locking: :exclusive, **options) do |db|
          assert(db.execute('SELECT id FROM added').rows == [[7]], 'foreign history commit lost')
          assert(db.integrity_check.findings.empty?, "#{source}: post-commit integrity")
        end
      end
    end
  end

  stamp_and_remap_validation
  golden_histories
  exercise(false)
  exercise(true)
  puts 'ruby-native v53: shared histories, deferred commits, folding, reopen, backups and integrity passed'
end
