package jed

// The v53 commit manifest (spec/fileformat/format.md *Commit manifest*; spec/design/manifest-crc32.md).
// An entry is a page id and a CRC-32 (8 bytes): a plaintext page's entry records the checksum field
// the page already holds, so a commit hashes each page once and recovery checks the page's own
// checksum and compares it; an encrypted page has no plaintext checksum, so its entry is the
// CRC-32/IEEE of its ciphertext. The manifest-overflow chain's digest is a CRC-32 too.

import (
	"encoding/binary"
	"hash/crc32"
)

// manifestEntrySize is a manifest entry's bytes.
const manifestEntrySize = 8

// entryDigest reads the digest of the manifest entry at e.
func entryDigest(e []byte) uint64 {
	return uint64(binary.BigEndian.Uint32(e[4:]))
}

// putManifestEntry writes one entry at b.
func putManifestEntry(b []byte, e manifestEntry) {
	binary.BigEndian.PutUint32(b, e.index)
	binary.BigEndian.PutUint32(b[4:], uint32(e.digest))
}

// pageDigest is the manifest identity of a body page's stored bytes.
func pageDigest(block []byte, encrypted bool) uint64 {
	if encrypted {
		return uint64(crc32.ChecksumIEEE(block))
	}
	// The page's own checksum field. The reader verifies the page against it separately.
	return uint64(binary.BigEndian.Uint32(block[12:16]))
}

// chainDigest extends the digest of the manifest-overflow chain.
func chainDigest(crc uint64, b []byte) uint64 {
	return uint64(crc32.Update(uint32(crc), crc32.IEEETable, b))
}
