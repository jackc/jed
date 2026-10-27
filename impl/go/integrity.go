package jed

// The integrity check (spec/design/integrity-check.md): a host-API walk of one database's committed
// generation that reports what is wrong with its storage as structured findings, in the contract order
// of §4. The walk reads the pinned generation's pages through the handle's pager, marks every page it
// reaches in one reachability bitmap, and checks the invariants between pages that no checksum covers.
// spec/fileformat/integrity.rb is the reference: finding order, containment, and the pages/rows
// counters follow it exactly.
//
// Memory is bounded independent of the row count (§5.4, CLAUDE.md §9): one bitmap of page_count bits,
// one root-to-leaf path per tree walk, the derived entries of one row, and the catalog chain. Index
// agreement re-walks the table and probes the index once per derived entry through the index's own
// lookup, counting the index's entries during its structural walk, so neither side is materialized.

import (
	"bytes"
	"context"
	"encoding/binary"
	"errors"
	"fmt"
	"runtime"
	"sort"
)

// IntegrityLevel selects how much an integrity check covers (spec/design/integrity-check.md §3).
type IntegrityLevel int

const (
	// IntegrityFull is every check of IntegrityQuick plus agreement between each index and its table
	// (§3.4). The zero value, so the default.
	IntegrityFull IntegrityLevel = iota
	// IntegrityQuick checks the generation, every tree and page, and the space accounting, but not
	// index agreement (SQLite's quick_check boundary).
	IntegrityQuick
)

// IntegrityUnlimited is the IntegrityCheckOptions.MaxFindings value that sets no limit.
const IntegrityUnlimited = -1

// defaultIntegrityMaxFindings is the finding limit of the zero options (§2; SQLite's limit too).
const defaultIntegrityMaxFindings = 100

// IntegrityCheckOptions configures Database.IntegrityCheck. The zero value is the thorough default:
// a full check that stops after 100 findings.
type IntegrityCheckOptions struct {
	// Level is IntegrityFull (the zero value) or IntegrityQuick.
	Level IntegrityLevel
	// MaxFindings stops the check after this many findings, setting IntegrityReport.Truncated. Zero is
	// the default limit of 100; a negative value (IntegrityUnlimited) sets no limit. (The design's
	// max_findings spells "no limit" as 0; Go keeps the zero value for the default.)
	MaxFindings int
}

// IntegrityFinding is one thing wrong with a database (§2). Every field but Detail is part of the
// cross-core contract.
type IntegrityFinding struct {
	// Kind is a registered finding kind (spec/integrity/findings.toml; the Integrity* constants).
	Kind string
	// Database is the name the check was called with.
	Database string
	// Object is the table, index, or sequence the finding is about; "" when there is none.
	Object string
	// Page is the page the finding is about, or nil.
	Page *uint32
	// Key is the row's storage key when the finding is about one row, else nil.
	Key []byte
	// Detail is a human-readable explanation; not part of the contract.
	Detail string
}

// IntegritySkip is an object a requested check could not cover, and why (§3.5).
type IntegritySkip struct {
	// Object is the index (or table) the check skipped.
	Object string
	// Reason is a registered skip reason (the IntegritySkip* constants).
	Reason string
}

// IntegrityReport is the result of an integrity check (§2).
type IntegrityReport struct {
	// Findings in the contract order of §4; empty when the check found nothing.
	Findings []IntegrityFinding
	// Truncated is set when MaxFindings was reached: more findings may exist.
	Truncated bool
	// Skipped lists the checks that could not run on an object.
	Skipped []IntegritySkip
	// Pages is the distinct pages the walk reached, plus the free-list pages it read.
	Pages uint64
	// Rows is the records the walk visited in table leaves.
	Rows uint64
}

// OK reports whether the check found nothing: no findings and not truncated.
func (r IntegrityReport) OK() bool { return len(r.Findings) == 0 && !r.Truncated }

// IntegrityCheck checks the storage of database name — `main` or an attachment, exact name — and
// returns what it finds as structured findings (spec/design/integrity-check.md). It pins the
// database's committed snapshot as a read session does and never takes the writer gate: commits
// proceed meanwhile and the check does not see them. It verifies the pinned generation's meta,
// manifest, and catalog; walks every table, index, and overflow chain, checking page verification,
// node types, tree shape, key order, stored values, primary keys, and row counts; checks the free list
// and the live page count; and, at IntegrityFull, that each index (B-tree, expression, partial, GIN,
// GiST) holds exactly the entries the write path derives from its table's rows. Pages come through
// the handle's pager, so a page cached since it was verified is not re-read; a host that wants every
// byte read from storage checks a freshly opened handle. It never writes.
//
// A damaged page is a finding (XX001 while reading never fails the call). The call fails only for
// 42704 when name is not `main` or an attachment (`temp` included), 57014 when ctx is canceled (polled
// at every page and row; a canceled check returns no report), and an I/O error reading the store. A
// host-API act — not metered and not reachable from SQL.
func (db *Database) IntegrityCheck(ctx context.Context, name string, opts IntegrityCheckOptions) (IntegrityReport, error) {
	if ctx == nil {
		ctx = context.Background()
	}
	if err := ctx.Err(); err != nil {
		return IntegrityReport{}, errIntegrityCanceled()
	}
	c := db.core
	var att *attachment
	if name != "main" {
		att = c.attachment(name)
		if att == nil || name == "temp" {
			return IntegrityReport{}, newError(UndefinedObject, `database "`+name+`" is not attached`)
		}
		if att.svc != nil {
			// A file attachment under the page service is checked through its own handle, as an
			// ordinary read of it pins there (page-service.md §5.5).
			report, err := att.svc.IntegrityCheck(ctx, "main", opts)
			for i := range report.Findings {
				report.Findings[i].Database = name
			}
			return report, err
		}
	}
	pin, err := c.integrityPin(ctx, name, att)
	if err != nil {
		return IntegrityReport{}, err
	}
	defer pin.release()
	k := newIntegrityChecker(ctx, name, opts, pin.snap, pin.paging, pin.gen)
	return k.run(func() error { return k.checkLiveGeneration(pin) })
}

func errIntegrityCanceled() error {
	return newError(QueryCanceled, "canceling the integrity check due to user request")
}

// integrityGeneration is the generation a check examines: its meta, whether it is a file (with a
// persisted free-list chain and live count), and the pages its commit manifest depends on.
type integrityGeneration struct {
	meta meta
	// file is a file domain: its free list is a persisted chain and its meta records the live count.
	// An in-memory domain keeps its free list in RAM (memFree) and records no live count.
	file      bool
	protected map[uint32]bool
	memFree   []uint32
}

// integrityPin is a check's pinned snapshot with the generation descriptor captured beside it.
type integrityPin struct {
	snap    *snapshot
	paging  *sharedPaging
	gen     integrityGeneration
	release func()
	// metaRaw is the generation's meta as the handle validated or wrote it; slotRaw the meta slots as
	// stored when the pin was taken (nil when not re-read: memory, or a page client), and slot the slot
	// the generation's meta is expected in.
	metaRaw []byte
	slotRaw [2][]byte
	slot    uint32
	reread  bool
}

