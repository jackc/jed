//! The deterministic-simulation integrity checkers (spec/design/simulation-testing.md §8.3), a port
//! of the Go reference (`impl/go/sim_check_test.go`).
//!
//! The image checker opens a copy of the disk image the way a new process would — so the meta
//! selection and manifest validation of validated-cow.md §1 run first — checks it holds the expected
//! version, and then runs the engine's integrity check (spec/design/integrity-check.md) at the `full`
//! level with no finding limit over the selected generation: reachability and the live page count,
//! page verification and node structure, key order and primary-key placement, row counts, overflow
//! chain exclusivity, the free list against the reachable set and the manifest's dependencies, and
//! each column index against its table. Any finding fails the image.

use crate::error::{EngineError, SqlState};
use crate::format::{chain_pages, collect_tree_pages, sim_open_with_gist, sim_selected_meta};
use crate::integrity::{IntegrityCheckOptions, IntegrityLevel};
use crate::integrity_kinds as kinds;
use crate::shared::Database;
use crate::value::{Unfetched, Value};
use std::collections::HashSet;

/// Why the image checker rejected an image: its message, and the engine error behind it, if any
/// (a replay step needs to tell an `XX001` from a structural mismatch).
pub(crate) struct Fail {
    msg: String,
    engine: Option<EngineError>,
}

impl Fail {
    fn msg(msg: String) -> Fail {
        Fail { msg, engine: None }
    }

    /// Wrap an engine error under `ctx` (`"ctx: error"`).
    fn engine(ctx: &str) -> impl Fn(EngineError) -> Fail + '_ {
        move |e| Fail {
            msg: format!("{ctx}: {e}"),
            engine: Some(e),
        }
    }
}

/// Check `image` holds version `want` and is structurally sound; returns the meta's live page
/// count, page high-water, and the loaded free list's length.
pub(crate) fn integrity(
    image: &[u8],
    want: u64,
    credential: Option<&crate::crypto::Credential>,
) -> Result<(u32, u32, u32), String> {
    integrity_checked(image, want, credential).map_err(|f| f.msg)
}

/// [`integrity`]'s verdict for a replay step: `None` when the image passes, else the engine error
/// that rejected it, or the message of a rejection no engine error caused.
pub(crate) fn integrity_error(
    image: &[u8],
    want: u64,
    credential: Option<&crate::crypto::Credential>,
) -> Option<Result<EngineError, String>> {
    match integrity_checked(image, want, credential) {
        Ok(_) => None,
        Err(Fail {
            engine: Some(e), ..
        }) => Some(Ok(e)),
        Err(f) => Some(Err(f.msg)),
    }
}

fn integrity_checked(
    image: &[u8],
    want: u64,
    credential: Option<&crate::crypto::Credential>,
) -> Result<(u32, u32, u32), Fail> {
    let (engine, paging, _) =
        sim_open_with_gist(image, credential).map_err(Fail::engine("open"))?;
    let mt = sim_selected_meta(&paging).map_err(Fail::engine("meta"))?;
    if mt.txid != want {
        return Err(Fail::msg(format!(
            "the image holds version {}, want {}",
            mt.txid, want
        )));
    }
    let generation = crate::format::integrity_generation(&paging, mt.txid)
        .map_err(Fail::engine("meta"))?
        .ok_or_else(|| Fail::msg("the selected meta slot does not hold its version".into()))?;
    let opts = IntegrityCheckOptions {
        level: IntegrityLevel::Full,
        max_findings: 0,
        constraints: false,
        cancel: None,
    };
    let report = crate::integrity::run(&engine.committed, &paging, generation, "main", &opts)
        .map_err(Fail::engine("integrity check"))?;
    if let Some(f) = report.findings.first() {
        let msg = format!(
            "{}{}{}: {}",
            f.kind,
            f.object
                .as_ref()
                .map_or(String::new(), |o| format!(" in {o}")),
            f.page.map_or(String::new(), |p| format!(" at page {p}")),
            f.detail
        );
        // A page that does not verify or decode is what an engine read reports as XX001; the
        // structural findings are not engine errors.
        let unreadable = [
            kinds::META_INVALID,
            kinds::MANIFEST_PAGE_MISMATCH,
            kinds::CATALOG_PAGE_CORRUPT,
            kinds::PAGE_CORRUPT,
            kinds::VALUE_UNDECODABLE,
            kinds::OVERFLOW_CHAIN_CORRUPT,
            kinds::FREE_LIST_CORRUPT,
        ];
        if unreadable.contains(&f.kind) {
            return Err(Fail::engine("integrity check")(EngineError::new(
                SqlState::DataCorrupted,
                msg,
            )));
        }
        return Err(Fail::msg(msg));
    }
    Ok((mt.live_pages, mt.page_count, engine.free_pages.len() as u32))
}

