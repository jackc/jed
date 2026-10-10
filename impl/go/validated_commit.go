package jed

// Validated COW binds each published root to its exact newly written pages. The
// metadata and body may reach storage in any order; recovery accepts the root
// only when the complete dependency set matches (fileformat/format.md, v33).

import (
	"bytes"
	"encoding/binary"
	"hash/crc32"
	"sort"
)

const pageManifest byte = 8

var commitCRC64Table = func() [256]uint64 {
	var table [256]uint64
	for i := range table {
		v := uint64(i) << 56
		for bit := 0; bit < 8; bit++ {
			if v&(uint64(1)<<63) != 0 {
				v = v<<1 ^ 0x42F0E1EBA9EA3693
			} else {
				v <<= 1
			}
		}
		table[i] = v
	}
	return table
}()

func commitCRC64(crc uint64, b []byte) uint64 {
	for _, v := range b {
		crc = crc<<8 ^ commitCRC64Table[byte(crc>>56)^v]
	}
	return crc
}

func metaCRC(b []byte) uint32 {
	crc := crc32.Update(0, crc32.IEEETable, b[:32])
	return crc32.Update(crc, crc32.IEEETable, b[36:])
}

func zeroBytes(b []byte) bool {
	for _, v := range b {
		if v != 0 {
			return false
		}
	}
	return true
}

func manifestInlineCapacity(ps int) int   { return (ps - 64) / 12 }
func manifestOverflowCapacity(ps int) int { return (ps - 32) / 12 }

// Check before serialization, which can fault inherited pages. A failed writer
// must not do more storage work; a serialization error does not start a commit.
func (p *pager) checkValidatedCommit() error {
	if p.commitRequiresReopen() {
		return newError(IoError, "database writer requires reopen after failed commit")
	}
	return nil
}

func (p *pager) commitRequiresReopen() bool { return p.poisoned || p.commitInProgress }

// Begin only after serialization succeeds, before the first reserve/write. A
// stabilization or later commit failure requires recovery through a fresh open.
func (p *pager) beginValidatedCommit() error {
	if err := p.checkValidatedCommit(); err != nil {
		return err
	}
	p.commitInProgress = true
	if p.recoverySync {
		if err := p.sync(); err != nil {
			return err
		}
		p.recoverySync = false
	}
	return nil
}

func (p *pager) finishValidatedCommit(m []byte) {
	p.validatedMeta = append(p.validatedMeta[:0], m...)
	// Rebuild protection when a shared reload next parses this locally-published root.
	p.manifestProtected = nil
	p.recoverySync = false
	p.commitInProgress = false
}

func (p *pager) selectValidatedMeta() (meta, error) {
	b0, err := p.readBlock(0)
	if err != nil {
		return meta{}, err
	}
	b1, err := p.readBlock(1)
	if err != nil {
		return meta{}, err
	}
	blocks := [][]byte{b0, b1}
	if a, ok := parseMeta(b0); ok {
		if b, ok := parseMeta(b1); ok && b.txid > a.txid {
			blocks[0], blocks[1] = b1, b0
		}
	} else {
		blocks[0], blocks[1] = b1, b0
	}
	for _, raw := range blocks {
		m, ok := parseMeta(raw)
		if !ok || m.pageCount > p.allocatedPages {
			continue
		}
		if !bytes.Equal(raw, p.validatedMeta) || p.manifestProtected == nil {
			valid, err := p.validateCommitManifest(m, raw)
			if err != nil {
				return meta{}, err
			}
			if !valid {
				continue
			}
			adopted := !bytes.Equal(raw, p.validatedMeta)
			p.validatedMeta = append(p.validatedMeta[:0], raw...)
			p.recoverySync = p.recoverySync || (adopted && m.dirtyCount != 0)
		}
		return m, nil
	}
	return meta{}, newError(DataCorrupted, "no valid meta page and commit manifest")
}

