# frozen_string_literal: true

# Encryption at rest (spec/fileformat/encrypted.md): the shared vectors, the byte-exact encrypted
# goldens, and the host-API surface the corpus cannot express — credential errors, credential change,
# rekey, compaction, backups, scratch encryption and encrypted attachments. Mirrors the Go core's
# crypto_test.go, encrypted_golden_test.go and encryption_test.go.
require 'tmpdir'
require 'tempfile'
require 'fileutils'
require_relative '../lib/jed_native'

module RubyEncryptionTest
  ROOT = File.expand_path('../../../spec', __dir__)
  GOLDENS = File.join(ROOT, 'fileformat', 'fixtures')
  VECTORS = File.join(ROOT, 'fileformat', 'encryption_vectors.toml')
  GOLDEN_PAGE = 1024
  GOLDEN_SEED = 0x4A454442

  def self.assert(value, message = 'assertion failed')
    raise message unless value
  end

  def self.error(code)
    yield
  rescue JedNative::Error => exception
    assert(exception.sqlstate == code, "expected #{code}, got #{exception.sqlstate}: #{exception.message}")
  else
    raise "expected #{code}"
  end

  def self.hex(text) = [text].pack('H*')
  def self.golden_key = JedNative::Credential.key("\x4A".b * 32)

  # A minimal reader for the vectors file: its tables and arrays of tables of string/integer fields.
  def self.vectors
    tables = {}
    current = nil
    File.foreach(VECTORS) do |line|
      line = line.strip
      next if line.empty? || line.start_with?('#')
      if (name = line[/\A\[\[(\w+)\]\]\z/, 1])
        (tables[name] ||= []) << (current = {})
      elsif (name = line[/\A\[(\w+)\]\z/, 1])
        tables[name] = current = {}
      elsif (match = line.match(/\A(\w+) = (?:"(.*)"|(\d+))\z/))
        (current || tables)[match[1]] = match[2] || Integer(match[3])
      end
    end
    tables
  end

  def self.vector_xaes
    vectors.fetch('xaes').each do |v|
      key = JedNative::XaesKey.new(hex(v['key_hex']))
      sealed, tag = key.seal(hex(v['nonce_hex']), hex(v['plaintext_hex']), hex(v['aad_hex']))
      assert(sealed + tag == hex(v['sealed_hex']), "xaes #{v['name']} differs")
      plain = key.open(hex(v['nonce_hex']), sealed, tag, hex(v['aad_hex']))
      assert(plain == hex(v['plaintext_hex']), "xaes #{v['name']} does not open")
      tampered = tag.dup
      tampered.setbyte(0, tampered.getbyte(0) ^ 1)
      assert(key.open(hex(v['nonce_hex']), sealed, tampered, hex(v['aad_hex'])).nil?, 'a tampered tag opened')
    end
  end

  def self.vector_keys
    v = vectors
    dek = hex(v['subkeys']['dek_hex'])
    assert(JedNative::Crypto.hkdf(dek, ''.b, 'jed/v1/page') == hex(v['subkeys']['page_key_hex']), 'page key')
    assert(JedNative::Crypto.hkdf(dek, ''.b, 'jed/v1/meta') == hex(v['subkeys']['meta_key_hex']), 'meta key')
    k = v['host_kek']
    assert(JedNative::Crypto.hkdf(hex(k['key_hex']), hex(k['kdf_salt_hex']), 'jed/v1/kek') == hex(k['kek_hex']), 'host kek')
    a = v['argon2id']
    out = JedNative::Crypto.argon2id(a['password'], a['salt'], a['iterations'], a['memory_kib'], a['parallelism'])
    assert(out == hex(a['key_hex']), 'argon2id')
    block = v['key_block']
    codec = JedNative::PageCodec.create(golden_key, nil, JedNative::SeededRandom.new(block['seed']))
    assert(codec.key_block == hex(block['block_hex']), 'key block')
    assert(codec.dek == hex(block['dek_hex']), 'key block dek')
    unlocked = JedNative::PageCodec.unlock(codec.key_block_page(1024), codec.key_block_page(1024), golden_key, nil)
    assert(unlocked.dek == codec.dek, 'unwrap')
    error('28P01') { JedNative::PageCodec.unlock(codec.key_block_page(1024), nil, JedNative::Credential.key("\x4B".b * 32), nil) }
    error('28P01') { JedNative::PageCodec.unlock(codec.key_block_page(1024), nil, JedNative::Credential.passphrase('x'), nil) }
    rewrap = v['rewrap']
    rotated = JedNative::PageCodec.new(codec.dek, JedNative::SeededRandom.new(rewrap['seed']), codec.key_block)
    new_block = rotated.rewrap(JedNative::Credential.key(hex(rewrap['key_hex'])), nil)
    assert(new_block == hex(rewrap['block_hex']), 'rewrap block')
  end

  def self.unlock(image, ps = GOLDEN_PAGE)
    JedNative::PageCodec.unlock(image.byteslice(2 * ps, ps), image.byteslice(3 * ps, ps), golden_key, nil)
  end

  # Each encrypted golden decodes under the goldens' key, and its logical database re-encodes, under a
  # fresh key block drawn from the seeded stream, to exactly its bytes (encrypted.md §7, §8).
  def self.golden_roundtrip
    count = 0
    Dir[File.join(GOLDENS, 'encrypted_*.jed')].sort.each do |path|
      next if File.basename(path).include?('rekey')
      image = File.binread(path)
      state = JedNative::FileFormat.new(crypto: unlock(image)).decode_image(image)
      writer = JedNative::FileFormat.new(crypto: JedNative::PageCodec.create(golden_key, nil, JedNative::SeededRandom.new(GOLDEN_SEED)))
      actual = writer.build_image(state[:types], state[:sequences], state[:tables], GOLDEN_PAGE, state[:collations], views: state[:views])
      unless actual == image
        at = (0...[actual.bytesize, image.bytesize].min).find { |i| actual.getbyte(i) != image.getbyte(i) }
        raise "#{File.basename(path)}: re-encoded bytes differ (#{actual.bytesize} vs #{image.bytesize}, first at page #{at && at / GOLDEN_PAGE})"
      end
      keyless = begin
        JedNative::FileFormat.new.decode_image(image)
      rescue StandardError
        nil
      end
      assert(keyless.nil?, "#{File.basename(path)} decoded without a key")
      count += 1
    end
    assert(count == 5, "expected 5 encrypted goldens, got #{count}")
  end

  def self.filler(n)
    x = 0x4A454442
    Array.new(n) do
      x ^= (x << 13) & 0xFFFF_FFFF
      x ^= x >> 17
      x ^= (x << 5) & 0xFFFF_FFFF
      x
    end
  end

  ALPHA = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/'

  GOLDEN_SQL = {
    'encrypted_empty_db.jed' => [[], 'SELECT 1'],
    'encrypted_pk_table.jed' => [['CREATE TABLE t (id i32 PRIMARY KEY, v i16)'] +
                                 (1..300).map { |i| "INSERT INTO t VALUES (#{i}, #{i == 3 ? 'NULL' : i * 10})" },
                                 'SELECT count(*), sum(id), sum(v), count(v) FROM t'],
    'encrypted_overflow_table.jed' => [['CREATE TABLE t (id i32 PRIMARY KEY, body text, blob bytea)',
                                        "INSERT INTO t VALUES (1, '#{filler(2500).map { |x| ALPHA[x % 64] }.join}', '\\x#{filler(1200).map { |x| format('%02x', x % 256) }.join}')",
                                        "INSERT INTO t VALUES (2, 'small', '\\xcafe')",
                                        'INSERT INTO t VALUES (3, NULL, NULL)'],
                                       'SELECT id, md5(body), md5(blob) FROM t ORDER BY id'],
    'encrypted_index_table.jed' => [['CREATE TABLE t (a i32 PRIMARY KEY, v i64, r i32range)',
                                     'CREATE INDEX t_r ON t USING gist (r)', 'CREATE INDEX t_v ON t (v)'] +
                                    (1..150).map { |a| "INSERT INTO t VALUES (#{a}, #{a * 3}, i32range(#{a}, #{a + 5}))" },
                                    'SELECT count(*) FILTER (WHERE r && i32range(40, 44)), count(*) FILTER (WHERE v = 300), sum(v) FROM t'],
    'encrypted_catalog.jed' => [(0...30).map { |i| format('CREATE TABLE t%02d (id i32 PRIMARY KEY, a text, b i64)', i) },
                                'SELECT count(*) FROM t29']
  }.freeze

  # The cross-core contract as the Go/Rust tests state it: the logical database built by SQL in an
  # encrypted file — its trees split at the encrypted capacity by incremental commits — has the
  # golden as its sealed from-scratch image, and the golden reads back with the key.
  def self.golden_sql
    GOLDEN_SQL.each do |name, (statements, probe)|
      Dir.mktmpdir('ruby-native-encrypted') do |dir|
        db = JedNative.create(File.join(dir, 'golden.jed'), page_size: GOLDEN_PAGE, credential: golden_key, fsync: false)
        statements.each { |sql| db.execute(sql) }
        want = db.query(probe).rows
        state = db.engine.export_storage
        writer = JedNative::FileFormat.new(crypto: JedNative::PageCodec.create(golden_key, nil, JedNative::SeededRandom.new(GOLDEN_SEED)))
        writer.instance_variable_set(:@collation_key, db.engine.send(:statistics_codec).instance_variable_get(:@collation_key))
        image = writer.build_image(state[:types], state[:sequences], state[:tables], GOLDEN_PAGE, state[:collations], views: state[:views])
        db.close
        golden = File.binread(File.join(GOLDENS, name))
        unless image == golden
          at = (0...[image.bytesize, golden.bytesize].min).find { |i| image.getbyte(i) != golden.getbyte(i) }
          raise "#{name}: sealed image differs (#{image.bytesize} vs #{golden.bytesize}, page #{at && at / GOLDEN_PAGE})"
        end
        path = File.join(dir, 'read.jed')
        File.binwrite(path, golden)
        JedNative.open(path, credential: golden_key) do |read|
          assert(read.query(probe).rows == want, "#{name}: rows read back differ")
        end
      end
    end
  end

  # A rekey's cross-core bytes (encrypted.md §12): a plaintext file holding encrypted_index_table's
  # rows, built descending, encrypted in place, is the golden built at the rekey's txid.
  def self.golden_rekey
    Dir.mktmpdir('ruby-native-rekey') do |dir|
      path = File.join(dir, 'rekey.jed')
      environment = JedNative::Environment.new.insecure_seeded_key_entropy(GOLDEN_SEED)
      db = JedNative.create(path, page_size: GOLDEN_PAGE, fsync: false, environment: environment,
                                  scratch_encryption: :on, locking: :exclusive)
      db.execute('CREATE TABLE t (a i32 PRIMARY KEY, v i64, r i32range)')
      db.execute('CREATE INDEX t_r ON t USING gist (r)')
      db.execute('CREATE INDEX t_v ON t (v)')
      150.downto(1) { |a| db.execute("INSERT INTO t VALUES (#{a}, #{a * 3}, i32range(#{a}, #{a + 5}))") }
      probe = 'SELECT count(*) FILTER (WHERE r && i32range(40, 44)), count(*) FILTER (WHERE v = 300), sum(v) FROM t'
      want = db.query(probe).rows
      db.rekey('main', golden_key)
      assert(db.query(probe).rows == want, "rows after the rekey: #{db.query(probe).rows}")
      db.execute('INSERT INTO t VALUES (151, 1, i32range(1, 2))')
      db.execute('DELETE FROM t WHERE a = 151')
      db.close
      error('28P02') { JedNative.open(path) }
      db = JedNative.open(path, credential: golden_key)
      db.rekey('main', nil) # decrypt again
      db.close
      JedNative.open(path) { |plain| assert(plain.query(probe).rows == want, 'rows after decrypting') }
    end
    Dir.mktmpdir('ruby-native-rekey') do |dir|
      path = File.join(dir, 'rekey.jed')
      environment = JedNative::Environment.new.insecure_seeded_key_entropy(GOLDEN_SEED)
      db = JedNative.create(path, page_size: GOLDEN_PAGE, fsync: false, environment: environment,
                                  scratch_encryption: :on, locking: :exclusive)
      db.execute('CREATE TABLE t (a i32 PRIMARY KEY, v i64, r i32range)')
      db.execute('CREATE INDEX t_r ON t USING gist (r)')
      db.execute('CREATE INDEX t_v ON t (v)')
      150.downto(1) { |a| db.execute("INSERT INTO t VALUES (#{a}, #{a * 3}, i32range(#{a}, #{a + 5}))") }
      db.rekey('main', golden_key)
      db.close
      golden = File.binread(File.join(GOLDENS, 'encrypted_rekey_index_table.jed'))
      image = File.binread(path)
      unless image == golden
        at = (0...[image.bytesize, golden.bytesize].min).find { |i| image.getbyte(i) != golden.getbyte(i) }
        raise "the rekeyed file differs from the golden (#{image.bytesize} vs #{golden.bytesize}, page #{at && at / GOLDEN_PAGE})"
      end
    end
  end

  FAST = JedNative::Argon2Params.new(memory_kib: 64, iterations: 1, parallelism: 1)

  def self.populate(db, rows = 300)
    db.execute('CREATE TABLE t (id i32 PRIMARY KEY, v text, w text)')
    db.execute('CREATE INDEX t_v ON t (v)')
    rows.times { |i| db.execute("INSERT INTO t VALUES (#{i}, 'v#{i}', repeat('w', #{(i % 7) * 400}))") }
    db.execute('DELETE FROM t WHERE id % 5 = 0')
  end

  PROBE = 'SELECT count(*), sum(length(w)), min(v) FROM t'

  # Credential errors (encrypted.md §9) and the bytes a credential keeps out of the file.
  def self.credential_errors
    Dir.mktmpdir('ruby-native-encryption') do |dir|
      path = File.join(dir, 'a.jed')
      error('0A000') { JedNative.create(path, page_size: 512, credential: golden_key) }
      error('55000') { JedNative.memory(credential: golden_key) }
      JedNative.create(path, credential: golden_key, fsync: false) { |db| populate(db); db.execute("INSERT INTO t VALUES (9999, 'v17-v18-v19-v20', '')"); db.compact }
      assert(!File.binread(path).include?('v17-v18-v19-v20'), 'plaintext reached the file')
      error('28P02') { JedNative.open(path) }
      error('28P01') { JedNative.open(path, credential: JedNative::Credential.key("\x4B".b * 32)) }
      error('28P01') { JedNative.open(path, credential: JedNative::Credential.passphrase('x')) }
      plain = File.join(dir, 'plain.jed')
      JedNative.create(plain, fsync: false) { |db| populate(db, 10) }
      error('28P03') { JedNative.open(plain, credential: golden_key) }
      JedNative.open(plain) do |db|
        error('28P03') { db.change_credential('main', golden_key) }
        error('55000') { db.change_credential('nope', golden_key) } rescue error('42704') { db.change_credential('nope', golden_key) }
      end
      assert(golden_key.inspect == 'Credential.key(<redacted>)' && !golden_key.to_s.include?('J'), 'a credential rendered its secret')
      # A tampered body page fails as XX001, never wrong rows.
      bytes = File.binread(path)
      root = JedNative.open(path, credential: golden_key) { |db| db.engine.instance_variable_get(:@storage).meta[:root_page] }
      bytes.setbyte(root * 8192 + 100, bytes.getbyte(root * 8192 + 100) ^ 1)
      File.binwrite(path, bytes)
      failed = begin
        JedNative.open(path, credential: golden_key) { |db| db.query('SELECT count(*), sum(length(w)) FROM t WHERE v > $1', '') }
        false
      rescue JedNative::Error => e
        e.sqlstate == 'XX001'
      end
      assert(failed, 'a tampered page did not fail XX001')
    end
  end

  # A credential change rewraps the data key: the old credential stops working, the new one opens,
  # and a passphrase credential stretches with Argon2id (encrypted.md §5).
  def self.credential_change
    Dir.mktmpdir('ruby-native-encryption') do |dir|
      path = File.join(dir, 'a.jed')
      want = nil
      JedNative.create(path, credential: golden_key, fsync: false) do |db|
        populate(db)
        want = db.query(PROBE).rows
        db.change_credential('main', JedNative::Credential.passphrase('correct horse'), FAST)
        assert(db.query(PROBE).rows == want, 'rows after a credential change')
        db.execute("INSERT INTO t VALUES (1000, 'z', 'z')")
        db.execute('DELETE FROM t WHERE id = 1000')
        error('22023') { db.change_credential('main', JedNative::Credential.passphrase('p'), JedNative::Argon2Params.new(memory_kib: 1, iterations: 1, parallelism: 1)) }
      end
      image = File.binread(path)
      assert(image.byteslice(2 * 8192, 128) == image.byteslice(3 * 8192, 128), 'key-block copies differ after a change')
      assert(JedNative::KeyBlock.generation(image.byteslice(2 * 8192, 128)) == 2, 'generation after one change')
      error('28P01') { JedNative.open(path, credential: golden_key) }
      JedNative.open(path, credential: JedNative::Credential.passphrase('correct horse')) do |db|
        assert(db.query(PROBE).rows == want, 'rows under the new credential')
        db.change_credential('main', golden_key)
      end
      # A torn change: page 3 holds the newer copy, page 2 the older; the newer one opens and a
      # writable open repairs the pair.
      older = JedNative.open(path, credential: golden_key) { |db| db.engine.instance_variable_get(:@storage).crypto.key_block }
      bytes = File.binread(path)
      newer = JedNative::PageCodec.unlock(bytes.byteslice(2 * 8192, 8192), nil, golden_key, JedNative::OSKeyRandom.new)
                                  .rewrap(JedNative::Credential.passphrase('p2'), FAST)
      bytes[3 * 8192, 8192] = newer.ljust(8192, "\0")
      File.binwrite(path, bytes)
      error('28P01') { JedNative.open(path, credential: golden_key, read_only: true) }
      JedNative.open(path, credential: JedNative::Credential.passphrase('p2')) { |db| assert(db.query(PROBE).rows == want, 'rows under a torn change') }
      repaired = File.binread(path)
      assert(repaired.byteslice(2 * 8192, 128) == newer && older != newer, 'a writable open did not repair the key block')
    end
  end

  # Compaction and both backup forms keep the key (encrypted.md §13), and an encrypted file
  # compacts through a sealed rewrite.
  def self.compaction_and_backup
    Dir.mktmpdir('ruby-native-encryption') do |dir|
      path = File.join(dir, 'a.jed')
      want = nil
      JedNative.create(path, credential: golden_key, fsync: false, page_size: 1024) do |db|
        populate(db)
        want = db.query(PROBE).rows
        db.backup(File.join(dir, 'identical.jed'))
        db.backup(File.join(dir, 'compacted.jed'), form: :compacted)
        before = File.size(path)
        db.compact
        assert(File.size(path) < before, 'compaction did not shrink the file')
        assert(db.query(PROBE).rows == want, 'rows after compaction')
        db.execute("UPDATE t SET w = repeat('q', 2000) WHERE id = 1")
        want = db.query(PROBE).rows
      end
      JedNative.open(path, credential: golden_key) { |db| assert(db.query(PROBE).rows == want, 'rows after reopening the compacted file') }
      %w[identical compacted].each do |name|
        copy = File.join(dir, "#{name}.jed")
        error('28P02') { JedNative.open(copy) }
        JedNative.open(copy, credential: golden_key) do |db|
          assert(db.query('SELECT count(*) FROM t').rows == [[240]], "#{name} backup rows")
          report = db.integrity_check(max_findings: 0)
          assert(report.findings.empty?, "#{name} backup integrity: #{report.findings.map(&:kind)}")
        end
      end
    end
  end

  # Scratch encryption (encrypted.md §11): an encrypted handle's spilled sorts and hash tables still
  # answer exactly, an encrypted attachment needs scratch_encryption = on, and attachments take their
  # own credential.
  def self.scratch_and_attachments
    Dir.mktmpdir('ruby-native-encryption') do |dir|
      path = File.join(dir, 'a.jed')
      JedNative.create(path, credential: golden_key, fsync: false) do |db|
        populate(db)
        want = db.query('SELECT v, count(*) FROM t GROUP BY v ORDER BY v DESC LIMIT 5').rows
        db.work_mem = 1024
        assert(db.engine.scratch_sealed?, 'an encrypted handle seals its scratch')
        assert(db.query('SELECT v, count(*) FROM t GROUP BY v ORDER BY v DESC LIMIT 5').rows == want, 'spilled rows')
        sorted = db.query('SELECT id FROM t ORDER BY w DESC, id').rows
        assert(sorted.length == 240, 'spilled sort')
      end
      io = JedNative::SealedScratchIO.new(Tempfile.new('t').tap(&:binmode), JedNative::SeededRandom.new(1))
      io.write('a' * 5000)
      io.seek(4090)
      io.write('xyz' * 3000)
      io.seek(0)
      assert(io.read(20_000) == 'a' * 4090 + 'xyz' * 3000, 'sealed scratch round trip')
      io.close!
      plain = File.join(dir, 'main.jed')
      JedNative.create(plain, fsync: false) do |db|
        error('55000') { db.attach('enc', path, credential: golden_key) }
        error('55000') { db.attach('mem', :memory, credential: golden_key) }
      end
      JedNative.open(plain, scratch_encryption: :on) do |db|
        error('28P02') { db.attach('enc', path) }
        error('28P01') { db.attach('enc', path, credential: JedNative::Credential.key("\x4B".b * 32)) }
        db.attach('enc', path, credential: golden_key)
        assert(db.query('SELECT count(*) FROM enc.t').rows == [[240]], 'rows through an encrypted attachment')
        db.execute("INSERT INTO enc.t VALUES (5000, 'x', 'y')")
        db.change_credential('enc', JedNative::Credential.passphrase('att'), FAST)
        db.compact('enc')
        assert(db.query('SELECT count(*) FROM enc.t').rows == [[241]], 'rows after changing an attachment')
      end
      JedNative.open(path, credential: JedNative::Credential.passphrase('att')) { |db| assert(db.query('SELECT count(*) FROM t').rows == [[241]], 'attachment rows') }
    end
  end

  def self.run
    tests = %i[vector_xaes vector_keys golden_roundtrip golden_sql golden_rekey credential_errors credential_change
               compaction_and_backup scratch_and_attachments]
    tests.each do |name|
      public_send(name)
      puts "ruby-native encryption: #{name} passed"
    end
    puts "ruby-native encryption: #{tests.length} tests passed"
  end
end

RubyEncryptionTest.run
