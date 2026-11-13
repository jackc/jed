package jed

// PROTOTYPE (spec/design/deferred-interior.md): deferred interior pages.
//
// A copy-on-write commit rewrites every interior page above a changed leaf, and the catalog page that
// holds the tree's root pointer. With JED_DEFER_INTERIOR=on a commit writes only the nodes whose own
// content changed. An interior node whose only change is which page a child lives at keeps its page;
// the meta page records "in interior page P, the child pointer in slot i now names page C" in a remap
// list. Readers apply it as they read an interior's child pointers. When the list would overflow, the
// commit folds: every stale interior and its ancestors are written fresh and the list empties.
//
// This is §9 of the design doc -- the ratified byte layout, keyed by (parent page, slot) -- not the
// earlier prototype, which keyed entries globally by the child's on-disk page and therefore had to
// hold those pages off the free list (§8.3). A (parent, slot) entry dies in the commit that orphans
// its parent, so nothing is held. Go-only, no format version bump: v53 lands it with the narrow
// manifest and the page stamp.

import (
	"encoding/binary"
	"os"
	"sort"
)

// deferInterior turns the prototype on.
var deferInterior = os.Getenv("JED_DEFER_INTERIOR") == "on"

// remapEntrySize is one entry's bytes (deferred-interior.md §9.2): parent u32, slot u16, page u32,
// and in an encrypted file the 16-byte AEAD tag of page, since every pointer binds the page it names.
func remapEntrySize(encrypted bool) int {
	if encrypted {
		return 10 + cryptoTagLen
	}
	return 10
}

// remapArea is the meta's manifest area [lo, hi): after the header (and an encrypted meta's tags),
// before an encrypted meta's reserve.
func remapArea(ps int, encrypted bool) (int, int) {
	if encrypted {
		return 96, ps - cryptoReserve
	}
	return 64, ps
}

// remapCapFor is the entries a meta carries: one third of the manifest area, at every page size
// (§9.2, §9.7). remapReserveFor is the region's bytes, the last of the area.
func remapCapFor(ps int, encrypted bool) int {
	lo, hi := remapArea(ps, encrypted)
	return ((hi - lo) / 3) / remapEntrySize(encrypted)
}

func remapReserveFor(ps int, encrypted bool) int {
	return remapEntrySize(encrypted) * remapCapFor(ps, encrypted)
}

// deferOn reports whether a file of page size ps uses the deferred-interior layout. The layout is
// fixed by the format, not by the meta: there is no marker, and a slot with no entries carries a zero
// region and remap_entries = 0.
func deferOn(ps int, encrypted bool) bool {
	return deferInterior && remapCapFor(ps, encrypted) > 0
}

// remapKey names one child pointer: slot slot of the interior page parent (§9.1).
type remapKey struct {
	parent uint32
	slot   uint16
}

func (a remapKey) less(b remapKey) bool {
	return a.parent < b.parent || (a.parent == b.parent && a.slot < b.slot)
}

// remapState is the remap list in memory: in the interior page parent, the child pointer in slot
// slot names a page the node has since moved off; the subtree root is at the entry's page. Resolution
// is per page and never transitive. maxSlot is the highest slot each parent keys, for the
// slot <= item_count check when the parent is read.
type remapState struct {
	entries map[remapKey]pageRef
	maxSlot map[uint32]uint16
}

func newRemapState() *remapState {
	return &remapState{entries: map[remapKey]pageRef{}, maxSlot: map[uint32]uint16{}}
}

func (r *remapState) len() int {
	if r == nil {
		return 0
	}
	return len(r.entries)
}

func (r *remapState) clone() *remapState {
	out := newRemapState()
	if r != nil {
		for k, v := range r.entries {
			out.entries[k] = v
		}
		for k, v := range r.maxSlot {
			out.maxSlot[k] = v
		}
	}
	return out
}

// set records that slot slot of the retained interior parent now resolves to cur. It overwrites an
// existing entry, so a child that moves repeatedly under one parent occupies one entry forever.
func (r *remapState) set(parent uint32, slot uint16, cur pageRef) {
	r.entries[remapKey{parent, slot}] = cur
	if m, ok := r.maxSlot[parent]; !ok || slot > m {
		r.maxSlot[parent] = slot
	}
}

// has reports whether the entry for (parent, slot) exists.
func (r *remapState) has(k remapKey) bool {
	if r == nil {
		return false
	}
	_, ok := r.entries[k]
	return ok
}

