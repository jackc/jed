package jed

// Encryption at rest (spec/design/encryption.md; the bytes in spec/fileformat/encrypted.md): the
// XAES-256-GCM page codec, the subkeys, and the key block that wraps a file's data key.
//
// The codec sits in the core above the block seam (hosts.md §6): format.go builds each page's
// plaintext exactly as the plaintext layout would (with payload capacity C = page_size − 56) and hands
// it here to be sealed; a reader that follows a bound pointer hands the stored bytes and the pointer's
// tag back to be checked and opened. The hosts never see plaintext.
//
// Every primitive comes from a vetted library (CLAUDE.md §14 clause 3): AES, AES-GCM, HKDF and
// SHA-256 from the Go standard library, Argon2id from golang.org/x/crypto/argon2. Only XAES-256-GCM's
// key derivation (https://c2sp.org/XAES-256-GCM — two AES blocks and a GF(2^128) doubling) is written
// here, over the library AES, and checked against the C2SP vectors in
// spec/fileformat/encryption_vectors.toml.

import (
	"bytes"
	"crypto/aes"
	"crypto/cipher"
	"crypto/hkdf"
	"crypto/sha256"
	"encoding/binary"
	"fmt"

	"golang.org/x/crypto/argon2"
)

const (
	// suiteXAES is the encrypted variant's suite byte (meta offset 6, key block offset 6):
	// XAES-256-GCM pages, HKDF-SHA-256 subkeys, an AES-256-GCM key wrap (encrypted.md §1).
	suiteXAES byte = 1
	// cryptoTagLen is the bytes a bound pointer adds to a page index (encrypted.md §2).
	cryptoTagLen   = 16
	cryptoNonceLen = 24
	// cryptoReserve is the reserved bytes at the end of every sealed page: the nonce, then the tag.
	cryptoReserve = cryptoNonceLen + cryptoTagLen
	// firstBodyEncrypted is the first body page of an encrypted file: pages 0–1 are the meta slots and
	// 2–3 the key block.
	firstBodyEncrypted uint32 = 4
	// minEncryptedPageSize is the smallest page size an encrypted file may have (encrypted.md §1).
	minEncryptedPageSize = 1024
)

// pageTag is a page's 16-byte AEAD tag — what a bound pointer carries. Zero in a pointer to no page,
// and in every pointer of a plaintext file (which stores none).
type pageTag [cryptoTagLen]byte

var noTag pageTag

const (
	credentialPassphrase byte = 1
	credentialKey        byte = 2
)

// Credential unlocks an encrypted database: a passphrase (the key-encryption key is Argon2id of it)
// or a 32-byte key the host keeps elsewhere (the KEK is HKDF of it). It is an argument of
// create/open/attach, never a setting, and no formatting verb ever shows it
// (spec/design/encryption.md §5). Build one with PassphraseCredential or KeyCredential.
type Credential struct {
	kind       byte
	passphrase string
	key        [32]byte
}

// PassphraseCredential is a passphrase credential.
func PassphraseCredential(passphrase string) *Credential {
	return &Credential{kind: credentialPassphrase, passphrase: passphrase}
}

// KeyCredential is a host-supplied 32-byte key credential.
func KeyCredential(key [32]byte) *Credential {
	return &Credential{kind: credentialKey, key: key}
}

// String never shows the secret.
func (c Credential) String() string {
	if c.kind == credentialPassphrase {
		return "Credential.Passphrase(<redacted>)"
	}
	return "Credential.Key(<redacted>)"
}

// GoString never shows the secret.
func (c Credential) GoString() string { return c.String() }

// Format renders every verb (%v, %+v, %#v, %s, %x, …) as the redacted String.
func (c Credential) Format(f fmt.State, _ rune) { _, _ = f.Write([]byte(c.String())) }

// Argon2Params are the Argon2id parameters a passphrase credential is stretched with at create
// (encrypted.md §5). The zero value means the default, RFC 9106's second recommendation; any other
// value must stay within the format's bounds (22023 otherwise), which a reader checks before running
// the KDF.
type Argon2Params struct {
	MemoryKiB   uint32
	Iterations  uint32
	Parallelism uint32
}