/// Check the LIVE process, not a fresh open of its image: every page its committed in-memory trees
/// reference — tree nodes, and the overflow chains reached through resident rows' unfetched
/// references as well as faulted leaves — must lie below the page high-water and off the in-memory
/// free list the next commit allocates from. A fresh open re-derives both from disk, so only this
/// view sees a resident node whose references went stale.
pub(crate) fn check_live(db: &Database) -> Result<(), String> {
    let state = db.sim_live_state();
    let snap = &state.committed;
    let page_count = state.page_count;
    let free: HashSet<u32> = state.free_pages.iter().copied().collect();
    let paging = state.paging.clone();
    let fetch = |p: crate::format::PageRef| paging.read_body(p);

    let check = |what: &str, pg: u32| -> Result<(), String> {
        if pg >= page_count {
            return Err(format!(
                "{what} references page {pg} past the high-water {page_count}"
            ));
        }
        if free.contains(&pg) {
            return Err(format!(
                "{what} references page {pg}, which is on the free list"
            ));
        }
        Ok(())
    };
    let sorted = |set: HashSet<u32>| {
        let mut v: Vec<u32> = set.into_iter().collect();
        v.sort();
        v
    };
    let mut claimed: HashSet<u32> = HashSet::new();
    for name in snap.sim_store_names() {
        let store = snap.sim_store(&name).expect("a listed store exists");
        let mut nodes = HashSet::new();
        if let Some(root) = store.tree_root() {
            collect_tree_pages(root, &mut nodes);
        }
        for pg in sorted(nodes) {
            check(&format!("table {name}'s tree"), pg)?;
        }
        let entries = store
            .iter_entries()
            .map_err(|e| format!("table {name}: {e}"))?;
        for (_, row) in &entries {
            for v in row {
                let Some((first, len)) = external_ref(v) else {
                    continue;
                };
                let pages = chain_pages(first, len as usize, &fetch, paging.cap())
                    .map_err(|e| format!("table {name}: an overflow chain: {e}"))?;
                for pg in pages {
                    check(&format!("table {name}'s overflow chain"), pg)?;
                    // Chain exclusivity (large-values.md §15): a kept chain moves to the
                    // rewritten record; two committed records never share one.
                    if !claimed.insert(pg) {
                        return Err(format!(
                            "table {name}: overflow page {pg} is referenced twice"
                        ));
                    }
                }
            }
        }
    }
    for name in snap.sim_index_store_names() {
        let ist = snap.sim_index_store(&name).expect("a listed store exists");
        let mut nodes = HashSet::new();
        if let Some(root) = ist.tree_root() {
            collect_tree_pages(root, &mut nodes);
        }
        for pg in sorted(nodes) {
            check(&format!("index {name}'s tree"), pg)?;
        }
    }
    Ok(())
}

/// The overflow chain a stored value names — its first page and the bytes the chain carries — or
/// `None` for an inline or resident value.
fn external_ref(v: &Value) -> Option<(crate::format::PageRef, u32)> {
    match v {
        Value::Unfetched(Unfetched::External { first, len, .. }) => Some((*first, *len)),
        Value::Unfetched(Unfetched::ExternalComp {
            first, stored_len, ..
        }) => Some((*first, *stored_len)),
        _ => None,
    }
}
