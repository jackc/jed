# The encrypted variant's cryptography for the Ruby reference (spec/fileformat/encrypted.md §3):
# XAES-256-GCM's key derivation written from its specification (https://c2sp.org/XAES-256-GCM) over
# the openssl gem's AES, AES-256-GCM, HKDF-SHA-256 and the key wrap. No Argon2id: the goldens and
# vectors derive their KEK from a host key (kdf 2); the Argon2id vector is checked by the cores.

require "openssl"

module Xaes
  module_function

  # AES-256 of one 16-byte block under `key`.
  def aes_block(key, block)
    c = OpenSSL::Cipher.new("aes-256-ecb").encrypt
    c.key = key
    c.padding = 0
    c.update(block) + c.final
  end

  # L = AES_K(0^128) doubled in GF(2^128): the CMAC subkey K1 (NIST SP 800-38B §6.1).
  def subkey(key)
    l = aes_block(key, "\0" * 16).bytes
    carry = l[0] >> 7
    k1 = l.each_with_index.map { |b, i| ((b << 1) & 0xff) | (i < 15 ? l[i + 1] >> 7 : 0) }
    k1[15] ^= 0x87 if carry == 1
    k1.pack("C*")
  end

  def derive(key, nonce)
    k1 = subkey(key)
    m1 = "\x00\x01X\x00".b + nonce[0, 12]
    m2 = "\x00\x02X\x00".b + nonce[0, 12]
    xor = ->(a, b) { a.bytes.zip(b.bytes).map { |x, y| x ^ y }.pack("C*") }
    aes_block(key, xor.(m1, k1)) + aes_block(key, xor.(m2, k1))
  end

  def gcm_seal(key, nonce, plaintext, aad)
    c = OpenSSL::Cipher.new("aes-256-gcm").encrypt
    c.key = key
    c.iv = nonce
    c.auth_data = aad
    ct = plaintext.empty? ? "".b : c.update(plaintext)
    ct += c.final
    ct.b + c.auth_tag(16)
  end

  def gcm_open(key, nonce, sealed, aad)
    c = OpenSSL::Cipher.new("aes-256-gcm").decrypt
    c.key = key
    c.iv = nonce
    c.auth_tag = sealed[-16, 16]
    c.auth_data = aad
    body = sealed[0...-16]
    (body.empty? ? "".b : c.update(body)) + c.final
  rescue OpenSSL::Cipher::CipherError
    nil
  end

  # XAES-256-GCM seal: ciphertext ‖ 16-byte tag.
  def seal(key, nonce, plaintext, aad)
    gcm_seal(derive(key, nonce), nonce[12, 12], plaintext, aad)
  end

  def open(key, nonce, sealed, aad)
    gcm_open(derive(key, nonce), nonce[12, 12], sealed, aad)
  end

  def hkdf(ikm, salt, info)
    OpenSSL::KDF.hkdf(ikm, salt: salt, info: info, length: 32, hash: "SHA256")
  end

  def page_key(dek) = hkdf(dek, "".b, "jed/v1/page")
  def meta_key(dek) = hkdf(dek, "".b, "jed/v1/meta")
  def host_kek(host_key, kdf_salt) = hkdf(host_key, kdf_salt, "jed/v1/kek")
end

# The insecure seeded key-entropy stream (encrypted.md §8): splitmix64, big-endian 8-byte chunks, a
# final partial chunk the high bytes of one more draw (entropy.md §2).
class SeededStream
  MASK = (1 << 64) - 1

  def initialize(seed)
    @state = seed & MASK
  end

  def next_u64
    @state = (@state + 0x9E3779B97F4A7C15) & MASK
    x = @state
    x = ((x ^ (x >> 30)) * 0xBF58476D1CE4E5B9) & MASK
    x = ((x ^ (x >> 27)) * 0x94D049BB133111EB) & MASK
    x ^ (x >> 31)
  end

  def bytes(n)
    out = "".b
    out << [next_u64].pack("Q>")[0, [n - out.bytesize, 8].min] while out.bytesize < n
    out
  end
end