// DefaultArgon2Params is RFC 9106's second recommendation: m = 64 MiB, t = 3, p = 4.
func DefaultArgon2Params() Argon2Params {
	return Argon2Params{MemoryKiB: 65536, Iterations: 3, Parallelism: 4}
}

func (p Argon2Params) orDefault() Argon2Params {
	if p == (Argon2Params{}) {
		return DefaultArgon2Params()
	}
	return p
}

func (p Argon2Params) valid() bool {
	return p.Parallelism >= 1 && p.Parallelism <= 16 &&
		p.Iterations >= 1 && p.Iterations <= 16 &&
		p.MemoryKiB >= 8*p.Parallelism && p.MemoryKiB <= 1<<20
}

func cryptoCorrupt(msg string) error { return newError(DataCorrupted, msg) }

// xaesKey is an XAES-256-GCM key: the AES key schedule and its CMAC subkey K1, both computed once
// (C2SP XAES-256-GCM: L = AES_K(0), K1 = L·x in GF(2^128)).
type xaesKey struct {
	block cipher.Block
	k1    [16]byte
}

func newXaesKey(key []byte) xaesKey {
	block, err := aes.NewCipher(key)
	if err != nil {
		panic("a 32-byte AES key is always valid: " + err.Error())
	}
	var l [16]byte
	block.Encrypt(l[:], l[:])
	var k1 [16]byte
	for i := 0; i < 16; i++ {
		k1[i] = l[i] << 1
		if i < 15 {
			k1[i] |= l[i+1] >> 7
		}
	}
	// Branch-free: XOR 0x87 into the last byte when the shifted-out bit was set.
	k1[15] ^= 0x87 & -(l[0] >> 7)
	return xaesKey{block: block, k1: k1}
}

// derive is the per-nonce AES-256-GCM AEAD under Kx = AES_K(M1 ⊕ K1) ‖ AES_K(M2 ⊕ K1), with
// Mi = 00 0i 58 00 ‖ N[0:12].
func (k *xaesKey) derive(nonce []byte) cipher.AEAD {
	var kx [32]byte
	for i := 0; i < 2; i++ {
		var m [16]byte
		m[1] = byte(i + 1)
		m[2] = 'X'
		copy(m[4:], nonce[:12])
		for j := range m {
			m[j] ^= k.k1[j]
		}
		k.block.Encrypt(kx[16*i:16*i+16], m[:])
	}
	return newGCM(kx[:])
}

func newGCM(key []byte) cipher.AEAD {
	block, err := aes.NewCipher(key)
	if err != nil {
		panic("a 32-byte AES key is always valid: " + err.Error())
	}
	g, err := cipher.NewGCM(block)
	if err != nil {
		panic("AES-GCM over AES-256 is always available: " + err.Error())
	}
	return g
}

// seal encrypts buf in place under the 24-byte nonce and returns the tag.
func (k *xaesKey) seal(nonce, buf, aad []byte) pageTag {
	out := k.derive(nonce).Seal(nil, nonce[12:], buf, aad)
	copy(buf, out[:len(buf)])
	var tag pageTag
	copy(tag[:], out[len(buf):])
	return tag
}

// open decrypts buf in place, reporting whether the tag authenticates it.
func (k *xaesKey) open(nonce, buf, aad []byte, tag pageTag) bool {
	sealed := make([]byte, 0, len(buf)+cryptoTagLen)
	sealed = append(append(sealed, buf...), tag[:]...)
	plain, err := k.derive(nonce).Open(nil, nonce[12:], sealed, aad)
	if err != nil {
		return false
	}
	copy(buf, plain)
	return true
}

func hkdf32(ikm, salt []byte, info string) [32]byte {
	if len(salt) == 0 {
		salt = nil // RFC 5869's zero salt
	}
	out, err := hkdf.Key(sha256.New, ikm, salt, info, 32)
	if err != nil {
		panic("32 bytes is a valid HKDF-SHA-256 length: " + err.Error())
	}
	var k [32]byte
	copy(k[:], out)
	return k
}

const (
	roleBody byte = 0x01
	roleMeta byte = 0x02
)

// adPrefix is the 10-byte AD prefix of every seal: "JEDB" ‖ suite ‖ role ‖ u32 position
// (encrypted.md §3).
func adPrefix(role byte, position uint32) []byte {
	ad := make([]byte, 10)
	copy(ad, "JEDB")
	ad[4] = suiteXAES
	ad[5] = role
	binary.BigEndian.PutUint32(ad[6:], position)
	return ad
}

