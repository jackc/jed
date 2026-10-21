package jed

// Encryption at rest (spec/design/encryption.md; bytes in spec/fileformat/encrypted.md). The
// credential is a host-API argument the SQL corpus cannot reach, so its contract lives here: an
// encrypted file round-trips every page kind (tree nodes, overflow chains, indexes, the free list),
// stores no plaintext, refuses a wrong or missing credential, detects tampering and replay as XX001,
// survives compaction, and writes the same bytes for the same history under a seeded key source. The
// cross-core byte contract is pinned by the encrypted_* goldens (encrypted_golden_test.go). Mirrors
// impl/rust/tests/encryption.rs.

import (
	"bytes"
	"encoding/binary"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

const encPage = 1024

func encKey() *Credential { return KeyCredential(filledKey(0x4A)) }

func encSeededEnv() *Environment {
	b := NewEnvironmentBuilder()
	b.SetInsecureSeededKeyEntropy(0x4A454442)
	return b.Build()
}

func encCreate(t *testing.T, path string, cred *Credential) *Database {
	t.Helper()
	db, err := CreateDatabase(CreateOptions{Path: path, PageSize: encPage, SkipFsync: true, Environment: encSeededEnv(), Credential: cred})
	if err != nil {
		t.Fatalf("create encrypted db: %v", err)
	}
	return db
}

func encOpen(path string, cred *Credential) (*Database, error) {
	return OpenDatabaseWithOptions(path, OpenOptions{SkipFsync: true, Credential: cred})
}

func encTryRows(db *Database, sql string) ([][]string, error) {
	out, err := queryOutcome(db, sql, nil)
	if err != nil {
		return nil, err
	}
	return renderRows(out.Rows), nil
}

func encRows(t *testing.T, db *Database, sql string) [][]string {
	t.Helper()
	rows, err := encTryRows(db, sql)
	if err != nil {
		t.Fatalf("%s: %v", sql, err)
	}
	return rows
}

const encSummary = "SELECT count(*), sum(id), max(note), sum(length(blob)), count(*) FILTER (WHERE span && i32range(10, 20)) FROM t"

// encLast is the last commit of encPopulate: one row, so most live pages are shared with the
// generation before it and a tampered one is caught by its pointer, not by the newest commit's
// manifest.
const encLast = "INSERT INTO t VALUES (5000, 'tail', NULL, NULL)"

// encPopulateButLast builds every page kind an encrypted file holds: a multi-level table tree, a
// secondary and a GiST index, incompressible values long enough to spill into overflow chains, and
// enough churn to persist a free list — up to its last commit, which tamper tests may knock out
// (recovery then falls back to this state).
func encPopulateButLast(t *testing.T, db *Database) {
	t.Helper()
	mustExec(t, db, "CREATE TABLE t (id i64 PRIMARY KEY, note text, blob bytea, span i32range)")
	mustExec(t, db, "CREATE INDEX t_note ON t (note)")
	mustExec(t, db, "CREATE INDEX t_span ON t USING gist (span)")
	mustExec(t, db, "INSERT INTO t SELECT g, 'secret-' || g, NULL, i32range(g, g + 3) FROM generate_series(1, 400) g")
	mustExec(t, db, "UPDATE t SET blob = decode(repeat(md5(id::text), 40), 'hex') WHERE id % 50 = 0")
	mustExec(t, db, "DELETE FROM t WHERE id % 3 = 0")
}

func encPopulate(t *testing.T, db *Database) {
	t.Helper()
	encPopulateButLast(t, db)
	mustExec(t, db, encLast)
}

func encCode(err error) string { return cryptoCode(err) }

func TestEncryptedRoundTripsEveryPageKindAndStoresNoPlaintext(t *testing.T) {
	t.Parallel()
	path := filepath.Join(t.TempDir(), "enc_round_trip.jed")
	db := encCreate(t, path, encKey())
	encPopulate(t, db)
	before := encRows(t, db, encSummary)
	if before[0][0] != "268" {
		t.Fatalf("count = %s, want 268", before[0][0])
	}
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}

	raw, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	if bytes.Contains(raw, []byte("secret-")) {
		t.Fatal("a row value is stored in plaintext")
	}
	if bytes.Contains(raw, []byte("t_not")) {
		t.Fatal("the catalog is stored in plaintext")
	}

	db, err = encOpen(path, encKey())
	if err != nil {
		t.Fatal(err)
	}
	if got := encRows(t, db, encSummary); fmt.Sprint(got) != fmt.Sprint(before) {
		t.Fatalf("reopened summary %v, want %v", got, before)
	}
	mustExec(t, db, "INSERT INTO t VALUES (1000, 'later', NULL, NULL)")
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}
	db, err = encOpen(path, encKey())
	if err != nil {
		t.Fatal(err)
	}
	defer db.Close()
	if got := encRows(t, db, "SELECT note FROM t WHERE id = 1000"); got[0][0] != "later" {
		t.Fatalf("got %v", got)
	}
}

