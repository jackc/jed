package jed

// The encrypted variant's cryptography (spec/fileformat/encrypted.md §3–§5) against the shared
// vectors (spec/fileformat/encryption_vectors.toml, authored by the Ruby reference): XAES-256-GCM
// including the two C2SP vectors, the HKDF subkeys, the host-key KEK, the goldens' key block from the
// seeded stream, and Argon2id. Byte-level checks, so they live here rather than in the SQL corpus
// (CLAUDE.md §10). Mirrors impl/rust/src/crypto.rs's tests.

import (
	"bytes"
	"encoding/hex"
	"errors"
	"io"
	"testing"

	"golang.org/x/crypto/argon2"
)

func mustHex(t *testing.T, s string) []byte {
	t.Helper()
	b, err := hex.DecodeString(s)
	if err != nil {
		t.Fatalf("hex %q: %v", s, err)
	}
	return b
}

// filledKey is a 32-byte key of one repeated byte.
func filledKey(b byte) [32]byte {
	var k [32]byte
	for i := range k {
		k[i] = b
	}
	return k
}

func xaesSeal(key, nonce, plaintext, aad []byte) []byte {
	k := newXaesKey(key)
	buf := append([]byte(nil), plaintext...)
	tag := k.seal(nonce, buf, aad)
	return append(buf, tag[:]...)
}

func TestXaesC2SPVectors(t *testing.T) {
	nonce := []byte("ABCDEFGHIJKLMNOPQRSTUVWX")
	for _, v := range []struct {
		key  byte
		aad  string
		want string
	}{
		{1, "", "ce546ef63c9cc60765923609b33a9a1974e96e52daf2fcf7075e2271"},
		{3, "c2sp.org/XAES-256-GCM", "986ec1832593df5443a179437fd083bf3fdb41abd740a21f71eb769d"},
	} {
		key := bytes.Repeat([]byte{v.key}, 32)
		sealed := xaesSeal(key, nonce, []byte("XAES-256-GCM"), []byte(v.aad))
		if !bytes.Equal(sealed, mustHex(t, v.want)) {
			t.Fatalf("key %d: sealed %x, want %s", v.key, sealed, v.want)
		}
		k := newXaesKey(key)
		var tag pageTag
		copy(tag[:], sealed[12:])
		body := append([]byte(nil), sealed[:12]...)
		if !k.open(nonce, body, []byte(v.aad), tag) || string(body) != "XAES-256-GCM" {
			t.Fatalf("key %d: open failed", v.key)
		}
		if k.open(nonce, append([]byte(nil), sealed[:12]...), []byte("x"), tag) {
			t.Fatalf("key %d: opened under the wrong AD", v.key)
		}
	}
}