// dropParent forgets every entry the interior page parent keys: the commit retired that parent
// (wrote it fresh, merged it away, or dropped its table), so its page is about to be orphaned and must
// never be read through a surviving entry (§9.5).
func (r *remapState) dropParent(parent uint32) {
	if _, ok := r.maxSlot[parent]; !ok {
		return
	}
	for k := range r.entries {
		if k.parent == parent {
			delete(r.entries, k)
		}
	}
	delete(r.maxSlot, parent)
}

// keys reports whether the interior page parent keys any entry -- whether it is stale: its on-disk
// pointers disagree with the live tree.
func (r *remapState) keys(parent uint32) bool {
	if r == nil {
		return false
	}
	_, ok := r.maxSlot[parent]
	return ok
}

// parents is the set of pages that key an entry.
func (r *remapState) parents() map[uint32]bool {
	out := map[uint32]bool{}
	if r != nil {
		for p := range r.maxSlot {
			out[p] = true
		}
	}
	return out
}

// lookup is where the child in slot slot of the interior page parent lives now.
func (r *remapState) lookup(parent uint32, slot int) (pageRef, bool) {
	if r == nil || slot > 0xFFFF {
		return pageRef{}, false
	}
	to, ok := r.entries[remapKey{parent, uint16(slot)}]
	return to, ok
}

// checkParent is the read-time rule of §9.3: an interior page with item_count n has slots [0, n], so
// an entry it keys past n is corruption.
func (r *remapState) checkParent(parent uint32, n int) error {
	if r == nil {
		return nil
	}
	if m, ok := r.maxSlot[parent]; ok && int(m) > n {
		return newError(DataCorrupted, "remap entry names a slot past its parent's item count")
	}
	return nil
}

// resolve redirects the n+1 child pointers just read from the interior page parent.
func (r *remapState) resolve(parent uint32, children []pageRef) error {
	if r == nil {
		return nil
	}
	if err := r.checkParent(parent, len(children)-1); err != nil {
		return err
	}
	for i := range children {
		if to, ok := r.lookup(parent, i); ok {
			children[i] = to
		}
	}
	return nil
}

// sorted is the entries in (parent, slot) order, the order they are stored.
func (r *remapState) sorted() []remapKey {
	if r == nil {
		return nil
	}
	out := make([]remapKey, 0, len(r.entries))
	for k := range r.entries {
		out = append(out, k)
	}
	sort.Slice(out, func(i, j int) bool { return out[i].less(out[j]) })
	return out
}

// encodeRemap writes the list into a meta page (§9.2): remap_entries at offset 60, the entries
// ascending from the start of the region, the rest of the region zero. Before the meta's checksum is
// computed, and before an encrypted meta is sealed (the seal covers the region).
func encodeRemap(m []byte, r *remapState) {
	encrypted := m[6] != 0
	_, hi := remapArea(len(m), encrypted)
	size := remapEntrySize(encrypted)
	at := hi - remapReserveFor(len(m), encrypted)
	clear(m[at:hi])
	keys := r.sorted()
	binary.BigEndian.PutUint32(m[60:], uint32(len(keys)))
	for _, k := range keys {
		to := r.entries[k]
		binary.BigEndian.PutUint32(m[at:], k.parent)
		binary.BigEndian.PutUint16(m[at+4:], k.slot)
		binary.BigEndian.PutUint32(m[at+6:], to.page)
		if encrypted {
			copy(m[at+10:at+size], to.tag[:])
		}
		at += size
	}
}

// decodeRemap reads and validates a meta page's list (§9.3), first and pageCount being the meta's
// body bounds; false when the slot is structurally invalid, so the other slot may be tried. Without
// the deferred layout offset 60 is reserved zero, as through v52.
func decodeRemap(m []byte, first, pageCount uint32) (*remapState, bool) {
	count := binary.BigEndian.Uint32(m[60:64])
	encrypted := m[6] != 0
	if !deferOn(len(m), encrypted) {
		return nil, count == 0
	}
	if count > uint32(remapCapFor(len(m), encrypted)) {
		return nil, false
	}
	_, hi := remapArea(len(m), encrypted)
	size := remapEntrySize(encrypted)
	at := hi - remapReserveFor(len(m), encrypted)
	r := newRemapState()
	targets := make(map[uint32]bool, count)
	var prev remapKey
	for i := uint32(0); i < count; i++ {
		k := remapKey{parent: binary.BigEndian.Uint32(m[at:]), slot: binary.BigEndian.Uint16(m[at+4:])}
		to := pageRef{page: binary.BigEndian.Uint32(m[at+6:])}
		if encrypted {
			copy(to.tag[:], m[at+10:at+size])
		}
		if (i > 0 && !prev.less(k)) || k.parent < first || k.parent >= pageCount ||
			to.page < first || to.page >= pageCount || to.page == k.parent || targets[to.page] {
			return nil, false
		}
		prev = k
		targets[to.page] = true
		r.set(k.parent, k.slot, to)
		at += size
	}
	return r, zeroBytes(m[at:hi])
}

