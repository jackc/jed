//! The deterministic-simulation integrity checkers (spec/design/simulation-testing.md §8.3), a port
//! of the Go reference (`impl/go/sim_check_test.go`).
//!
//! The image checker opens a copy of the disk image the way a new process would — so the meta
//! selection and manifest validation of validated-cow.md §1 run first — and then walks the selected
//! generation:
//!
//!   - the live page count at meta offset 56 equals the pages reachable from the catalog root;
//!   - every reachable page lies in [2, page_count);
//!   - the free list has no duplicates and is disjoint from the reachable set and from the selected
//!     generation's manifest dependencies;
//!   - each table's storage keys ascend strictly and equal the primary-key encoding of their rows;
//!   - each table's persisted row count equals its leaf total;
//!   - each column index holds exactly the entries derived from its table's rows.

use crate::blockstore::MemoryBlockStore;
use crate::catalog::IndexKind;
use crate::collation::Collation;
use crate::executor::{Engine, Snapshot, encode_pk_key, index_entry_keys_columns};
use crate::format::{
    ROOT_PAGE, chain_pages, collect_tree_pages, reachable_pages, sim_selected_meta,
};
use crate::pager::Pager;
use crate::paging::{DEFAULT_CACHE_BYTES, cache_leaves};
use crate::shared::Database;
use crate::types::Type;
use crate::value::{Unfetched, Value};
use std::collections::HashSet;
use std::sync::Arc;

/// Check `image` holds version `want` and is structurally sound; returns the meta's live page
/// count, page high-water, and the loaded free list's length.
pub(crate) fn integrity(image: &[u8], want: u64) -> Result<(u32, u32, u32), String> {
    let pager = Pager::from_store(Box::new(MemoryBlockStore::new(image.to_vec())))
        .map_err(|e| format!("open: {e}"))?;
    let page_size = pager.page_size();
    let engine = Engine::open_paged(
        pager,
        cache_leaves(DEFAULT_CACHE_BYTES, page_size),
        &crate::environment::Environment::default(),
    )
    .map_err(|e| format!("open: {e}"))?;
    let paging = engine
        .paging
        .clone()
        .expect("an opened engine carries a paging context");
    let mt = sim_selected_meta(&paging).map_err(|e| format!("meta: {e}"))?;
    if mt.txid != want {
        return Err(format!(
            "the image holds version {}, want {}",
            mt.txid, want
        ));
    }
    let snap = &engine.committed;

    let reached = reachable_pages(snap, &paging, mt.root_page)
        .map_err(|e| format!("reachability walk: {e}"))?;
    if reached.len() as u32 != mt.live_pages {
        return Err(format!(
            "live page count {}, but {} pages are reachable",
            mt.live_pages,
            reached.len()
        ));
    }
    let mut sorted: Vec<u32> = reached.iter().copied().collect();
    sorted.sort();
    for pg in sorted {
        if pg < ROOT_PAGE || pg >= mt.page_count {
            return Err(format!(
                "reachable page {pg} outside [{ROOT_PAGE}, {})",
                mt.page_count
            ));
        }
    }
    let mut seen: HashSet<u32> = HashSet::new();
    for &pg in &engine.free_pages {
        if seen.contains(&pg) {
            return Err(format!("free page {pg} listed twice"));
        }
        if pg < ROOT_PAGE || pg >= mt.page_count {
            return Err(format!(
                "free page {pg} outside [{ROOT_PAGE}, {})",
                mt.page_count
            ));
        }
        if reached.contains(&pg) {
            return Err(format!("free page {pg} is reachable"));
        }
        if mt.protected.contains(&pg) {
            return Err(format!(
                "free page {pg} is a dependency of the selected generation"
            ));
        }
        seen.insert(pg);
    }

    let mut claimed: HashSet<u32> = HashSet::new();
    for name in snap.table_names() {
        check_table(snap, &name, &paging, &mut claimed)
            .map_err(|e| format!("table {name}: {e}"))?;
    }
    Ok((mt.live_pages, mt.page_count, engine.free_pages.len() as u32))
}

