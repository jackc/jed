//! The owner's validation of an incoming CommitDelta (spec/design/page-service.md §5.6;
//! spec/pageservice/README.md §3.5). It is structural and cheap: it catches client bugs, not malice
//! (page-service.md §13). Every check runs before the page it guards is written, so a rejected delta
//! leaves only unreachable orphans — pages beyond the committed high-water or in the granted free
//! list — and never touches a page any pinned snapshot or the fallback meta can reach.

use std::collections::{HashMap, HashSet};

use crate::error::{EngineError, Result, SqlState};
use crate::format;
use crate::pageservice::codec::{CommitBegin, IndexedPage};

/// The owner state a delta is checked against, fixed at `begin_write`.
#[derive(Clone)]
pub(crate) struct DeltaBase {
    pub(crate) committed_txid: u64,
    pub(crate) page_size: u32,
    /// The committed high-water.
    pub(crate) page_count: u32,
    /// The reusable free pages when the reuse grant passed; `None` otherwise.
    pub(crate) granted: Option<HashSet<u32>>,
}

impl DeltaBase {
    /// Whether a delta may write `index`: a body page beyond the committed high-water or a granted
    /// free page, below the new page count.
    pub(crate) fn legal(&self, index: u32, new_page_count: u32) -> bool {
        index >= 2
            && index < new_page_count
            && (index >= self.page_count
                || self
                    .granted
                    .as_ref()
                    .is_some_and(|granted| granted.contains(&index)))
    }
}

/// One incoming CommitDelta after `commit_begin` passed: the meta and manifest, and which listed
/// pages have arrived.
pub(crate) struct Delta {
    pub(crate) txid: u64,
    pub(crate) meta: Vec<u8>,
    /// The new meta's page count and free-list head.
    pub(crate) page_count: u32,
    pub(crate) free_list_head: u32,
    /// Every index the manifest lists, with the CRC-64 of its bytes.
    expected: HashMap<u32, u64>,
    received: HashSet<u32>,
    /// The overflow chain, in chain order; it arrives whole in `commit_begin`.
    pub(crate) manifest: Vec<IndexedPage>,
    /// Every index the commit depends on (listed and manifest pages): the pager's validated set
    /// once the commit is published, so the owner's reload need not re-read them.
    pub(crate) protected: HashSet<u32>,
    /// Every index the delta writes, ascending: the invalidation window's entry.
    pub(crate) written: Vec<u32>,
}

fn invalid(reason: impl std::fmt::Display) -> EngineError {
    EngineError::new(
        SqlState::ProtocolViolation,
        format!("invalid commit delta: {reason}"),
    )
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(b[at..at + 4].try_into().unwrap())
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_be_bytes(b[at..at + 8].try_into().unwrap())
}

/// The running state of listing a manifest's entries.
struct Listing {
    last: u32,
    root_seen: bool,
    free_head_seen: bool,
    listed: u32,
}

impl Listing {
    /// List `count` 12-byte manifest entries from `entries` into `delta`.
    fn list(
        &mut self,
        base: &DeltaBase,
        root_page: u32,
        delta: &mut Delta,
        entries: &[u8],
        count: usize,
    ) -> Result<()> {
        for entry in entries[..count * 12].as_chunks::<12>().0 {
            let index = u32_at(entry, 0);
            if index <= self.last {
                return Err(invalid("manifest indices are not strictly ascending"));
            }
            if !base.legal(index, delta.page_count) {
                return Err(invalid(format!(
                    "page {index} is neither beyond the high-water nor a granted free page"
                )));
            }
            self.last = index;
            delta.expected.insert(index, u64_at(entry, 4));
            delta.protected.insert(index);
            self.root_seen |= index == root_page;
            self.free_head_seen |= index == delta.free_list_head;
            self.listed += 1;
        }
        Ok(())
    }
}