// deferCtx is one commit's deferral state, decided before allocation (deferred-interior.md §9.4).
type deferCtx struct {
	// remap is the entry set this commit publishes: the snapshot's E0 minus every entry whose parent
	// the commit retires; serializeDirty adds an entry for each moved child of a retained interior.
	remap *remapState
	// retain is the persisted page of every interior that keeps its page, by the page it replaces.
	retain map[uint32]bool
	// retained counts the retained pages, which the live-page delta would otherwise count as replaced;
	// retainedIDs is them in serialization order.
	retained    uint32
	retainedIDs []uint32
	// migrate is the page this commit moves down rather than keeps in place (0: none) -- the page that
	// blocks trailing truncation, chosen by migrateHint (validated_commit.go; §9.6).
	migrate uint32
	// capacity is the entries the meta can carry; serializeDirty checks the result against it.
	capacity int
}

// catState is the committed catalog chain, kept so a commit whose catalog bytes are unchanged does not
// rewrite it.
type catState struct {
	payloads [][]byte
	root     pageRef
}

// originOf is the persisted page a copy of n replaces, and whether the copy's separators are still
// that page's.
func originOf(n *pnode) (pageRef, bool) {
	if n.page != 0 {
		return n.ref(), true
	}
	return n.origin, n.sameKeys
}

// retainable is §9.4 step 2 for a dirty interior: it replaces a persisted page one for one (a), its
// separators are still that page's, so every slot index is fixed for the page's lifetime (b), and its
// page is not the migration page (c).
func retainable(n *pnode, migrate uint32) bool {
	return n.origin.page != 0 && n.sameKeys && n.origin.page != migrate
}

// planNode decides retention for the subtree under n without allocating, recording what
// serializeDirty will: the clean pages it keeps (the shared boundary of the live-page delta), the
// interiors that keep their page, and the (parent, slot) of every child of a retained interior that
// moves. It reports whether n itself gets a fresh page.
func planNode(n *pnode, migrate uint32, kept, retain map[uint32]bool, adds *[]remapKey) bool {
	if n.page != 0 {
		kept[n.page] = true
		return false
	}
	if len(n.children) == 0 {
		return true // a dirty leaf is always written fresh
	}
	var moved []int
	for i, c := range n.children {
		if c.node == nil {
			kept[c.page] = true
			continue
		}
		if planNode(c.node, migrate, kept, retain, adds) {
			moved = append(moved, i)
		}
	}
	if !retainable(n, migrate) {
		return true
	}
	retain[n.origin.page] = true
	for _, i := range moved {
		*adds = append(*adds, remapKey{parent: n.origin.page, slot: uint16(i)})
	}
	return false
}

// visitRetired walks the previous snapshot's subtree under n exactly as the live-page delta's orphan
// walk does (orphanedNodePages) -- skipping every subtree the new snapshot keeps -- and records each
// interior it reaches that keys an entry. Those are the parents this commit retires or retains: written
// fresh, merged away, dropped with a table, or kept in place.
func visitRetired(n *pnode, kept, parents, out map[uint32]bool) {
	if n == nil || n.page == 0 || kept[n.page] || len(n.children) == 0 {
		return
	}
	if parents[n.page] {
		out[n.page] = true
	}
	for _, c := range n.children {
		if c.node != nil {
			visitRetired(c.node, kept, parents, out)
		}
	}
}

// eachCommittedTree calls f with the root of every tree a commit serializes, in serialization order:
// each table's data tree, then its non-GiST index trees (GiST rewrites its R-tree whole and keeps no
// B-tree pages).
func (s *snapshot) eachCommittedTree(f func(*pnode)) {
	keys := make([]string, 0, len(s.tables))
	for k := range s.tables {
		keys = append(keys, k)
	}
	sort.Strings(keys)
	for _, k := range keys {
		if root := s.stores[k].treeRoot(); root != nil {
			f(root)
		}
		for _, idx := range s.tables[k].Indexes {
			if idx.Kind == indexGist {
				continue
			}
			if root := s.indexStores[idx.Name].treeRoot(); root != nil {
				f(root)
			}
		}
	}
}