// integrityPin pins database name's committed snapshot (main through a read begin, an attachment
// beside main's pin) and captures its generation from the storage: a file's validated meta, or an
// in-memory domain's last adopted catalog root and RAM free list. A commit that lands between the pin
// and the capture makes them disagree; the pin is then retaken.
func (c *sharedCore) integrityPin(ctx context.Context, name string, att *attachment) (*integrityPin, error) {
	for attempt := 0; ; attempt++ {
		if ctx.Err() != nil {
			return nil, errIntegrityCanceled()
		}
		var rt *roots
		var v, gen uint64
		st := c.storage
		if att == nil {
			var err error
			if rt, v, gen, err = c.refreshAndPin(); err != nil {
				return nil, err
			}
		} else {
			rt, v, gen = c.pinLatest()
			st = att.storage
		}
		release := func() { c.deregisterPin(gen, v) }
		snap := rt.committed
		if att != nil {
			snap = rt.attached[name]
			if snap == nil {
				release()
				return nil, newError(UndefinedObject, `database "`+name+`" is not attached`)
			}
		}
		pin, ok, err := st.integrityCapture(snap)
		if err != nil {
			release()
			return nil, err
		}
		if ok {
			pin.release = release
			return pin, nil
		}
		release()
		if attempt > 100000 {
			return nil, newError(ObjectInUse, `could not pin a consistent generation of database "`+name+`"`)
		}
		runtime.Gosched()
	}
}

// integrityCapture captures the generation snap reads from this storage, or ok false when the storage
// has moved past it (a commit between the pin and this call).
func (st *storage) integrityCapture(snap *snapshot) (*integrityPin, bool, error) {
	st.mu.Lock()
	defer st.mu.Unlock()
	paging := st.paging
	pin := &integrityPin{snap: snap, paging: paging}
	if st.path == "" {
		// In memory: the last adopted commit's catalog root, or, before any commit (and after a
		// compaction), the store's own meta.
		var m meta
		if st.budget.lastCatRoot.page != 0 {
			m = meta{txid: st.stagedTxid, rootPage: st.budget.lastCatRoot.page, rootTag: st.budget.lastCatRoot.tag}
		} else {
			found := false
			for slot := uint32(0); slot < 2; slot++ {
				raw, err := paging.readBlock(slot)
				if err != nil {
					return nil, false, err
				}
				if pm, ok := parseMeta(raw); ok && (!found || pm.txid > m.txid) {
					m, found = pm, true
				}
			}
			if !found {
				return nil, false, newError(DataCorrupted, "no valid meta page")
			}
		}
		if m.txid != snap.txid {
			return nil, false, nil
		}
		m.pageCount = st.pageCount
		pin.gen = integrityGeneration{meta: m, memFree: append([]uint32(nil), st.freePages...)}
		return pin, true, nil
	}
	var raw []byte
	var protected map[uint32]bool
	err := paging.withPager(func(p *pager) error {
		raw = append([]byte(nil), p.validatedMeta...)
		protected = make(map[uint32]bool, len(p.manifestProtected))
		for pg := range p.manifestProtected {
			protected[pg] = true
		}
		// The slot the newest meta is in: the floor's, or the other one while a relaxed batch is pending
		// (validated-cow.md §4).
		pin.slot = p.floorSlot
		if len(p.pending) > 0 {
			pin.slot ^= 1
		}
		if st.serviceClient {
			return nil // a page client's meta is its owner's, served to it (page-service.md §5.3)
		}
		pin.reread = true
		for slot := uint32(0); slot < 2; slot++ {
			b, err := p.readBlock(slot)
			if err != nil {
				return err
			}
			pin.slotRaw[slot] = b
		}
		return nil
	})
	if err != nil {
		return nil, false, err
	}
	m, ok := parseMeta(raw)
	if !ok || m.txid != snap.txid {
		return nil, false, nil
	}
	pin.metaRaw = raw
	pin.gen = integrityGeneration{meta: m, file: true, protected: protected}
	return pin, true, nil
}

// checkLiveGeneration re-verifies a live handle's pinned generation (§3.1): its meta slot still holds
// the meta the handle validated or wrote, and its commit manifest still validates. On a fresh open
// neither can fail. A valid manifest's dependencies become the generation's protected set.
func (k *integrityChecker) checkLiveGeneration(pin *integrityPin) error {
	if !pin.reread {
		return nil
	}
	slot := uint32(2)
	for s := uint32(0); s < 2; s++ {
		if bytes.Equal(pin.slotRaw[s], pin.metaRaw) {
			slot = s
			break
		}
	}
	if slot == 2 {
		if err := k.add(IntegrityMetaInvalid, "", integrityPage(pin.slot), nil, "the generation's meta slot no longer holds its meta"); err != nil {
			return err
		}
	}
	protected, bad, valid, err := commitManifestDependencies(k.gen.meta, pin.metaRaw, k.paging.pgr.pageSize, k.paging.readBlock)
	if err != nil {
		return err
	}
	if valid {
		k.gen.protected = protected
		return nil
	}
	page := bad
	if page == 0 {
		page = pin.slot
		if slot < 2 {
			page = slot
		}
	}
	return k.add(IntegrityManifestPageMismatch, "", integrityPage(page), nil, "the commit manifest no longer validates")
}

// integrityMaxTreeDepth bounds a tree walk (spec/design/integrity-check.md §5.2): the root is at depth
// 0, and a page reached at this depth or deeper is reported page_corrupt without being read.
const integrityMaxTreeDepth = 256

// errIntegrityTruncated ends a check whose next finding would pass max_findings (§4).
var errIntegrityTruncated = errors.New("integrity check truncated")

// integrityChecker is one check in progress.
type integrityChecker struct {
	ctx    context.Context
	db     string
	full   bool
	max    int // 0 = no limit
	snap   *snapshot
	paging *sharedPaging
	gen    integrityGeneration
	pcap   pageCap
	first  uint32
	// reached is the reachability bitmap over [0, page_count); reachedN its population.
	reached  []uint64
	reachedN uint64
	// damaged is set once any tree (or the catalog) was unreadable: live_count_mismatch is then not
	// reported (§5.2).
	damaged bool
	// gistRoots is each GiST index's R-tree root from the stored catalog; gistKnown is false when the
	// catalog could not be decoded.
	gistRoots map[string]pageRef
	gistKnown bool
	// freeChain is the free-list chain pages read and verified so far; they count toward pages.
	freeChain uint64
	report    IntegrityReport
	// eng is the transient engine index agreement resolves and evaluates definitions through.
	eng *engine
}

func newIntegrityChecker(ctx context.Context, db string, opts IntegrityCheckOptions, snap *snapshot, paging *sharedPaging, gen integrityGeneration) *integrityChecker {
	max := opts.MaxFindings
	switch {
	case max == 0:
		max = defaultIntegrityMaxFindings
	case max < 0:
		max = 0
	}
	pcap := paging.cap()
	return &integrityChecker{
		ctx:     ctx,
		db:      db,
		full:    opts.Level != IntegrityQuick,
		max:     max,
		snap:    snap,
		paging:  paging,
		gen:     gen,
		pcap:    pcap,
		first:   pcap.firstBody(),
		reached: make([]uint64, (uint64(gen.meta.pageCount)+63)/64),
	}
}