// pageCodec is one open encrypted database's keys and nonce source: everything a commit needs to seal
// and a reader needs to open. Shared by the database's paging context.
type pageCodec struct {
	page, meta xaesKey
	// rng is the database's key-entropy stream (encrypted.md §8): one per database.
	rng RandomSource
	// keyBlock is the selected key block copy, kept for compaction, which writes it to both pages of
	// the new file (encrypted.md §5).
	keyBlock []byte
}

func newPageCodec(dek []byte, rng RandomSource, keyBlock []byte) *pageCodec {
	pk := hkdf32(dek, nil, "jed/v1/page")
	mk := hkdf32(dek, nil, "jed/v1/meta")
	return &pageCodec{page: newXaesKey(pk[:]), meta: newXaesKey(mk[:]), rng: rng, keyBlock: keyBlock}
}

func (c *pageCodec) nonce() ([]byte, error) {
	n := make([]byte, cryptoNonceLen)
	if err := c.rng.Fill(n); err != nil {
		return nil, err
	}
	return n, nil
}

// sealBody seals body page index in place (encrypted.md §4): page is the full page, its first
// page_size − 40 bytes the plaintext (header with a zero CRC field, payload, zero fill). Draws one
// nonce and returns the tag a pointer to the page binds.
func (c *pageCodec) sealBody(index uint32, page []byte) (pageTag, error) {
	nonce, err := c.nonce()
	if err != nil {
		return noTag, err
	}
	split := len(page) - cryptoReserve
	tag := c.page.seal(nonce, page[:split], adPrefix(roleBody, index))
	copy(page[split:], nonce)
	copy(page[split+cryptoNonceLen:], tag[:])
	return tag, nil
}

// openBody opens the stored body page index that a pointer binding expected names: it checks the
// stored tag against the pointer's, then decrypts. It returns the plaintext page (page_size − 40
// bytes) with its CRC field stamped as the plaintext layout's per-page CRC, so the ordinary page parse
// accepts it. A mismatch or an authentication failure is XX001; so is a plaintext whose CRC field is
// not zero.
func (c *pageCodec) openBody(index uint32, stored []byte, expected pageTag) ([]byte, error) {
	if len(stored) < minEncryptedPageSize {
		return nil, cryptoCorrupt("short encrypted page")
	}
	split := len(stored) - cryptoReserve
	nonce := stored[split : split+cryptoNonceLen]
	var tag pageTag
	copy(tag[:], stored[split+cryptoNonceLen:])
	if tag != expected {
		return nil, cryptoCorrupt("page does not match the pointer that names it")
	}
	body := append([]byte(nil), stored[:split]...)
	if !c.page.open(nonce, body, adPrefix(roleBody, index), tag) {
		return nil, cryptoCorrupt("page fails authentication")
	}
	if !zeroBytes(body[12:16]) {
		return nil, cryptoCorrupt("encrypted page carries a CRC")
	}
	binary.BigEndian.PutUint32(body[12:], pageCRC(body))
	return body, nil
}

// sealMeta seals meta slot slot in place (encrypted.md §6): every field and the inline manifest are
// written; manifest is the commit's manifest-overflow chain in chain order. Writes the nonce and tag
// into the reserve, then the meta CRC.
func (c *pageCodec) sealMeta(slot uint32, meta []byte, manifest []dirtyPage) error {
	nonce, err := c.nonce()
	if err != nil {
		return err
	}
	ps := len(meta)
	clear(meta[32:36])
	clear(meta[ps-cryptoReserve:])
	tag := c.meta.seal(nonce, nil, metaAAD(slot, meta, manifest))
	copy(meta[ps-cryptoReserve:], nonce)
	copy(meta[ps-cryptoTagLen:], tag[:])
	binary.BigEndian.PutUint32(meta[32:], metaCRC(meta))
	return nil
}

