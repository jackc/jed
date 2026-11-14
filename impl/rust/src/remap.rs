//! PROTOTYPE: deferred interior pages (spec/design/deferred-interior.md §9), behind
//! `JED_DEFER_INTERIOR=on` and without a version bump, mirroring the Go core's `remap.go` function for
//! function so the two write byte-identical files.
//!
//! A commit writes only the nodes whose own content changed. An interior whose only change is which
//! page a child lives at keeps its page, and the meta records "in interior page P, the child pointer
//! in slot i now names page C" in a remap list. Readers apply it as they read an interior's child
//! pointers. When the list would overflow the commit folds: every stale interior and its ancestors
//! are written fresh and the list empties. An entry dies in the commit that orphans its parent, so no
//! page is held off the free list (§9.5).
use super::*;
use std::collections::BTreeMap;

/// One entry's bytes (§9.2): parent u32, slot u16, page u32, and an encrypted file's tag of page.
pub(crate) fn entry_size(encrypted: bool) -> usize {
    if encrypted {
        10 + crate::crypto::TAG_LEN
    } else {
        10
    }
}

/// The meta's manifest area `[lo, hi)`: after the header (and an encrypted meta's tags), before an
/// encrypted meta's reserve.
pub(crate) fn area(ps: usize, encrypted: bool) -> (usize, usize) {
    if encrypted {
        (96, ps - crate::crypto::RESERVE)
    } else {
        (64, ps)
    }
}

/// The entries a meta carries: one third of the manifest area at every page size (§9.2, §9.7).
pub(crate) fn capacity(ps: usize, encrypted: bool) -> usize {
    let (lo, hi) = area(ps, encrypted);
    ((hi - lo) / 3) / entry_size(encrypted)
}

/// Whether a file of page size `ps` uses the deferred-interior layout. The layout is fixed by the
/// format, not the meta: there is no marker, and a slot with no entries is a zero region.
pub(crate) fn defer_on(ps: usize, encrypted: bool) -> bool {
    commit_manifest::defer_interior() && capacity(ps, encrypted) > 0
}

/// The bytes the region takes from the end of the manifest area: 0 without the layout.
pub(crate) fn reserve(ps: usize, encrypted: bool) -> usize {
    if defer_on(ps, encrypted) {
        entry_size(encrypted) * capacity(ps, encrypted)
    } else {
        0
    }
}

/// The remap list, keyed by `(parent page, slot)` and ordered as it is stored.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct RemapState {
    entries: BTreeMap<(u32, u16), PageRef>,
}

impl RemapState {
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Slot `slot` of the retained interior `parent` now resolves to `cur`; an existing entry is
    /// overwritten, so a child that moves repeatedly under one parent occupies one entry forever.
    pub(crate) fn set(&mut self, parent: u32, slot: u16, cur: PageRef) {
        self.entries.insert((parent, slot), cur);
    }

    pub(crate) fn has(&self, parent: u32, slot: u16) -> bool {
        self.entries.contains_key(&(parent, slot))
    }

    /// Forget every entry `parent` keys: the commit retires that parent, so its page is about to be
    /// orphaned and must never be read through a surviving entry (§9.5).
    pub(crate) fn drop_parent(&mut self, parent: u32) {
        let keys: Vec<_> = self
            .entries
            .range((parent, 0)..=(parent, u16::MAX))
            .map(|(k, _)| *k)
            .collect();
        for k in keys {
            self.entries.remove(&k);
        }
    }

    /// Whether interior page `parent` keys any entry -- is stale.
    pub(crate) fn keys(&self, parent: u32) -> bool {
        self.entries
            .range((parent, 0)..=(parent, u16::MAX))
            .next()
            .is_some()
    }

    /// The pages that key an entry.
    pub(crate) fn parents(&self) -> HashSet<u32> {
        self.entries.keys().map(|(p, _)| *p).collect()
    }

