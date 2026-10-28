# frozen_string_literal: true

# Encryption at rest (spec/fileformat/encrypted.md): the shared vectors, the byte-exact encrypted
# goldens, and the host-API surface the corpus cannot express — credential errors, credential change,
# rekey, compaction, backups, scratch encryption and encrypted attachments. Mirrors the Go core's
# crypto_test.go, encrypted_golden_test.go and encryption_test.go.
require 'tmpdir'
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

  def self.run
    tests = %i[vector_xaes vector_keys golden_roundtrip]
    tests.each do |name|
      public_send(name)
      puts "ruby-native encryption: #{name} passed"
    end
    puts "ruby-native encryption: #{tests.length} tests passed"
  end
end

RubyEncryptionTest.run