// Validation uses one body buffer and one overflow buffer at a time. Counts are
// bounded by the physical file, ids strictly increase, and chain ordinals prevent
// cycles without a file-sized visited set.
func (p *pager) validateCommitManifest(m meta, raw []byte) (bool, error) {
	protected := make(map[uint32]bool)
	last := uint32(1)
	seen := uint32(0)
	rootSeen, freeHeadSeen := false, m.freeListHead == 0
	validate := func(entries []byte, count uint32) (bool, error) {
		for i := uint32(0); i < count; i++ {
			e := entries[int(i)*12:]
			id := binary.BigEndian.Uint32(e)
			if id <= last || id >= m.pageCount {
				return false, nil
			}
			last = id
			protected[id] = true
			rootSeen = rootSeen || id == m.rootPage
			freeHeadSeen = freeHeadSeen || id == m.freeListHead
			block, err := p.readBlock(id)
			if err != nil {
				return false, err
			}
			if len(block) != int(p.pageSize) || block[0] < 1 || block[0] > 7 || pageCRC(block) != binary.BigEndian.Uint32(block[12:16]) || commitCRC64(0, block) != binary.BigEndian.Uint64(e[4:]) {
				return false, nil
			}
			seen++
		}
		return true, nil
	}
	inline := min(m.dirtyCount, uint32(manifestInlineCapacity(len(raw))))
	if ok, err := validate(raw[64:], inline); !ok || err != nil {
		return ok, err
	}
	id := m.manifestHead
	chainCRC := uint64(0)
	for ordinal := uint32(0); ordinal < m.manifestPages; ordinal++ {
		if id < 2 || id >= m.pageCount {
			return false, nil
		}
		protected[id] = true
		block, err := p.readBlock(id)
		if err != nil {
			return false, err
		}
		pg, err := parsePage(block)
		if err != nil {
			return false, nil
		}
		count := min(m.dirtyCount-seen, uint32(manifestOverflowCapacity(len(block))))
		if pg.pageType != pageManifest || !zeroBytes(block[1:4]) || pg.itemCount != count ||
			binary.BigEndian.Uint64(block[16:24]) != m.txid || binary.BigEndian.Uint32(block[24:28]) != ordinal || !zeroBytes(block[28:32]) || !zeroBytes(block[32+12*int(count):]) {
			return false, nil
		}
		var index [4]byte
		binary.BigEndian.PutUint32(index[:], id)
		chainCRC = commitCRC64(chainCRC, index[:])
		chainCRC = commitCRC64(chainCRC, block)
		if ok, err := validate(block[32:], count); !ok || err != nil {
			return ok, err
		}
		id = pg.nextPage
	}
	valid := id == 0 && seen == m.dirtyCount && chainCRC == m.manifestCRC && (m.dirtyCount == 0 || (rootSeen && freeHeadSeen))
	if valid {
		p.manifestProtected = protected
	}
	return valid, nil
}

// commitManifest encodes the fixed meta inline region and the reserved overflow
// chain. The dirty set includes free-list pages, but not the manifest itself.
func commitManifest(ps uint32, txid uint64, root, pageCount, head, livePages uint32, dirty []dirtyPage, manifestIDs []uint32) ([]byte, []dirtyPage) {
	ordered := append([]dirtyPage(nil), dirty...)
	sort.Slice(ordered, func(i, j int) bool { return ordered[i].index < ordered[j].index })
	meta := metaPage(ps, txid, root, pageCount, head, livePages)
	binary.BigEndian.PutUint32(meta[40:], uint32(len(ordered)))
	binary.BigEndian.PutUint32(meta[44:], uint32(len(manifestIDs)))
	if len(manifestIDs) > 0 {
		binary.BigEndian.PutUint32(meta[36:], manifestIDs[0])
	}
	encode := func(b []byte, records []dirtyPage) {
		for i, pg := range records {
			binary.BigEndian.PutUint32(b[i*12:], pg.index)
			binary.BigEndian.PutUint64(b[i*12+4:], commitCRC64(0, pg.bytes))
		}
	}
	inline := min(len(ordered), manifestInlineCapacity(int(ps)))
	encode(meta[64:], ordered[:inline])
	remaining := ordered[inline:]
	pages := make([]dirtyPage, 0, len(manifestIDs))
	chainCRC := uint64(0)
	for ordinal, id := range manifestIDs {
		n := min(len(remaining), manifestOverflowCapacity(int(ps)))
		payload := make([]byte, 16+n*12)
		binary.BigEndian.PutUint64(payload, txid)
		binary.BigEndian.PutUint32(payload[8:], uint32(ordinal))
		encode(payload[16:], remaining[:n])
		remaining = remaining[n:]
		next := uint32(0)
		if ordinal+1 < len(manifestIDs) {
			next = manifestIDs[ordinal+1]
		}
		block := makePage(int(ps), pageManifest, uint32(n), next, payload)
		var index [4]byte
		binary.BigEndian.PutUint32(index[:], id)
		chainCRC = commitCRC64(chainCRC, index[:])
		chainCRC = commitCRC64(chainCRC, block)
		pages = append(pages, dirtyPage{index: id, bytes: block})
	}
	binary.BigEndian.PutUint64(meta[48:], chainCRC)
	binary.BigEndian.PutUint32(meta[32:], metaCRC(meta))
	return meta, pages
}