func TestEncryptedCredentialsFailClosed(t *testing.T) {
	t.Parallel()
	dir := t.TempDir()
	path := filepath.Join(dir, "enc_credentials.jed")
	_ = encCreate(t, path, encKey()).Close()
	code := func(db *Database, err error) string {
		if err == nil {
			_ = db.Close()
			t.Fatal("open must fail")
		}
		return encCode(err)
	}
	if c := code(encOpen(path, KeyCredential(filledKey(1)))); c != "28P01" {
		t.Fatalf("wrong key: %s", c)
	}
	if c := code(encOpen(path, PassphraseCredential("x"))); c != "28P01" {
		t.Fatalf("wrong kind: %s", c)
	}
	if c := code(encOpen(path, nil)); c != "28P02" {
		t.Fatalf("no credential: %s", c)
	}

	plain := filepath.Join(dir, "enc_plain.jed")
	pdb, err := CreateDatabase(CreateOptions{Path: plain, PageSize: encPage, SkipFsync: true})
	if err != nil {
		t.Fatal(err)
	}
	_ = pdb.Close()
	if c := code(encOpen(plain, encKey())); c != "28P03" {
		t.Fatalf("plaintext file: %s", c)
	}

	if c := code(CreateDatabase(CreateOptions{Credential: encKey()})); c != "55000" {
		t.Fatalf("in-memory: %s", c)
	}
	if c := code(CreateDatabase(CreateOptions{Path: filepath.Join(dir, "enc_small.jed"), PageSize: 512, SkipFsync: true, Credential: encKey()})); c != "0A000" {
		t.Fatalf("small page: %s", c)
	}

	// An in-memory attachment has nothing at rest; an encrypted one may not join a handle whose
	// scratch files are plaintext (spec/design/encryption.md §6).
	host, err := CreateDatabase(CreateOptions{Path: filepath.Join(dir, "enc_host.jed"), PageSize: encPage, SkipFsync: true})
	if err != nil {
		t.Fatal(err)
	}
	defer host.Close()
	if err := host.Attach("m", AttachMemory().WithCredential(encKey()), false); encCode(err) != "55000" {
		t.Fatalf("in-memory attachment: %v", err)
	}
	if err := host.Attach("e", AttachFile(path).WithCredential(encKey()), false); encCode(err) != "55000" {
		t.Fatalf("encrypted attachment to a spilling handle: %v", err)
	}
	if err := host.Attach("p", AttachFile(plain).WithCredential(encKey()), false); encCode(err) != "28P03" {
		t.Fatalf("credential for a plaintext attachment: %v", err)
	}
}

func TestEncryptedPassphraseUnlocksAndTheKeyIsRedacted(t *testing.T) {
	t.Parallel()
	path := filepath.Join(t.TempDir(), "enc_passphrase.jed")
	db, err := CreateDatabase(CreateOptions{
		Path: path, PageSize: encPage, SkipFsync: true,
		Credential: PassphraseCredential("correct horse"),
		KDF:        Argon2Params{MemoryKiB: 64, Iterations: 1, Parallelism: 1},
	})
	if err != nil {
		t.Fatal(err)
	}
	mustExec(t, db, "CREATE TABLE t (id i32 PRIMARY KEY)")
	mustExec(t, db, "INSERT INTO t VALUES (7)")
	_ = db.Close()
	db, err = encOpen(path, PassphraseCredential("correct horse"))
	if err != nil {
		t.Fatal(err)
	}
	if got := encRows(t, db, "SELECT id FROM t"); got[0][0] != "7" {
		t.Fatalf("got %v", got)
	}
	_ = db.Close()
	if _, err := encOpen(path, PassphraseCredential("wrong")); encCode(err) != "28P01" {
		t.Fatalf("wrong passphrase: %v", err)
	}
	cred := PassphraseCredential("correct horse")
	for _, shown := range []string{fmt.Sprint(cred), fmt.Sprintf("%v %+v %#v %s %x %q", cred, cred, cred, cred, cred, cred), fmt.Sprint(*cred)} {
		if bytes.Contains([]byte(shown), []byte("horse")) {
			t.Fatalf("the passphrase is shown: %s", shown)
		}
	}
	key := KeyCredential(filledKey(0x41))
	if shown := fmt.Sprintf("%v %#v %x", key, key, key); bytes.Contains([]byte(shown), []byte("AAAA")) || bytes.Contains([]byte(shown), []byte("4141")) {
		t.Fatalf("the key is shown: %s", shown)
	}
}

func TestEncryptedTamperingAndReplayAreDetected(t *testing.T) {
	t.Parallel()
	path := filepath.Join(t.TempDir(), "enc_tamper.jed")
	db := encCreate(t, path, encKey())
	encPopulateButLast(t, db)
	fallback := fmt.Sprint(encRows(t, db, encSummary))
	mustExec(t, db, encLast)
	_ = db.Close()
	original, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	expected := func() string {
		db, err := encOpen(path, encKey())
		if err != nil {
			t.Fatal(err)
		}
		defer db.Close()
		return fmt.Sprint(encRows(t, db, encSummary))
	}()

	// outcome opens the file and reads it whole: an XX001 at either step is detection; anything else
	// must be the newest rows or — for a page the newest commit wrote, which recovery checks against
	// its manifest — the previous generation's. Never other rows.
	outcome := func(label string) bool {
		db, err := encOpen(path, encKey())
		if err != nil {
			if encCode(err) != "XX001" {
				t.Fatalf("%s: open: %v", label, err)
			}
			return true
		}
		defer db.Close()
		got, err := encTryRows(db, encSummary)
		if err != nil {
			if encCode(err) != "XX001" {
				t.Fatalf("%s: read: %v", label, err)
			}
			return true
		}
		if s := fmt.Sprint(got); s != expected && s != fallback {
			t.Fatalf("%s: rows %s", label, s)
		}
		return false
	}

	// Flip one ciphertext byte of body pages in turn (encrypted.md §4).
	pages := len(original) / encPage
	flips := []int{}
	for p := 4; p < pages; p += 5 {
		flips = append(flips, p)
	}
	flips = append(flips, pages-1)
	for _, p := range flips {
		b := append([]byte(nil), original...)
		b[p*encPage+40] ^= 0x01
		if err := os.WriteFile(path, b, 0o644); err != nil {
			t.Fatal(err)
		}
		outcome(fmt.Sprintf("flip page %d", p))
	}

	// Replay: copy a genuine body page over another index. A per-page AEAD alone would accept it;
	// the pointer binding names the overwritten page's tag, so a read that reaches it fails.
	detected := 0
	for p := 5; p < pages; p += 7 {
		b := append([]byte(nil), original...)
		copy(b[p*encPage:(p+1)*encPage], original[(p-1)*encPage:p*encPage])
		if err := os.WriteFile(path, b, 0o644); err != nil {
			t.Fatal(err)
		}
		if outcome(fmt.Sprintf("replay onto page %d", p)) {
			detected++
		}
	}
	if detected == 0 {
		t.Fatal("no replay reached a live page")
	}
	if err := os.WriteFile(path, original, 0o644); err != nil {
		t.Fatal(err)
	}
}