/// Also checks chain exclusivity (large-values.md §15): every overflow page the snapshot reaches is
/// named by exactly one stored value, across all tables (`claimed`). A rewritten record may keep an
/// unchanged value's chain, but two records of one snapshot never share one.
fn check_table(
    snap: &Snapshot,
    name: &str,
    paging: &crate::paging::SharedPaging,
    claimed: &mut HashSet<u32>,
) -> Result<(), String> {
    let fetch = |p: u32| paging.pager().read_block(p);
    let table = snap.table(name).ok_or("no table definition")?;
    let store = snap.sim_store(name).ok_or("no row store")?;
    let mut entries = store.iter_entries().map_err(|e| e.to_string())?;
    let colls: Vec<Option<Arc<Collation>>> = table
        .columns
        .iter()
        .map(|c| c.collation.as_ref().and_then(|n| snap.resolve_collation(n)))
        .collect();
    let pk: Vec<(usize, Type)> = table
        .pk
        .iter()
        .map(|&i| (i, table.columns[i].ty.clone()))
        .collect();
    let col_types = store.col_types();
    for i in 0..entries.len() {
        for v in &entries[i].1 {
            if let Some((first, len)) = external_ref(v) {
                let pages = chain_pages(first, len as usize, &fetch)
                    .map_err(|e| format!("row {i}: an overflow chain: {e}"))?;
                for pg in pages {
                    if !claimed.insert(pg) {
                        return Err(format!("overflow page {pg} is referenced twice"));
                    }
                }
            }
        }
        store
            .resolve_all(&mut entries[i].1)
            .map_err(|e| format!("row {i}: {e}"))?;
        if i > 0 && entries[i - 1].0 >= entries[i].0 {
            return Err(format!("storage keys out of order at row {i}"));
        }
        if !pk.is_empty() {
            let want =
                encode_pk_key(&pk, col_types, &colls, &entries[i].1).map_err(|e| e.to_string())?;
            if want != entries[i].0 {
                return Err(format!(
                    "row {i} is stored under a key that is not its primary key"
                ));
            }
        }
    }
    if let Some(count) = store.count()
        && count != entries.len() as i64
    {
        return Err(format!(
            "persisted row count {count}, but the tree holds {} rows",
            entries.len()
        ));
    }
    for def in &table.indexes {
        if def.column_ordinals().is_none()
            || def.predicate.is_some()
            || def.kind != IndexKind::Btree
        {
            continue; // expression, partial, GIN, and GiST indexes are not generated yet
        }
        let mut want: Vec<Vec<u8>> = Vec::new();
        for (key, row) in &entries {
            let keys = index_entry_keys_columns(&table.columns, col_types, &colls, def, key, row)
                .map_err(|e| e.to_string())?;
            want.extend(keys);
        }
        want.sort();
        let ist = snap
            .sim_index_store(&def.name)
            .ok_or_else(|| format!("index {} has no store", def.name))?;
        let got = ist.iter_entries().map_err(|e| e.to_string())?;
        if got.len() != want.len() {
            return Err(format!(
                "index {} holds {} entries, want {}",
                def.name,
                got.len(),
                want.len()
            ));
        }
        for (i, (k, _)) in got.iter().enumerate() {
            if *k != want[i] {
                return Err(format!(
                    "index {} entry {i} does not match its table",
                    def.name
                ));
            }
        }
    }
    Ok(())
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
    let fetch = |p: u32| paging.pager().read_block(p);

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
                let pages = chain_pages(first, len as usize, &fetch)
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
fn external_ref(v: &Value) -> Option<(u32, u32)> {
    match v {
        Value::Unfetched(Unfetched::External {
            first_page, len, ..
        }) => Some((*first_page, *len)),
        Value::Unfetched(Unfetched::ExternalComp {
            first_page,
            stored_len,
            ..
        }) => Some((*first_page, *stored_len)),
        _ => None,
    }
}