func TestSharedEncryptionVectors(t *testing.T) {
	path := specPath(t, "fileformat/encryption_vectors.toml")
	xaes := readTomlTables(t, path, "xaes")
	if len(xaes) < 3 {
		t.Fatalf("want the xaes vectors, got %d", len(xaes))
	}
	for _, v := range xaes {
		got := xaesSeal(mustHex(t, v.str("key_hex")), mustHex(t, v.str("nonce_hex")), mustHex(t, v.str("plaintext_hex")), mustHex(t, v.str("aad_hex")))
		if !bytes.Equal(got, mustHex(t, v.str("sealed_hex"))) {
			t.Fatalf("%s: sealed %x", v.str("name"), got)
		}
	}

	sub := readTomlTable(t, path, "subkeys")
	dek := mustHex(t, sub.str("dek_hex"))
	if pk := hkdf32(dek, nil, "jed/v1/page"); !bytes.Equal(pk[:], mustHex(t, sub.str("page_key_hex"))) {
		t.Fatalf("page key %x", pk)
	}
	if mk := hkdf32(dek, nil, "jed/v1/meta"); !bytes.Equal(mk[:], mustHex(t, sub.str("meta_key_hex"))) {
		t.Fatalf("meta key %x", mk)
	}

	hk := readTomlTable(t, path, "host_kek")
	var hostKey [32]byte
	copy(hostKey[:], mustHex(t, hk.str("key_hex")))
	block := make([]byte, keyBlockLen)
	block[7] = kdfHostKey
	copy(block[32:64], mustHex(t, hk.str("kdf_salt_hex")))
	if got, ok := kek(block, KeyCredential(hostKey)); !ok || !bytes.Equal(got[:], mustHex(t, hk.str("kek_hex"))) {
		t.Fatalf("host KEK %x (ok %v)", got, ok)
	}

	kb := readTomlTable(t, path, "key_block")
	gotBlock, gotDEK, err := newKeyBlock(KeyCredential(hostKey), Argon2Params{}, NewSeededRandom(uint64(kb.int("seed"))))
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(gotBlock, mustHex(t, kb.str("block_hex"))) {
		t.Fatalf("key block %x", gotBlock)
	}
	if !bytes.Equal(gotDEK, mustHex(t, kb.str("dek_hex"))) {
		t.Fatalf("key block DEK %x", gotDEK)
	}

	// A credential change of that block (encrypted.md §5): the same DEK at the next generation.
	rw := readTomlTable(t, path, "rewrap")
	page := make([]byte, 1024)
	copy(page, gotBlock)
	codec, err := unlockCodec(page, page, KeyCredential(hostKey), NewSeededRandom(uint64(rw.int("seed"))))
	if err != nil {
		t.Fatal(err)
	}
	var newKey [32]byte
	copy(newKey[:], mustHex(t, rw.str("key_hex")))
	rewrapped, err := codec.rewrap(KeyCredential(newKey), Argon2Params{})
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(rewrapped, mustHex(t, rw.str("block_hex"))) {
		t.Fatalf("rewrapped key block %x", rewrapped)
	}

	a := readTomlTable(t, path, "argon2id")
	out := argon2.IDKey([]byte(a.str("password")), []byte(a.str("salt")), uint32(a.int("iterations")), uint32(a.int("memory_kib")), uint8(a.int("parallelism")), 32)
	if !bytes.Equal(out, mustHex(t, a.str("key_hex"))) {
		t.Fatalf("argon2id %x", out)
	}
}

func TestArgon2idReferenceVector(t *testing.T) {
	out := argon2.IDKey([]byte("password"), []byte("somesalt"), 2, 65536, 1, 32)
	if !bytes.Equal(out, mustHex(t, "09316115d5cf24ed5a15a31a3ba326e5cf32edc24702987c02b6566f61913cf7")) {
		t.Fatalf("argon2id %x", out)
	}
}

func cryptoCode(err error) string {
	var e *EngineError
	if errors.As(err, &e) {
		return e.Code()
	}
	return ""
}

func TestKeyBlockRoundTripAndWrongCredential(t *testing.T) {
	cred := KeyCredential(filledKey(9))
	codec, err := createCodec(cred, Argon2Params{}, NewSeededRandom(7))
	if err != nil {
		t.Fatal(err)
	}
	page := codec.keyBlockPage(1024)
	if _, err := unlockCodec(page, page, cred, NewSeededRandom(7)); err != nil {
		t.Fatalf("unlock: %v", err)
	}
	if _, err := unlockCodec(page, page, KeyCredential(filledKey(8)), NewSeededRandom(7)); cryptoCode(err) != "28P01" {
		t.Fatalf("wrong key: %v", err)
	}
	if _, err := unlockCodec(page, page, PassphraseCredential("x"), NewSeededRandom(7)); cryptoCode(err) != "28P01" {
		t.Fatalf("wrong kind: %v", err)
	}
	torn := append([]byte(nil), page...)
	torn[40] ^= 1
	if _, err := unlockCodec(page, torn, cred, NewSeededRandom(7)); err != nil {
		t.Fatalf("one torn copy: %v", err)
	}
	if _, err := unlockCodec(torn, torn, cred, NewSeededRandom(7)); cryptoCode(err) != "XX001" {
		t.Fatalf("no valid copy: %v", err)
	}
}