func TestEncryptedCompactionKeepsTheKeyAndReseals(t *testing.T) {
	t.Parallel()
	path := filepath.Join(t.TempDir(), "enc_compact.jed")
	db := encCreate(t, path, encKey())
	encPopulate(t, db)
	before := fmt.Sprint(encRows(t, db, encSummary))
	raw, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	keyBlock := append([]byte(nil), raw[2*encPage:4*encPage]...)
	if err := db.Compact("main"); err != nil {
		t.Fatal(err)
	}
	if got := fmt.Sprint(encRows(t, db, encSummary)); got != before {
		t.Fatalf("after compaction %s, want %s", got, before)
	}
	_ = db.Close()
	raw, err = os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(raw[2*encPage:4*encPage], keyBlock) {
		t.Fatal("compaction changed the key block")
	}
	db, err = encOpen(path, encKey())
	if err != nil {
		t.Fatal(err)
	}
	defer db.Close()
	if got := fmt.Sprint(encRows(t, db, encSummary)); got != before {
		t.Fatalf("reopened after compaction %s, want %s", got, before)
	}
}

func TestEncryptedSeededHistoryWritesIdenticalBytes(t *testing.T) {
	t.Parallel()
	dir := t.TempDir()
	image := func(name string) []byte {
		path := filepath.Join(dir, name)
		db := encCreate(t, path, encKey())
		encPopulate(t, db)
		_ = db.Close()
		raw, err := os.ReadFile(path)
		if err != nil {
			t.Fatal(err)
		}
		return raw
	}
	if !bytes.Equal(image("enc_seed_a.jed"), image("enc_seed_b.jed")) {
		t.Fatal("the same seeded history wrote different bytes")
	}
}

func TestEncryptedDatabaseSealsItsScratch(t *testing.T) {
	t.Parallel()
	dir := t.TempDir()
	db := encCreate(t, filepath.Join(dir, "enc_scratch.jed"), encKey())
	defer db.Close()
	if !spillEncrypts(db.core.storage.spill) {
		t.Fatal("an encrypted database must seal its scratch files")
	}
	db.setSpillDirForTest(dir)
	if !spillEncrypts(db.core.storage.spill) {
		t.Fatal("a test spill directory must keep an encrypted database's scratch sealed")
	}
}

// Backups of an encrypted database (page-service.md §9, encrypted.md §13): the page-identical copy
// keeps the ciphertext pages at their indexes and the key block, under sealed bootstrap metas; the
// compacted copy reseals every page under the same data key. Both open with the credential.
func TestEncryptedDatabaseBacksUpInBothForms(t *testing.T) {
	t.Parallel()
	dir := t.TempDir()
	path := filepath.Join(dir, "enc_backup_src.jed")
	db := encCreate(t, path, encKey())
	defer db.Close()
	encPopulate(t, db)
	before := fmt.Sprint(encRows(t, db, encSummary))
	source, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	for _, form := range []struct {
		name          string
		pageIdentical bool
	}{{"pages", true}, {"compact", false}} {
		dest := filepath.Join(dir, "enc_backup_"+form.name+".jed")
		if err := BackupForTesting(db, dest, form.pageIdentical); err != nil {
			t.Fatalf("%s: %v", form.name, err)
		}
		copied, err := os.ReadFile(dest)
		if err != nil {
			t.Fatal(err)
		}
		if bytes.Contains(copied, []byte("secret-")) {
			t.Fatalf("%s: plaintext in the copy", form.name)
		}
		if !bytes.Equal(copied[2*encPage:4*encPage], source[2*encPage:4*encPage]) {
			t.Fatalf("%s: the key block differs", form.name)
		}
		if form.pageIdentical {
			// Every page the copy holds past the metas is the source's, byte for byte.
			for i := 4; i < len(copied)/encPage; i++ {
				page := copied[i*encPage : (i+1)*encPage]
				if !zeroBytes(page) && !bytes.Equal(page, source[i*encPage:(i+1)*encPage]) {
					t.Fatalf("%s: page %d differs from the source", form.name, i)
				}
			}
		}
		if c := encOpenCode(dest, nil); c != "28P02" {
			t.Fatalf("%s: no credential: %q", form.name, c)
		}
		cdb := encMustOpen(t, dest, encKey())
		if got := fmt.Sprint(encRows(t, cdb, encSummary)); got != before {
			t.Fatalf("%s: copy %s, want %s", form.name, got, before)
		}
		mustExec(t, cdb, "INSERT INTO t VALUES (9000, 'in the copy', NULL, NULL)")
		_ = cdb.Close()
	}
	// After a credential change the copy carries the new key block.
	if err := db.ChangeCredential("main", KeyCredential(filledKey(5)), Argon2Params{}); err != nil {
		t.Fatal(err)
	}
	dest := filepath.Join(dir, "enc_backup_changed.jed")
	if err := BackupForTesting(db, dest, true); err != nil {
		t.Fatal(err)
	}
	if c := encOpenCode(dest, encKey()); c != "28P01" {
		t.Fatalf("the old key opens the copy: %q", c)
	}
	cdb := encMustOpen(t, dest, KeyCredential(filledKey(5)))
	defer cdb.Close()
	if got := fmt.Sprint(encRows(t, cdb, encSummary)); got != before {
		t.Fatalf("changed copy %s, want %s", got, before)
	}
}