// run runs the check after generation (the live caller's meta re-verification, or nil) and returns
// the report. A truncated check reports pages and rows as counted when it stopped.
func (k *integrityChecker) run(generation func() error) (IntegrityReport, error) {
	k.report.Findings = []IntegrityFinding{}
	k.report.Skipped = []IntegritySkip{}
	err := func() error {
		if generation != nil {
			if err := generation(); err != nil {
				return err
			}
		}
		if err := k.checkCatalog(); err != nil {
			return err
		}
		for _, sq := range k.snap.sequencesSorted() {
			if sq.LastValue < sq.MinValue || sq.LastValue > sq.MaxValue {
				detail := fmt.Sprintf("value %d outside [%d, %d]", sq.LastValue, sq.MinValue, sq.MaxValue)
				if err := k.add(IntegritySequenceOutOfRange, sq.Name, nil, nil, detail); err != nil {
					return err
				}
			}
		}
		names := make([]string, 0, len(k.snap.tables))
		for name := range k.snap.tables {
			names = append(names, name)
		}
		sort.Strings(names)
		for _, name := range names {
			if err := k.checkTable(name); err != nil {
				return err
			}
		}
		if err := k.checkFreeList(); err != nil {
			return err
		}
		if k.gen.file && !k.damaged && uint64(k.gen.meta.livePages) != k.reachedN {
			detail := fmt.Sprintf("meta records %d live pages, %d are reachable", k.gen.meta.livePages, k.reachedN)
			return k.add(IntegrityLiveCountMismatch, "", nil, nil, detail)
		}
		return nil
	}()
	if err != nil && !errors.Is(err, errIntegrityTruncated) {
		return IntegrityReport{}, err
	}
	k.report.Pages = k.reachedN + k.freeChain
	return k.report, nil
}

func integrityPage(p uint32) *uint32 { return &p }

// add records a finding, or ends the check (errIntegrityTruncated) when the limit is reached.
func (k *integrityChecker) add(kind, object string, page *uint32, key []byte, detail string) error {
	if k.max > 0 && len(k.report.Findings) == k.max {
		k.report.Truncated = true
		return errIntegrityTruncated
	}
	if key != nil {
		key = append([]byte{}, key...)
	}
	k.report.Findings = append(k.report.Findings, IntegrityFinding{Kind: kind, Database: k.db, Object: object, Page: page, Key: key, Detail: detail})
	return nil
}

func (k *integrityChecker) skip(object, reason string) {
	k.report.Skipped = append(k.report.Skipped, IntegritySkip{Object: object, Reason: reason})
}

func (k *integrityChecker) poll() error {
	if k.ctx.Err() != nil {
		return errIntegrityCanceled()
	}
	return nil
}

func (k *integrityChecker) inRange(p uint32) bool { return p >= k.first && p < k.gen.meta.pageCount }

func (k *integrityChecker) isReached(p uint32) bool {
	return p < k.gen.meta.pageCount && k.reached[p/64]&(1<<(p%64)) != 0
}

// mark records an in-range page as reached; false when it already was.
func (k *integrityChecker) mark(p uint32) bool {
	if k.isReached(p) {
		return false
	}
	k.reached[p/64] |= 1 << (p % 64)
	k.reachedN++
	return true
}

// reach records page as reached by object's walk, or reports why the walk must not read it (§3.2):
// out of the body range, or already reached.
func (k *integrityChecker) reach(object string, page uint32) (bool, error) {
	if !k.inRange(page) {
		detail := fmt.Sprintf("page %d outside [%d, %d)", page, k.first, k.gen.meta.pageCount)
		return false, k.add(IntegrityPageOutOfRange, object, integrityPage(page), nil, detail)
	}
	if !k.mark(page) {
		return false, k.add(IntegrityPageReachedTwice, object, integrityPage(page), nil, fmt.Sprintf("page %d is already part of the database", page))
	}
	return true, nil
}

// isCorruption reports whether err is a statement about the bytes (XX001), which the check reports as
// a finding, rather than a failure to read them.
func isCorruption(err error) bool {
	var ee *EngineError
	return errors.As(err, &ee) && ee.State == DataCorrupted
}

// read reads and verifies the page ref names. A page that fails verification is returned as corrupt
// (a finding for the caller); any other error ends the check.
func (k *integrityChecker) read(ref pageRef) (pg page, corrupt error, err error) {
	if err := k.poll(); err != nil {
		return page{}, nil, err
	}
	block, err := k.paging.readBody(ref)
	if err == nil {
		pg, err = parsePage(block)
	}
	if err != nil {
		if isCorruption(err) {
			return page{}, err, nil
		}
		return page{}, nil, err
	}
	return pg, nil, nil
}

// --- the catalog (§3.1) ------------------------------------------------------------------------

// integrityCatalogPage is one stored catalog chain page: its index, header, and content after the
// successor's tag.
type integrityCatalogPage struct {
	page      uint32
	flags     byte
	itemCount uint32
	content   []byte
}

// checkCatalog walks the generation's catalog chain (its pages reached first, §3.2), decodes the
// stored table entries for their GiST roots — the only tree roots the snapshot does not hold — and
// compares the chain with the encoding of the snapshot's catalog. The check continues from the
// snapshot's catalog either way.
func (k *integrityChecker) checkCatalog() error {
	var stored []integrityCatalogPage
	corrupt := func(page uint32, detail string) error {
		k.damaged = true
		return k.add(IntegrityCatalogPageCorrupt, "", integrityPage(page), nil, detail)
	}
	for p := (pageRef{page: k.gen.meta.rootPage, tag: k.gen.meta.rootTag}); p.page != 0; {
		if !k.inRange(p.page) || !k.mark(p.page) {
			return corrupt(p.page, "the catalog chain leaves the body range or revisits a page")
		}
		pg, bad, err := k.read(p)
		if err != nil {
			return err
		}
		if bad != nil {
			return corrupt(p.page, bad.Error())
		}
		if pg.pageType != pageCatalog || pg.flags&^1 != 0 {
			return corrupt(p.page, "expected a catalog page")
		}
		next, content, err := chainContent(pg, k.pcap)
		if err != nil {
			return corrupt(p.page, err.Error())
		}
		stored = append(stored, integrityCatalogPage{page: p.page, flags: pg.flags, itemCount: pg.itemCount, content: content})
		p = next
	}
	page, detail, ok := k.decodeGistRoots(stored)
	if !ok {
		return corrupt(page, detail)
	}
	return k.compareCatalog(stored)
}

// decodeGistRoots decodes the stored catalog's entries up to the last table entry, recording each
// GiST index's root. It reports the first page of an entry that does not decode.
func (k *integrityChecker) decodeGistRoots(stored []integrityCatalogPage) (uint32, string, bool) {
	k.gistRoots = map[string]pageRef{}
	for i := 0; i < len(stored); i++ {
		start := stored[i]
		payload := start.content
		if start.flags&1 != 0 {
			if start.itemCount != 1 {
				return start.page, "a continued catalog page holds other entries", false
			}
			payload = append([]byte(nil), payload...)
			for {
				i++
				if i == len(stored) {
					return start.page, "a continued catalog entry ends the chain", false
				}
				cont := stored[i]
				if cont.itemCount != 0 {
					return cont.page, "bad catalog continuation page", false
				}
				payload = append(payload, cont.content...)
				if cont.flags&1 == 0 {
					break
				}
			}
		}
		pos := 0
		for n := uint32(0); n < start.itemCount; n++ {
			kind, err := readU8(payload, &pos)
			if err != nil {
				return start.page, err.Error(), false
			}
			switch kind {
			case 0:
				var rowCount int64
				table, _, indexRoots, err := decodeTableEntry(payload, &pos, &rowCount, k.pcap)
				if err != nil {
					return start.page, err.Error(), false
				}
				for j, idx := range table.Indexes {
					if idx.Kind == indexGist && j < len(indexRoots) {
						k.gistRoots[idx.Name] = indexRoots[j]
					}
				}
			case 1:
				_, err = decodeCompositeTypeEntry(payload, &pos)
			case 2:
				_, err = decodeSequenceEntry(payload, &pos)
			case 3:
				_, _, err = decodeCollationEntry(payload, &pos, k.snap.env)
			case 4, 5:
				// Statistics and views follow every table entry (format.md *Catalog*).
				k.gistKnown = true
				return 0, "", true
			default:
				return start.page, "unknown catalog entry kind", false
			}
			if err != nil {
				return start.page, err.Error(), false
			}
		}
	}
	k.gistKnown = true
	return 0, "", true
}