// planDeferral is §9.4 before allocation: retention, the entry set E, and the fold test. e0 is the
// committed list and prev the committed snapshot it describes. It returns nil when the commit must
// fold (|E| > capacity).
//
// The drop rule (§9.5) is taken from the walk every writer already makes for the live-page delta,
// which reaches exactly the previous snapshot's interiors the new one does not share. An E0 parent it
// reaches and the commit does not retain is retired, and its entries go in this commit -- the same
// commit that orphans its page -- so no entry survives its parent and no page is held off the free
// list. A page client applies the same rule to its own mutation.
func (s *snapshot) planDeferral(prev *snapshot, e0 *remapState, migrate uint32, capacity int) *deferCtx {
	kept := map[uint32]bool{}
	retain := map[uint32]bool{}
	var adds []remapKey
	s.eachCommittedTree(func(root *pnode) { planNode(root, migrate, kept, retain, &adds) })
	e := e0.clone()
	if e.len() > 0 && prev == nil {
		return nil // without the committed snapshot the drop rule cannot run: fold rather than guess
	}
	if e.len() > 0 {
		reached := map[uint32]bool{}
		parents := e0.parents()
		for _, st := range prev.stores {
			visitRetired(st.treeRoot(), kept, parents, reached)
		}
		for _, ist := range prev.indexStores {
			visitRetired(ist.treeRoot(), kept, parents, reached)
		}
		for p := range reached {
			if !retain[p] {
				e.dropParent(p)
			}
		}
	}
	size := e.len()
	for _, k := range adds {
		if !e.has(k) {
			size++
		}
	}
	if size > capacity {
		return nil
	}
	return &deferCtx{remap: e, retain: retain, migrate: migrate, capacity: capacity}
}

// dirtyNodes counts the dirty nodes under n.
func dirtyNodes(n *pnode) int {
	if n == nil || n.page != 0 {
		return 0
	}
	count := 1
	for _, c := range n.children {
		if c.node != nil {
			count += dirtyNodes(c.node)
		}
	}
	return count
}

// unstale returns a copy of n's subtree in which every stale interior -- one whose page keys an entry
// in parents -- and every ancestor of one is a dirty node the commit will write fresh; nil when
// nothing under n is stale. This is the fold (§9.4).
func unstale(n *pnode, parents map[uint32]bool) *pnode {
	if n == nil || n.isLeaf() {
		return nil
	}
	var children []childRef
	for i, c := range n.children {
		if c.node == nil {
			continue
		}
		r := unstale(c.node, parents)
		if r == nil {
			continue
		}
		if children == nil {
			children = append(make([]childRef, 0, len(n.children)), n.children...)
		}
		children[i] = residentRef(r)
	}
	stale := n.page != 0 && parents[n.page]
	if children == nil && !stale {
		return nil
	}
	if n.page == 0 {
		// A dirty node of this transaction: only its child slots change.
		if children != nil {
			return &pnode{keys: n.keys, children: children, owner: n.owner}
		}
		return nil
	}
	if children == nil {
		children = n.children
	}
	return &pnode{keys: n.keys, children: children}
}

// unstale folds every tree of the snapshot (see unstale).
func (s *snapshot) unstale(parents map[uint32]bool) {
	if len(parents) == 0 {
		return
	}
	for _, store := range s.stores {
		store.rows.unstale(parents)
	}
	for _, store := range s.indexStores {
		store.rows.unstale(parents)
	}
}

func (m *pMap) unstale(parents map[uint32]bool) {
	if r := unstale(m.root, parents); r != nil {
		m.root = r
	}
}

// dirtyNodeCount is the snapshot's dirty tree nodes.
func (s *snapshot) dirtyNodeCount() int {
	count := 0
	for _, store := range s.stores {
		count += dirtyNodes(store.treeRoot())
	}
	for _, store := range s.indexStores {
		count += dirtyNodes(store.treeRoot())
	}
	return count
}

// resealRemapMeta recomputes a meta page's checksum after its remap list was written.
func resealRemapMeta(m []byte) { binary.BigEndian.PutUint32(m[32:], metaCRC(m)) }

// remapStat appends one byte to the file JED_DEFER_STATS names (test tooling): 'd' a deferring commit
// that left a non-empty list, 'e' one that left it empty, 'f' a fold, 'o' an open that applied a
// non-empty list, 'n' any other open.
func remapStat(c byte) {
	if remapStatsPath == "" {
		return
	}
	if f, err := os.OpenFile(remapStatsPath, os.O_APPEND|os.O_CREATE|os.O_WRONLY, 0o644); err == nil {
		f.Write([]byte{c})
		f.Close()
	}
}

var remapStatsPath = os.Getenv("JED_DEFER_STATS")