// encKeyPages is the file's two key-block pages (2 and 3).
func encKeyPages(t *testing.T, path string) ([]byte, []byte) {
	t.Helper()
	raw, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	return raw[2*encPage : 3*encPage], raw[3*encPage : 4*encPage]
}

func encSetKeyPages(t *testing.T, path string, page2, page3 []byte) {
	t.Helper()
	raw, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	copy(raw[2*encPage:], page2)
	copy(raw[3*encPage:], page3)
	if err := os.WriteFile(path, raw, 0o644); err != nil {
		t.Fatal(err)
	}
}

// encOpenCode opens path with cred and reports the open's error code ("" when it opened).
func encOpenCode(path string, cred *Credential) string {
	db, err := encOpen(path, cred)
	if err == nil {
		_ = db.Close()
		return ""
	}
	return encCode(err)
}

func encMustOpen(t *testing.T, path string, cred *Credential) *Database {
	t.Helper()
	db, err := encOpen(path, cred)
	if err != nil {
		t.Fatalf("open: %v", err)
	}
	return db
}

var encSmallKDF = Argon2Params{MemoryKiB: 64, Iterations: 1, Parallelism: 1}

func TestEncryptedChangingTheCredentialRewrapsTheKeyBlockOnly(t *testing.T) {
	t.Parallel()
	path := filepath.Join(t.TempDir(), "enc_change.jed")
	db := encCreate(t, path, encKey())
	encPopulate(t, db)
	before := fmt.Sprint(encRows(t, db, encSummary))
	raw, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	body := raw[4*encPage:]
	pass := PassphraseCredential("new phrase")
	if err := db.ChangeCredential("main", pass, encSmallKDF); err != nil {
		t.Fatal(err)
	}
	// The handle keeps working, and no body page was rewritten.
	if got := fmt.Sprint(encRows(t, db, encSummary)); got != before {
		t.Fatalf("after the change %s, want %s", got, before)
	}
	_ = db.Close()
	raw, err = os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(raw[4*encPage:], body) {
		t.Fatal("a credential change rewrote a body page")
	}
	page2, page3 := encKeyPages(t, path)
	if !bytes.Equal(page2, page3) {
		t.Fatal("the key-block copies differ after a credential change")
	}
	if !bytes.Equal(page2[8:16], []byte{0, 0, 0, 0, 0, 0, 0, 2}) {
		t.Fatalf("generation %x, want 2", page2[8:16])
	}

	if c := encOpenCode(path, encKey()); c != "28P01" {
		t.Fatalf("the old credential: %q", c)
	}
	db = encMustOpen(t, path, pass)
	if got := fmt.Sprint(encRows(t, db, encSummary)); got != before {
		t.Fatalf("reopened %s, want %s", got, before)
	}
	// A second change, back to a host key, and a write after it.
	if err := db.ChangeCredential("main", KeyCredential(filledKey(7)), Argon2Params{}); err != nil {
		t.Fatal(err)
	}
	mustExec(t, db, "INSERT INTO t VALUES (6000, 'after', NULL, NULL)")
	_ = db.Close()
	db = encMustOpen(t, path, KeyCredential(filledKey(7)))
	defer db.Close()
	if got := encRows(t, db, "SELECT count(*) FROM t"); got[0][0] != "269" {
		t.Fatalf("count %v, want 269", got)
	}
	if page2, _ := encKeyPages(t, path); !bytes.Equal(page2[8:16], []byte{0, 0, 0, 0, 0, 0, 0, 3}) {
		t.Fatalf("generation %x, want 3", page2[8:16])
	}
}

func TestEncryptedCredentialChangeErrors(t *testing.T) {
	t.Parallel()
	dir := t.TempDir()
	path := filepath.Join(dir, "enc_change_errors.jed")
	db := encCreate(t, path, encKey())
	newCred := KeyCredential(filledKey(2))
	for _, c := range []struct {
		name string
		cred *Credential
		kdf  Argon2Params
		want string
	}{
		{"nope", newCred, Argon2Params{}, "42704"},
		{"temp", newCred, Argon2Params{}, "42704"},
		{"main", PassphraseCredential("p"), Argon2Params{MemoryKiB: 1, Iterations: 1, Parallelism: 1}, "22023"},
	} {
		if got := encCode(db.ChangeCredential(c.name, c.cred, c.kdf)); got != c.want {
			t.Fatalf("%s: %q, want %s", c.name, got, c.want)
		}
	}
	mustExec(t, db, "CREATE TABLE t (id i32 PRIMARY KEY)")
	writer := db.Session(SessionOptions{})
	if err := writer.Begin(true); err != nil {
		t.Fatal(err)
	}
	if _, err := writer.Exec(t.Context(), "INSERT INTO t VALUES (1)"); err != nil {
		t.Fatal(err)
	}
	if got := encCode(db.ChangeCredential("main", newCred, Argon2Params{})); got != "55006" {
		t.Fatalf("under an open writer: %q", got)
	}
	if err := writer.Rollback(); err != nil {
		t.Fatal(err)
	}
	writer.Close()
	_ = db.Close()

	ro, err := OpenDatabaseWithOptions(path, OpenOptions{SkipFsync: true, ReadOnly: true, Credential: encKey()})
	if err != nil {
		t.Fatal(err)
	}
	if got := encCode(ro.ChangeCredential("main", newCred, Argon2Params{})); got != "25006" {
		t.Fatalf("read-only: %q", got)
	}
	_ = ro.Close()

	memory, err := CreateDatabase(CreateOptions{})
	if err != nil {
		t.Fatal(err)
	}
	defer memory.Close()
	if got := encCode(memory.ChangeCredential("main", newCred, Argon2Params{})); got != "55000" {
		t.Fatalf("in-memory: %q", got)
	}
	plain, err := CreateDatabase(CreateOptions{Path: filepath.Join(dir, "enc_change_plain.jed"), PageSize: encPage, SkipFsync: true})
	if err != nil {
		t.Fatal(err)
	}
	defer plain.Close()
	if got := encCode(plain.ChangeCredential("main", newCred, Argon2Params{})); got != "28P03" {
		t.Fatalf("plaintext: %q", got)
	}
}