    /// Every entry, in `(parent, slot)` order.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (u32, u16, PageRef)> + '_ {
        self.entries.iter().map(|(&(p, s), r)| (p, s, *r))
    }

    /// Redirect the `n + 1` child pointers just read from interior page `parent` (§9.1: one lookup per
    /// slot, never transitive). An entry naming a slot past the parent's item count is `XX001`.
    pub(crate) fn resolve(&self, parent: u32, children: &mut [PageRef]) -> Result<()> {
        let mut range = self
            .entries
            .range((parent, 0)..=(parent, u16::MAX))
            .peekable();
        if range.peek().is_none() {
            return Ok(());
        }
        for (&(_, slot), to) in range {
            let Some(child) = children.get_mut(slot as usize) else {
                return Err(corrupt(
                    "remap entry names a slot past its parent's item count",
                ));
            };
            *child = *to;
        }
        Ok(())
    }

    /// Write the list into meta page `m` (§9.2): `remap_entries` at offset 60, the entries ascending
    /// from the start of the region, the rest of the region zero. Before the meta's checksum is
    /// computed and before an encrypted meta is sealed (the seal covers the region).
    pub(crate) fn encode(&self, m: &mut [u8]) {
        let encrypted = m[6] != 0;
        let ps = m.len();
        let (_, hi) = area(ps, encrypted);
        let size = entry_size(encrypted);
        let mut at = hi - reserve(ps, encrypted);
        m[at..hi].fill(0);
        m[60..64].copy_from_slice(&(self.entries.len() as u32).to_be_bytes());
        for (&(parent, slot), to) in &self.entries {
            m[at..at + 4].copy_from_slice(&parent.to_be_bytes());
            m[at + 4..at + 6].copy_from_slice(&slot.to_be_bytes());
            m[at + 6..at + 10].copy_from_slice(&to.page.to_be_bytes());
            if encrypted {
                m[at + 10..at + size].copy_from_slice(&to.tag);
            }
            at += size;
        }
    }

    /// Read and validate meta page `m`'s list (§9.3), `first` and `page_count` being the meta's body
    /// bounds: `None` when the slot is structurally invalid. Without the layout offset 60 is reserved
    /// zero, as through v52.
    pub(crate) fn decode(m: &[u8], first: u32, page_count: u32) -> Option<RemapState> {
        let count = u32::from_be_bytes(m[60..64].try_into().unwrap()) as usize;
        let encrypted = m[6] != 0;
        let ps = m.len();
        if !defer_on(ps, encrypted) {
            return (count == 0).then(RemapState::default);
        }
        if count > capacity(ps, encrypted) {
            return None;
        }
        let (_, hi) = area(ps, encrypted);
        let size = entry_size(encrypted);
        let mut at = hi - reserve(ps, encrypted);
        let mut out = RemapState::default();
        let mut targets = HashSet::with_capacity(count);
        let mut prev: Option<(u32, u16)> = None;
        for _ in 0..count {
            let parent = u32::from_be_bytes(m[at..at + 4].try_into().unwrap());
            let slot = u16::from_be_bytes([m[at + 4], m[at + 5]]);
            let mut to = PageRef {
                page: u32::from_be_bytes(m[at + 6..at + 10].try_into().unwrap()),
                tag: crate::crypto::NO_TAG,
            };
            if encrypted {
                to.tag.copy_from_slice(&m[at + 10..at + size]);
            }
            if prev.is_some_and(|p| p >= (parent, slot))
                || parent < first
                || parent >= page_count
                || to.page < first
                || to.page >= page_count
                || to.page == parent
                || !targets.insert(to.page)
            {
                return None;
            }
            prev = Some((parent, slot));
            out.entries.insert((parent, slot), to);
            at += size;
        }
        m[at..hi].iter().all(|b| *b == 0).then_some(out)
    }
}

/// The committed catalog chain, kept so a commit whose catalog bytes are unchanged does not rewrite
/// it (§9.1 scope).
#[derive(Clone, Debug)]
pub(crate) struct CatState {
    pub(crate) payloads: Vec<Vec<u8>>,
    pub(crate) root: PageRef,
}

/// One commit's deferral state, decided before allocation (§9.4).
pub(crate) struct DeferCtx {
    /// The entry set this commit publishes: the committed list minus every entry whose parent the
    /// commit retires; serialization adds an entry for each moved child of a retained interior.
    pub(crate) remap: RemapState,
    /// The persisted page of every interior that keeps its page.
    pub(crate) retain: HashSet<u32>,
    /// The retained pages (counted back into the live-page delta), in serialization order.
    pub(crate) retained: u32,
    pub(crate) retained_ids: Vec<u32>,
    /// The page that blocks trailing truncation, which this commit moves down (§9.6; 0: none).
    pub(crate) migrate: u32,
    pub(crate) capacity: usize,
}

/// §9.6: the top page of the file when it is not free and some lower page is free, else 0.
pub(crate) fn migrate_hint(free: &[u32], high: u32) -> u32 {
    if high < 3 {
        return 0;
    }
    let top = high - 1;
    let mut lower = false;
    for &p in free {
        if p == top {
            return 0; // already free: the last commit's trailing run will take it
        }
        lower = lower || p < top;
    }
    if lower { top } else { 0 }
}

/// §9.4 step 2 for a dirty interior: it replaces a persisted page one for one (a), its separators are
/// still that page's (b), and its page is not the migration page (c).
fn retainable(n: &Node, migrate: u32) -> bool {
    n.origin.page != 0 && n.same_keys && n.origin.page != migrate
}

