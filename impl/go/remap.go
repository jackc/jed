package jed

// PROTOTYPE (spec/design/deferred-interior.md): deferred interior pages.
//
// A copy-on-write commit rewrites every interior page above a changed leaf, and the catalog page that
// holds the tree's root pointer. With JED_DEFER_INTERIOR=on a commit writes only the nodes whose own
// content changed. An interior node whose only change is which page a child lives at keeps its page;
// the meta page records "the node the disk calls page D is now at page C" in a remap list. Open
// applies the list as it reads child pointers. When the list would overflow, or a commit cannot defer
// (more dirty nodes than the list has room for), the commit folds: every stale interior and its
// ancestors are written fresh and the list empties.
//
// Go-only, no format version bump. The other cores, verify.rb, backup, and the page
// service do not understand the list.

import (
	"encoding/binary"
	"os"
	"sort"
)

// deferInterior turns the prototype on.
var deferInterior = os.Getenv("JED_DEFER_INTERIOR") == "on"

// remapBudget is the bytes a meta page of 4 KiB or more sets aside for its remap list, taken from the
// end of the inline manifest area: a 4-byte count and a 4-byte marker after the entries. An entry is
// the on-disk page id and the current page id (8 bytes); in an encrypted file it also carries the
// current page's tag (encrypted.md §2), 24 bytes, since every pointer binds the page it names.
const remapBudget = 2728

func remapEntrySize(encrypted bool) int {
	if encrypted {
		return 8 + cryptoTagLen
	}
	return 8
}

// remapArea is the bytes of a meta page of ps bytes that can hold manifest entries and the list:
// after the header (and an encrypted meta's tags), before an encrypted meta's reserve.
func remapArea(ps int, encrypted bool) (int, int) {
	if encrypted {
		return 96, ps - cryptoReserve
	}
	return 64, ps
}

// remapCapFor is the entries a meta page carries: remapBudget's worth from 4 KiB up, and a third of
// the area on the small pages the simulator uses. remapReserveFor is their bytes.
func remapCapFor(ps int, encrypted bool) int {
	budget := remapBudget
	if ps < 4096 {
		lo, hi := remapArea(ps, encrypted)
		budget = (hi - lo) / 3
	}
	return max(0, (budget-8)/remapEntrySize(encrypted))
}

func remapReserveFor(ps int, encrypted bool) int {
	return remapEntrySize(encrypted)*remapCapFor(ps, encrypted) + 8
}

// deferOn reports whether a file of page size ps defers interior pages.
func deferOn(ps int, encrypted bool) bool {
	return deferInterior && ps >= 256 && remapCapFor(ps, encrypted) > 0
}

var remapMagic = [4]byte{'J', 'R', 'M', 'P'}

// remapState is the remap list in memory. fwd maps the page id an on-disk pointer holds to where the
// node is now (with its tag in an encrypted file); rev maps a current page back to that id.
type remapState struct {
	fwd map[uint32]pageRef
	rev map[uint32]uint32
}

func newRemapState() *remapState {
	return &remapState{fwd: map[uint32]pageRef{}, rev: map[uint32]uint32{}}
}

func (r *remapState) len() int {
	if r == nil {
		return 0
	}
	return len(r.fwd)
}

func (r *remapState) clone() *remapState {
	out := newRemapState()
	if r != nil {
		for k, v := range r.fwd {
			out.fwd[k] = v
			out.rev[v.page] = k
		}
	}
	return out
}

// replace records that the node at page old now lives at cur, in the same slot of a parent whose
// page is not rewritten.
func (r *remapState) replace(old uint32, cur pageRef) {
	disk := old
	if d, ok := r.rev[old]; ok {
		disk = d
		delete(r.rev, old)
	}
	r.fwd[disk] = cur
	r.rev[cur.page] = disk
}

// drop forgets the entry that resolves to page: its parent was written fresh and names it directly.
func (r *remapState) drop(page uint32) {
	if d, ok := r.rev[page]; ok {
		delete(r.rev, page)
		delete(r.fwd, d)
	}
}