func TestEncryptedAttachmentChangesItsOwnCredential(t *testing.T) {
	t.Parallel()
	dir := t.TempDir()
	path := filepath.Join(dir, "enc_change_attach.jed")
	db := encCreate(t, path, encKey())
	mustExec(t, db, "CREATE TABLE t (id i32 PRIMARY KEY)")
	mustExec(t, db, "INSERT INTO t VALUES (1)")
	_ = db.Close()
	// An encrypted attachment joins a handle whose scratch files are sealed: an encrypted main.
	main := encCreate(t, filepath.Join(dir, "enc_change_main.jed"), KeyCredential(filledKey(3)))
	if err := main.Attach("other", AttachFile(path).WithCredential(encKey()), false); err != nil {
		t.Fatal(err)
	}
	if err := main.ChangeCredential("other", KeyCredential(filledKey(4)), Argon2Params{}); err != nil {
		t.Fatal(err)
	}
	_ = main.Close()
	if c := encOpenCode(path, encKey()); c != "28P01" {
		t.Fatalf("the old credential: %q", c)
	}
	db = encMustOpen(t, path, KeyCredential(filledKey(4)))
	defer db.Close()
	if got := encRows(t, db, "SELECT id FROM t"); got[0][0] != "1" {
		t.Fatalf("got %v", got)
	}
}

// A crash during a credential change (encryption.md §5): the copy that is not selected is written
// first, so a crash before the first barrier leaves the old credential working, and one after it the
// new — never neither. A writable open repairs a differing pair; a read-only one leaves it.
func TestEncryptedInterruptedCredentialChangeLeavesExactlyOneCredential(t *testing.T) {
	t.Parallel()
	path := filepath.Join(t.TempDir(), "enc_change_crash.jed")
	db := encCreate(t, path, encKey())
	mustExec(t, db, "CREATE TABLE t (id i32 PRIMARY KEY)")
	mustExec(t, db, "INSERT INTO t VALUES (1)")
	_ = db.Close()
	old, _ := encKeyPages(t, path)
	old = append([]byte(nil), old...)
	newKey := KeyCredential(filledKey(8))
	db = encMustOpen(t, path, encKey())
	if err := db.ChangeCredential("main", newKey, Argon2Params{}); err != nil {
		t.Fatal(err)
	}
	_ = db.Close()
	newer, _ := encKeyPages(t, path)
	newer = append([]byte(nil), newer...)
	pagesAre := func(label string, page2, page3 []byte) {
		t.Helper()
		got2, got3 := encKeyPages(t, path)
		if !bytes.Equal(got2, page2) || !bytes.Equal(got3, page3) {
			t.Fatalf("%s: unexpected key-block pages", label)
		}
	}

	// Torn first write (page 3, the unselected copy of an equal pair): the old credential.
	torn := append([]byte(nil), newer...)
	for i := 100; i < len(torn); i++ {
		torn[i] = 0xEE
	}
	encSetKeyPages(t, path, old, torn)
	if c := encOpenCode(path, newKey); c != "28P01" {
		t.Fatalf("torn first write, new credential: %q", c)
	}
	if c := encOpenCode(path, encKey()); c != "" {
		t.Fatalf("torn first write, old credential: %q", c)
	}
	pagesAre("the open repairs the torn copy", old, old)

	// The first write landed, the second did not: the new credential, and a read-only open leaves the
	// pair as it is.
	encSetKeyPages(t, path, old, newer)
	if c := encOpenCode(path, encKey()); c != "28P01" {
		t.Fatalf("first write landed, old credential: %q", c)
	}
	ro, err := OpenDatabaseWithOptions(path, OpenOptions{SkipFsync: true, ReadOnly: true, Credential: newKey})
	if err != nil {
		t.Fatal(err)
	}
	if got := encRows(t, ro, "SELECT id FROM t"); got[0][0] != "1" {
		t.Fatalf("got %v", got)
	}
	_ = ro.Close()
	pagesAre("a read-only open leaves the pair", old, newer)
	if c := encOpenCode(path, newKey); c != "" {
		t.Fatalf("first write landed, new credential: %q", c)
	}
	pagesAre("a writable open repairs the pair", newer, newer)

	// Two valid copies of one generation that differ are tampering.
	forged := append([]byte(nil), newer...)
	forged[40] ^= 1
	binary.BigEndian.PutUint32(forged[124:], crc32IEEE(forged[:124]))
	encSetKeyPages(t, path, newer, forged)
	if c := encOpenCode(path, newKey); c != "XX001" {
		t.Fatalf("forged copy: %q", c)
	}
}

