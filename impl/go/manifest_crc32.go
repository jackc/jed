package jed

// PROTOTYPE (spec/design/manifest-crc32.md): narrow commit-manifest entries.
//
// A v50 manifest entry is a page id and the CRC-64/ECMA-182 of the page's complete bytes (12 bytes), a
// second hash of a page that already carries a CRC-32 in its header. With JED_MANIFEST_CRC32=on an
// entry is the page id and that CRC-32 (8 bytes): a plaintext page's entry records the checksum field
// the page already holds, so a commit hashes each page once and recovery checks the page's own
// checksum once and compares it. An encrypted page has no plaintext checksum, so its entry is the
// CRC-32/IEEE of its ciphertext. The chain digest of the manifest-overflow pages is a CRC-32 too.
//
// A meta page marks narrow entries with the top bit of its dirty-count field, so both layouts read.
// Go-only, no format version bump.

import (
	"encoding/binary"
	"hash/crc32"
	"os"
)

// manifestCRC32 makes this process write narrow manifests.
var manifestCRC32 = os.Getenv("JED_MANIFEST_CRC32") == "on"

// manifestNarrowFlag is the bit of a meta's dirty-count field (offset 40) that marks narrow entries.
const manifestNarrowFlag = uint32(1) << 31

func manifestEntrySize(narrow bool) int {
	if narrow {
		return 8
	}
	return 12
}

// entryDigest reads the digest of the manifest entry at e.
func entryDigest(e []byte, narrow bool) uint64 {
	if narrow {
		return uint64(binary.BigEndian.Uint32(e[4:]))
	}
	return binary.BigEndian.Uint64(e[4:])
}

// putManifestEntry writes one entry at b.
func putManifestEntry(b []byte, e manifestEntry, narrow bool) {
	binary.BigEndian.PutUint32(b, e.index)
	if narrow {
		binary.BigEndian.PutUint32(b[4:], uint32(e.digest))
		return
	}
	binary.BigEndian.PutUint64(b[4:], e.digest)
}

// pageDigest is the manifest identity of a body page's stored bytes.
func pageDigest(block []byte, encrypted, narrow bool) uint64 {
	switch {
	case !narrow:
		return commitCRC64(0, block)
	case encrypted:
		return uint64(crc32.ChecksumIEEE(block))
	default:
		// The page's own checksum field. The reader verifies the page against it separately.
		return uint64(binary.BigEndian.Uint32(block[12:16]))
	}
}

// chainDigest extends the digest of the manifest-overflow chain.
func chainDigest(crc uint64, b []byte, narrow bool) uint64 {
	if narrow {
		return uint64(crc32.Update(uint32(crc), crc32.IEEETable, b))
	}
	return commitCRC64(crc, b)
}