type validatedCommitPlan struct {
	pages           []dirtyPage // free-list pages, then manifest overflow
	meta            []byte
	free            []uint32
	pageCount, live uint32
	generation      uint64
}

func planValidatedCommit(snap *snapshot, paging *sharedPaging, write incrementalWrite, pageSize, liveAtCompaction uint32, generation uint64, canReclaim, canReuse bool, livePages uint32) (validatedCommitPlan, error) {
	candidates, live, gen, err := freeListCandidates(snap, paging, write.rootPage, write.pages, write.freeRemaining, write.pageCount, liveAtCompaction, generation, canReclaim)
	if err != nil {
		return validatedCommitPlan{}, err
	}
	safe := write.freeRemaining
	if !canReuse {
		safe = nil
	}
	ps := int(pageSize)
	reserved := 0
	for {
		ids := make([]uint32, reserved)
		drawn := min(len(safe), reserved)
		copy(ids, safe[:drawn])
		next := write.pageCount
		for i := drawn; i < reserved; i++ {
			if next == ^uint32(0) {
				return validatedCommitPlan{}, newError(ProgramLimitExceeded, "database page limit exceeded")
			}
			ids[i] = next
			next++
		}
		// Reserved safe ids form a sorted prefix of safe, also sorted within candidates.
		persist := make([]uint32, 0, len(candidates))
		j := 0
		for _, id := range candidates {
			for j < drawn && ids[j] < id {
				j++
			}
			if j == drawn || ids[j] != id {
				persist = append(persist, id)
			}
		}
		fl, head, free, pc, err := serializeFreeList(persist, safe[drawn:], ps-pageHeader, ps, next)
		if err != nil {
			return validatedCommitPlan{}, err
		}
		left := max(0, len(write.pages)+len(fl)-manifestInlineCapacity(ps))
		required := (left + manifestOverflowCapacity(ps) - 1) / manifestOverflowCapacity(ps)
		if required > reserved {
			reserved = required
			continue
		}
		dirty := make([]dirtyPage, 0, len(write.pages)+len(fl))
		dirty = append(dirty, write.pages...)
		dirty = append(dirty, fl...)
		meta, overflow := commitManifest(pageSize, snap.txid, write.rootPage, pc, head, livePages, dirty, ids)
		return validatedCommitPlan{pages: append(fl, overflow...), meta: meta, free: free, pageCount: pc, live: live, generation: gen}, nil
	}
}

func planSharedValidatedCommit(ps uint32, snap *snapshot, write incrementalWrite, livePages uint32) (validatedCommitPlan, error) {
	left := max(0, len(write.pages)-manifestInlineCapacity(int(ps)))
	count := (left + manifestOverflowCapacity(int(ps)) - 1) / manifestOverflowCapacity(int(ps))
	if uint64(write.pageCount)+uint64(count) > uint64(^uint32(0)) {
		return validatedCommitPlan{}, newError(ProgramLimitExceeded, "database page limit exceeded")
	}
	ids := make([]uint32, count)
	for i := range ids {
		ids[i] = write.pageCount + uint32(i)
	}
	pc := write.pageCount + uint32(count)
	meta, pages := commitManifest(ps, snap.txid, write.rootPage, pc, 0, livePages, write.pages, ids)
	return validatedCommitPlan{pages: pages, meta: meta, pageCount: pc, generation: snap.txid}, nil
}