// encScratchFiles is the contents of every scratch file in dir.
func encScratchFiles(t *testing.T, dir string) [][]byte {
	t.Helper()
	entries, err := os.ReadDir(dir)
	if err != nil {
		t.Fatal(err)
	}
	var out [][]byte
	for _, e := range entries {
		if !strings.HasPrefix(e.Name(), "jed-spill-") {
			continue
		}
		b, err := os.ReadFile(filepath.Join(dir, e.Name()))
		if err != nil {
			continue // closed and removed meanwhile
		}
		out = append(out, b)
	}
	return out
}

// Scratch encryption (encryption.md §6): an encrypted database spills its sorts, hash operators and
// temp tables to sealed scratch files, with the same rows and cost as running in memory and no
// plaintext in the scratch directory.
func TestEncryptedDatabaseSpillsToSealedScratch(t *testing.T) {
	t.Parallel()
	dir := t.TempDir()
	scratch := filepath.Join(dir, "scratch")
	if err := os.Mkdir(scratch, 0o755); err != nil {
		t.Fatal(err)
	}
	db := encCreate(t, filepath.Join(dir, "enc_scratch.jed"), encKey())
	defer db.Close()
	db.setSpillDirForTest(scratch)
	mustExec(t, db, "CREATE TABLE t (id i64 PRIMARY KEY, k i32, s text)")
	mustExec(t, db, "INSERT INTO t SELECT g, g % 37, 'secret-' || (g * 7919 % 1000) FROM generate_series(1, 3000) g")
	session := db.Session(SessionOptions{})
	defer session.Close()
	for _, sql := range []string{
		"SELECT s, id FROM t ORDER BY s DESC, id",
		"SELECT k, count(DISTINCT s), max(s) FROM t GROUP BY k ORDER BY k",
		"SELECT a.id, b.id FROM t a JOIN t b ON a.s = b.s ORDER BY a.id, b.id",
	} {
		session.SetWorkMem(0)
		resident, err := queryOutcome(session, sql, nil)
		if err != nil {
			t.Fatal(err)
		}
		session.SetWorkMem(256)
		rows, err := session.Query(t.Context(), sql)
		if err != nil {
			t.Fatal(err)
		}
		if !rows.Next() {
			t.Fatalf("%s: no rows", sql)
		}
		files := encScratchFiles(t, scratch)
		if len(files) == 0 {
			t.Fatalf("%s: nothing spilled", sql)
		}
		for _, b := range files {
			if bytes.Contains(b, []byte("secret-")) {
				t.Fatalf("%s: plaintext in scratch", sql)
			}
		}
		_ = rows.Close()
		spilled, err := queryOutcome(session, sql, nil)
		if err != nil {
			t.Fatal(err)
		}
		if fmt.Sprint(spilled.Rows) != fmt.Sprint(resident.Rows) || spilled.Cost != resident.Cost {
			t.Fatalf("%s: spilled result or cost differs from resident", sql)
		}
	}
	// A temp table past temp_buffers spills its pages to a sealed scratch store.
	spills := TempSpills()
	session.SetTempBuffers(8192)
	session.SetMaxTempStorageBytes(0)
	if _, err := session.ExecuteScript("CREATE TEMP TABLE tt AS SELECT id, s FROM t"); err != nil {
		t.Fatal(err)
	}
	if TempSpills() <= spills {
		t.Fatal("the temp table did not spill")
	}
	for _, b := range encScratchFiles(t, scratch) {
		if bytes.Contains(b, []byte("secret-")) {
			t.Fatal("plaintext temp page")
		}
	}
	out, err := queryOutcome(session, "SELECT count(*), max(s) FROM tt", nil)
	if err != nil {
		t.Fatal(err)
	}
	if got := renderRows(out.Rows); got[0][0] != "3000" {
		t.Fatalf("temp table count %v", got)
	}
}

// An encrypted attachment may not join a handle whose scratch files are plaintext; a handle created
// with ScratchEncryptionOn takes it (encryption.md §6).
func TestScratchEncryptionGatesEncryptedAttachments(t *testing.T) {
	t.Parallel()
	dir := t.TempDir()
	enc := filepath.Join(dir, "enc_scratch_att.jed")
	_ = encCreate(t, enc, encKey()).Close()
	plain := func(name string, scratch ScratchEncryption) *Database {
		t.Helper()
		db, err := CreateDatabase(CreateOptions{Path: filepath.Join(dir, name), PageSize: encPage, SkipFsync: true, ScratchEncryption: scratch})
		if err != nil {
			t.Fatal(err)
		}
		return db
	}
	attach := func(db *Database) error { return db.Attach("e", AttachFile(enc).WithCredential(encKey()), false) }
	p := plain("enc_scratch_p1.jed", ScratchEncryptionAuto)
	if err := attach(p); encCode(err) != "55000" {
		t.Fatalf("plaintext scratch: %v", err)
	}
	_ = p.Close()
	sealed := plain("enc_scratch_p2.jed", ScratchEncryptionOn)
	if err := attach(sealed); err != nil {
		t.Fatalf("sealed scratch: %v", err)
	}
	_ = sealed.Close()
	// The open option seals as well.
	opened, err := OpenDatabaseWithOptions(filepath.Join(dir, "enc_scratch_p1.jed"), OpenOptions{SkipFsync: true, ScratchEncryption: ScratchEncryptionOn})
	if err != nil {
		t.Fatal(err)
	}
	defer opened.Close()
	if err := attach(opened); err != nil {
		t.Fatalf("sealed scratch at open: %v", err)
	}
}

