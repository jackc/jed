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

// manifestInlineArea is the byte range of a meta page of ps bytes that holds its inline manifest:
// after the header, before an encrypted meta's reserve. An encrypted meta's inline manifest starts
// after the catalog-root and free-list tags (spec/fileformat/encrypted.md §6).
func manifestInlineArea(ps int, pcap pageCap) (int, int) {
	if pcap.encrypted() {
		return 96, ps - cryptoReserve
	}
	return 64, ps
}

func manifestInlineCapacity(ps int, pcap pageCap) int {
	lo, hi := manifestInlineArea(ps, pcap)
	return (hi - lo) / 12
}
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

// abandonValidatedCommit ends a commit whose meta was never written: the pages it wrote are unreachable
// orphans of the committed state (beyond its high-water or in its free list), so the handle stays
// usable. Only the page-service owner calls it, for a client delta it rejected or aborted.
func (p *pager) abandonValidatedCommit() {
	p.commitInProgress = false
}

// finishValidatedCommit records a commit whose meta m was written to slot. durable is whether its
// barrier ran (synchronous = on, or a page client's commit): the meta is then the floor and nothing is
// pending. A relaxed commit instead adds its own dirty entries to the pending set (validated-cow.md §4).
func (p *pager) finishValidatedCommit(m []byte, slot uint32, durable bool, dirty []manifestEntry) {
	p.validatedMeta = append(p.validatedMeta[:0], m...)
	// Rebuild protection when a shared reload next parses this locally-published root.
	p.manifestProtected = nil
	p.recoverySync = false
	p.commitInProgress = false
	if durable {
		p.floorSlot = slot
		p.pending = nil
		return
	}
	if p.pending == nil {
		p.pending = make(map[uint32]uint64, len(dirty))
	}
	for _, e := range dirty {
		p.pending[e.index] = e.digest
	}
}

// commitSlot is the meta slot the next commit writes: the one not holding the floor (format.md, *Slot
// alternation*). Under synchronous = on this is txid & 1.
func (p *pager) commitSlot() uint32 { return p.floorSlot ^ 1 }

// pendingEntries is the pending set in page order (validated-cow.md §4).
func (p *pager) pendingEntries() []manifestEntry {
	entries := make([]manifestEntry, 0, len(p.pending))
	for id, digest := range p.pending {
		entries = append(entries, manifestEntry{index: id, digest: digest})
	}
	sort.Slice(entries, func(i, j int) bool { return entries[i].index < entries[j].index })
	return entries
}

// flushPending is a flush (validated-cow.md §4): one durable barrier makes every pending commit
// durable, so the newest meta — in the non-floor slot — becomes the floor. A failed barrier poisons the
// pager (sync), so the next commit and close report it. shrink lets a flush that follows a lowering
// commit shorten the file (settleTruncation); a page client's begin_write flush passes false and leaves
// the shrink to the owner's next barrier (format.md *Trailing truncation*).
func (p *pager) flushPending(shrink bool) error {
	if len(p.pending) == 0 {
		return nil
	}
	if err := p.sync(); err != nil {
		return err
	}
	p.floorSlot ^= 1
	p.pending = nil
	if shrink {
		p.shrinkAfterBarrier()
	}
	return nil
}

// batchCapReached reports whether the pending set fills the inline manifest of one meta page: the
// batch cap, after which a flush follows (validated-cow.md §4).
func (p *pager) batchCapReached() bool {
	return len(p.pending) >= manifestInlineCapacity(int(p.pageSize), capOf(p.pageSize, p.codec != nil))
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
	slots := []uint32{0, 1}
	if a, ok := parseMeta(b0); ok {
		if b, ok := parseMeta(b1); ok && b.txid > a.txid {
			blocks[0], blocks[1] = b1, b0
			slots[0], slots[1] = 1, 0
		}
	} else {
		blocks[0], blocks[1] = b1, b0
		slots[0], slots[1] = 1, 0
	}
	for k, raw := range blocks {
		m, ok := parseMeta(raw)
		if !ok || m.pageCount > p.allocatedPages {
			continue
		}
		// A slot of the other variant is not a candidate of this file (encrypted.md §6; the unlock
		// already refused files whose valid slots disagree).
		if (m.suite != 0) != (p.codec != nil) {
			continue
		}
		if p.trustMeta {
			// A page client is served the meta without its slot; it authenticates under either slot
			// index (spec/fileformat/encrypted.md §14). XX001 when it does not.
			if p.codec != nil {
				if err := p.verifyServedMeta(m, raw); err != nil {
					return meta{}, err
				}
			}
			p.validatedMeta = append(p.validatedMeta[:0], raw...)
			p.manifestProtected = map[uint32]bool{}
			return m, nil
		}
		if !bytes.Equal(raw, p.validatedMeta) || p.manifestProtected == nil {
			valid, err := p.validateCommitManifest(m, raw)
			if err != nil {
				return meta{}, err
			}
			if !valid {
				continue
			}
			// An encrypted meta is authenticated after keyless selection; a CRC-valid meta that
			// fails its tag was tampered with — XX001, never a fallback.
			if p.codec != nil {
				chain, err := p.manifestChain(m)
				if err != nil {
					return meta{}, err
				}
				if err := p.codec.verifyMeta(slots[k], raw, chain); err != nil {
					p.manifestProtected = nil
					return meta{}, err
				}
			}
			adopted := !bytes.Equal(raw, p.validatedMeta)
			p.validatedMeta = append(p.validatedMeta[:0], raw...)
			p.recoverySync = p.recoverySync || (adopted && m.dirtyCount != 0)
			if adopted {
				// The adopted generation is the floor once stabilized (validated-cow.md §3–§4). When both
				// slots carry its txid (create, compaction), the floor slot is txid & 1, so the next commit
				// writes the other slot exactly as txid parity would.
				p.floorSlot = slots[k]
				if other, ok := parseMeta(blocks[1-k]); ok && other.txid == m.txid {
					p.floorSlot = uint32(m.txid & 1)
				}
				p.pending = nil
			}
		}
		return m, nil
	}
	return meta{}, newError(DataCorrupted, "no valid meta page and commit manifest")
}