func TestCredentialChangeWritesTheUnselectedCopyFirst(t *testing.T) {
	cred := KeyCredential(filledKey(9))
	codec, err := createCodec(cred, Argon2Params{}, NewSeededRandom(3))
	if err != nil {
		t.Fatal(err)
	}
	old := codec.keyBlockPage(1024)
	next, err := codec.rewrap(KeyCredential(filledKey(5)), Argon2Params{})
	if err != nil {
		t.Fatal(err)
	}
	newer := make([]byte, 1024)
	copy(newer, next)
	torn := append([]byte(nil), old...)
	torn[50] ^= 1
	for _, c := range []struct {
		page2, page3 []byte
		want         uint32
	}{
		{old, old, 3},
		{old, newer, 2},
		{newer, old, 3},
		{torn, old, 2},
		{old, torn, 3},
	} {
		if got := unselectedKeyPage(c.page2, c.page3); got != c.want {
			t.Fatalf("unselected page %d, want %d", got, c.want)
		}
	}
	// The newer copy is selected: the new credential unlocks, the old one no longer does.
	if _, err := unlockCodec(old, newer, KeyCredential(filledKey(5)), NewSeededRandom(3)); err != nil {
		t.Fatalf("new credential: %v", err)
	}
	if _, err := unlockCodec(old, newer, cred, NewSeededRandom(3)); cryptoCode(err) != "28P01" {
		t.Fatalf("old credential: %v", err)
	}
}

func TestBodyPageBindsIndexAndTag(t *testing.T) {
	codec, err := createCodec(KeyCredential(filledKey(1)), Argon2Params{}, NewSeededRandom(1))
	if err != nil {
		t.Fatal(err)
	}
	page := make([]byte, 1024)
	page[0] = pageLeaf
	copy(page[16:], "rows")
	plain := append([]byte(nil), page[:1024-cryptoReserve]...)
	tag, err := codec.sealBody(9, page)
	if err != nil {
		t.Fatal(err)
	}
	opened, err := codec.openBody(9, page, tag)
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(opened[16:], plain[16:]) {
		t.Fatal("opened page differs from its plaintext")
	}
	if _, err := codec.openBody(10, page, tag); cryptoCode(err) != "XX001" {
		t.Fatalf("another index: %v", err)
	}
	if _, err := codec.openBody(9, page, noTag); cryptoCode(err) != "XX001" {
		t.Fatalf("another tag: %v", err)
	}
}

// memSpill is an in-memory spill host whose stored bytes a test can read and alter.
type memSpill struct{ files []*memSpillFile }

type memSpillFile struct{ data []byte }

func (h *memSpill) create(string) (spillFile, error) {
	f := &memSpillFile{}
	h.files = append(h.files, f)
	return f, nil
}

func (f *memSpillFile) ReadAt(p []byte, off int64) (int, error) {
	if off >= int64(len(f.data)) {
		return 0, io.EOF
	}
	n := copy(p, f.data[off:])
	if n < len(p) {
		return n, io.EOF
	}
	return n, nil
}

func (f *memSpillFile) WriteAt(p []byte, off int64) (int, error) {
	if end := int(off) + len(p); end > len(f.data) {
		f.data = append(f.data, make([]byte, end-len(f.data))...)
	}
	return copy(f.data[off:], p), nil
}

func (f *memSpillFile) size() (int64, error) { return int64(len(f.data)), nil }

func (f *memSpillFile) truncate(n int64) error {
	if int(n) <= len(f.data) {
		f.data = f.data[:n]
	} else {
		f.data = append(f.data, make([]byte, int(n)-len(f.data))...)
	}
	return nil
}

func (f *memSpillFile) close() error { return nil }