/// Run the checks before any page is written (README.md §3.5 check A): the meta, then the manifest
/// chain and every listed index.
pub(crate) fn begin_delta(base: &DeltaBase, cb: CommitBegin) -> Result<Delta> {
    if cb.txid != base.committed_txid + 1 {
        return Err(invalid(format!(
            "txid {} does not follow the committed txid {}",
            cb.txid, base.committed_txid
        )));
    }
    if cb.meta.len() != base.page_size as usize {
        return Err(invalid("the meta page has the wrong size"));
    }
    let Some(m) = format::meta_fields(&cb.meta) else {
        return Err(invalid("the meta page does not parse"));
    };
    if m.txid != cb.txid {
        return Err(invalid("the meta page names another txid"));
    }
    if m.page_count < base.page_count {
        return Err(invalid("the page count shrinks"));
    }
    if m.dirty_count == 0 {
        return Err(invalid("the manifest lists no page"));
    }
    if cb.manifest_pages.len() != m.manifest_pages as usize {
        return Err(invalid(
            "commit_begin carries the wrong number of manifest pages",
        ));
    }
    let ps = base.page_size as usize;
    let (inline_capacity, overflow_capacity) = format::manifest_capacities(ps);
    let mut delta = Delta {
        txid: cb.txid,
        meta: cb.meta,
        page_count: m.page_count,
        free_list_head: m.free_list_head,
        expected: HashMap::with_capacity(m.dirty_count as usize),
        received: HashSet::with_capacity(m.dirty_count as usize),
        manifest: Vec::with_capacity(cb.manifest_pages.len()),
        protected: HashSet::with_capacity(m.dirty_count as usize + cb.manifest_pages.len()),
        written: Vec::new(),
    };
    let mut listing = Listing {
        last: 1,
        root_seen: false,
        free_head_seen: m.free_list_head == 0,
        listed: 0,
    };
    let meta = delta.meta.clone();
    listing.list(
        base,
        m.root_page,
        &mut delta,
        &meta[64..],
        (m.dirty_count as usize).min(inline_capacity),
    )?;
    let mut id = m.manifest_head;
    let mut chain_crc = 0u64;
    for (ordinal, page) in cb.manifest_pages.into_iter().enumerate() {
        if page.index != id || !base.legal(id, m.page_count) || delta.protected.contains(&id) {
            return Err(invalid("the manifest chain names an illegal page"));
        }
        let block = page.page;
        let count = ((m.dirty_count - listing.listed) as usize).min(overflow_capacity);
        let header = (block.len() == ps)
            .then(|| format::page_header(&block).ok())
            .flatten();
        let Some(header) = header else {
            return Err(invalid("a manifest page is malformed"));
        };
        if header.page_type != format::PAGE_MANIFEST_TYPE
            || block[1..4] != [0; 3]
            || header.item_count as usize != count
            || u64_at(&block, 16) != m.txid
            || u32_at(&block, 24) as usize != ordinal
            || block[28..32] != [0; 4]
            || block[32 + 12 * count..].iter().any(|b| *b != 0)
        {
            return Err(invalid("a manifest page is malformed"));
        }
        chain_crc = format::commit_crc64(chain_crc, &id.to_be_bytes());
        chain_crc = format::commit_crc64(chain_crc, &block);
        delta.protected.insert(id);
        listing.list(base, m.root_page, &mut delta, &block[32..], count)?;
        delta.manifest.push(IndexedPage {
            index: id,
            page: block,
        });
        id = header.next_page;
    }
    if id != 0 || chain_crc != m.manifest_crc {
        return Err(invalid("the manifest chain does not match the meta"));
    }
    if listing.listed != m.dirty_count || !listing.root_seen || !listing.free_head_seen {
        return Err(invalid(
            "the manifest does not cover the catalog root and free-list head",
        ));
    }
    delta.written = delta.protected.iter().copied().collect();
    delta.written.sort_unstable();
    Ok(delta)
}

impl Delta {
    /// Check B before one body page is written: it is listed, not yet received, and matches its
    /// manifest identity.
    pub(crate) fn check_page(&mut self, page: &IndexedPage) -> Result<()> {
        let index = page.index;
        let Some(&crc) = self.expected.get(&index) else {
            return Err(invalid(format!(
                "page {index} is not listed in the manifest"
            )));
        };
        if self.received.contains(&index) {
            return Err(invalid(format!("page {index} arrived twice")));
        }
        let b = &page.page;
        if b.len() != self.meta.len()
            || !(1..=7).contains(&b[0])
            || format::page_crc(b) != u32_at(b, 12)
            || format::commit_crc64(0, b) != crc
        {
            return Err(invalid(format!(
                "page {index} does not match its manifest identity"
            )));
        }
        self.received.insert(index);
        Ok(())
    }