// encSealedHandle creates a file database whose scratch files are sealed, under the seeded key
// stream, exclusively locked (so it can be rekeyed); cred nil creates it plaintext.
func encSealedHandle(t *testing.T, path string, cred *Credential, pageSize uint32) *Database {
	t.Helper()
	db, err := CreateDatabase(CreateOptions{
		Path: path, PageSize: pageSize, SkipFsync: true, Environment: encSeededEnv(), Credential: cred,
		ScratchEncryption: ScratchEncryptionOn, Locking: LockingExclusive,
	})
	if err != nil {
		t.Fatalf("create: %v", err)
	}
	return db
}

// Rotating the data key, encrypting a plaintext file and decrypting an encrypted one (encryption.md
// §5): every tree rebuilt under the target layout, the rows unchanged.
func TestEncryptedRekeyRotatesEncryptsAndDecrypts(t *testing.T) {
	t.Parallel()
	path := filepath.Join(t.TempDir(), "enc_rekey.jed")
	db := encSealedHandle(t, path, nil, encPage)
	encPopulate(t, db)
	before := fmt.Sprint(encRows(t, db, encSummary))

	// Encrypt the plaintext file in place.
	if err := db.Rekey("main", encKey(), Argon2Params{}); err != nil {
		t.Fatal(err)
	}
	if got := fmt.Sprint(encRows(t, db, encSummary)); got != before {
		t.Fatalf("after encrypting %s, want %s", got, before)
	}
	mustExec(t, db, "INSERT INTO t VALUES (7000, 'after-encrypt', NULL, NULL)")
	_ = db.Close()
	raw, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	if bytes.Contains(raw, []byte("secret-")) {
		t.Fatal("a row value is stored in plaintext after encrypting")
	}
	if c := encOpenCode(path, nil); c != "28P02" {
		t.Fatalf("no credential: %q", c)
	}
	db = encMustOpen(t, path, encKey())
	if got := encRows(t, db, "SELECT count(*) FROM t"); got[0][0] != "269" {
		t.Fatalf("count %v", got)
	}
	keyBlock := append([]byte(nil), raw[2*encPage:3*encPage]...)

	// Rotate: a new data key under a passphrase; the old key no longer opens the file.
	pass := PassphraseCredential("rotated")
	if err := db.Rekey("main", pass, encSmallKDF); err != nil {
		t.Fatal(err)
	}
	_ = db.Close()
	rotated, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	if bytes.Equal(rotated[2*encPage:3*encPage], keyBlock) {
		t.Fatal("rotation kept the key block")
	}
	if !bytes.Equal(rotated[2*encPage+8:2*encPage+16], []byte{0, 0, 0, 0, 0, 0, 0, 1}) {
		t.Fatalf("a rotated key block starts a new lineage at generation 1: %x", rotated[2*encPage+8:2*encPage+16])
	}
	if c := encOpenCode(path, encKey()); c != "28P01" {
		t.Fatalf("the old key: %q", c)
	}
	db = encMustOpen(t, path, pass)
	if got := encRows(t, db, "SELECT count(*) FROM t"); got[0][0] != "269" {
		t.Fatalf("count %v", got)
	}

	// Decrypt.
	if err := db.Rekey("main", nil, Argon2Params{}); err != nil {
		t.Fatal(err)
	}
	if got := encRows(t, db, "SELECT note FROM t WHERE id = 7000"); got[0][0] != "after-encrypt" {
		t.Fatalf("got %v", got)
	}
	_ = db.Close()
	if c := encOpenCode(path, encKey()); c != "28P03" {
		t.Fatalf("a credential for the decrypted file: %q", c)
	}
	db = encMustOpen(t, path, nil)
	defer db.Close()
	if got := encRows(t, db, "SELECT count(*) FROM t"); got[0][0] != "269" {
		t.Fatalf("count %v", got)
	}
	if got := encRows(t, db, "SELECT count(*) FROM t WHERE note = 'secret-10'"); got[0][0] != "1" {
		t.Fatalf("index lookup %v", got)
	}
}

func TestEncryptedRekeyErrors(t *testing.T) {
	t.Parallel()
	dir := t.TempDir()
	// A handle whose scratch files are plaintext cannot encrypt.
	plain, err := CreateDatabase(CreateOptions{Path: filepath.Join(dir, "enc_rekey_plain.jed"), PageSize: encPage, SkipFsync: true, Locking: LockingExclusive})
	if err != nil {
		t.Fatal(err)
	}
	if got := encCode(plain.Rekey("main", encKey(), Argon2Params{})); got != "55000" {
		t.Fatalf("plaintext scratch: %q", got)
	}
	_ = plain.Close()
	// The owner of a database the page service shares rekeys it (encrypted.md §14).
	served, err := CreateDatabase(CreateOptions{Path: filepath.Join(dir, "enc_rekey_served.jed"), PageSize: encPage, SkipFsync: true, ScratchEncryption: ScratchEncryptionOn})
	if err != nil {
		t.Fatal(err)
	}
	if err := served.Rekey("main", encKey(), Argon2Params{}); err != nil {
		t.Fatalf("served: %v", err)
	}
	_ = served.Close()
	small := encSealedHandle(t, filepath.Join(dir, "enc_rekey_small.jed"), nil, 512)
	if got := encCode(small.Rekey("main", encKey(), Argon2Params{})); got != "0A000" {
		t.Fatalf("small page: %q", got)
	}
	_ = small.Close()
	memory, err := CreateDatabase(CreateOptions{})
	if err != nil {
		t.Fatal(err)
	}
	if got := encCode(memory.Rekey("main", encKey(), Argon2Params{})); got != "55000" {
		t.Fatalf("in-memory: %q", got)
	}
	db := encSealedHandle(t, filepath.Join(dir, "enc_rekey_err.jed"), encKey(), encPage)
	defer db.Close()
	if got := encCode(db.Rekey("nope", encKey(), Argon2Params{})); got != "42704" {
		t.Fatalf("unknown name: %q", got)
	}
	mustExec(t, db, "CREATE TABLE t (id i32 PRIMARY KEY)")
	mustExec(t, db, "INSERT INTO t VALUES (1), (2)")
	reader := db.Session(SessionOptions{})
	defer reader.Close()
	rows, err := reader.Query(t.Context(), "SELECT id FROM t")
	if err != nil {
		t.Fatal(err)
	}
	if !rows.Next() {
		t.Fatal("no rows")
	}
	if got := encCode(db.Rekey("main", nil, Argon2Params{})); got != "55006" {
		t.Fatalf("under a reader: %q", got)
	}
	_ = rows.Close()
}

