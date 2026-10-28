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
	"errors"
	"fmt"
	"io"
	"sync"

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
	// dek is the data key, kept so a credential change can rewrap it (encrypted.md §5).
	dek []byte
	// keyBlock is the selected key block copy, kept for compaction, which writes it to both pages of
	// the new file, and replaced by a credential change (encrypted.md §5). Guarded by keyBlockMu.
	keyBlockMu sync.Mutex
	keyBlock   []byte
}

func newPageCodec(dek []byte, rng RandomSource, keyBlock []byte) *pageCodec {
	pk := hkdf32(dek, nil, "jed/v1/page")
	mk := hkdf32(dek, nil, "jed/v1/meta")
	return &pageCodec{page: newXaesKey(pk[:]), meta: newXaesKey(mk[:]), rng: rng, dek: dek, keyBlock: keyBlock}
}

// selectedKeyBlock is the selected key-block copy (128 bytes; the rest of its page is zero).
func (c *pageCodec) selectedKeyBlock() []byte {
	c.keyBlockMu.Lock()
	defer c.keyBlockMu.Unlock()
	return c.keyBlock
}

// keyBlockPage is the selected copy as a whole key-block page of pageSize bytes.
func (c *pageCodec) keyBlockPage(pageSize int) []byte {
	page := make([]byte, pageSize)
	copy(page, c.selectedKeyBlock())
	return page
}

// rewrap is the key block for a credential change (encrypted.md §5): the same DEK wrapped for cred
// under a fresh KDF salt and wrap nonce, drawn in that order from the key-entropy stream, at the next
// generation. Runs the KDF, so a caller derives it before taking the writer gate.
func (c *pageCodec) rewrap(cred *Credential, params Argon2Params) ([]byte, error) {
	generation := keyBlockGeneration(c.selectedKeyBlock())
	if generation == ^uint64(0) {
		return nil, cryptoCorrupt("key block generation exhausted")
	}
	return wrapKeyBlock(c.dek, generation+1, cred, params, c.rng)
}

// keyBlockGeneration is a key block's generation (offset 8).
func keyBlockGeneration(block []byte) uint64 { return binary.BigEndian.Uint64(block[8:]) }

// withKeyBlock is c with block as its selected key-block copy — the same keys and key-entropy stream —
// for a writer that copies the file's current pair rather than the one c unlocked from (a compacted
// backup through a page client, encrypted.md §13).
func (c *pageCodec) withKeyBlock(block []byte) *pageCodec {
	return &pageCodec{page: c.page, meta: c.meta, rng: c.rng, dek: c.dek, keyBlock: block}
}

// setKeyBlock adopts block as the selected copy once a credential change has written both pages.
func (c *pageCodec) setKeyBlock(block []byte) {
	c.keyBlockMu.Lock()
	defer c.keyBlockMu.Unlock()
	c.keyBlock = block
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
	block, err := wrapKeyBlock(dek, 1, cred, params, rng)
	if err != nil {
		return nil, nil, err
	}
	return block, dek, nil
}