// verifyMeta checks the stored meta slot slot against its tag; manifest is its manifest-overflow
// chain as stored. A failure is XX001 (never a fallback — encrypted.md §6).
func (c *pageCodec) verifyMeta(slot uint32, stored []byte, manifest []dirtyPage) error {
	ps := len(stored)
	nonce := stored[ps-cryptoReserve : ps-cryptoTagLen]
	var tag pageTag
	copy(tag[:], stored[ps-cryptoTagLen:])
	zeroed := append([]byte(nil), stored...)
	clear(zeroed[32:36])
	clear(zeroed[ps-cryptoReserve:])
	if !c.meta.open(nonce, nil, metaAAD(slot, zeroed, manifest), tag) {
		return cryptoCorrupt("meta page fails authentication")
	}
	return nil
}

func metaAAD(slot uint32, zeroed []byte, manifest []dirtyPage) []byte {
	aad := make([]byte, 0, 10+len(zeroed)*(1+len(manifest)))
	aad = append(aad, adPrefix(roleMeta, slot)...)
	aad = append(aad, zeroed...)
	for _, p := range manifest {
		aad = binary.BigEndian.AppendUint32(aad, p.index)
		aad = append(aad, p.bytes...)
	}
	return aad
}

// --- the key block (pages 2 and 3, encrypted.md §5) ---------------------------------------------

const (
	keyBlockVersion uint16 = 1
	// keyBlockLen is the key block's encoded length; the rest of its page is zero.
	keyBlockLen      = 128
	kdfArgon2id byte = 1
	kdfHostKey  byte = 2
)

var keyBlockMagic = []byte("JEDK")

// kek derives the key-encryption key for one key-block copy (encrypted.md §3); ok=false when the
// credential is the wrong kind for the copy. The copy's parameters were bounds-checked by
// parseKeyBlock (or set by newKeyBlock) before this runs.
func kek(block []byte, cred *Credential) ([32]byte, bool) {
	salt := block[32:64]
	switch {
	case block[7] == kdfHostKey && cred.kind == credentialKey:
		return hkdf32(cred.key[:], salt, "jed/v1/kek"), true
	case block[7] == kdfArgon2id && cred.kind == credentialPassphrase:
		m := binary.BigEndian.Uint32(block[16:])
		t := binary.BigEndian.Uint32(block[20:])
		p := binary.BigEndian.Uint32(block[24:])
		var out [32]byte
		copy(out[:], argon2.IDKey([]byte(cred.passphrase), salt, t, m, uint8(p), 32))
		return out, true
	}
	// The wrong kind of credential cannot unwrap this copy.
	return [32]byte{}, false
}

func wrongCredential() error {
	return newError(InvalidPassword, "the credential does not unlock this database")
}

// newKeyBlock builds a new key block for cred: it draws the DEK, the KDF salt and the wrap nonce from
// rng, in that order (encrypted.md §8). Returns the 128-byte block and the DEK.
func newKeyBlock(cred *Credential, params Argon2Params, rng RandomSource) ([]byte, []byte, error) {
	dek := make([]byte, 32)
	if err := rng.Fill(dek); err != nil {
		return nil, nil, err
	}
	block := make([]byte, keyBlockLen)
	copy(block, keyBlockMagic)
	binary.BigEndian.PutUint16(block[4:], keyBlockVersion)
	block[6] = suiteXAES
	binary.BigEndian.PutUint64(block[8:], 1)
	switch cred.kind {
	case credentialPassphrase:
		params = params.orDefault()
		if !params.valid() {
			return nil, nil, newError(InvalidParameterValue, "Argon2id parameters are out of range")
		}
		block[7] = kdfArgon2id
		binary.BigEndian.PutUint32(block[16:], params.MemoryKiB)
		binary.BigEndian.PutUint32(block[20:], params.Iterations)
		binary.BigEndian.PutUint32(block[24:], params.Parallelism)
	case credentialKey:
		block[7] = kdfHostKey
	default:
		return nil, nil, newError(InvalidParameterValue, "an empty credential")
	}
	if err := rng.Fill(block[32:76]); err != nil {
		return nil, nil, err
	}
	key, _ := kek(block, cred)
	sealed := newGCM(key[:]).Seal(nil, block[64:76], dek, block[:76])
	copy(block[76:124], sealed)
	binary.BigEndian.PutUint32(block[124:], crc32IEEE(block[:124]))
	return block, dek, nil
}