// Validation uses one body buffer and one overflow buffer at a time. Counts are
// bounded by the physical file, ids strictly increase, and chain ordinals prevent
// cycles without a file-sized visited set.
func (p *pager) validateCommitManifest(m meta, raw []byte) (bool, error) {
	// An encrypted meta: body pages start past the key block, the inline manifest after the tags, and
	// a listed page is ciphertext, so only its digest is checked (encrypted.md §6).
	pcap := capOf(uint32(len(raw)), m.suite != 0)
	first := pcap.firstBody()
	protected := make(map[uint32]bool)
	last := first - 1
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
			if len(block) != int(p.pageSize) || commitCRC64(0, block) != binary.BigEndian.Uint64(e[4:]) {
				return false, nil
			}
			if !pcap.encrypted() && (block[0] < 1 || block[0] > 7 || pageCRC(block) != binary.BigEndian.Uint32(block[12:16])) {
				return false, nil
			}
			seen++
		}
		return true, nil
	}
	lo, _ := manifestInlineArea(len(raw), pcap)
	inline := min(m.dirtyCount, uint32(manifestInlineCapacity(len(raw), pcap)))
	if ok, err := validate(raw[lo:], inline); !ok || err != nil {
		return ok, err
	}
	id := m.manifestHead
	chainCRC := uint64(0)
	for ordinal := uint32(0); ordinal < m.manifestPages; ordinal++ {
		if id < first || id >= m.pageCount {
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

// manifestChain is the manifest-overflow chain a valid meta names, in chain order — what an encrypted
// meta's tag covers (encrypted.md §6). Reads only the pages the meta counts; a malformed chain was
// already refused by validateCommitManifest.
// verifyServedMeta authenticates a served meta under the pager's codec for either slot index
// (spec/fileformat/encrypted.md §14): XX001 when neither tag checks.
func (p *pager) verifyServedMeta(m meta, raw []byte) error {
	chain, err := p.manifestChain(m)
	if err != nil {
		return err
	}
	if err := p.codec.verifyMeta(0, raw, chain); err != nil {
		return p.codec.verifyMeta(1, raw, chain)
	}
	return nil
}

func (p *pager) manifestChain(m meta) ([]dirtyPage, error) {
	var out []dirtyPage
	id := m.manifestHead
	for i := uint32(0); i < m.manifestPages; i++ {
		block, err := p.readBlock(id)
		if err != nil {
			return nil, err
		}
		out = append(out, dirtyPage{index: id, bytes: block})
		id = binary.BigEndian.Uint32(block[8:12])
	}
	return out, nil
}

// manifestEntry is one manifest record: a page id and the CRC-64 of its complete encoded bytes.
type manifestEntry struct {
	index  uint32
	digest uint64
}

// manifestEntriesOf digests written pages for a manifest.
func manifestEntriesOf(pages []dirtyPage) []manifestEntry {
	entries := make([]manifestEntry, len(pages))
	for i, pg := range pages {
		entries[i] = manifestEntry{index: pg.index, digest: commitCRC64(0, pg.bytes)}
	}
	return entries
}

// commitManifest encodes the fixed meta inline region and the reserved overflow
// chain. The entries include free-list pages, but not the manifest itself; under
// batched durability they also include the pending set (validated-cow.md §4).
func commitManifest(pcap pageCap, ps uint32, txid uint64, root pageRef, pageCount uint32, head pageRef, livePages uint32, entries []manifestEntry, manifestIDs []uint32) ([]byte, []dirtyPage) {
	ordered := append([]manifestEntry(nil), entries...)
	sort.Slice(ordered, func(i, j int) bool { return ordered[i].index < ordered[j].index })
	meta := metaPage(pcap, ps, txid, root, pageCount, head, livePages)
	binary.BigEndian.PutUint32(meta[40:], uint32(len(ordered)))
	binary.BigEndian.PutUint32(meta[44:], uint32(len(manifestIDs)))
	if len(manifestIDs) > 0 {
		binary.BigEndian.PutUint32(meta[36:], manifestIDs[0])
	}
	encode := func(b []byte, records []manifestEntry) {
		for i, e := range records {
			binary.BigEndian.PutUint32(b[i*12:], e.index)
			binary.BigEndian.PutUint64(b[i*12+4:], e.digest)
		}
	}
	lo, _ := manifestInlineArea(int(ps), pcap)
	inline := min(len(ordered), manifestInlineCapacity(int(ps), pcap))
	encode(meta[lo:], ordered[:inline])
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

// trailingRun splits the sorted safe free pages at the start k of their trailing run below the
// high-water: every page in [k, high) is in safe (format.md *Trailing truncation*). It returns the
// pages below k and k itself; k == high when page high−1 is not safe.
func trailingRun(safe []uint32, high uint32) ([]uint32, uint32) {
	k := high
	n := len(safe)
	for n > 0 && safe[n-1] == k-1 {
		n--
		k--
	}
	return safe[:n], k
}

type validatedCommitPlan struct {
	pages []dirtyPage // free-list pages, then manifest overflow
	// manifest is the manifest-overflow pages among pages, in chain order — what an encrypted meta's
	// tag authenticates (sealMeta).
	manifest []dirtyPage
	// meta is the meta page, unsealed: sealMeta completes it for the slot it is written to.
	meta            []byte
	free            []uint32
	pageCount, live uint32
	generation      uint64
	// dirty is the commit's own manifest entries (its body and free-list pages), which a relaxed commit
	// adds to the pending set (validated-cow.md §4).
	dirty []manifestEntry
}

// planValidatedCommit plans a file commit's free-list and manifest pages and its meta. pending is the
// batch's pending set (nil under synchronous = on): its pages join the manifest and stay off the free
// list (validated-cow.md §4). truncate lets the plan lower the high-water past the trailing run of safe
// free pages (format.md *Trailing truncation*): only an owner's (or an alone process's) own commit that
// plans against a durable floor — nothing pending — sets it, never a page client's planned delta.
func planValidatedCommit(snap *snapshot, paging *sharedPaging, write incrementalWrite, pageSize, liveAtCompaction uint32, generation uint64, canReclaim, canReuse, truncate bool, livePages uint32, pending []manifestEntry, allowed map[uint32]bool) (validatedCommitPlan, error) {
	candidates, live, gen, err := freeListCandidates(snap, paging, write.root, write.pages, write.freeRemaining, write.pageCount, liveAtCompaction, generation, canReclaim, pending)
	if err != nil {
		return validatedCommitPlan{}, err
	}
	safe := write.freeRemaining
	if !canReuse {
		safe = nil
	} else if allowed != nil {
		// A pending batch reuses only pages free at its floor (validated-cow.md §4 rule 3).
		filtered := make([]uint32, 0, len(safe))
		for _, id := range safe {
			if allowed[id] {
				filtered = append(filtered, id)
			}
		}
		safe = filtered
	}
	base := write.pageCount
	if truncate {
		var k uint32
		safe, k = trailingRun(safe, base)
		if k < base {
			base = k
			cut := len(candidates)
			for cut > 0 && candidates[cut-1] >= k {
				cut--
			}
			candidates = candidates[:cut]
		}
	}
	ps := int(pageSize)
	reserved := 0
	for {
		ids := make([]uint32, reserved)
		drawn := min(len(safe), reserved)
		copy(ids, safe[:drawn])
		next := base
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
		seal := sealerOf(ps, paging)
		fl, head, free, pc, err := serializeFreeList(persist, safe[drawn:], seal, next)
		if err != nil {
			return validatedCommitPlan{}, err
		}
		left := max(0, len(pending)+len(write.pages)+len(fl)-manifestInlineCapacity(ps, seal.cap))
		required := (left + manifestOverflowCapacity(ps) - 1) / manifestOverflowCapacity(ps)
		if required > reserved {
			reserved = required
			continue
		}
		dirty := make([]manifestEntry, 0, len(write.pages)+len(fl))
		dirty = append(dirty, manifestEntriesOf(write.pages)...)
		dirty = append(dirty, manifestEntriesOf(fl)...)
		entries := append(append(make([]manifestEntry, 0, len(pending)+len(dirty)), pending...), dirty...)
		meta, overflow := commitManifest(seal.cap, pageSize, snap.txid, write.root, pc, head, livePages, entries, ids)
		return validatedCommitPlan{pages: append(fl, overflow...), manifest: overflow, meta: meta, free: free, pageCount: pc, live: live, generation: gen, dirty: dirty}, nil
	}
}

// sealMeta completes a commit's meta for slot (spec/fileformat/encrypted.md §6): an encrypted file
// seals a copy over its manifest-overflow chain; a plaintext meta is final as planned. Draws one nonce.
func sealMeta(p *pager, slot uint32, meta []byte, manifest []dirtyPage) ([]byte, error) {
	if p.codec == nil {
		return meta, nil
	}
	sealed := append([]byte(nil), meta...)
	if err := p.codec.sealMeta(slot, sealed, manifest); err != nil {
		return nil, err
	}
	return sealed, nil
}