// wrapKeyBlock is the 128-byte key block wrapping dek for cred at generation: it draws the KDF salt,
// then the wrap nonce, from rng (encrypted.md §5, §8).
func wrapKeyBlock(dek []byte, generation uint64, cred *Credential, params Argon2Params, rng RandomSource) ([]byte, error) {
	block := make([]byte, keyBlockLen)
	copy(block, keyBlockMagic)
	binary.BigEndian.PutUint16(block[4:], keyBlockVersion)
	block[6] = suiteXAES
	binary.BigEndian.PutUint64(block[8:], generation)
	switch cred.kind {
	case credentialPassphrase:
		params = params.orDefault()
		if !params.valid() {
			return nil, newError(InvalidParameterValue, "Argon2id parameters are out of range")
		}
		block[7] = kdfArgon2id
		binary.BigEndian.PutUint32(block[16:], params.MemoryKiB)
		binary.BigEndian.PutUint32(block[20:], params.Iterations)
		binary.BigEndian.PutUint32(block[24:], params.Parallelism)
	case credentialKey:
		block[7] = kdfHostKey
	default:
		return nil, newError(InvalidParameterValue, "an empty credential")
	}
	if err := rng.Fill(block[32:76]); err != nil {
		return nil, err
	}
	key, _ := kek(block, cred)
	sealed := newGCM(key[:]).Seal(nil, block[64:76], dek, block[:76])
	copy(block[76:124], sealed)
	binary.BigEndian.PutUint32(block[124:], crc32IEEE(block[:124]))
	return block, nil
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

// unselectedKeyPage is the key-block page a credential change writes first (encrypted.md §5): the copy
// that is not the selected one — the older or invalid copy, page 3 when both are equal — so the only
// valid copy is never the first overwritten.
func unselectedKeyPage(page2, page3 []byte) uint32 {
	_, ga, okA := parseKeyBlock(page2)
	_, gb, okB := parseKeyBlock(page3)
	switch {
	case okA && okB && gb > ga:
		return 2
	case !okA && okB:
		return 2
	default:
		return 3
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

// scratchCodec is the codec of a rebuild's scratch domain when the target is encrypted
// (spec/design/encryption.md §5): a throwaway random key from the OS CSPRNG, never the database's
// key-entropy stream, so it perturbs no persisted draw.
func scratchCodec() (*pageCodec, error) {
	rng := OSRandomSource()
	key := make([]byte, 32)
	if err := rng.Fill(key); err != nil {
		return nil, err
	}
	var k [32]byte
	copy(k[:], key)
	return createCodec(KeyCredential(k), Argon2Params{}, rng)
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
// encrypted. Slots that disagree, or an unknown suite, are XX001. found is false when NEITHER slot is
// CRC-valid: the file declares nothing, so the caller must not read suite 0 as "plaintext" (a damaged
// encrypted header opened with its credential is XX001, never 28P03) — slot selection reports the
// corruption.
func fileSuite(slot0, slot1 []byte) (suite byte, found bool, err error) {
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
			return 0, false, cryptoCorrupt("unknown encryption suite")
		}
		if found && s != suite {
			return 0, false, cryptoCorrupt("meta slots disagree on encryption")
		}
		found, suite = true, s
	}
	return suite, found, nil
}

// --- scratch encryption (spec/design/encryption.md §6, spec/fileformat/encrypted.md §11) ---------

const (
	// scratchBlock is the plaintext bytes of one sealed scratch block; each is stored as the block's
	// ciphertext, its 24-byte nonce and its 16-byte tag.
	scratchBlock  = 4096
	scratchStored = scratchBlock + cryptoReserve
	roleScratch   = 0x03
)

// encryptingSpillHost is a spill host whose every scratch file is sealed (encryption.md §6): each file
// gets a random key and id from the handle's scratch stream, never persisted, and stores its bytes in
// sealed 4 KiB blocks whose associated data binds the file id, the block ordinal and the block's write
// counter, so a replayed older version of a rewritten block fails. Results and cost do not change.
type encryptingSpillHost struct {
	inner spillHost
	rng   RandomSource
}

func (h *encryptingSpillHost) encrypts() bool { return true }

func (h *encryptingSpillHost) create(kind string) (spillFile, error) {
	key := make([]byte, 32)
	if err := h.rng.Fill(key); err != nil {
		return nil, err
	}
	var id [16]byte
	if err := h.rng.Fill(id[:]); err != nil {
		return nil, err
	}
	inner, err := h.inner.create(kind)
	if err != nil {
		return nil, err
	}
	return &sealedSpillFile{inner: inner, key: newXaesKey(key), id: id, rng: h.rng, cachedAt: -1}, nil
}

// spillEncrypts reports whether every file host makes is sealed (scratch encryption).
func spillEncrypts(host spillHost) bool {
	e, ok := host.(interface{ encrypts() bool })
	return ok && e.encrypts()
}

// sealScratch is host, sealed with keys from a scratch stream of env's key entropy when encrypt (and
// not already sealed).
func sealScratch(host spillHost, encrypt bool, env *Environment) spillHost {
	if host == nil || !encrypt || spillEncrypts(host) {
		return host
	}
	return &encryptingSpillHost{inner: host, rng: env.keyStream()}
}

var errScratchAuth = errors.New("a scratch block fails authentication")

type sealedSpillFile struct {
	inner spillFile
	key   xaesKey
	id    [16]byte
	rng   RandomSource
	mu    sync.Mutex
	// size is the logical (plaintext) size.
	logical int64
	// counters is each block's write counter, never reset (truncation included), so no sealed version
	// of a block can authenticate again once a newer one was sealed.
	counters []uint64
	// live is whether each block holds written bytes; a block never written, or cut by a truncation,
	// reads as zeros.
	live []bool
	// cached is the block last written (cachedAt its ordinal, −1 for none), kept in plaintext until
	// another block is written, so a run of small writes into one block seals it once.
	cachedAt int
	cached   []byte
}

// ad is the associated data of block ordinal at write counter counter.
func (f *sealedSpillFile) ad(ordinal int, counter uint64) []byte {
	ad := make([]byte, 0, 42)
	ad = append(ad, adPrefix(roleScratch, 0)[:6]...)
	ad = append(ad, f.id[:]...)
	ad = binary.BigEndian.AppendUint64(ad, uint64(ordinal))
	return binary.BigEndian.AppendUint64(ad, counter)
}

// load is block ordinal's plaintext: the cached block, zeros for a block never written, else the
// stored block opened against its counter. Caller holds mu.
func (f *sealedSpillFile) load(ordinal int) ([]byte, error) {
	if f.cachedAt == ordinal {
		return append([]byte(nil), f.cached...), nil
	}
	if ordinal >= len(f.live) || !f.live[ordinal] {
		return make([]byte, scratchBlock), nil
	}
	counter := f.counters[ordinal]
	stored := make([]byte, scratchStored)
	if _, err := f.inner.ReadAt(stored, int64(ordinal)*scratchStored); err != nil && !errors.Is(err, io.EOF) {
		return nil, err
	}
	var tag pageTag
	copy(tag[:], stored[scratchBlock+cryptoNonceLen:])
	block := stored[:scratchBlock]
	if !f.key.open(stored[scratchBlock:scratchBlock+cryptoNonceLen], block, f.ad(ordinal, counter), tag) {
		return nil, errScratchAuth
	}
	return block, nil
}

// flushCached seals and stores the cached block, if any, at its next write counter. Caller holds mu.
func (f *sealedSpillFile) flushCached() error {
	if f.cachedAt < 0 {
		return nil
	}
	ordinal, block := f.cachedAt, f.cached
	f.cachedAt, f.cached = -1, nil
	for len(f.counters) <= ordinal {
		f.counters = append(f.counters, 0)
		f.live = append(f.live, false)
	}
	counter := f.counters[ordinal] + 1
	nonce := make([]byte, cryptoNonceLen)
	if err := f.rng.Fill(nonce); err != nil {
		return err
	}
	tag := f.key.seal(nonce, block, f.ad(ordinal, counter))
	stored := make([]byte, 0, scratchStored)
	stored = append(append(append(stored, block...), nonce...), tag[:]...)
	if _, err := f.inner.WriteAt(stored, int64(ordinal)*scratchStored); err != nil {
		return err
	}
	f.counters[ordinal] = counter
	f.live[ordinal] = true
	return nil
}

func (f *sealedSpillFile) ReadAt(p []byte, off int64) (int, error) {
	f.mu.Lock()
	defer f.mu.Unlock()
	if off >= f.logical {
		return 0, io.EOF
	}
	n := int(min(int64(len(p)), f.logical-off))
	for done := 0; done < n; {
		at := off + int64(done)
		ordinal, within := int(at/scratchBlock), int(at%scratchBlock)
		take := min(scratchBlock-within, n-done)
		block, err := f.load(ordinal)
		if err != nil {
			return done, err
		}
		copy(p[done:done+take], block[within:within+take])
		done += take
	}
	if n < len(p) {
		return n, io.EOF
	}
	return n, nil
}

func (f *sealedSpillFile) WriteAt(p []byte, off int64) (int, error) {
	f.mu.Lock()
	defer f.mu.Unlock()
	for done := 0; done < len(p); {
		at := off + int64(done)
		ordinal, within := int(at/scratchBlock), int(at%scratchBlock)
		take := min(scratchBlock-within, len(p)-done)
		if f.cachedAt != ordinal {
			var block []byte
			if take == scratchBlock {
				block = make([]byte, scratchBlock)
			} else {
				var err error
				if block, err = f.load(ordinal); err != nil {
					return done, err
				}
			}
			if err := f.flushCached(); err != nil {
				return done, err
			}
			f.cachedAt, f.cached = ordinal, block
		}
		copy(f.cached[within:within+take], p[done:done+take])
		done += take
	}
	f.logical = max(f.logical, off+int64(len(p)))
	return len(p), nil
}

func (f *sealedSpillFile) size() (int64, error) {
	f.mu.Lock()
	defer f.mu.Unlock()
	return f.logical, nil
}

func (f *sealedSpillFile) truncate(n int64) error {
	f.mu.Lock()
	defer f.mu.Unlock()
	blocks := int((n + scratchBlock - 1) / scratchBlock)
	if n < f.logical {
		// Zero the kept part of a cut block, so growing again reads zeros past n.
		if within := int(n % scratchBlock); within != 0 {
			ordinal := int(n / scratchBlock)
			block, err := f.load(ordinal)
			if err != nil {
				return err
			}
			clear(block[within:])
			if f.cachedAt != ordinal {
				if err := f.flushCached(); err != nil {
					return err
				}
			}
			f.cachedAt, f.cached = ordinal, block
		}
		if f.cachedAt >= blocks {
			f.cachedAt, f.cached = -1, nil
		}
		for i := blocks; i < len(f.live); i++ {
			f.live[i] = false
		}
		if err := f.inner.truncate(int64(blocks) * scratchStored); err != nil {
			return err
		}
	}
	f.logical = n
	return nil
}

func (f *sealedSpillFile) close() error {
	f.mu.Lock()
	f.cachedAt, f.cached = -1, nil
	f.mu.Unlock()
	return f.inner.close()
}