// parseKeyBlock validates one key-block copy (encrypted.md §5): ok=false when it is not a valid copy.
func parseKeyBlock(page []byte) ([]byte, uint64, bool) {
	if len(page) < keyBlockLen {
		return nil, 0, false
	}
	b := page[:keyBlockLen]
	u := func(at int) uint32 { return binary.BigEndian.Uint32(b[at:]) }
	generation := binary.BigEndian.Uint64(b[8:])
	params := Argon2Params{MemoryKiB: u(16), Iterations: u(20), Parallelism: u(24)}
	kdfOK := false
	switch b[7] {
	case kdfArgon2id:
		kdfOK = params.valid()
	case kdfHostKey:
		kdfOK = zeroBytes(b[16:28])
	}
	valid := bytes.Equal(b[:4], keyBlockMagic) &&
		binary.BigEndian.Uint16(b[4:]) == keyBlockVersion &&
		b[6] == suiteXAES &&
		generation >= 1 &&
		kdfOK &&
		u(28) == 0 &&
		u(124) == crc32IEEE(b[:124]) &&
		zeroBytes(page[keyBlockLen:])
	if !valid {
		return nil, 0, false
	}
	return b, generation, true
}

// selectKeyBlock picks the key-block copy to unwrap (encrypted.md §5): the valid copy with the greater
// generation; two valid copies of one generation must be identical; no valid copy is XX001.
func selectKeyBlock(page2, page3 []byte) ([]byte, error) {
	a, ga, okA := parseKeyBlock(page2)
	b, gb, okB := parseKeyBlock(page3)
	switch {
	case !okA && !okB:
		return nil, cryptoCorrupt("no valid key block")
	case okA && !okB:
		return a, nil
	case !okA && okB:
		return b, nil
	case ga == gb && !bytes.Equal(a, b):
		return nil, cryptoCorrupt("key block copies disagree")
	case gb > ga:
		return b, nil
	default:
		return a, nil
	}
}

// unwrapDEK unwraps the selected copy's DEK with cred: 28P01 when it does not unwrap.
func unwrapDEK(block []byte, cred *Credential) ([]byte, error) {
	key, ok := kek(block, cred)
	if !ok {
		return nil, wrongCredential()
	}
	dek, err := newGCM(key[:]).Open(nil, block[64:76], block[76:124], block[:76])
	if err != nil {
		return nil, wrongCredential()
	}
	return dek, nil
}

// createCodec builds a new encrypted database's codec and key-block copy (create).
func createCodec(cred *Credential, params Argon2Params, rng RandomSource) (*pageCodec, error) {
	block, dek, err := newKeyBlock(cred, params, rng)
	if err != nil {
		return nil, err
	}
	return newPageCodec(dek, rng, block), nil
}

// unlockCodec unlocks an encrypted file from its key-block pages: select the copy, unwrap its DEK with
// cred.
func unlockCodec(page2, page3 []byte, cred *Credential, rng RandomSource) (*pageCodec, error) {
	block, err := selectKeyBlock(page2, page3)
	if err != nil {
		return nil, err
	}
	dek, err := unwrapDEK(block, cred)
	if err != nil {
		return nil, err
	}
	return newPageCodec(dek, rng, append([]byte(nil), block...)), nil
}

// fileSuite is the suite a file's meta slots declare (offset 6, with the reserve byte at 7 —
// encrypted.md §6), read from the CRC-valid slots before any key is available: 0 plaintext, suiteXAES
// encrypted. Slots that disagree, or an unknown suite, are XX001.
func fileSuite(slot0, slot1 []byte) (byte, error) {
	found := false
	var suite byte
	for _, m := range [][]byte{slot0, slot1} {
		if len(m) < 64 || binary.BigEndian.Uint32(m[32:36]) != metaCRC(m) {
			continue
		}
		var s byte
		switch {
		case m[6] == 0 && m[7] == 0:
			s = 0
		case m[6] == suiteXAES && int(m[7]) == cryptoReserve:
			s = suiteXAES
		default:
			return 0, cryptoCorrupt("unknown encryption suite")
		}
		if found && s != suite {
			return 0, cryptoCorrupt("meta slots disagree on encryption")
		}
		found, suite = true, s
	}
	// Neither slot is valid: slot selection reports the corruption.
	return suite, nil
}
