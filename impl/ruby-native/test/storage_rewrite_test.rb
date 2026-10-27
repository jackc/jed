# frozen_string_literal: true

require 'tmpdir'
require_relative '../lib/jed_native'

module RubyRewriteTest
  def self.assert(condition, message)
    raise message unless condition
  end

  def self.run
    fixtures = File.expand_path('../../../spec/fileformat/fixtures', __dir__)
    count = 0
    Dir[File.join(fixtures, '*_table.jed')].sort.each do |path|
      next if File.basename(path).match?(/collation|expr_index|partial_index|timezone_index|hostfunc_index|exclude_followon|^encrypted_/)
      bytes = File.binread(path)
      codec = JedNative::FileFormat.new
      state = codec.decode_image(bytes, lazy: true)
      output = JedNative::PageBuffer.new(256, bytes.bytesize / 256)
      JedNative::PhysicalImage.new(codec, 256, source_image: bytes).write(state, state[:meta][:txid]) do |page, block|
        output[page * 256, 256] = block
      end
      assert(bytes == output.to_s, "physical rewrite differs from shared fixture #{File.basename(path)}")
      count += 1
    end
    puts "ruby-native rewrite: #{count} shared byte fixtures passed"
    Dir.mktmpdir('jed-ruby-rewrite') do |directory|
      path = File.join(directory, 'source.jed')
      database = JedNative.create(path, page_size: 512, fsync: false)
      session = database.session
      session.execute('CREATE TABLE t(id i32 PRIMARY KEY, v text, r int4range)')
      session.execute("INSERT INTO t SELECT i, 'abcdefghij' || i::text, int4range(i,i+2) FROM generate_series(1,80) i")
      session.execute('CREATE INDEX ti ON t((id % 3))')
      session.execute('CREATE INDEX tr ON t USING gist(r)')
      # Incompressible payload forces an overflow chain on these small pages.
      text = (0...3200).map { |i| ((i * 113 + i * i / 7) % 90 + 33).chr }.join
      session.execute('UPDATE t SET v=$1 WHERE id=1', text)
      session.execute('DELETE FROM t WHERE id>20')
      storage = session.engine.instance_variable_get(:@storage)
      original = storage.meta.dup
      compacted = File.join(directory, 'compacted.jed')
      identical = File.join(directory, 'identical.jed')
      storage.backup(compacted, form: :compacted)
      storage.backup(identical, form: :page_identical)
      codec = JedNative::FileFormat.new
      compact_state = codec.decode_image(File.binread(compacted))
      identical_state = codec.decode_image(File.binread(identical))
      assert(compact_state[:tables].first[:rows] == identical_state[:tables].first[:rows], 'backup rows differ')
      assert(compact_state[:meta][:txid] == original[:txid] + 1, 'compacted version')
      assert(identical_state[:meta][:txid] == original[:txid], 'identical version')
      assert(identical_state[:meta][:page_count] == original[:page_count], 'identical page count')
      session.close
      storage.compact
      assert(File.binread(path) == File.binread(compacted), 'compaction and backup differ byte-for-byte')
      first = File.binread(path)
      storage.compact
      second = File.binread(path)
      assert(first.byteslice(1024..) == second.byteslice(1024..), 'second compaction changed body')
      database.close
      reopened = JedNative.open(path)
      assert(reopened.execute('SELECT v FROM t WHERE id=1').rows == [[text]], 'compaction changed overflow text')
      reopened.close
      puts 'ruby-native rewrite: backup forms, compaction, overflow and fixed point passed'
    ensure
      session&.close
      database&.close
      reopened&.close
    end
  end
end

RubyRewriteTest.run
