//! Incremental free-list reclamation (spec/design/incremental-reclaim.md; format.md *Reclamation*).
//! Each file commit lists the pages it orphans in its own persisted free list, so dead pages do not
//! wait for a reachability rebuild.
//!
//! The rules (design doc §3):
//!   - commit T lists its orphans in its own free list, but never reuses them itself: they are
//!     reachable from T's fallback meta (T−1);
//!   - from T+1 they are reusable, subject to the reader watermark: in memory each listed set keeps
//!     its generation (T) until no reader pins a version older than T ([`IncrState::gens`]);
//!   - under synchronous = off an orphan that was written inside the pending batch is in the floor's
//!     manifest, so it is held in memory and listed only by the first commit planned with nothing
//!     pending ([`IncrState::held`]); the batch reuse rule (only pages free at the floor) does the
//!     rest;
//!   - a rare rebuild still runs, triggered by the exact count of unlisted dead pages
//!     ([`IncrState::leaked`]).
//!
//! Only a file's owner (or an alone process) reclaims incrementally; a page client's commit is
//! planned against its owner's free list and leaves its orphans unlisted. In-memory databases and
//! temp domains keep their post-commit rebuild (`Storage::maybe_compact`).

use std::collections::HashSet;

use crate::format::{CommitPlan, IncrementalWrite};

/// A file storage's incremental-reclamation state. Nothing in it is persisted: it is rebuilt at open
/// from what the load reads anyway, and its loss (a role change, a foreign commit) only leaks pages
/// until the leak-triggered rebuild.
#[derive(Clone, Debug, Default)]
pub(crate) struct IncrState {
    /// Whether `catalog`, `gist` and `manifest` name the committed state's catalog chain, GiST
    /// R-tree pages and manifest-overflow pages. A page client's commit leaves them unknown (the
    /// owner did not plan it); the next owner commit then leaks them.
    pub(crate) known: bool,
    pub(crate) catalog: Vec<u32>,
    pub(crate) gist: Vec<u32>,
    pub(crate) manifest: Vec<u32>,
    /// How many manifest-overflow pages the committed meta names (known or not).
    pub(crate) manifest_count: u32,
    /// The free pages listed since the base generation (the storage's `free_gen_txid`), oldest
    /// first, each tagged with the version from which it is dead.
    pub(crate) gens: Vec<FreeGen>,
    /// The orphans written inside the pending relaxed batch (validated-cow.md §4), sorted.
    pub(crate) held: Vec<u32>,
    /// The unlisted dead pages the state started with (a reopen loses the held pages and a one-entry
    /// list; a page client's commit leaks its orphans). The simulator's growth oracle allows them.
    pub(crate) tolerated: i64,
    pub(crate) unknown_before: bool,
    /// The B-tree / overflow orphans of the commit being planned (`admit_file` computes them with the
    /// live-page delta, [`crate::format::LiveCount::after_ids`]).
    pub(crate) tree: Vec<u32>,
}

/// What `plan_free_list` needs of an incremental commit: the orphans to list (never reused by this
/// commit) and whether the leak trigger fires.
pub(crate) struct IncrPlan {
    pub(crate) listed: Vec<u32>,
    pub(crate) due: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct FreeGen {
    pub(crate) txid: u64,
    /// Sorted.
    pub(crate) pages: Vec<u32>,
}

/// The leak trigger (incremental-reclaim.md §3.5): a rebuild runs once the unlisted dead pages
/// exceed `compact_min_pages` and `live / reclaim_leak_den` (shared data, `spec/cost/schedule.toml`).
pub(crate) fn leak_due(leaked: i64, live: u32) -> bool {
    use crate::costs::{COMPACT_MIN_PAGES, RECLAIM_LEAK_DEN};
    leaked > COMPACT_MIN_PAGES as i64 && (leaked as u64) * RECLAIM_LEAK_DEN > u64::from(live)
}

impl IncrState {
    /// The state of a freshly created file: every body page `[first_body, page_count)` of the
    /// from-scratch image is a catalog page.
    pub(crate) fn fresh(first_body: u32, page_count: u32) -> IncrState {
        IncrState {
            known: true,
            catalog: (first_body..page_count).collect(),
            ..IncrState::default()
        }
    }

    /// Fold every generation no reader can still observe into the base generation, raising
    /// `free_gen_txid` to the largest merged txid.
    pub(crate) fn merge(&mut self, oldest: u64, free_gen_txid: &mut u64) {
        let mut n = 0;
        while n < self.gens.len() && self.gens[n].txid <= oldest {
            *free_gen_txid = (*free_gen_txid).max(self.gens[n].txid);
            n += 1;
        }
        if n > 0 {
            self.gens.drain(..n);
        }
    }

    /// The listed pages a reader may still observe: those of the unmerged generations; `None` when
    /// every generation is merged.
    pub(crate) fn newer(&self) -> Option<HashSet<u32>> {
        if self.gens.is_empty() {
            return None;
        }
        Some(
            self.gens
                .iter()
                .flat_map(|g| g.pages.iter().copied())
                .collect(),
        )
    }

    /// The exact count of dead pages no list holds, in the committed state: every page below the
    /// high-water is a meta or key-block page, live, free, a free-list chain page, a manifest-overflow
    /// page of the committed meta, held, or leaked.
    pub(crate) fn leaked(
        &self,
        page_count: u32,
        first_body: u32,
        free: usize,
        chain: usize,
        live: u32,
    ) -> i64 {
        i64::from(page_count)
            - i64::from(first_body)
            - free as i64
            - i64::from(live)
            - chain as i64
            - i64::from(self.manifest_count)
            - self.held.len() as i64
    }