// A row that fits a plaintext page but not an encrypted one (40 bytes less, wider pointers): the
// encryption fails before the file is replaced, which still holds every row (encryption.md §5).
func TestEncryptedRekeyTheTargetCannotStoreLeavesTheFileUntouched(t *testing.T) {
	t.Parallel()
	path := filepath.Join(t.TempDir(), "enc_rekey_toolong.jed")
	db := encSealedHandle(t, path, nil, encPage)
	defer db.Close()
	mustExec(t, db, "CREATE TABLE t (k text PRIMARY KEY)")
	// The longest key the plaintext layout stores.
	n := 1000
	for {
		if _, err := encTryRows(db, fmt.Sprintf("INSERT INTO t VALUES (repeat('k', %d)) RETURNING 1", n)); err == nil {
			break
		}
		n--
	}
	before, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	err = db.Rekey("main", encKey(), Argon2Params{})
	if err == nil {
		t.Fatal("the key does not fit the encrypted layout")
	}
	if encCode(err) == "XX001" {
		t.Fatalf("not corruption: %v", err)
	}
	if after, _ := os.ReadFile(path); !bytes.Equal(after, before) {
		t.Fatal("a failed rekey changed the file")
	}
	if got := encRows(t, db, "SELECT length(k) FROM t"); got[0][0] != fmt.Sprint(n) {
		t.Fatalf("got %v, want %d", got, n)
	}
	if _, err := os.Stat(path + ".jedtmp"); !os.IsNotExist(err) {
		t.Fatalf("the temp file was left behind: %v", err)
	}
}

// The same history and rekey write the same bytes under the seeded key source.
func TestEncryptedSeededRekeyWritesIdenticalBytes(t *testing.T) {
	t.Parallel()
	dir := t.TempDir()
	image := func(name string) []byte {
		path := filepath.Join(dir, name)
		db := encSealedHandle(t, path, encKey(), encPage)
		encPopulate(t, db)
		if err := db.Rekey("main", KeyCredential(filledKey(6)), Argon2Params{}); err != nil {
			t.Fatal(err)
		}
		_ = db.Close()
		raw, err := os.ReadFile(path)
		if err != nil {
			t.Fatal(err)
		}
		return raw
	}
	if !bytes.Equal(image("enc_rekey_seed_a.jed"), image("enc_rekey_seed_b.jed")) {
		t.Fatal("the same seeded rekey wrote different bytes")
	}
}

// A rebuild larger than the page-cache budget moves its scratch domain to a sealed scratch file
// instead of holding every rebuilt page in memory (encryption.md §5, §6).
func TestEncryptedLargeRekeySpillsItsScratchDomain(t *testing.T) {
	t.Parallel()
	dir := t.TempDir()
	path := filepath.Join(dir, "enc_rekey_large.jed")
	scratch := filepath.Join(dir, "scratch")
	if err := os.Mkdir(scratch, 0o755); err != nil {
		t.Fatal(err)
	}
	_ = encSealedHandle(t, path, encKey(), encPage).Close()
	db, err := OpenDatabaseWithOptions(path, OpenOptions{SkipFsync: true, CacheBytes: 64 * 1024, Credential: encKey(), Locking: LockingExclusive})
	if err != nil {
		t.Fatal(err)
	}
	db.setSpillDirForTest(scratch)
	mustExec(t, db, "CREATE TABLE big (id i64 PRIMARY KEY, s text)")
	mustExec(t, db, "INSERT INTO big SELECT g, 'secret-' || md5(g::text) || md5((g * 3)::text) FROM generate_series(1, 30000) g")
	const probe = "SELECT count(*), max(s), sum(length(s)) FROM big"
	want := fmt.Sprint(encRows(t, db, probe))
	spills := TempSpills()
	if err := db.Rekey("main", KeyCredential(filledKey(2)), Argon2Params{}); err != nil {
		t.Fatal(err)
	}
	if TempSpills() <= spills {
		t.Fatal("the rebuild did not spill its scratch domain")
	}
	if got := fmt.Sprint(encRows(t, db, probe)); got != want {
		t.Fatalf("after the rekey %s, want %s", got, want)
	}
	_ = db.Close()
	if entries, _ := os.ReadDir(scratch); len(entries) != 0 {
		t.Fatalf("scratch files left behind: %d", len(entries))
	}
	db = encMustOpen(t, path, KeyCredential(filledKey(2)))
	defer db.Close()
	if got := fmt.Sprint(encRows(t, db, probe)); got != want {
		t.Fatalf("reopened %s, want %s", got, want)
	}
}
