# frozen_string_literal: true

require 'tmpdir'
require_relative '../lib/jed_native'

module RubyGistStorageTest
  module NoPublicationDecode
    def read_gist_tree(...)
      raise 'publication decoded its own GiST tree' if Thread.current[:jed_gist_publication_test]
      super
    end
  end
  JedNative::FileFormat.prepend(NoPublicationDecode)

  def self.run
    database = JedNative.memory
    session = database.session
    session.execute('CREATE TABLE t (id bigint PRIMARY KEY, span int4range)')
    session.execute("INSERT INTO t VALUES (1, '[1,11)')")
    # One leaf entry stores two u16 lengths, a nine-byte bounded int4range,
    # and the eight-byte primary key. Q3 excludes routing pages.
    session.max_query_memory_bytes = 20
    begin
      session.execute('CREATE INDEX t_span ON t USING gist(span)')
      raise 'GiST pending entry fit below its canonical21 bytes'
    rescue JedNative::Error => error
      raise unless error.sqlstate == '54P05'
    end
    session.max_query_memory_bytes = 21
    session.execute('CREATE INDEX t_span ON t USING gist(span)')
    session.close
    database.close

    Dir.mktmpdir('jed-ruby-gist-adoption') do |directory|
      path = File.join(directory, 'db')
      database = JedNative.create(path, locking: :exclusive)
      session = database.session
      session.execute('CREATE TABLE t (id bigint PRIMARY KEY, span int4range)')
      session.execute('INSERT INTO t SELECT g, int4range(g::i32, (g+10)::i32) FROM generate_series(1,80) g')
      Thread.current[:jed_gist_publication_test] = true
      session.execute('CREATE INDEX t_span ON t USING gist(span)')
      Thread.current[:jed_gist_publication_test] = false
      expected = session.execute("SELECT id FROM t WHERE span @> '[31,32)'::int4range ORDER BY id").rows
      session.close
      database.close
      database = JedNative.open(path, locking: :exclusive)
      raise 'GiST adoption changed reopened answers' unless database.execute("SELECT id FROM t WHERE span @> '[31,32)'::int4range ORDER BY id").rows == expected
      database.close
    end
    puts 'ruby-native GiST storage: exact pending-entry allowance and metadata-only publication passed'
  ensure
    Thread.current[:jed_gist_publication_test] = false
    session&.close
    database&.close
  end
end

RubyGistStorageTest.run
