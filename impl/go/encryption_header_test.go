package jed

// A damaged encrypted header is XX001, never 28P03 (spec/fileformat/encrypted.md §6): when neither
// meta slot is CRC-valid the file declares no variant, so a credential is neither applicable nor
// inapplicable and slot selection reports the corruption, with and without the credential.

import (
	"encoding/binary"
	"os"
	"path/filepath"
	"testing"
)

func TestEncryptedDamagedHeaderIsCorruptionNotCredentialNotApplicable(t *testing.T) {
	t.Parallel()
	path := filepath.Join(t.TempDir(), "header.jed")
	db := encCreate(t, path, encKey())
	encPopulate(t, db)
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}
	raw, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	// Slot 0's reserve byte and page-size field change (the page size reads as 256) and its CRC is
	// recomputed over the slot as written, so at the page size the file now declares neither slot is
	// CRC-valid.
	binary.BigEndian.PutUint32(raw[7:11], 1)
	binary.BigEndian.PutUint32(raw[32:36], metaCRC(raw[:encPage]))
	if err := os.WriteFile(path, raw, 0o644); err != nil {
		t.Fatal(err)
	}
	if code := encOpenCode(path, encKey()); code != "XX001" {
		t.Fatalf("open with the credential: %s, want XX001", code)
	}
	if code := encOpenCode(path, nil); code != "XX001" {
		t.Fatalf("open without a credential: %s, want XX001", code)
	}
}
