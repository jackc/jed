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
	"fmt"
	"os"
	"path/filepath"
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

	// An in-memory attachment has nothing at rest; an encrypted one may not join a handle that can
	// spill plaintext to disk (spec/fileformat/encrypted.md §10).
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

func TestEncryptedDatabaseHasNoScratchTargetAndNoBackup(t *testing.T) {
	t.Parallel()
	dir := t.TempDir()
	db := encCreate(t, filepath.Join(dir, "enc_scratch.jed"), encKey())
	defer db.Close()
	if db.core.storage.spill != nil {
		t.Fatal("an encrypted database must have no scratch target")
	}
	db.setSpillDirForTest(dir)
	if db.core.storage.spill != nil {
		t.Fatal("a test spill directory must not give an encrypted database a scratch target")
	}
	if err := BackupForTesting(db, filepath.Join(dir, "backup.jed"), false); encCode(err) != "0A000" {
		t.Fatalf("backup of an encrypted database: %v", err)
	}
}