// compareCatalog checks that the stored chain is the encoding of the snapshot's catalog, page by page
// (§3.1): the same flags, entry counts, and content, zero-padded.
func (k *integrityChecker) compareCatalog(stored []integrityCatalogPage) error {
	keys := make([]string, 0, len(k.snap.tables))
	for name := range k.snap.tables {
		keys = append(keys, name)
	}
	sort.Strings(keys)
	rootDataPage := make([]pageRef, len(keys))
	indexRoots := make([][]pageRef, len(keys))
	for ti, name := range keys {
		if root := k.snap.stores[name].treeRoot(); root != nil {
			rootDataPage[ti] = root.ref()
		}
		for _, idx := range k.snap.tables[name].Indexes {
			var r pageRef
			if idx.Kind == indexGist {
				r = k.gistRoots[idx.Name]
			} else if ist := k.snap.indexStores[idx.Name]; ist != nil {
				if root := ist.treeRoot(); root != nil {
					r = root.ref()
				}
			}
			indexRoots[ti] = append(indexRoots[ti], r)
		}
	}
	entries, err := k.snap.catalogEntries(keys, rootDataPage, indexRoots, k.pcap)
	if err != nil {
		if isCorruption(err) {
			return k.add(IntegrityCatalogMismatch, "", integrityPage(k.gen.meta.rootPage), nil, err.Error())
		}
		return err
	}
	layout := catalogLayout(entries, k.pcap.chain())
	for i, want := range layout {
		if i == len(stored) {
			return k.add(IntegrityCatalogMismatch, "", integrityPage(stored[len(stored)-1].page), nil, "the stored catalog chain is shorter than the catalog's encoding")
		}
		got := stored[i]
		var flags byte
		if want.continued {
			flags = 1
		}
		if got.flags != flags || got.itemCount != want.itemCount || len(got.content) < len(want.payload) ||
			!bytes.Equal(got.content[:len(want.payload)], want.payload) || !zeroBytes(got.content[len(want.payload):]) {
			return k.add(IntegrityCatalogMismatch, "", integrityPage(got.page), nil, "the stored catalog page is not the encoding of the catalog")
		}
	}
	if len(stored) > len(layout) {
		return k.add(IntegrityCatalogMismatch, "", integrityPage(stored[len(layout)].page), nil, "the stored catalog chain is longer than the catalog's encoding")
	}
	return nil
}

// --- trees (§3.2) and index agreement (§3.4) -----------------------------------------------------

// checkTable walks table name's tree, checks its row count, then each of its indexes in name order:
// structure, then agreement under the full level.
func (k *integrityChecker) checkTable(name string) error {
	table := k.snap.tables[name]
	store := k.snap.stores[name]
	colls := make([]*Collation, len(table.Columns))
	for i, c := range table.Columns {
		if c.Collation != "" {
			colls[i] = k.snap.resolveCollation(c.Collation)
		}
	}
	var root pageRef
	if r := store.treeRoot(); r != nil {
		root = r.ref()
	}
	walk := &integrityTree{k: k, object: name, colTypes: store.colTypes, table: table, colls: colls, leafDepth: -1, readable: true}
	if root.page != 0 {
		if err := walk.visit(root, 0, nil, nil); err != nil {
			return err
		}
	}
	tableDamaged := !walk.readable || walk.badRow
	if count, known := store.Count(); !tableDamaged && known && uint64(count) != walk.records {
		detail := fmt.Sprintf("catalog records %d rows, the tree holds %d", count, walk.records)
		if err := k.add(IntegrityRowCountMismatch, name, nil, nil, detail); err != nil {
			return err
		}
	}
	k.damaged = k.damaged || tableDamaged
	indexes := append([]indexDef(nil), table.Indexes...)
	sort.Slice(indexes, func(a, b int) bool { return indexes[a].Name < indexes[b].Name })
	for _, def := range indexes {
		if err := k.checkIndex(table, store, colls, root, tableDamaged, def); err != nil {
			return err
		}
	}
	return nil
}

func (k *integrityChecker) checkIndex(table *catTable, store *tableStore, colls []*Collation, tableRoot pageRef, tableDamaged bool, def indexDef) error {
	// An index whose walk reported anything, even an order or bounds finding that leaves it readable,
	// is not checked for agreement: its own lookups cannot be trusted to find what it holds (§5.2).
	before := len(k.report.Findings)
	readable := true
	var entries uint64
	var gistOps []gistOpclass
	if def.Kind == indexGist {
		gistOps = gistOpclassesFor(def.columnOrdinals(), table.Columns)
		walk := &integrityGist{k: k, object: def.Name, ops: gistOps, leafDepth: -1, readable: true}
		if !k.gistKnown {
			walk.readable = false
		} else if root := k.gistRoots[def.Name]; root.page != 0 {
			if err := walk.visit(root, 0, nil); err != nil {
				return err
			}
		}
		readable, entries = walk.readable, walk.entries
	} else {
		var root pageRef
		if ist := k.snap.indexStores[def.Name]; ist != nil {
			if r := ist.treeRoot(); r != nil {
				root = r.ref()
			}
		}
		walk := &integrityTree{k: k, object: def.Name, gin: def.Kind == indexGin, leafDepth: -1, readable: true}
		if root.page != 0 {
			if err := walk.visit(root, 0, nil, nil); err != nil {
				return err
			}
		}
		readable, entries = walk.readable, walk.records
		if walk.gin {
			entries = walk.postings
		}
	}
	k.damaged = k.damaged || !readable
	if !k.full {
		return nil
	}
	// The definition and environment skips come first, decided before any row is read (§3.5).
	rindex, reason, err := k.resolveForAgreement(table, def)
	if err != nil {
		return err
	}
	if reason != "" {
		k.skip(def.Name, reason)
		return nil
	}
	if tableDamaged {
		k.skip(def.Name, IntegritySkipTableUnreadable)
		return nil
	}
	if !readable || len(k.report.Findings) != before {
		k.skip(def.Name, IntegritySkipIndexUnreadable)
		return nil
	}
	ist := k.snap.indexStores[def.Name]
	if ist == nil && def.Kind != indexGist {
		return newError(DataCorrupted, "index "+def.Name+" has no store")
	}
	eng := k.engine()
	env := eng.indexMaintEnv()
	var found uint64
	err = k.scanRows(tableRoot, store.colTypes, 0, func(key []byte, row storedRow) error {
		// Each row's derivation is one maintenance evaluation, with its own scalar allowance as the
		// write that stored it had (unmetered otherwise: §5.4).
		eng.session.resetStatementMemory()
		eks, err := indexEntryKeys(table.Columns, store.colTypes, colls, &rindex, key, row, env)
		if err != nil {
			var ee *EngineError
			if !errors.As(err, &ee) {
				return err
			}
			return k.add(IntegrityIndexEntryError, def.Name, nil, key, err.Error())
		}
		if def.Kind == indexGin {
			// Entry order is the terms' byte order, column tag included (§3.4).
			n := len(key)
			sort.SliceStable(eks, func(a, b int) bool {
				return bytes.Compare(eks[a][:len(eks[a])-n], eks[b][:len(eks[b])-n]) < 0
			})
		}
		for _, ek := range eks {
			var ok bool
			var err error
			switch def.Kind {
			case indexGin:
				ok, err = k.ginHolds(ist, ek[:len(ek)-len(key)], key)
			case indexGist:
				ok, err = k.gistHolds(gistOps, k.gistRoots[def.Name], ek[:len(ek)-len(key)], key)
			default:
				_, ok, err = ist.Get(ek)
			}
			if err != nil {
				return err
			}
			if ok {
				found++
			} else if err := k.add(IntegrityIndexEntryMissing, def.Name, nil, key, "no entry for the row"); err != nil {
				return err
			}
		}
		return nil
	})
	if err != nil {
		return err
	}
	if entries != found {
		detail := fmt.Sprintf("index holds %d entries, %d derive from rows", entries, found)
		return k.add(IntegrityIndexEntryCountMismatch, def.Name, nil, nil, detail)
	}
	return nil
}

