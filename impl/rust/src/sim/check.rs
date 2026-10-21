//! The deterministic-simulation integrity checkers (spec/design/simulation-testing.md §8.3), a port
//! of the Go reference (`impl/go/sim_check_test.go`).
//!
//! The image checker opens a copy of the disk image the way a new process would — so the meta
//! selection and manifest validation of validated-cow.md §1 run first — and then walks the selected
//! generation:
//!
//!   - the live page count at meta offset 56 equals the pages reachable from the catalog root, GiST
//!     R-tree pages included;
//!   - every reachable page lies in [2, page_count);
//!   - the free list has no duplicates and is disjoint from the reachable set and from the selected
//!     generation's manifest dependencies;
//!   - each table's storage keys ascend strictly and equal the primary-key encoding of their rows;
//!   - each table's persisted row count equals its leaf total;
//!   - each column index holds exactly the entries derived from its table's rows.

use crate::catalog::IndexKind;
use crate::collation::Collation;
use crate::error::EngineError;
use crate::executor::{Snapshot, encode_pk_key, index_entry_keys_columns};
use crate::format::{
    chain_pages, collect_tree_pages, reachable_pages, sim_open_with_gist, sim_selected_meta,
};
use crate::shared::Database;
use crate::types::Type;
use crate::value::{Unfetched, Value};
use std::collections::HashSet;
use std::sync::Arc;

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

    /// Wrap an engine error under `ctx` (`"ctx: error"`, or the bare error for an empty `ctx`).
    fn engine(ctx: &str) -> impl Fn(EngineError) -> Fail + '_ {
        move |e| Fail {
            msg: if ctx.is_empty() {
                e.to_string()
            } else {
                format!("{ctx}: {e}")
            },
            engine: Some(e),
        }
    }

    fn context(self, ctx: &str) -> Fail {
        Fail {
            msg: format!("{ctx}: {}", self.msg),
            engine: self.engine,
        }
    }
}

impl From<String> for Fail {
    fn from(msg: String) -> Fail {
        Fail::msg(msg)
    }
}

impl From<&str> for Fail {
    fn from(msg: &str) -> Fail {
        Fail::msg(msg.to_string())
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
    // A GiST R-tree is read whole at load (gist.md §4.1); its pages are the ones the load visits,
    // which the reachability walk does not list.
    let (engine, paging, gist) =
        sim_open_with_gist(image, credential).map_err(Fail::engine("open"))?;
    let mt = sim_selected_meta(&paging).map_err(Fail::engine("meta"))?;
    if mt.txid != want {
        return Err(Fail::msg(format!(
            "the image holds version {}, want {}",
            mt.txid, want
        )));
    }
    let snap = &engine.committed;

    let root = crate::format::PageRef {
        page: mt.root_page,
        tag: mt.root_tag,
    };
    let mut reached =
        reachable_pages(snap, &paging, root).map_err(Fail::engine("reachability walk"))?;
    for pg in gist {
        if !reached.insert(pg) {
            return Err(format!("GiST page {pg} is reachable twice").into());
        }
    }
    if reached.len() as u32 != mt.live_pages {
        return Err(format!(
            "live page count {}, but {} pages are reachable",
            mt.live_pages,
            reached.len()
        )
        .into());
    }
    let mut sorted: Vec<u32> = reached.iter().copied().collect();
    sorted.sort();
    // An encrypted file's body pages start after its key block (encrypted.md §1).
    let first = paging.cap().first_body();
    for pg in sorted {
        if pg < first || pg >= mt.page_count {
            return Err(format!("reachable page {pg} outside [{first}, {})", mt.page_count).into());
        }
    }
    let mut seen: HashSet<u32> = HashSet::new();
    for &pg in &engine.free_pages {
        if seen.contains(&pg) {
            return Err(format!("free page {pg} listed twice").into());
        }
        if pg < first || pg >= mt.page_count {
            return Err(format!("free page {pg} outside [{first}, {})", mt.page_count).into());
        }
        if reached.contains(&pg) {
            return Err(format!("free page {pg} is reachable").into());
        }
        if mt.protected.contains(&pg) {
            return Err(
                format!("free page {pg} is a dependency of the selected generation").into(),
            );
        }
        seen.insert(pg);
    }

    let mut claimed: HashSet<u32> = HashSet::new();
    for name in snap.table_names() {
        check_table(snap, &name, &paging, &mut claimed)
            .map_err(|f| f.context(&format!("table {name}")))?;
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
) -> Result<(), Fail> {
    let fetch = |p: crate::format::PageRef| paging.read_body(p);
    let table = snap.table(name).ok_or("no table definition")?;
    let store = snap.sim_store(name).ok_or("no row store")?;
    let mut entries = store.iter_entries().map_err(Fail::engine(""))?;
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
                let ctx = format!("row {i}: an overflow chain");
                let pages = chain_pages(first, len as usize, &fetch, paging.cap())
                    .map_err(Fail::engine(&ctx))?;
                for pg in pages {
                    if !claimed.insert(pg) {
                        return Err(format!("overflow page {pg} is referenced twice").into());
                    }
                }
            }
        }
        let ctx = format!("row {i}");
        store
            .resolve_all(&mut entries[i].1)
            .map_err(Fail::engine(&ctx))?;
        if i > 0 && entries[i - 1].0 >= entries[i].0 {
            return Err(format!("storage keys out of order at row {i}").into());
        }
        if !pk.is_empty() {
            let want =
                encode_pk_key(&pk, col_types, &colls, &entries[i].1).map_err(Fail::engine(""))?;
            if want != entries[i].0 {
                return Err(
                    format!("row {i} is stored under a key that is not its primary key").into(),
                );
            }
        }
    }
    if let Some(count) = store.count()
        && count != entries.len() as i64
    {
        return Err(format!(
            "persisted row count {count}, but the tree holds {} rows",
            entries.len()
        )
        .into());
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
                .map_err(Fail::engine(""))?;
            want.extend(keys);
        }
        want.sort();
        let ist = snap
            .sim_index_store(&def.name)
            .ok_or_else(|| format!("index {} has no store", def.name))?;
        let got = ist.iter_entries().map_err(Fail::engine(""))?;
        if got.len() != want.len() {
            return Err(format!(
                "index {} holds {} entries, want {}",
                def.name,
                got.len(),
                want.len()
            )
            .into());
        }
        for (i, (k, _)) in got.iter().enumerate() {
            if *k != want[i] {
                return Err(
                    format!("index {} entry {i} does not match its table", def.name).into(),
                );
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
