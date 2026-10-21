package jed

// The encrypted variant's cross-core byte contract (spec/fileformat/encrypted.md; CLAUDE.md §8): each
// encrypted_* golden is the from-scratch image of a logical database under the goldens' host key,
// with the key material and every page nonce drawn from the insecure seeded stream. This core must
// (a) WRITE the same logical database — built in an encrypted file, so its trees split at the
// encrypted capacity — to exactly the golden's bytes, and (b) READ the golden back with the key. The
// goldens are authored by spec/fileformat/verify.rb. Mirrors impl/rust/tests/encrypted_golden.rs.

import (
	"bytes"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

const (
	encGoldenPage = 1024
	encGoldenSeed = 0x4A454442
)

func encGoldenKey() *Credential { return KeyCredential(filledKey(0x4A)) }

// encryptedImageForTesting is the from-scratch image of db's committed snapshot in the encrypted
// variant, under a fresh key block for cred whose key material — and every page nonce after it —
// comes from the insecure stream seeded with seed (encrypted.md §8): the bytes an encrypted_* golden
// pins.
func encryptedImageForTesting(db *Database, seed uint64, cred *Credential) ([]byte, error) {
	codec, err := createCodec(cred, Argon2Params{}, NewSeededRandom(seed))
	if err != nil {
		return nil, err
	}
	snap := db.core.roots.Load().committed
	return snap.toImageSealed(db.PageSize(), 1, codec)
}

// encRenderRows renders a result set for comparison (renderRows, as text).
func encRenderRows(rows [][]Value) string { return fmt.Sprint(renderRows(rows)) }

const goldenAlpha64 = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"

// encFiller is the format.md filler64 incompressible filler (xorshift32 seeded JEDB).
func encFiller(n int) []uint32 {
	x := uint32(0x4A454442)
	out := make([]uint32, n)
	for i := range out {
		x ^= x << 13
		x ^= x >> 17
		x ^= x << 5
		out[i] = x
	}
	return out
}

func encFillerText(n int) string {
	var b strings.Builder
	for _, x := range encFiller(n) {
		b.WriteByte(goldenAlpha64[x%64])
	}
	return b.String()
}

func encFillerBytesHex(n int) string {
	var b strings.Builder
	for _, x := range encFiller(n) {
		fmt.Fprintf(&b, "%02x", x%256)
	}
	return b.String()
}

// checkEncryptedGolden builds name's logical database with sql, compares its sealed image with the
// golden, and reads the golden back, checking probe against what the built database returns.
func checkEncryptedGolden(t *testing.T, name string, sql []string, probe string) {
	t.Helper()
	dir := t.TempDir()
	db, err := CreateDatabase(CreateOptions{Path: filepath.Join(dir, "golden.jed"), PageSize: encGoldenPage, SkipFsync: true, Credential: encGoldenKey()})
	if err != nil {
		t.Fatalf("create: %v", err)
	}
	for _, stmt := range sql {
		mustExec(t, db, stmt)
	}
	want := encRenderRows(queryRows(t, db, probe))
	image, err := encryptedImageForTesting(db, encGoldenSeed, encGoldenKey())
	if err != nil {
		t.Fatalf("%s: image: %v", name, err)
	}
	_ = db.Close()
	golden := fixture(t, name)
	if !bytes.Equal(image, golden) {
		diff := -1
		for i := range min(len(image), len(golden)) {
			if image[i] != golden[i] {
				diff = i
				break
			}
		}
		t.Fatalf("%s: the sealed image differs from the golden (len %d vs %d, first difference at byte %d, page %d)",
			name, len(image), len(golden), diff, diff/encGoldenPage)
	}

	copyPath := filepath.Join(dir, "read.jed")
	if err := os.WriteFile(copyPath, golden, 0o644); err != nil {
		t.Fatal(err)
	}
	rdb, err := OpenDatabaseWithOptions(copyPath, OpenOptions{Credential: encGoldenKey()})
	if err != nil {
		t.Fatalf("open %s: %v", name, err)
	}
	defer rdb.Close()
	if got := encRenderRows(queryRows(t, rdb, probe)); got != want {
		t.Fatalf("%s: rows read back %q, want %q", name, got, want)
	}
}

func TestEncryptedGoldenEmptyDB(t *testing.T) {
	t.Parallel()
	checkEncryptedGolden(t, "encrypted_empty_db.jed", nil, "SELECT 1")
}

func TestEncryptedGoldenPKTable(t *testing.T) {
	t.Parallel()
	sql := []string{"CREATE TABLE t (id i32 PRIMARY KEY, v i16)"}
	for i := 1; i <= 300; i++ {
		v := fmt.Sprint(i * 10)
		if i == 3 {
			v = "NULL"
		}
		sql = append(sql, fmt.Sprintf("INSERT INTO t VALUES (%d, %s)", i, v))
	}
	checkEncryptedGolden(t, "encrypted_pk_table.jed", sql, "SELECT count(*), sum(id), sum(v), count(v) FROM t")
}

func TestEncryptedGoldenOverflowTable(t *testing.T) {
	t.Parallel()
	sql := []string{
		"CREATE TABLE t (id i32 PRIMARY KEY, body text, blob bytea)",
		fmt.Sprintf("INSERT INTO t VALUES (1, '%s', '\\x%s')", encFillerText(2500), encFillerBytesHex(1200)),
		"INSERT INTO t VALUES (2, 'small', '\\xcafe')",
		"INSERT INTO t VALUES (3, NULL, NULL)",
	}
	checkEncryptedGolden(t, "encrypted_overflow_table.jed", sql, "SELECT id, md5(body), md5(blob) FROM t ORDER BY id")
}

func TestEncryptedGoldenIndexTable(t *testing.T) {
	t.Parallel()
	sql := []string{
		"CREATE TABLE t (a i32 PRIMARY KEY, v i64, r i32range)",
		"CREATE INDEX t_r ON t USING gist (r)",
		"CREATE INDEX t_v ON t (v)",
	}
	for a := 1; a <= 150; a++ {
		sql = append(sql, fmt.Sprintf("INSERT INTO t VALUES (%d, %d, i32range(%d, %d))", a, a*3, a, a+5))
	}
	checkEncryptedGolden(t, "encrypted_index_table.jed", sql,
		"SELECT count(*) FILTER (WHERE r && i32range(40, 44)), count(*) FILTER (WHERE v = 300), sum(v) FROM t")
}

func TestEncryptedGoldenCatalog(t *testing.T) {
	t.Parallel()
	var sql []string
	for i := 0; i < 30; i++ {
		sql = append(sql, fmt.Sprintf("CREATE TABLE t%02d (id i32 PRIMARY KEY, a text, b i64)", i))
	}
	checkEncryptedGolden(t, "encrypted_catalog.jed", sql, "SELECT count(*) FROM t29")
}