    /// Check C at `commit_end`: every listed page arrived.
    pub(crate) fn complete(&self) -> Result<()> {
        if self.received.len() != self.expected.len() {
            return Err(invalid("commit_end before every listed page arrived"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PS: u32 = 512;

    /// A valid CommitDelta over a committed state of `committed_pages` pages: dirty pages at
    /// `indices` (the first is the catalog root), its meta, and its manifest chain.
    fn test_delta(committed_pages: u32, indices: &[u32]) -> (CommitBegin, Vec<IndexedPage>) {
        let ps = PS as usize;
        let mut page_count = committed_pages;
        let mut dirty = Vec::new();
        for (i, &index) in indices.iter().enumerate() {
            let page_type = if i == 0 { 1 } else { 2 }; // catalog, then leaves
            dirty.push((
                index,
                format::test_page(ps, page_type, &[i as u8, (i >> 8) as u8, 7]),
            ));
            page_count = page_count.max(index + 1);
        }
        let (inline, per) = format::manifest_capacities(ps);
        let overflow = dirty.len().saturating_sub(inline).div_ceil(per);
        let mut ids = Vec::new();
        for _ in 0..overflow {
            ids.push(page_count);
            page_count += 1;
        }
        let meta = format::meta_page(PS, 6, indices[0], page_count, 0, indices.len() as u32);
        let (meta, chain) = format::encode_manifest(meta, &dirty, &ids);
        let cb = CommitBegin {
            txid: 6,
            meta,
            manifest_pages: chain
                .into_iter()
                .map(|(index, page)| IndexedPage { index, page })
                .collect(),
        };
        let body = dirty
            .into_iter()
            .map(|(index, page)| IndexedPage { index, page })
            .collect();
        (cb, body)
    }

    /// Run the owner's three checks over a delta, returning the first failure.
    fn apply_delta(base: &DeltaBase, cb: CommitBegin, body: &[IndexedPage]) -> Result<()> {
        let mut delta = begin_delta(base, cb)?;
        for page in body {
            if !base.legal(page.index, delta.page_count) {
                return Err(invalid("accepted an illegal page"));
            }
            delta.check_page(page)?;
        }
        delta.complete()
    }

    #[test]
    fn delta_validation_accepts_a_valid_delta_and_refuses_every_fault() {
        let base = DeltaBase {
            committed_txid: 5,
            page_size: PS,
            page_count: 10,
            granted: Some(HashSet::from([3])),
        };
        // 40 dirty pages overflow the 512-byte meta's 37 inline entries into one manifest page.
        let mut indices = vec![12, 3];
        let mut next = 13;
        while indices.len() < 40 {
            indices.push(next);
            next += 1;
        }
        let (cb, body) = test_delta(10, &indices);
        assert_eq!(cb.manifest_pages.len(), 1);
        apply_delta(&base, cb.clone(), &body).expect("a valid delta is accepted");

        type Mutation = fn(&mut DeltaBase, &mut CommitBegin, &mut Vec<IndexedPage>);
        let cases: Vec<(&str, Mutation)> = vec![
            ("txid does not follow", |_, cb, _| cb.txid = 7),
            ("committed txid moved", |b, _, _| b.committed_txid = 4),
            ("no reuse grant", |b, _, _| b.granted = None),
            ("page count shrinks", |b, _, _| b.page_count = 80),
            ("page size differs", |b, _, _| b.page_size = 1024),
            ("manifest page missing", |_, cb, _| {
                cb.manifest_pages.clear()
            }),
            ("body page missing", |_, _, body| {
                body.remove(0);
            }),
            ("body page twice", |_, _, body| {
                let first = body[0].clone();
                body.push(first);
            }),
            ("unlisted page", |_, _, body| body[0].index = 99),
            ("page swapped", |_, _, body| {
                let first = body[0].page.clone();
                body[0].page = body[1].page.clone();
                body[1].page = first;
            }),
        ];
        for (name, mutate) in cases {
            let mut b = base.clone();
            let mut c = cb.clone();
            let mut pages = body.clone();
            mutate(&mut b, &mut c, &mut pages);
            match apply_delta(&b, c, &pages) {
                Err(e) if e.state == SqlState::ProtocolViolation => {}
                other => panic!("{name}: got {:?}, want 08P01", other.err()),
            }
        }

        // Every single-byte change to the meta, the manifest, or a body page is caught: each is
        // covered by a CRC the owner checks before writing anything it guards.
        let mut rng: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = |n: usize| {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            (rng % n as u64) as usize
        };
        for trial in 0..3000 {
            let mut c = cb.clone();
            let mut pages = body.clone();
            let target = match next(3) {
                0 => &mut c.meta,
                1 => &mut c.manifest_pages[0].page,
                _ => {
                    let i = next(pages.len());
                    &mut pages[i].page
                }
            };
            let at = next(target.len());
            target[at] ^= (1 + next(255)) as u8;
            assert!(
                apply_delta(&base, c, &pages).is_err(),
                "trial {trial}: a corrupted delta passed validation"
            );
        }
    }
}