// lookup is where the node an on-disk pointer calls page lives now.
func (r *remapState) lookup(page uint32) (pageRef, bool) {
	if r == nil {
		return pageRef{}, false
	}
	to, ok := r.fwd[page]
	return to, ok
}

// held is the on-disk page ids the list is keyed by, which must not be reused while their entry
// exists: a pointer holding a reused id would be redirected.
func (r *remapState) held() []uint32 {
	if r == nil {
		return nil
	}
	out := make([]uint32, 0, len(r.fwd))
	for k := range r.fwd {
		out = append(out, k)
	}
	return out
}

// metaRemapReserve is the bytes a meta page sets aside for its remap list: remapReserveFor when it
// carries the marker at the end of its area, else 0.
func metaRemapReserve(m []byte) int {
	n := len(m)
	if n < 256 {
		return 0
	}
	encrypted := m[6] != 0
	_, hi := remapArea(n, encrypted)
	if remapCapFor(n, encrypted) == 0 || [4]byte(m[hi-4:hi]) != remapMagic {
		return 0
	}
	return remapReserveFor(n, encrypted)
}

// encodeRemap writes the list into a meta page, before its checksum is computed and before an
// encrypted meta is sealed (the seal authenticates the whole page, the list included).
func encodeRemap(m []byte, r *remapState) {
	encrypted := m[6] != 0
	_, hi := remapArea(len(m), encrypted)
	size := remapEntrySize(encrypted)
	copy(m[hi-4:], remapMagic[:])
	keys := r.held()
	sort.Slice(keys, func(i, j int) bool { return keys[i] < keys[j] })
	binary.BigEndian.PutUint32(m[hi-8:], uint32(len(keys)))
	for i, k := range keys {
		at := hi - 8 - size*(i+1)
		to := r.fwd[k]
		binary.BigEndian.PutUint32(m[at:], k)
		binary.BigEndian.PutUint32(m[at+4:], to.page)
		if encrypted {
			copy(m[at+8:at+size], to.tag[:])
		}
	}
}

// decodeRemap reads a meta page's list; nil when it carries none.
func decodeRemap(m []byte) (*remapState, bool) {
	if metaRemapReserve(m) == 0 {
		return nil, true
	}
	encrypted := m[6] != 0
	_, hi := remapArea(len(m), encrypted)
	size := remapEntrySize(encrypted)
	count := int(binary.BigEndian.Uint32(m[hi-8:]))
	if count > remapCapFor(len(m), encrypted) {
		return nil, false
	}
	r := newRemapState()
	for i := 0; i < count; i++ {
		at := hi - 8 - size*(i+1)
		k := binary.BigEndian.Uint32(m[at:])
		to := pageRef{page: binary.BigEndian.Uint32(m[at+4:])}
		if encrypted {
			copy(to.tag[:], m[at+8:at+size])
		}
		if _, dup := r.fwd[k]; dup || k == 0 || to.page == 0 {
			return nil, false
		}
		r.fwd[k] = to
		r.rev[to.page] = k
	}
	return r, true
}

// deferCtx is one commit's deferral state: the list as the commit leaves it, and how many pages kept
// their place that the live-page delta would otherwise count as replaced.
type deferCtx struct {
	remap    *remapState
	retained uint32
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

// dirtyNodes counts the dirty nodes under n — an upper bound on the remap entries a commit can add.
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

// unstale returns a copy of n's subtree in which every stale interior, and every ancestor of one, is
// a dirty node the commit will write fresh; nil when nothing under n is stale.
func unstale(n *pnode) *pnode {
	if n == nil || n.isLeaf() {
		return nil
	}
	var children []childRef
	for i, c := range n.children {
		if c.node == nil {
			continue
		}
		r := unstale(c.node)
		if r == nil {
			continue
		}
		if children == nil {
			children = append(make([]childRef, 0, len(n.children)), n.children...)
		}
		children[i] = residentRef(r)
	}
	if children == nil && !n.stale {
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
func (s *snapshot) unstale() {
	for _, store := range s.stores {
		store.rows.unstale()
	}
	for _, store := range s.indexStores {
		store.rows.unstale()
	}
}

func (m *pMap) unstale() {
	if r := unstale(m.root); r != nil {
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