// engine is the transient read engine the agreement pass resolves and evaluates index definitions
// through: the pinned snapshot in the handle's environment, with a fresh default session (no cost
// ceiling — the check is unmetered).
func (k *integrityChecker) engine() *engine {
	if k.eng == nil {
		k.eng = &engine{committed: k.snap, pageSize: k.paging.pgr.pageSize, session: newSession(k.snap.env)}
	}
	return k.eng
}

// resolveForAgreement resolves def as the write path does for maintenance, or names the skip that
// prevents its agreement check (§3.5), in the registered order: its stored text does not parse; a host
// function it pinned is not registered at that component and version; a collation or time zone it
// depends on is not loaded or is at another version.
func (k *integrityChecker) resolveForAgreement(table *catTable, def indexDef) (resolvedIndex, string, error) {
	if def.unparsed() {
		return resolvedIndex{}, IntegritySkipUnparsableDefinition, nil
	}
	env := k.snap.env
	for _, dep := range def.HostDeps {
		id, ok := env.hosts.resolveHost(dep.Name, dep.ArgTypes)
		if !ok || !hostDepMatches(env.hosts.function(id), []hostFuncDep{dep}) {
			return resolvedIndex{}, IntegritySkipMissingHostFunction, nil
		}
	}
	if verifyIndexTimezoneDeps(def.TimezoneDeps, env) != nil {
		return resolvedIndex{}, IntegritySkipMissingLoadedData, nil
	}
	rindex, err := k.engine().resolveIndex(table, def)
	if err != nil {
		var ee *EngineError
		if !errors.As(err, &ee) {
			return resolvedIndex{}, "", err
		}
		switch ee.State {
		case UnparsableStoredDefinition:
			return resolvedIndex{}, IntegritySkipUnparsableDefinition, nil
		case UndefinedFunction:
			return resolvedIndex{}, IntegritySkipMissingHostFunction, nil
		case CollationVersionMismatch:
			return resolvedIndex{}, IntegritySkipMissingLoadedData, nil
		}
		return resolvedIndex{}, "", err
	}
	// The collations the index's keys encode under or its expressions compare with: a key column's,
	// an expression key's derived collation, and every column an expression or the predicate reads.
	unavailable := func(name string) bool {
		if name == "" {
			return false
		}
		if _, _, _, _, skewed := k.snap.collationSkew(name); skewed {
			return true
		}
		return env.collation(name) == nil
	}
	var refs func(node *rExpr) bool
	refs = func(node *rExpr) bool {
		if node == nil {
			return false
		}
		if node.kind == reColumn && node.index < len(table.Columns) && unavailable(table.Columns[node.index].Collation) {
			return true
		}
		for _, child := range rexprChildren(node) {
			if refs(child) {
				return true
			}
		}
		return false
	}
	for _, key := range rindex.Keys {
		if key.Expr == nil {
			if unavailable(table.Columns[key.Col].Collation) {
				return resolvedIndex{}, IntegritySkipMissingLoadedData, nil
			}
			continue
		}
		if (key.Coll != nil && unavailable(key.Coll.Name)) || refs(key.Expr) {
			return resolvedIndex{}, IntegritySkipMissingLoadedData, nil
		}
	}
	if refs(rindex.Predicate) {
		return resolvedIndex{}, IntegritySkipMissingLoadedData, nil
	}
	return rindex, "", nil
}

// ginHolds reports whether the GIN index holds the posting (term, key), found as its maintenance
// finds it (gin.md §4.5): the first run at or after term ‖ key, decoded with the term's length. A run
// that does not decode holds nothing (§3.4).
func (k *integrityChecker) ginHolds(ist *tableStore, term, key []byte) (bool, error) {
	run, err := ginRunAtOrAfter(ist, term, key)
	if err != nil {
		if isCorruption(err) {
			return false, nil
		}
		return false, err
	}
	if run == nil {
		return false, nil
	}
	keys, err := ginDecodeRun(len(term), run)
	if err != nil {
		return false, nil
	}
	_, ok := ginSearchKeys(keys, key)
	return ok, nil
}

// gistHolds reports whether the GiST R-tree rooted at root holds the leaf entry (boundBytes, key):
// it descends into every child whose bound contains the derived bound (§3.4).
func (k *integrityChecker) gistHolds(ops []gistOpclass, root pageRef, boundBytes, key []byte) (bool, error) {
	if root.page == 0 {
		return false, nil
	}
	pos := 0
	bound, err := readBoundTuple(ops, boundBytes, &pos)
	if err != nil || pos != len(boundBytes) {
		return false, nil
	}
	return k.gistSearch(ops, root, bound, boundBytes, key, 0)
}

func (k *integrityChecker) gistSearch(ops []gistOpclass, ref pageRef, bound []gistBound, boundBytes, key []byte, depth int) (bool, error) {
	if depth >= integrityMaxTreeDepth {
		return false, nil
	}
	pg, bad, err := k.read(ref)
	if err != nil || bad != nil {
		return false, err
	}
	items, known, bad := parseIntegrityGist(pg, k.pcap, ops)
	if bad != nil || !known {
		return false, nil
	}
	for _, it := range items {
		if pg.pageType == pageGistLeaf {
			if bytes.Equal(it.raw, boundBytes) && bytes.Equal(it.skey, key) {
				return true, nil
			}
			continue
		}
		if !gistBoundContains(it.bound, bound) {
			continue
		}
		ok, err := k.gistSearch(ops, it.child, bound, boundBytes, key, depth+1)
		if err != nil || ok {
			return ok, err
		}
	}
	return false, nil
}