func TestSealedScratchRoundTripsAndDetectsReplay(t *testing.T) {
	mem := &memSpill{}
	host := &encryptingSpillHost{inner: mem, rng: NewSeededRandom(5)}
	if !spillEncrypts(host) || spillEncrypts(mem) {
		t.Fatal("only the sealing host encrypts")
	}
	file, err := host.create("hash")
	if err != nil {
		t.Fatal(err)
	}
	var model []byte
	write := func(f spillFile, at int, b []byte) []byte {
		t.Helper()
		if _, err := f.WriteAt(b, int64(at)); err != nil {
			t.Fatal(err)
		}
		if len(model) < at+len(b) {
			model = append(model, make([]byte, at+len(b)-len(model))...)
		}
		copy(model[at:], b)
		return append([]byte(nil), model...)
	}
	read := func(f spillFile) []byte {
		t.Helper()
		n, err := f.size()
		if err != nil {
			t.Fatal(err)
		}
		out := make([]byte, n)
		if _, err := f.ReadAt(out, 0); err != nil {
			t.Fatal(err)
		}
		return out
	}
	write(file, 0, bytes.Repeat([]byte("secret-scratch-"), 700))
	write(file, 5000, []byte("patched"))
	write(file, 20000, []byte("far away"))
	expect := write(file, 4090, []byte("straddles a block boundary"))
	if !bytes.Equal(read(file), expect) {
		t.Fatal("sealed scratch does not round-trip")
	}
	if n, err := file.ReadAt(make([]byte, 4), int64(len(expect))); n != 0 || err != io.EOF {
		t.Fatalf("read past the end: %d, %v", n, err)
	}
	// Writing another block seals the cached one; nothing is stored in plaintext.
	write(file, 30000, []byte("x"))
	stored := append([]byte(nil), mem.files[0].data...)
	if bytes.Contains(stored, []byte("secret-scratch-")) {
		t.Fatal("scratch stored in plaintext")
	}

	// Replay: put back an older sealed version of block 1, which was rewritten since.
	block1 := append([]byte(nil), stored[scratchStored:2*scratchStored]...)
	write(file, 4100, []byte("newer"))
	write(file, 30001, []byte("y"))
	if got := read(file)[4100:4105]; string(got) != "newer" {
		t.Fatalf("got %q", got)
	}
	copy(mem.files[0].data[scratchStored:], block1)
	if _, err := file.ReadAt(make([]byte, 5), 4100); err == nil {
		t.Fatal("a replayed block must fail")
	}

	// A block sealed before a truncation does not authenticate after the block is rewritten.
	file, err = host.create("rows")
	if err != nil {
		t.Fatal(err)
	}
	memFile := mem.files[len(mem.files)-1]
	for _, w := range []struct {
		at int64
		b  []byte
	}{{0, bytes.Repeat([]byte{1}, 4096)}, {4096, bytes.Repeat([]byte{2}, 4096)}, {8192, []byte{0}}} {
		if _, err := file.WriteAt(w.b, w.at); err != nil {
			t.Fatal(err)
		}
	}
	sealed := append([]byte(nil), memFile.data[scratchStored:2*scratchStored]...)
	if err := file.truncate(4096); err != nil {
		t.Fatal(err)
	}
	for _, w := range []struct {
		at int64
		b  []byte
	}{{4096, bytes.Repeat([]byte{3}, 4096)}, {8192, []byte{0}}} {
		if _, err := file.WriteAt(w.b, w.at); err != nil {
			t.Fatal(err)
		}
	}
	copy(memFile.data[scratchStored:], sealed)
	if _, err := file.ReadAt(make([]byte, 5), 4096); err == nil {
		t.Fatal("a pre-truncation block must fail")
	}

	// Truncation zeroes the cut tail.
	file, err = host.create("rows")
	if err != nil {
		t.Fatal(err)
	}
	if _, err := file.WriteAt(bytes.Repeat([]byte{7}, 9000), 0); err != nil {
		t.Fatal(err)
	}
	if err := file.truncate(4500); err != nil {
		t.Fatal(err)
	}
	if _, err := file.WriteAt([]byte{1}, 8999); err != nil {
		t.Fatal(err)
	}
	got := read(file)
	if len(got) != 9000 || !bytes.Equal(got[:4500], bytes.Repeat([]byte{7}, 4500)) || !bytes.Equal(got[4500:8999], make([]byte, 4499)) || got[8999] != 1 {
		t.Fatal("truncation did not zero the cut tail")
	}
}