/// Decide retention under `n` without allocating, recording what serialization will: the clean pages
/// it keeps, the interiors that keep their page, and the `(parent, slot)` of each moved child of a
/// retained interior. Reports whether `n` gets a fresh page.
fn plan_node(
    n: &Node,
    migrate: u32,
    kept: &mut HashSet<u32>,
    retain: &mut HashSet<u32>,
    adds: &mut Vec<(u32, u16)>,
) -> bool {
    let page = n.page_ref().page;
    if page != 0 {
        kept.insert(page);
        return false;
    }
    if n.children.is_empty() {
        return true; // a dirty leaf is always written fresh
    }
    let mut moved = Vec::new();
    for (i, c) in n.children.iter().enumerate() {
        match c {
            Child::OnDisk(p) => {
                kept.insert(p.page);
            }
            Child::Resident(c) => {
                if plan_node(c, migrate, kept, retain, adds) {
                    moved.push(i);
                }
            }
        }
    }
    if !retainable(n, migrate) {
        return true;
    }
    retain.insert(n.origin.page);
    adds.extend(moved.into_iter().map(|i| (n.origin.page, i as u16)));
    false
}

/// Walk the previous snapshot's subtree under `n` as the live-page delta's orphan walk does
/// (`orphaned_node_pages`), skipping every subtree the new snapshot keeps, and record each interior it
/// reaches that keys an entry: the parents this commit retires or retains.
fn visit_retired(n: &Node, kept: &HashSet<u32>, parents: &HashSet<u32>, out: &mut HashSet<u32>) {
    let page = n.page_ref().page;
    if page == 0 || kept.contains(&page) || n.children.is_empty() {
        return;
    }
    if parents.contains(&page) {
        out.insert(page);
    }
    for c in &n.children {
        if let Child::Resident(c) = c {
            visit_retired(c, kept, parents, out);
        }
    }
}

/// §9.4 before allocation: retention, the entry set, and the fold test. `e0` is the committed list
/// and `prev` the committed snapshot it describes. `None` when the commit must fold. The drop rule
/// (§9.5) comes from the walk the live-page delta makes: a committed parent it reaches that the commit
/// does not retain is retired in this commit, the one that orphans its page.
pub(crate) fn plan_deferral(
    snap: &Snapshot,
    prev: Option<&Snapshot>,
    e0: &RemapState,
    migrate: u32,
    capacity: usize,
) -> Option<DeferCtx> {
    let mut kept = HashSet::new();
    let mut retain = HashSet::new();
    let mut adds = Vec::new();
    snap.each_committed_tree(&mut |root| {
        plan_node(root, migrate, &mut kept, &mut retain, &mut adds);
    });
    let mut e = e0.clone();
    if !e.is_empty() {
        // Without the committed snapshot the drop rule cannot run: fold rather than guess.
        let prev = prev?;
        let parents = e0.parents();
        let mut reached = HashSet::new();
        for st in prev.stores_iter() {
            if let Some(root) = st.tree_root() {
                visit_retired(root, &kept, &parents, &mut reached);
            }
        }
        for st in prev.index_stores_iter() {
            if let Some(root) = st.tree_root() {
                visit_retired(root, &kept, &parents, &mut reached);
            }
        }
        for p in reached {
            if !retain.contains(&p) {
                e.drop_parent(p);
            }
        }
    }
    let size = e.len() + adds.iter().filter(|(p, s)| !e.has(*p, *s)).count();
    if size > capacity {
        return None;
    }
    Some(DeferCtx {
        remap: e,
        retain,
        retained: 0,
        retained_ids: Vec::new(),
        migrate,
        capacity,
    })
}

/// A copy of `n`'s subtree in which every stale interior -- one whose page keys an entry in
/// `parents` -- and every ancestor of one is a dirty node the commit writes fresh; `None` when nothing
/// under `n` is stale. The fold (§9.4).
pub(crate) fn unstale(n: &Arc<Node>, parents: &HashSet<u32>) -> Option<Arc<Node>> {
    if n.children.is_empty() {
        return None;
    }
    let mut children: Option<Vec<Child>> = None;
    for (i, c) in n.children.iter().enumerate() {
        let Child::Resident(c) = c else { continue };
        let Some(r) = unstale(c, parents) else {
            continue;
        };
        children.get_or_insert_with(|| n.children.clone())[i] = Child::Resident(r);
    }
    let page = n.page_ref().page;
    let stale = page != 0 && parents.contains(&page);
    if children.is_none() && !stale {
        return None;
    }
    if page == 0 && children.is_none() {
        return None; // a dirty node with no stale descendant: written fresh anyway
    }
    let children = children.unwrap_or_else(|| n.children.clone());
    Some(Node::new_interior_fresh(n.keys.clone(), children))
}