// integrityTree walks one B+tree depth-first in child order (format.md *Interior node*), checking
// page verification, node types, uniform leaf depth, strict key order within a node, and that every
// key lies in the bounds its parent's separators give it; for a table, it decodes and checks every
// row (§3.2).
type integrityTree struct {
	k        *integrityChecker
	object   string
	colTypes []colType
	table    *catTable // nil for an index
	colls    []*Collation
	// leafDepth is the depth of the first leaf, -1 before it.
	leafDepth int
	// readable is cleared when part of the tree could not be read; badRow when a row stopped at a
	// value finding (§5.2).
	readable bool
	badRow   bool
	// records is the leaf records the walk read.
	records uint64
	// gin marks a GIN index, whose records are posting runs; postings counts them from the runs'
	// tails (§3.2).
	gin      bool
	postings uint64
}

func (w *integrityTree) corrupt(page uint32, detail string) error {
	w.readable = false
	return w.k.add(IntegrityPageCorrupt, w.object, integrityPage(page), nil, detail)
}

func (w *integrityTree) visit(ref pageRef, depth int, lo, hi []byte) error {
	ok, err := w.k.reach(w.object, ref.page)
	if err != nil || !ok {
		w.readable = w.readable && ok
		return err
	}
	if depth >= integrityMaxTreeDepth {
		// Reached, then reported without being read (§5.2).
		w.readable = false
		return w.k.add(IntegrityPageCorrupt, w.object, integrityPage(ref.page), nil, "tree deeper than any valid tree")
	}
	pg, bad, err := w.k.read(ref)
	if err != nil {
		return err
	}
	if bad != nil {
		return w.corrupt(ref.page, bad.Error())
	}
	switch pg.pageType {
	case pageInterior:
		children, seps, err := parseIntegrityInterior(pg, w.k.pcap)
		if err != nil {
			return w.corrupt(ref.page, err.Error())
		}
		if err := w.checkKeys(ref.page, seps, lo, hi); err != nil {
			return err
		}
		for i, child := range children {
			clo, chi := lo, hi
			if i > 0 {
				clo = seps[i-1]
			}
			if i < len(seps) {
				chi = seps[i]
			}
			if err := w.visit(child, depth+1, clo, chi); err != nil {
				return err
			}
		}
		return nil
	case pageLeaf:
		if w.leafDepth < 0 {
			w.leafDepth = depth
		} else if depth != w.leafDepth {
			detail := fmt.Sprintf("leaf at depth %d, others at %d", depth, w.leafDepth)
			if err := w.k.add(IntegrityTreeDepthUneven, w.object, integrityPage(ref.page), nil, detail); err != nil {
				return err
			}
		}
		n := int(pg.itemCount)
		leaf, err := parsePaxLeaf(pg.payload, n, w.colTypes)
		if err != nil {
			return w.corrupt(ref.page, err.Error())
		}
		keys := make([][]byte, n)
		for i := range keys {
			keys[i] = leaf.key(i)
		}
		if err := w.checkKeys(ref.page, keys, lo, hi); err != nil {
			return err
		}
		if w.gin {
			var postings uint64
			for _, key := range keys {
				c, err := ginRunPostings(key)
				if err != nil {
					return w.corrupt(ref.page, err.Error())
				}
				postings += c
			}
			w.postings += postings
		}
		w.records += uint64(n)
		if w.table == nil {
			return nil
		}
		for i := 0; i < n; i++ {
			if err := w.checkRow(ref.page, leaf, i, keys[i]); err != nil {
				return err
			}
		}
		return nil
	default:
		w.readable = false
		return w.k.add(IntegrityPageTypeWrong, w.object, integrityPage(ref.page), nil, fmt.Sprintf("page type %d in a B-tree", pg.pageType))
	}
}

// checkKeys reports at most one keys_unordered and one separator_violation for a node's keys: they
// must ascend strictly and lie in [lo, hi) (nil bounds are open).
func (w *integrityTree) checkKeys(page uint32, keys [][]byte, lo, hi []byte) error {
	for i := 1; i < len(keys); i++ {
		if bytes.Compare(keys[i-1], keys[i]) >= 0 {
			if err := w.k.add(IntegrityKeysUnordered, w.object, integrityPage(page), nil, "keys do not ascend strictly"); err != nil {
				return err
			}
			break
		}
	}
	for _, key := range keys {
		if (lo != nil && bytes.Compare(key, lo) < 0) || (hi != nil && bytes.Compare(key, hi) >= 0) {
			return w.k.add(IntegritySeparatorViolation, w.object, integrityPage(page), nil, "a key lies outside the bounds its parent gives it")
		}
	}
	return nil
}

// checkRow decodes record i of a table leaf, stopping at its first value finding, and checks that a
// row of a table with a primary key is stored under its primary-key encoding.
func (w *integrityTree) checkRow(page uint32, leaf *paxLeaf, i int, key []byte) error {
	k := w.k
	k.report.Rows++
	if err := k.poll(); err != nil {
		return err
	}
	row := make(storedRow, len(w.colTypes))
	fetch := func(ref pageRef) ([]byte, error) {
		if !k.inRange(ref.page) {
			return nil, integrityChainStop{kind: IntegrityPageOutOfRange, page: ref.page}
		}
		if !k.mark(ref.page) {
			return nil, integrityChainStop{kind: IntegrityPageReachedTwice, page: ref.page}
		}
		if err := k.poll(); err != nil {
			return nil, integrityChainFatal{err}
		}
		block, err := k.paging.readBody(ref)
		if err != nil && !isCorruption(err) {
			return nil, integrityChainFatal{err}
		}
		return block, err
	}
	for c, ty := range w.colTypes {
		if leaf.isNull(c, i) {
			row[c] = NullValue()
			continue
		}
		raw, err := leaf.value(c, i)
		var v Value
		if err == nil {
			v, err = decodeIntegrityValue(ty, raw, fetch, k.pcap)
		}
		if err == nil {
			row[c] = v
			continue
		}
		column := "column " + w.table.Columns[c].Name
		var stop integrityChainStop
		var fatal integrityChainFatal
		switch {
		case errors.As(err, &fatal):
			return fatal.err
		case errors.As(err, &stop):
			w.badRow = true
			return k.add(stop.kind, w.object, integrityPage(stop.page), key, column+": "+stop.Error())
		}
		w.badRow = true
		kind := IntegrityValueUndecodable
		if _, fixed := fixedValueWidth(ty); !fixed && len(raw) > 0 && (raw[0] == tagExternal || raw[0] == tagExternalComp) {
			kind = IntegrityOverflowChainCorrupt
		}
		return k.add(kind, w.object, integrityPage(page), key, column+": "+err.Error())
	}
	if len(w.table.PK) == 0 {
		return nil
	}
	want, err := encodePkKey(w.colTypes, w.table.PK, w.colls, row)
	if err == nil && bytes.Equal(want, key) {
		return nil
	}
	return k.add(IntegrityRowKeyNotPrimaryKey, w.object, integrityPage(page), key, "stored key is not the row's primary-key encoding")
}

// integrityChainStop is an overflow pointer the walk must not follow: out of the body range or
// already reached. The finding names that page and the row.
type integrityChainStop struct {
	kind string
	page uint32
}

func (e integrityChainStop) Error() string {
	if e.kind == IntegrityPageOutOfRange {
		return fmt.Sprintf("page %d out of range", e.page)
	}
	return fmt.Sprintf("page %d reached twice", e.page)
}

