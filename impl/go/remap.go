package jed

// PROTOTYPE (spec/design/deferred-interior.md): deferred interior pages.
//
// A copy-on-write commit rewrites every interior page above a changed leaf, and the catalog page that
// holds the tree's root pointer. With JED_DEFER_INTERIOR=on a commit writes only the nodes whose own
// content changed. An interior node whose only change is which page a child lives at keeps its page;
// the meta page records "the node the disk calls page D is now at page C" in a remap list. Open
// applies the list as it reads child pointers. When the list would overflow, or a commit cannot defer
// (a relaxed batch, a page client, an encrypted file), the commit folds: every stale interior and its
// ancestors are written fresh and the list empties.
//
// Go-only, plaintext only, no format version bump. The other cores, verify.rb, backup, and the page
// service do not understand the list.

import (
	"encoding/binary"
	"os"
	"sort"
)

// deferInterior turns the prototype on.
var deferInterior = os.Getenv("JED_DEFER_INTERIOR") == "on"

// remapCap is the most entries a meta page carries; remapReserve the bytes it sets aside for them at
// the end of the page, taken from the inline manifest: 8 bytes per entry, then the count and a marker.
const (
	remapCap     = 340
	remapReserve = 8*remapCap + 8
)

// remapCapFor is the entries a meta page of ps bytes carries: remapCap from 4 KiB up, and a third of
// the space after the header on the small pages the simulator uses. remapReserveFor is their bytes.
func remapCapFor(ps int) int {
	if ps >= 4096 {
		return remapCap
	}
	return (ps - 64) / 24
}

func remapReserveFor(ps int) int { return 8*remapCapFor(ps) + 8 }

// remapMinPage is the smallest page size that carries a remap list.
const remapMinPage = 256

// deferOn reports whether a plaintext file of page size ps defers interior pages.
func deferOn(ps int) bool { return deferInterior && ps >= remapMinPage }

var remapMagic = [4]byte{'J', 'R', 'M', 'P'}

// remapState is the remap list in memory. fwd maps the page id an on-disk pointer holds to the page
// the node is at now; rev is its inverse.
type remapState struct {
	fwd map[uint32]uint32
	rev map[uint32]uint32
}

func newRemapState() *remapState {
	return &remapState{fwd: map[uint32]uint32{}, rev: map[uint32]uint32{}}
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
			out.rev[v] = k
		}
	}
	return out
}

// replace records that the node at page old now lives at page cur, in the same slot of a parent whose
// page is not rewritten.
func (r *remapState) replace(old, cur uint32) {
	disk := old
	if d, ok := r.rev[old]; ok {
		disk = d
		delete(r.rev, old)
	}
	r.fwd[disk] = cur
	r.rev[cur] = disk
}

// drop forgets the entry that resolves to page: its parent was written fresh and names it directly.
func (r *remapState) drop(page uint32) {
	if d, ok := r.rev[page]; ok {
		delete(r.rev, page)
		delete(r.fwd, d)
	}
}

// lookup is where the node an on-disk pointer calls page lives now.
func (r *remapState) lookup(page uint32) (uint32, bool) {
	if r == nil {
		return 0, false
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

// metaRemapReserve is the bytes a meta page sets aside for its remap list: remapReserve when it
// carries the marker, else 0.
func metaRemapReserve(m []byte) int {
	n := len(m)
	if n < remapMinPage || m[6] != 0 {
		return 0
	}
	if [4]byte(m[n-4:]) != remapMagic {
		return 0
	}
	return remapReserveFor(n)
}

// encodeRemap writes the list into a plaintext meta page (before its checksum is computed).
func encodeRemap(m []byte, r *remapState) {
	n := len(m)
	copy(m[n-4:], remapMagic[:])
	keys := r.held()
	sort.Slice(keys, func(i, j int) bool { return keys[i] < keys[j] })
	binary.BigEndian.PutUint32(m[n-8:], uint32(len(keys)))
	for i, k := range keys {
		at := n - 8 - 8*(i+1)
		binary.BigEndian.PutUint32(m[at:], k)
		binary.BigEndian.PutUint32(m[at+4:], r.fwd[k])
	}
}

// decodeRemap reads a meta page's list; nil when it carries none.
func decodeRemap(m []byte) (*remapState, bool) {
	if metaRemapReserve(m) == 0 {
		return nil, true
	}
	n := len(m)
	count := int(binary.BigEndian.Uint32(m[n-8:]))
	if count > remapCapFor(n) {
		return nil, false
	}
	r := newRemapState()
	for i := 0; i < count; i++ {
		at := n - 8 - 8*(i+1)
		k, v := binary.BigEndian.Uint32(m[at:]), binary.BigEndian.Uint32(m[at+4:])
		if _, dup := r.fwd[k]; dup || k == 0 || v == 0 {
			return nil, false
		}
		r.fwd[k] = v
		r.rev[v] = k
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
func originOf(n *pnode) (uint32, bool) {
	if n.page != 0 {
		return n.page, true
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
