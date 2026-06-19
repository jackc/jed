package jed

import "encoding/binary"

// UUID bit-level operations (spec/design/functions.md §12). Value<->text rendering/parsing
// lives in value.go; this is the SEMANTIC bit work — extracting the version and embedded
// timestamp from the 16 raw big-endian bytes (byte 0 is the most-significant), plus the pure
// generator byte builders (buildUUIDv4/buildUUIDv7 below). The PRNG draws + clock for the
// generators live on the entropy+clock seam (seam.go); the functions here are PURE —
// deterministic functions of their input bytes.

// gregorianOffset100ns is the number of 100-ns intervals between the Gregorian UUID epoch
// (1582-10-15 00:00:00 UTC) and the Unix epoch (1970-01-01 00:00:00 UTC) — the v1/v6
// timestamp base (= 0x01B21DD213814000).
const gregorianOffset100ns int64 = 122_192_928_000_000_000

// uuidIsRFC4122 reports whether the value carries the RFC 4122 variant (top two bits of byte 8
// are 10). Microsoft GUIDs (11), the legacy NCS variant (0), and the nil UUID (all zero) are not.
func uuidIsRFC4122(b []byte) bool { return b[8]&0xC0 == 0x80 }

// uuidExtractVersion returns the version nibble (high nibble of byte 6), 0..15, and true for an
// RFC 4122 UUID; false off-variant. Matches PostgreSQL 18 uuid_extract_version.
func uuidExtractVersion(b []byte) (int64, bool) {
	if !uuidIsRFC4122(b) {
		return 0, false
	}
	return int64((b[6] >> 4) & 0x0F), true
}

// uuidExtractTimestampMicros returns the embedded instant as microseconds since the Unix epoch
// (a timestamptz), and true, for an RFC 4122 UUID of VERSION 1 or 7 only; false otherwise.
// Matches PostgreSQL 18 uuid_extract_timestamp (v1/v7 only — v6 returns NULL there).
func uuidExtractTimestampMicros(b []byte) (int64, bool) {
	if !uuidIsRFC4122(b) {
		return 0, false
	}
	switch (b[6] >> 4) & 0x0F {
	case 7:
		return uuidV7Micros(b), true
	case 1:
		return uuidV1Micros(b), true
	default:
		return 0, false
	}
}

// uuidV7Micros reads the 48-bit big-endian Unix-millisecond field (bytes 0..5); micros = ms *
// 1000. A 48-bit ms times 1000 stays well within i64, so this cannot overflow.
func uuidV7Micros(b []byte) int64 {
	ms := int64(b[0])<<40 | int64(b[1])<<32 | int64(b[2])<<24 |
		int64(b[3])<<16 | int64(b[4])<<8 | int64(b[5])
	return ms * 1000
}

// uuidV1Micros reassembles the 60-bit Gregorian 100-ns count from time_low (bytes 0..3),
// time_mid (bytes 4..5), and time_hi (the low 12 bits of bytes 6..7), subtracts the 1582→1970
// epoch offset, then truncates 100-ns ticks to microseconds (toward zero — PG drops the
// sub-microsecond remainder).
func uuidV1Micros(b []byte) int64 {
	timeLow := uint64(binary.BigEndian.Uint32(b[0:4]))
	timeMid := uint64(binary.BigEndian.Uint16(b[4:6]))
	timeHi := uint64(binary.BigEndian.Uint16(b[6:8]) & 0x0FFF)
	ticks := timeHi<<48 | timeMid<<32 | timeLow
	unix100ns := int64(ticks) - gregorianOffset100ns
	return unix100ns / 10
}

// --- generator byte builders (spec/design/entropy.md §3) ---------------------
// Pure assembly of the 16 bytes from already-drawn random bytes (and, for v7, the timestamp +
// monotonic counter). The PRNG draws + clock resolution live on StmtRng (seam.go).

// buildUUIDv4 sets the version (4) and RFC 4122 variant over 16 random bytes (in place).
func buildUUIDv4(b []byte) []byte {
	b[6] = (b[6] & 0x0F) | 0x40 // version 4
	b[8] = (b[8] & 0x3F) | 0x80 // RFC 4122 variant
	return b
}

// buildUUIDv7 assembles a 48-bit big-endian Unix-millisecond timestamp (bytes 0..5), a 12-bit
// monotonic counter in rand_a (bytes 6..7 low, RFC 9562 Method 1), and 8 rand_b bytes (8..15),
// with the version (7) and variant overwritten.
func buildUUIDv7(unixMs uint64, counter uint16, randB [8]byte) []byte {
	b := make([]byte, 16)
	b[0] = byte(unixMs >> 40)
	b[1] = byte(unixMs >> 32)
	b[2] = byte(unixMs >> 24)
	b[3] = byte(unixMs >> 16)
	b[4] = byte(unixMs >> 8)
	b[5] = byte(unixMs)
	randA := counter & 0x0FFF
	b[6] = 0x70 | byte((randA>>8)&0x0F) // version 7 + rand_a high nibble
	b[7] = byte(randA & 0xFF)           // rand_a low byte
	copy(b[8:16], randB[:])
	b[8] = (b[8] & 0x3F) | 0x80 // RFC 4122 variant
	return b
}
