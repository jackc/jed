package jed

// The owner's validation of an incoming CommitDelta (spec/design/page-service.md §5.6;
// spec/pageservice/README.md §3.5). It is structural and cheap: it catches client bugs, not malice
// (page-service.md §13). Every check runs before the page it guards is written, so a rejected delta
// leaves only unreachable orphans — pages beyond the committed high-water or in the granted free list —
// and never touches a page any pinned snapshot or the fallback meta can reach.

import (
	"encoding/binary"
	"sort"
	"strconv"
)

// psDelta is one incoming CommitDelta after commit_begin passed: the decoded meta and manifest, and
// which listed pages have arrived.
type psDelta struct {
	txid uint64
	meta []byte
	m    meta
	// expected maps every index the manifest lists to the CRC-64 of its bytes.
	expected map[uint32]uint64
	received map[uint32]bool
	// manifest is the overflow chain, in chain order; it arrives whole in commit_begin.
	manifest []psIndexedPage
	// protected is every index the commit depends on (listed and manifest pages): the pager's
	// manifestProtected once the commit is published, so the owner's reload need not re-read them.
	protected map[uint32]bool
	// written is every index the delta writes, in ascending order: the invalidation window's entry.
	written []uint32
}

func errDelta(reason string) error {
	return newError(ProtocolViolation, "invalid commit delta: "+reason)
}

// psDeltaBase is the owner state a delta is checked against, fixed at begin_write.
type psDeltaBase struct {
	committedTxid uint64
	pageSize      uint32
	pageCount     uint32          // the committed high-water
	granted       map[uint32]bool // the reusable free pages when the reuse grant passed; nil otherwise
}

// legal reports whether a delta may write index: a body page beyond the committed high-water or a
// granted free page, below the new page count.
func (b psDeltaBase) legal(index, newPageCount uint32) bool {
	return index >= 2 && index < newPageCount && (index >= b.pageCount || b.granted[index])
}

// beginDelta runs the checks before any page is written (README.md §3.5 check A): the meta, then the
// manifest chain and every listed index.
func beginDelta(base psDeltaBase, cb psCommitBegin) (*psDelta, error) {
	if cb.txid != base.committedTxid+1 {
		return nil, errDelta("txid " + strconv.FormatUint(cb.txid, 10) + " does not follow the committed txid " + strconv.FormatUint(base.committedTxid, 10))
	}
	if len(cb.meta) != int(base.pageSize) {
		return nil, errDelta("the meta page has the wrong size")
	}
	m, ok := parseMeta(cb.meta)
	if !ok {
		return nil, errDelta("the meta page does not parse")
	}
	if m.txid != cb.txid {
		return nil, errDelta("the meta page names another txid")
	}
	if m.pageCount < base.pageCount {
		return nil, errDelta("the page count shrinks")
	}
	if m.dirtyCount == 0 {
		return nil, errDelta("the manifest lists no page")
	}
	ps := int(base.pageSize)
	d := &psDelta{
		txid:      cb.txid,
		meta:      append([]byte(nil), cb.meta...),
		m:         m,
		expected:  make(map[uint32]uint64, m.dirtyCount),
		received:  make(map[uint32]bool, m.dirtyCount),
		protected: make(map[uint32]bool, int(m.dirtyCount)+len(cb.manifestPages)),
	}
	if len(cb.manifestPages) != int(m.manifestPages) {
		return nil, errDelta("commit_begin carries the wrong number of manifest pages")
	}
	last := uint32(1)
	rootSeen, freeHeadSeen := false, m.freeListHead == 0
	listed := uint32(0)
	list := func(entries []byte, count uint32) error {
		for i := uint32(0); i < count; i++ {
			e := entries[int(i)*12:]
			index := binary.BigEndian.Uint32(e)
			if index <= last {
				return errDelta("manifest indices are not strictly ascending")
			}
			if !base.legal(index, m.pageCount) {
				return errDelta("page " + strconv.FormatUint(uint64(index), 10) + " is neither beyond the high-water nor a granted free page")
			}
			last = index
			d.expected[index] = binary.BigEndian.Uint64(e[4:])
			d.protected[index] = true
			rootSeen = rootSeen || index == m.rootPage
			freeHeadSeen = freeHeadSeen || index == m.freeListHead
			listed++
		}
		return nil
	}
	// The page service serves plaintext files only (spec/fileformat/encrypted.md §10).
	if err := list(cb.meta[64:], min(m.dirtyCount, uint32(manifestInlineCapacity(ps, plainCap(uint32(ps)))))); err != nil {
		return nil, err
	}
	id := m.manifestHead
	chainCRC := uint64(0)
	for ordinal, pg := range cb.manifestPages {
		if pg.index != id || !base.legal(id, m.pageCount) || d.protected[id] {
			return nil, errDelta("the manifest chain names an illegal page")
		}
		block := pg.page
		parsed, err := parsePage(block)
		count := min(m.dirtyCount-listed, uint32(manifestOverflowCapacity(ps)))
		if err != nil || pageCRC(block) != binary.BigEndian.Uint32(block[12:16]) || parsed.pageType != pageManifest ||
			!zeroBytes(block[1:4]) || parsed.itemCount != count || binary.BigEndian.Uint64(block[16:24]) != m.txid ||
			binary.BigEndian.Uint32(block[24:28]) != uint32(ordinal) || !zeroBytes(block[28:32]) || !zeroBytes(block[32+12*int(count):]) {
			return nil, errDelta("a manifest page is malformed")
		}
		var index [4]byte
		binary.BigEndian.PutUint32(index[:], id)
		chainCRC = commitCRC64(chainCRC, index[:])
		chainCRC = commitCRC64(chainCRC, block)
		d.protected[id] = true
		if err := list(block[32:], count); err != nil {
			return nil, err
		}
		d.manifest = append(d.manifest, psIndexedPage{index: id, page: append([]byte(nil), block...)})
		id = parsed.nextPage
	}
	if id != 0 || chainCRC != m.manifestCRC {
		return nil, errDelta("the manifest chain does not match the meta")
	}
	if listed != m.dirtyCount || !rootSeen || !freeHeadSeen {
		return nil, errDelta("the manifest does not cover the catalog root and free-list head")
	}
	for index := range d.protected {
		d.written = append(d.written, index)
	}
	sortUint32s(d.written)
	return d, nil
}

// checkPage runs check B before one body page is written: it is listed, not yet received, and
// matches its manifest identity.
func (d *psDelta) checkPage(pg psIndexedPage) error {
	crc, ok := d.expected[pg.index]
	if !ok {
		return errDelta("page " + strconv.FormatUint(uint64(pg.index), 10) + " is not listed in the manifest")
	}
	if d.received[pg.index] {
		return errDelta("page " + strconv.FormatUint(uint64(pg.index), 10) + " arrived twice")
	}
	b := pg.page
	if b[0] < 1 || b[0] > 7 || pageCRC(b) != binary.BigEndian.Uint32(b[12:16]) || commitCRC64(0, b) != crc {
		return errDelta("page " + strconv.FormatUint(uint64(pg.index), 10) + " does not match its manifest identity")
	}
	d.received[pg.index] = true
	return nil
}

// complete runs check C at commit_end: every listed page arrived.
func (d *psDelta) complete() error {
	if len(d.received) != len(d.expected) {
		return errDelta("commit_end before every listed page arrived")
	}
	return nil
}

func sortUint32s(v []uint32) { sort.Slice(v, func(i, j int) bool { return v[i] < v[j] }) }
