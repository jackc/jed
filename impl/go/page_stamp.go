package jed

// PROTOTYPE (spec/design/manifest-crc32.md §2.1): the stale-page stamp.
//
// Narrowing a manifest entry to the page's CRC-32 (manifest_crc32.go) drops the strength with which
// recovery rejects a page a lost write left behind from 64 bits to 32. The stamp is the second,
// independent detector the maintainer ratified instead of a wider hash: page header offset 2, two
// bytes reserved and zero through v52, carries the low 16 bits of the txid of the commit that wrote
// the page. It sits inside the region the page's own CRC-32 already covers, so it costs no entry
// bytes and no second pass over the page -- which is the whole of the CPU win.
//
// A stale page carries the stamp of the commit that really wrote it, so it fails an equality check
// outright rather than at 2^-32, unless it is from a commit exactly 65536*k earlier.
//
// The check belongs to manifest validation ONLY. An ordinary page read is validated by its CRC-32,
// as today. That scoping is a precondition of deferred interiors (deferred-interior.md §9.4): a
// retained interior is never written and never listed, so it keeps an older stamp, and a stamp
// checked on every page read would make retention impossible. The same holds for a shared overflow
// chain (large-values.md §15) and a kept catalog chain.
//
// Go-only, no format version bump: v53 lands this together with the narrow manifest and the remap
// region, and only then is the stamp unconditional.

import (
	"encoding/binary"
	"os"
)

// pageTxidStamp makes this process stamp the pages it writes.
var pageTxidStamp = os.Getenv("JED_PAGE_TXID_STAMP") == "on"

// stampOf is the stamp a commit at txid writes into its pages, or 0 when the prototype is off (which
// is the v52 reserved value, so an unstamped page is byte-identical to one today).
func stampOf(txid uint64) uint16 {
	if !pageTxidStamp {
		return 0
	}
	return uint16(txid)
}

// stampWindow is the commit window a listed page's stamp must fall in: the half-open (floor, newest]
// of txids whose pages a manifest may list. Under synchronous = on a manifest describes one commit
// and the window is the single txid; under synchronous = off a cumulative manifest lists every page
// written since the durability floor (validated-cow.md §4), each page carrying the stamp of its own
// commit.
type stampWindow struct {
	floor  uint64 // the newest durable txid; pages at this txid or older are not this batch's
	newest uint64 // the txid of the meta being validated
}

// contains reports whether stamp is the low 16 bits of some txid in (w.floor, w.newest].
//
// A batch is bounded by the inline manifest capacity -- at most 1,016 commits, far below 2^16 -- so
// the window's low-16 values are distinct and this check is exact rather than approximate. A window
// wider than 2^16 cannot distinguish them, so it admits every stamp rather than reporting a false
// mismatch; a window that is empty or inverted does the same, leaving the CRC-32 as the sole check.
func (w stampWindow) contains(stamp uint16) bool {
	if !pageTxidStamp {
		return true
	}
	if w.newest <= w.floor {
		return true
	}
	span := w.newest - w.floor
	if span >= 1<<16 {
		return true
	}
	d := uint64(stamp - uint16(w.floor))
	return d >= 1 && d <= span
}

// manifestPageHeaderOK reports whether a manifest-overflow page's header bytes 1-3 are what the commit
// at txid wrote: flags zero and the commit's own stamp. A manifest page is always written by the
// commit whose meta names it -- under synchronous = off too, since each commit writes its own
// manifest -- so this is an exact equality, stronger than the window. With the stamp off it is the
// v52 rule that bytes 1-3 are zero.
func manifestPageHeaderOK(block []byte, txid uint64) bool {
	return block[1] == 0 && binary.BigEndian.Uint16(block[2:4]) == stampOf(txid)
}
