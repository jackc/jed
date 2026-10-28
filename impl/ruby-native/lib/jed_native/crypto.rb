# frozen_string_literal: true

require 'openssl'
require 'securerandom'
require_relative 'error'

module JedNative
  # Encryption at rest (spec/design/encryption.md; bytes in spec/fileformat/encrypted.md).
  #
  # Every primitive comes from an approved library (CLAUDE.md §14 clause 3): AES-256 (the raw block),
  # AES-256-GCM and HKDF-SHA-256 from Ruby's OpenSSL binding, the CSPRNG from SecureRandom, Argon2id
  # from the argon2-kdf gem (loaded only when a passphrase is used). Only XAES-256-GCM's key
  # derivation (https://c2sp.org/XAES-256-GCM: two AES blocks and a GF(2^128) doubling) is written
  # here, over the library AES, and checked against the C2SP vectors in encryption_vectors.toml.
  module Crypto
    SUITE = 1
    TAG = 16
    NONCE = 24
    RESERVE = NONCE + TAG
    FIRST_BODY = 4
    MIN_PAGE_SIZE = 1024
    KEY_BLOCK_LENGTH = 128
    KEY_BLOCK_VERSION = 1
    KDF_ARGON2ID = 1
    KDF_HOST_KEY = 2
    ROLE_BODY = 1
    ROLE_META = 2
    ROLE_SCRATCH = 3
    ZERO_TAG = ("\0" * TAG).b.freeze

    module_function

    def aes_block(key, block)
      cipher = OpenSSL::Cipher.new('aes-256-ecb').encrypt
      cipher.key = key
      cipher.padding = 0
      cipher.update(block) + cipher.final
    end

    def gcm_seal(key, nonce, plaintext, aad)
      cipher = OpenSSL::Cipher.new('aes-256-gcm').encrypt
      cipher.key = key
      cipher.iv = nonce
      cipher.auth_data = aad
      sealed = plaintext.empty? ? +''.b : cipher.update(plaintext)
      sealed << cipher.final
      [sealed.b, cipher.auth_tag(TAG)]
    end

    # The plaintext, or nil when the tag does not authenticate it.
    def gcm_open(key, nonce, ciphertext, tag, aad)
      cipher = OpenSSL::Cipher.new('aes-256-gcm').decrypt
      cipher.key = key
      cipher.iv = nonce
      cipher.auth_tag = tag
      cipher.auth_data = aad
      plain = ciphertext.empty? ? +''.b : cipher.update(ciphertext)
      plain << cipher.final
      plain.b
    rescue OpenSSL::Cipher::CipherError
      nil
    end

    def hkdf(ikm, salt, info)
      OpenSSL::KDF.hkdf(ikm, salt: salt, info: info, length: 32, hash: 'SHA256')
    end

    def ad_prefix(role, position)
      'JEDB'.b + [SUITE, role, position].pack('CCN')
    end

    def crc32(bytes) = Zlib.crc32(bytes)

    # Argon2id through the approved argon2-kdf gem's binding of the reference libargon2. The gem's
    # convenience method takes the memory cost as a power of two; the format records any KiB count, so
    # this calls the raw entry point it binds with the exact parameters (RFC 9106, version 0x13).
    def argon2id(password, salt, t, m, p)
      begin
        require 'argon2/kdf'
        require 'fiddle'
      rescue LoadError
        raise Error.new('0A000', 'a passphrase credential needs the argon2-kdf gem (gem install argon2-kdf)')
      end
      pwd = Fiddle::Pointer[password.b]
      salt_ptr = Fiddle::Pointer[salt.b]
      out = Fiddle::Pointer.malloc(32, Fiddle::RUBY_FREE)
      status = Argon2::KDF::FFI.argon2id_hash_raw(t, m, p, pwd, password.bytesize, salt_ptr, salt.bytesize, out, 32)
      raise Error.new('58000', 'Argon2id failed') unless status.zero?
      out[0, 32].b
    end
  end

  # An XAES-256-GCM key: the AES key and its CMAC subkey K1, both computed once (C2SP XAES-256-GCM:
  # L = AES_K(0^128), K1 = L doubled in GF(2^128)).
  class XaesKey
    def initialize(key)
      @key = key.b
      l = Crypto.aes_block(@key, "\0".b * 16).bytes
      k1 = Array.new(16) { |i| ((l[i] << 1) & 0xff) | (i < 15 ? l[i + 1] >> 7 : 0) }
      k1[15] ^= 0x87 if (l[0] >> 7) == 1
      @k1 = k1
      @ecb = OpenSSL::Cipher.new('aes-256-ecb').encrypt
      @ecb.key = @key
      @ecb.padding = 0
    end

    # Kx = AES_K(M1 ⊕ K1) ‖ AES_K(M2 ⊕ K1), with Mi = 00 0i 58 00 ‖ N[0:12].
    def derive(nonce)
      prefix = nonce.byteslice(0, 12).bytes
      blocks = [1, 2].map do |i|
        m = [0, i, 0x58, 0] + prefix
        m.each_index.map { |j| m[j] ^ @k1[j] }.pack('C*')
      end
      @ecb.update(blocks.join)
    end

    # [ciphertext, tag] of plaintext under the 24-byte nonce.
    def seal(nonce, plaintext, aad)
      Crypto.gcm_seal(derive(nonce), nonce.byteslice(12, 12), plaintext, aad)
    end

    def open(nonce, ciphertext, tag, aad)
      Crypto.gcm_open(derive(nonce), nonce.byteslice(12, 12), ciphertext, tag, aad)
    end
  end

  # A credential unlocks an encrypted database: a passphrase (the key-encryption key is Argon2id of
  # it) or a 32-byte key the host keeps elsewhere (the KEK is HKDF of it). A create/open/attach
  # argument, never a setting; it never renders its secret.
  class Credential
    attr_reader :kind

    def self.passphrase(text)
      raise Error.new('22023', 'a passphrase must be a String') unless text.is_a?(String)
      new(:passphrase, text.b)
    end

    def self.key(bytes)
      raise Error.new('22023', 'a credential key must be 32 bytes') unless bytes.is_a?(String) && bytes.bytesize == 32
      new(:key, bytes.b)
    end

    def initialize(kind, secret)
      @kind, @secret = kind, secret.dup.freeze
      freeze
    end

    def secret = @secret
    def passphrase? = @kind == :passphrase
    def to_s = passphrase? ? 'Credential.passphrase(<redacted>)' : 'Credential.key(<redacted>)'
    alias inspect to_s
  end

  # Argon2id parameters for a passphrase credential at create or credential change (encrypted.md §5).
  class Argon2Params
    attr_reader :memory_kib, :iterations, :parallelism

    def self.default = new(memory_kib: 65_536, iterations: 3, parallelism: 4)

    def initialize(memory_kib:, iterations:, parallelism:)
      @memory_kib, @iterations, @parallelism = memory_kib, iterations, parallelism
      freeze
    end

    def valid?
      [@memory_kib, @iterations, @parallelism].all?(Integer) &&
        @parallelism.between?(1, 16) && @iterations.between?(1, 16) &&
        @memory_kib >= 8 * @parallelism && @memory_kib <= 1 << 20
    end
  end

  # The key block (pages 2 and 3, encrypted.md §5).
  module KeyBlock
    MAGIC = 'JEDK'.b

    module_function

    def generation(block) = block.byteslice(8, 8).unpack1('Q>')

    # The KEK for one copy, or nil when the credential is the wrong kind for it.
    def kek(block, credential)
      salt = block.byteslice(32, 32)
      case [block.getbyte(7), credential.kind]
      when [Crypto::KDF_HOST_KEY, :key] then Crypto.hkdf(credential.secret, salt, 'jed/v1/kek')
      when [Crypto::KDF_ARGON2ID, :passphrase]
        m, t, p = block.byteslice(16, 12).unpack('N3')
        Crypto.argon2id(credential.secret, salt, t, m, p)
      end
    end

    # The 128-byte copy wrapping dek for credential at generation; draws the KDF salt, then the wrap
    # nonce, from random (encrypted.md §8).
    def wrap(dek, generation, credential, params, random)
      block = MAGIC + [Crypto::KEY_BLOCK_VERSION, Crypto::SUITE].pack('nC')
      if credential.passphrase?
        params ||= Argon2Params.default
        raise Error.new('22023', 'Argon2id parameters are out of range') unless params.valid?
        block << [Crypto::KDF_ARGON2ID, generation, params.memory_kib, params.iterations, params.parallelism, 0].pack('CQ>N4')
      else
        block << [Crypto::KDF_HOST_KEY, generation, 0, 0, 0, 0].pack('CQ>N4')
      end
      block << draw(random, 32) << draw(random, 12)
      sealed, tag = Crypto.gcm_seal(kek(block, credential), block.byteslice(64, 12), dek, block.byteslice(0, 76))
      block << sealed << tag
      block << [Crypto.crc32(block)].pack('N')
      block
    end

    def draw(random, count)
      bytes = random.bytes(count)
      raise Error.new('58000', 'key-entropy source returned the wrong byte count') unless bytes.bytesize == count
      bytes.b
    end

    # A valid copy's 128 bytes and generation, or nil.
    def parse(page)
      return nil unless page && page.bytesize >= Crypto::KEY_BLOCK_LENGTH
      b = page.byteslice(0, Crypto::KEY_BLOCK_LENGTH)
      m, t, p, reserved = b.byteslice(16, 16).unpack('N4')
      kdf_ok = case b.getbyte(7)
               when Crypto::KDF_ARGON2ID then Argon2Params.new(memory_kib: m, iterations: t, parallelism: p).valid?
               when Crypto::KDF_HOST_KEY then m.zero? && t.zero? && p.zero?
               else false
               end
      generation = generation(b)
      valid = b.byteslice(0, 4) == MAGIC && b.byteslice(4, 2).unpack1('n') == Crypto::KEY_BLOCK_VERSION &&
              b.getbyte(6) == Crypto::SUITE && generation >= 1 && kdf_ok && reserved.zero? &&
              b.byteslice(124, 4).unpack1('N') == Crypto.crc32(b.byteslice(0, 124)) &&
              page.byteslice(Crypto::KEY_BLOCK_LENGTH..).count("\0") == page.bytesize - Crypto::KEY_BLOCK_LENGTH
      valid ? [b, generation] : nil
    end

    # The copy to unwrap: the valid copy with the greater generation (XX001 when none, or two of one
    # generation that differ).
    def select(page2, page3)
      a = parse(page2)
      b = parse(page3)
      raise Error.new('XX001', 'no valid key block') unless a || b
      return a[0] unless b
      return b[0] unless a
      raise Error.new('XX001', 'key block copies disagree') if a[1] == b[1] && a[0] != b[0]

      b[1] > a[1] ? b[0] : a[0]
    end

    # The page a credential change writes first: the copy that is not selected.
    def unselected_page(page2, page3)
      a = parse(page2)
      b = parse(page3)
      (a && b && b[1] > a[1]) || (!a && b) ? 2 : 3
    end

    def unwrap(block, credential)
      key = kek(block, credential)
      dek = key && Crypto.gcm_open(key, block.byteslice(64, 12), block.byteslice(76, 32), block.byteslice(108, 16), block.byteslice(0, 76))
      raise Error.new('28P01', 'the credential does not unlock this database') unless dek
      dek
    end
  end

  # The keys and nonce source of one open encrypted database (encrypted.md §3, §8), and the tags of
  # the stored pages its file's bound pointers name. A reader learns a page's tag from the pointer it
  # followed (the meta, a catalog entry, an interior node, a chain page, a leaf's external value) and a
  # writer from the seal; every read of a body page compares the stored tag with that one first.
  class PageCodec
    attr_reader :dek, :random, :tags

    def self.create(credential, params, random)
      dek = KeyBlock.draw(random, 32)
      new(dek, random, KeyBlock.wrap(dek, 1, credential, params, random))
    end

    # A throwaway codec under a random key from the OS CSPRNG (a rebuild's sealed scratch domain).
    def self.throwaway
      create(Credential.key(SecureRandom.random_bytes(32)), nil, OSKeyRandom.new)
    end

    def self.unlock(page2, page3, credential, random)
      block = KeyBlock.select(page2, page3)
      new(KeyBlock.unwrap(block, credential), random, block)
    end

    def initialize(dek, random, key_block)
      @dek, @random, @key_block = dek.b.freeze, random, key_block.b.freeze
      @page = XaesKey.new(Crypto.hkdf(@dek, ''.b, 'jed/v1/page'))
      @meta = XaesKey.new(Crypto.hkdf(@dek, ''.b, 'jed/v1/meta'))
      @tags = {}
      @plain = {}
      @lock = Mutex.new
    end

    # The same keys and stream with a fresh pointer namespace: a new file's image (compaction, backup).
    def fork(key_block = @key_block)
      self.class.new(@dek, @random, key_block)
    end

    def key_block = @lock.synchronize { @key_block }
    def key_block=(block)
      @lock.synchronize { @key_block = block.b.freeze }
    end

    def key_block_page(page_size) = key_block.ljust(page_size, "\0")

    def nonce = KeyBlock.draw(@random, Crypto::NONCE)

    def tag(page) = page.zero? ? Crypto::ZERO_TAG : @tags[page]
    def note(page, tag)
      return if page.zero?
      @tags[page] = tag.b.freeze
    end

    def forget_tags = @tags.clear

    # A body page's stored bytes from its plaintext (the first page_size − 40 bytes, CRC field zero):
    # ciphertext ‖ nonce ‖ tag. Draws one nonce; records the tag.
    def seal_body(index, plaintext)
      nonce = self.nonce
      sealed, tag = @page.seal(nonce, plaintext, Crypto.ad_prefix(Crypto::ROLE_BODY, index))
      note(index, tag)
      sealed + nonce + tag
    end

    # The plaintext (page_size − 40 bytes) of stored body page index, whose pointer named tag.
    def open_body(index, stored, expected)
      raise Error.new('XX001', 'short encrypted page') if stored.bytesize < Crypto::MIN_PAGE_SIZE
      split = stored.bytesize - Crypto::RESERVE
      tag = stored.byteslice(split + Crypto::NONCE, Crypto::TAG)
      raise Error.new('XX001', "page #{index} has no bound pointer") unless expected
      raise Error.new('XX001', "page #{index} does not match the pointer that names it") unless tag == expected
      key = [index, tag]
      cached = @lock.synchronize { @plain[key] }
      return cached if cached
      plain = @page.open(stored.byteslice(split, Crypto::NONCE), stored.byteslice(0, split), tag,
                         Crypto.ad_prefix(Crypto::ROLE_BODY, index))
      raise Error.new('XX001', "page #{index} fails authentication") unless plain
      raise Error.new('XX001', 'encrypted page carries a CRC') unless plain.byteslice(12, 4) == "\0\0\0\0".b
      plain.freeze
      @lock.synchronize do
        @plain.shift while @plain.length >= 4096
        @plain[key] = plain
      end
      plain
    end

    # Seal meta slot `slot` in place (encrypted.md §6): the empty plaintext under the meta key, its AD
    # the slot, the meta page with its CRC and reserve zeroed, and each manifest page in chain order.
    def seal_meta(slot, meta, manifest)
      ps = meta.bytesize
      meta[32, 4] = "\0" * 4
      meta[ps - Crypto::RESERVE, Crypto::RESERVE] = "\0" * Crypto::RESERVE
      nonce = self.nonce
      _, tag = @meta.seal(nonce, ''.b, meta_aad(slot, meta, manifest))
      meta[ps - Crypto::RESERVE, Crypto::RESERVE] = nonce + tag
      meta[32, 4] = [FileFormat.new.meta_crc(meta)].pack('N')
      meta
    end

    def meta_authentic?(slot, stored, manifest)
      ps = stored.bytesize
      nonce = stored.byteslice(ps - Crypto::RESERVE, Crypto::NONCE)
      tag = stored.byteslice(ps - Crypto::TAG, Crypto::TAG)
      zeroed = stored.dup
      zeroed[32, 4] = "\0" * 4
      zeroed[ps - Crypto::RESERVE, Crypto::RESERVE] = "\0" * Crypto::RESERVE
      !@meta.open(nonce, ''.b, tag, meta_aad(slot, zeroed, manifest)).nil?
    end

    def verify_meta(slot, stored, manifest)
      raise Error.new('XX001', 'meta page fails authentication') unless meta_authentic?(slot, stored, manifest)
    end

    # The key block for a credential change: the same DEK at the next generation (encrypted.md §5).
    def rewrap(credential, params)
      generation = KeyBlock.generation(key_block)
      raise Error.new('XX001', 'key block generation exhausted') if generation == 0xFFFF_FFFF_FFFF_FFFF
      KeyBlock.wrap(@dek, generation + 1, credential, params, @random)
    end

    private

    def meta_aad(slot, zeroed, manifest)
      aad = Crypto.ad_prefix(Crypto::ROLE_META, slot) + zeroed
      manifest.each { |index, bytes| aad << [index].pack('N') << bytes }
      aad
    end
  end

  class PageCodec
    # A keyless stand-in that only selects the encrypted layout: a meta slot's fields can be read and
    # its manifest checked before the key block is unlocked (slot selection is keyless, §6).
    LAYOUT = new("\0".b * 32, nil, ''.b)
  end

  # A sealed scratch file (spec/fileformat/encrypted.md §11): a random key and file id from the
  # handle's scratch stream, never persisted; logical bytes in 4096-byte blocks, block i stored at
  # i·4136 as its XAES-256-GCM ciphertext, nonce and tag, its associated data binding the file id, the
  # ordinal and the block's in-memory write counter (never reset), so a replayed older version, a
  # moved block or an altered byte fails to open (58030). The block last written stays in plaintext
  # until another is written. Exposes the IO subset the scratch spools use: seek/pos/read/write/close!.
  class SealedScratchIO
    BLOCK = 4096
    STORED = BLOCK + Crypto::RESERVE

    def initialize(file, stream)
      @file, @stream = file, stream
      @key = XaesKey.new(KeyBlock.draw(stream, 32))
      @id = KeyBlock.draw(stream, 16)
      @size = @pos = 0
      @counters, @live = [], []
      @cached_at, @cached = nil, nil
    end

    def pos = @pos
    alias tell pos

    def seek(offset, whence = IO::SEEK_SET)
      @pos = whence == IO::SEEK_END ? @size + offset : whence == IO::SEEK_CUR ? @pos + offset : offset
      0
    end

    def read(length)
      return nil if @pos >= @size && length.positive?
      count = [length, @size - @pos].min
      out = +''.b
      while out.bytesize < count
        ordinal, within = (@pos + out.bytesize).divmod(BLOCK)
        take = [BLOCK - within, count - out.bytesize].min
        out << load(ordinal).byteslice(within, take)
      end
      @pos += count
      out
    end

    def write(bytes)
      bytes = bytes.b
      done = 0
      while done < bytes.bytesize
        ordinal, within = (@pos + done).divmod(BLOCK)
        take = [BLOCK - within, bytes.bytesize - done].min
        unless @cached_at == ordinal
          block = take == BLOCK ? ("\0".b * BLOCK) : load(ordinal).dup
          flush_cached
          @cached_at, @cached = ordinal, block
        end
        @cached[within, take] = bytes.byteslice(done, take)
        done += take
      end
      @pos += bytes.bytesize
      @size = [@size, @pos].max
      bytes.bytesize
    end

    def close!
      @cached_at = @cached = nil
      @file.close!
    end

    private

    def ad(ordinal, counter) = 'JEDB'.b + [Crypto::SUITE, Crypto::ROLE_SCRATCH].pack('CC') + @id + [ordinal, counter].pack('Q>Q>')

    def load(ordinal)
      return @cached if @cached_at == ordinal
      return "\0".b * BLOCK unless @live[ordinal]
      @file.seek(ordinal * STORED)
      stored = @file.read(STORED)
      raise Error.new('58030', 'truncated scratch block') unless stored && stored.bytesize == STORED
      plain = @key.open(stored.byteslice(BLOCK, Crypto::NONCE), stored.byteslice(0, BLOCK),
                        stored.byteslice(BLOCK + Crypto::NONCE, Crypto::TAG), ad(ordinal, @counters[ordinal]))
      raise Error.new('58030', 'a scratch block fails authentication') unless plain
      plain
    end

    def flush_cached
      return unless @cached_at
      ordinal, block = @cached_at, @cached
      @cached_at = @cached = nil
      counter = (@counters[ordinal] || 0) + 1
      nonce = KeyBlock.draw(@stream, Crypto::NONCE)
      sealed, tag = @key.seal(nonce, block, ad(ordinal, counter))
      @file.seek(ordinal * STORED)
      @file.write(sealed + nonce + tag)
      @counters[ordinal] = counter
      @live[ordinal] = true
    end
  end

  # The production key-entropy source: the OS CSPRNG (never the seeded SQL random source).
  class OSKeyRandom
    def bytes(length) = SecureRandom.random_bytes(length)
  end

  # The file suite the CRC-valid meta slots declare (offset 6, the reserve byte at 7): 0 or 1.
  def self.file_suite(slot0, slot1)
    codec = FileFormat.new
    suites = [slot0, slot1].filter_map do |meta|
      next unless meta && meta.bytesize >= 64 && meta.byteslice(32, 4).unpack1('N') == codec.meta_crc(meta)
      case [meta.getbyte(6), meta.getbyte(7)]
      when [0, 0] then 0
      when [Crypto::SUITE, Crypto::RESERVE] then Crypto::SUITE
      else raise Error.new('XX001', 'unknown encryption suite')
      end
    end
    raise Error.new('XX001', 'meta slots disagree on encryption') if suites.uniq.length > 1
    suites.first || 0
  end
end