    /// What the commit of `write` orphans, split into the pages its free list may list now and those
    /// written inside the pending batch, which are held (`pending` is the batch's pending set). A
    /// commit planned with nothing pending also lists the held pages. `free_chain` is the committed
    /// free list's chain pages.
    pub(crate) fn orphans(
        &self,
        free_chain: &[u32],
        pending: &[(u32, u64)],
        write: &crate::format::IncrementalWrite,
    ) -> (Vec<u32>, Vec<u32>) {
        let mut all = self.tree.clone();
        if self.known {
            // The previous catalog chain is orphaned whole unless this commit kept it unrewritten
            // (PROTOTYPE, deferred-interior.md §9.1).
            if !write.catalog_kept {
                all.extend_from_slice(&self.catalog);
            }
            all.extend_from_slice(&self.gist);
            all.extend_from_slice(&self.manifest);
        }
        all.extend_from_slice(free_chain);
        if pending.is_empty() {
            all.extend_from_slice(&self.held);
        }
        // PROTOTYPE (deferred-interior.md §9.5): a retained interior is still live at the page the
        // previous snapshot's node holds, which the orphan walk reaches because it must descend
        // through that node to the leaves and chains orphaned underneath.
        if !write.retained_ids.is_empty() {
            let retained: HashSet<u32> = write.retained_ids.iter().copied().collect();
            all.retain(|p| !retained.contains(p));
        }
        all.sort_unstable();
        for w in all.windows(2) {
            assert!(
                w[0] != w[1],
                "incremental reclaim: page {} orphaned twice",
                w[0]
            );
        }
        if pending.is_empty() {
            return (all, Vec::new());
        }
        let in_batch: HashSet<u32> = pending.iter().map(|(index, _)| *index).collect();
        let (held, listable): (Vec<u32>, Vec<u32>) =
            all.into_iter().partition(|p| in_batch.contains(p));
        (listable, held)
    }

    /// Record a published incremental commit at `txid`. `page_count` is the high-water before it.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn after(
        &mut self,
        txid: u64,
        write: &IncrementalWrite,
        plan: &CommitPlan,
        listed: Vec<u32>,
        held: Vec<u32>,
        pending: &[(u32, u64)],
        page_count: u32,
    ) {
        self.unknown_before = !self.known;
        if plan.rebuilt {
            self.tolerated = 0;
            // The rebuild listed every dead page except the batch's: only those stay held.
            self.gens.clear();
            let in_batch: HashSet<u32> = pending.iter().map(|(index, _)| *index).collect();
            self.held = merge_sorted(std::mem::take(&mut self.held), held)
                .into_iter()
                .filter(|p| in_batch.contains(p))
                .collect();
        } else {
            if !listed.is_empty() {
                self.gens.push(FreeGen {
                    txid,
                    pages: listed,
                });
            }
            if pending.is_empty() {
                self.held.clear();
            } else {
                self.held = merge_sorted(std::mem::take(&mut self.held), held);
            }
        }
        if plan.page_count < page_count {
            trim_gens(&mut self.gens, plan.page_count);
        }
        if !write.catalog_kept {
            self.catalog = write.catalog_ids.clone();
        }
        self.gist = write.gist_ids.clone();
        self.manifest = plan.manifest.iter().map(|(index, _)| *index).collect();
        self.manifest_count = self.manifest.len() as u32;
        self.known = true;
        self.tree.clear();
    }

    /// Record a commit this storage did not plan (a page client's, applied by its owner): the listed
    /// pages that survive keep their generations; the wholesale pages of the new state are unknown.
    pub(crate) fn foreign(&mut self, free: &[u32], manifest_pages: u32) {
        let listed: HashSet<u32> = free.iter().copied().collect();
        let mut gens = Vec::new();
        for g in &self.gens {
            let keep: Vec<u32> = g
                .pages
                .iter()
                .copied()
                .filter(|p| listed.contains(p))
                .collect();
            if !keep.is_empty() {
                gens.push(FreeGen {
                    txid: g.txid,
                    pages: keep,
                });
            }
        }
        self.gens = gens;
        self.known = false;
        self.catalog.clear();
        self.gist.clear();
        self.manifest.clear();
        self.manifest_count = manifest_pages;
        self.tree.clear();
    }
}

/// Narrow a reuse restriction (`None`: every free page) by the newer generations.
pub(crate) fn exclude_newer(
    allowed: Option<&HashSet<u32>>,
    newer: &HashSet<u32>,
    free: &[u32],
) -> HashSet<u32> {
    free.iter()
        .copied()
        .filter(|p| allowed.is_none_or(|a| a.contains(p)) && !newer.contains(p))
        .collect()
}

/// Drop every generation page at or past the new high-water (trailing truncation never cuts one, so
/// this is a precaution, incremental-reclaim.md §3.6).
fn trim_gens(gens: &mut Vec<FreeGen>, page_count: u32) {
    for g in gens.iter_mut() {
        g.pages.retain(|&p| p < page_count);
    }
    gens.retain(|g| !g.pages.is_empty());
}

/// Merge two ascending lists that share no page.
pub(crate) fn merge_sorted(a: Vec<u32>, b: Vec<u32>) -> Vec<u32> {
    if b.is_empty() {
        return a;
    }
    if a.is_empty() {
        return b;
    }
    let mut out = Vec::with_capacity(a.len() + b.len());
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => {
                out.push(a[i]);
                i += 1;
            }
            std::cmp::Ordering::Greater => {
                out.push(b[j]);
                j += 1;
            }
            std::cmp::Ordering::Equal => {
                panic!("incremental reclaim: page {} listed twice", a[i])
            }
        }
    }
    out.extend_from_slice(&a[i..]);
    out.extend_from_slice(&b[j..]);
    out
}