// integrityChainFatal carries an error that ends the check (cancellation, I/O) out of a chain read.
type integrityChainFatal struct{ err error }

func (e integrityChainFatal) Error() string { return e.err.Error() }

// decodeIntegrityValue decodes one stored value of a table leaf completely — an inline body, or an
// out-of-line value's chain read through fetch, decompressed and reconstructed — as a read of the
// column would (large-values.md §12–§14). raw is a fixed-width slot (untagged) or a tagged span.
func decodeIntegrityValue(ty colType, raw []byte, fetch func(pageRef) ([]byte, error), pcap pageCap) (Value, error) {
	pos := 0
	if _, fixed := fixedValueWidth(ty); fixed {
		return readInlineBody(ty, raw, &pos, decodeConstruct)
	}
	tag, err := readU8(raw, &pos)
	if err != nil {
		return Value{}, err
	}
	switch tag {
	case 0x00:
		return readInlineBody(ty, raw, &pos, decodeConstruct)
	case 0x01:
		return NullValue(), nil
	case tagExternal, tagExternalComp:
		first, err := readU32(raw, &pos)
		if err != nil {
			return Value{}, err
		}
		stored, err := readU32(raw, &pos)
		if err != nil {
			return Value{}, err
		}
		var rawLen uint32
		if tag == tagExternalComp {
			if rawLen, err = readU32(raw, &pos); err != nil {
				return Value{}, err
			}
		}
		ref, err := readPointerTag(raw, &pos, first, pcap)
		if err != nil {
			return Value{}, err
		}
		var visited []uint32
		payload, err := readOverflowChain(ref, int(stored), fetch, pcap, &visited)
		if err != nil {
			return Value{}, err
		}
		if tag == tagExternalComp {
			if payload, err = lz4Decompress(payload, int(rawLen)); err != nil {
				return Value{}, err
			}
		}
		return valueFromPayload(ty, payload)
	case tagInlineComp:
		rawLen, err := readU32(raw, &pos)
		if err != nil {
			return Value{}, err
		}
		compLen, err := readU16(raw, &pos)
		if err != nil {
			return Value{}, err
		}
		comp, err := take(raw, &pos, int(compLen))
		if err != nil {
			return Value{}, err
		}
		payload, err := lz4Decompress(comp, int(rawLen))
		if err != nil {
			return Value{}, err
		}
		return valueFromPayload(ty, payload)
	default:
		return Value{}, newError(DataCorrupted, "invalid value presence tag")
	}
}

// parseIntegrityInterior parses a B+tree interior: its n+1 child pointers and n separators.
func parseIntegrityInterior(pg page, pcap pageCap) ([]pageRef, [][]byte, error) {
	n := int(pg.itemCount)
	if (pcap.ptr()+4)*n+pcap.ptr() > len(pg.payload) {
		return nil, nil, newError(DataCorrupted, "B-tree interior item count overruns page")
	}
	pos := 0
	children := make([]pageRef, 0, n+1)
	for i := 0; i <= n; i++ {
		cp, err := readPageRef(pg.payload, &pos, pcap)
		if err != nil {
			return nil, nil, err
		}
		children = append(children, cp)
	}
	seps, err := readSeparators(pg.payload, &pos, n)
	if err != nil {
		return nil, nil, err
	}
	return children, seps, nil
}

// scanRows re-walks a table tree the structural walk found readable, in key order, yielding each
// row's storage key and decoded values: index agreement's per-row pass (§3.4). It reaches and reports
// nothing; the pages are immutable while the snapshot is pinned, so a failure here is an error.
func (k *integrityChecker) scanRows(ref pageRef, colTypes []colType, depth int, visit func(key []byte, row storedRow) error) error {
	if ref.page == 0 {
		return nil
	}
	if depth > maxTreeDepth {
		return newError(DataCorrupted, "B-tree deeper than any valid tree")
	}
	if err := k.poll(); err != nil {
		return err
	}
	block, err := k.paging.readBody(ref)
	if err != nil {
		return err
	}
	pg, err := parsePage(block)
	if err != nil {
		return err
	}
	switch pg.pageType {
	case pageInterior:
		children, _, err := parseIntegrityInterior(pg, k.pcap)
		if err != nil {
			return err
		}
		for _, child := range children {
			if err := k.scanRows(child, colTypes, depth+1, visit); err != nil {
				return err
			}
		}
		return nil
	case pageLeaf:
		n := int(pg.itemCount)
		leaf, err := parsePaxLeaf(pg.payload, n, colTypes)
		if err != nil {
			return err
		}
		row := make(storedRow, len(colTypes))
		for i := 0; i < n; i++ {
			if err := k.poll(); err != nil {
				return err
			}
			for c, ty := range colTypes {
				if leaf.isNull(c, i) {
					row[c] = NullValue()
					continue
				}
				raw, err := leaf.value(c, i)
				if err != nil {
					return err
				}
				if row[c], err = decodeIntegrityValue(ty, raw, k.paging.readBody, k.pcap); err != nil {
					return err
				}
			}
			if err := visit(leaf.key(i), row); err != nil {
				return err
			}
		}
		return nil
	default:
		return newError(DataCorrupted, "expected a B-tree node page")
	}
}

// ginRunPostings counts a GIN posting run's storage keys from its tail, without the term's length
// (spec/design/integrity-check.md §3.2): the body (its length in the last two bytes, after at least a
// term byte and a `last` byte) parses exactly as varint(shared) ‖ varint(suffix length) ‖ suffix
// triples, each varint at most three bytes, shared + suffix length > 0, the suffix within the body.
// The count is one (the `last` key) plus the triples.
func ginRunPostings(run []byte) (uint64, error) {
	n := len(run)
	if n < 4 {
		return 0, ginRunCorrupt()
	}
	bodyLen := int(binary.BigEndian.Uint16(run[n-2:]))
	if n-2-bodyLen < 2 {
		return 0, ginRunCorrupt()
	}
	body := run[n-2-bodyLen : n-2]
	count := uint64(1)
	for at := 0; at < len(body); {
		shared, err := ginReadVarint(body, &at)
		if err != nil {
			return 0, err
		}
		suffix, err := ginReadVarint(body, &at)
		if err != nil {
			return 0, err
		}
		if at+suffix > len(body) || shared+suffix == 0 {
			return 0, ginRunCorrupt()
		}
		at += suffix
		count++
	}
	return count, nil
}

// integrityGist walks a GiST R-tree (gist.md §4.1) for page verification, node types, uniform leaf
// depth, every bound decoding under the index's opclasses, and each interior bound containing every
// bound in the child it names (spec/design/integrity-check.md §3.2). entries counts its leaf entries.
type integrityGist struct {
	k         *integrityChecker
	object    string
	ops       []gistOpclass
	leafDepth int
	readable  bool
	entries   uint64
}

// visit walks the node ref names; parent is the bound its parent's entry gives it (nil at the root).
func (w *integrityGist) visit(ref pageRef, depth int, parent []gistBound) error {
	ok, err := w.k.reach(w.object, ref.page)
	if err != nil || !ok {
		w.readable = w.readable && ok
		return err
	}
	if depth >= integrityMaxTreeDepth {
		// Reached, then reported without being read (§5.2).
		w.readable = false
		return w.k.add(IntegrityPageCorrupt, w.object, integrityPage(ref.page), nil, "tree deeper than any valid tree")
	}
	pg, bad, err := w.k.read(ref)
	if err != nil {
		return err
	}
	var items []integrityGistItem
	var known bool
	if bad == nil {
		items, known, bad = parseIntegrityGist(pg, w.k.pcap, w.ops)
	}
	if bad != nil {
		w.readable = false
		return w.k.add(IntegrityPageCorrupt, w.object, integrityPage(ref.page), nil, bad.Error())
	}
	if !known {
		w.readable = false
		return w.k.add(IntegrityPageTypeWrong, w.object, integrityPage(ref.page), nil, fmt.Sprintf("page type %d in a GiST tree", pg.pageType))
	}
	leaf := pg.pageType == pageGistLeaf
	if leaf {
		if w.leafDepth < 0 {
			w.leafDepth = depth
		} else if depth != w.leafDepth {
			if err := w.k.add(IntegrityTreeDepthUneven, w.object, integrityPage(ref.page), nil, fmt.Sprintf("leaf at depth %d, others at %d", depth, w.leafDepth)); err != nil {
				return err
			}
		}
	}
	if parent != nil {
		for _, it := range items {
			if !gistBoundContains(parent, it.bound) {
				if err := w.k.add(IntegrityGistBoundsViolation, w.object, integrityPage(ref.page), nil, "a bound lies outside its parent's bound for this page"); err != nil {
					return err
				}
				break
			}
		}
	}
	if leaf {
		w.entries += uint64(len(items))
		return nil
	}
	for _, it := range items {
		if err := w.visit(it.child, depth+1, it.bound); err != nil {
			return err
		}
	}
	return nil
}

// integrityGistItem is one GiST node entry: its bound's bytes and decoded form, and a leaf's storage
// key or an interior's child.
type integrityGistItem struct {
	raw   []byte
	bound []gistBound
	skey  []byte
	child pageRef
}

// parseIntegrityGist parses a GiST node's entries, decoding every bound under ops exactly (trailing
// bytes are a failure); known is false for another page type.
func parseIntegrityGist(pg page, pcap pageCap, ops []gistOpclass) ([]integrityGistItem, bool, error) {
	payload := pg.payload
	pos := 0
	leaf := pg.pageType == pageGistLeaf
	switch {
	case leaf:
	case pg.pageType == pageGistInterior:
		if uint64(pg.itemCount)*uint64(2+pcap.ptr()) > uint64(len(payload)) {
			return nil, true, newError(DataCorrupted, "GiST interior item count overruns page")
		}
	default:
		return nil, false, nil
	}
	items := make([]integrityGistItem, 0, min(int(pg.itemCount), len(payload)/2+1))
	for i := uint32(0); i < pg.itemCount; i++ {
		n, err := gistReadU16(payload, &pos)
		if err != nil {
			return nil, true, err
		}
		raw, err := gistTakeBytes(payload, &pos, n)
		if err != nil {
			return nil, true, err
		}
		at := 0
		bound, err := readBoundTuple(ops, raw, &at)
		if err != nil {
			return nil, true, err
		}
		if at != len(raw) {
			return nil, true, newError(DataCorrupted, "GiST bound has trailing bytes")
		}
		it := integrityGistItem{raw: raw, bound: bound}
		if leaf {
			sn, err := gistReadU16(payload, &pos)
			if err != nil {
				return nil, true, err
			}
			if it.skey, err = gistTakeBytes(payload, &pos, sn); err != nil {
				return nil, true, err
			}
		} else if it.child, err = readPageRef(payload, &pos, pcap); err != nil {
			return nil, true, err
		}
		items = append(items, it)
	}
	return items, true, nil
}

// gistBoundContains reports whether tuple bound outer contains inner, per component: a range `@>`
// (anything contains empty, empty contains only empty); a scalar's [min, max] key bytes bytewise.
func gistBoundContains(outer, inner []gistBound) bool {
	for i := range outer {
		a, b := outer[i], inner[i]
		if a.rng != nil || b.rng != nil {
			if a.rng == nil || b.rng == nil || !rangeContains(a.rng, b.rng) {
				return false
			}
			continue
		}
		if bytes.Compare(a.smin, b.smin) > 0 || bytes.Compare(b.smax, a.smax) > 0 {
			return false
		}
	}
	return true
}

// --- space accounting (§3.3) ---------------------------------------------------------------------

// checkFreeList checks the generation's free list against the reached pages: a file's persisted chain
// (its pages walked first) and then each entry in list order, or an in-memory domain's RAM list. Each
// chain page read and verified counts toward pages at once (freeChain).
func (k *integrityChecker) checkFreeList() error {
	var entries []uint32
	if k.gen.file {
		inChain := map[uint32]bool{}
		for p := (pageRef{page: k.gen.meta.freeListHead, tag: k.gen.meta.freeListTag}); p.page != 0; {
			if !k.inRange(p.page) || inChain[p.page] {
				if err := k.add(IntegrityFreeListCorrupt, "", integrityPage(p.page), nil, fmt.Sprintf("free-list chain names page %d", p.page)); err != nil {
					return err
				}
				break
			}
			if k.isReached(p.page) {
				if err := k.add(IntegrityFreePageLive, "", integrityPage(p.page), nil, fmt.Sprintf("free-list page %d is reachable", p.page)); err != nil {
					return err
				}
			}
			pg, bad, err := k.read(p)
			if err != nil {
				return err
			}
			var next pageRef
			var content []byte
			if bad == nil {
				if pg.pageType != pageFreelist {
					bad = newError(DataCorrupted, "not a free-list page")
				} else if next, content, bad = chainContent(pg, k.pcap); bad == nil && uint64(pg.itemCount)*4 > uint64(len(content)) {
					bad = newError(DataCorrupted, "invalid free-list count")
				}
			}
			if bad != nil {
				if err := k.add(IntegrityFreeListCorrupt, "", integrityPage(p.page), nil, bad.Error()); err != nil {
					return err
				}
				break
			}
			inChain[p.page] = true
			k.freeChain++
			for i := uint32(0); i < pg.itemCount; i++ {
				at := int(i) * 4
				entries = append(entries, uint32(content[at])<<24|uint32(content[at+1])<<16|uint32(content[at+2])<<8|uint32(content[at+3]))
			}
			p = next
		}
	} else {
		entries = k.gen.memFree
	}
	seen := make([]uint64, len(k.reached))
	for _, p := range entries {
		var kind, detail string
		switch {
		case !k.inRange(p):
			kind, detail = IntegrityFreePageOutOfRange, fmt.Sprintf("free page %d outside [%d, %d)", p, k.first, k.gen.meta.pageCount)
		case seen[p/64]&(1<<(p%64)) != 0:
			kind, detail = IntegrityFreePageDuplicate, fmt.Sprintf("free page %d listed twice", p)
		case k.isReached(p):
			kind, detail = IntegrityFreePageLive, fmt.Sprintf("free page %d is reachable", p)
		case k.gen.protected[p]:
			kind, detail = IntegrityFreePageManifestDependency, fmt.Sprintf("free page %d is a dependency of the generation", p)
		}
		if k.inRange(p) {
			seen[p/64] |= 1 << (p % 64)
		}
		if kind != "" {
			if err := k.add(kind, "", integrityPage(p), nil, detail); err != nil {
				return err
			}
		}
	}
	return nil
}
