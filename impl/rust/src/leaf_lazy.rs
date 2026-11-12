//! Lazy leaf edits: changing rows of a clean (packed) leaf without decoding, measuring or encoding
//! the rows it does not change. A pure implementation change — file bytes, query results and costs
//! are the same with it on or off ([`crate::pmap::leaf_lazy`]).
//!
//!  1. A dirty leaf copied from a packed leaf keeps its rows as that page's until they are
//!     replaced ([`crate::pmap::LazyLeaf`]). Readers decode such a row from the page each time, as
//!     they do for a packed leaf; nothing is cached on a read path.
//!  2. The leaf's size facts come from the page. While its rows are its page's, a column no
//!     replacement changed is frozen: its saving is the one the page shows. Any other column is
//!     measured from stored spans and dense slots, without decoding values.
//!  3. At commit the leaf is encoded from its page: a leaf whose rows were only replaced copies the
//!     key section and every unchanged column region, and any other region is rebuilt from the
//!     page's stored spans plus the new rows' bytes.
//!
//! One assumption carries the frozen savings and the copied regions: a jed writer stores a region
//! in a compact form exactly when the size rule says that form is smaller, in the one layout the
//! encoder produces (`encode_region`). A page that breaks it — only a hand-altered, re-sealed page
//! could — may be measured by what it shows and copied as it is.

use std::borrow::Cow;

use super::leaf_compact::{ColAcc, ForCol, ForRange, for_eligible, for_slot, row_items};
use super::{
    Cap, ChainSource, CompressMemo, Disp, Item, LeafShape, LeafSizes, PackedLeaf, PaxDirs,
    RegionDir, Result, Row, Sealer, Unfetched, Value, all_inline_size, be_uint,
    encode_disposed_value, encode_leaf_keys, encode_leaf_pax, encode_region, encode_value,
    fixed_value_width, payload_bytes, put16, record_max, same_leaf_codecs, span_item,
    string_payload, value_facts,
};
use crate::pmap::Node;

/// Test switch: the commit encodes a lazy leaf both ways and panics unless the bytes are equal.
#[cfg(test)]
pub(crate) mod leaf_patch_check {
    use std::cell::Cell;

    thread_local! {
        static ON: Cell<bool> = const { Cell::new(false) };
    }

    pub(crate) fn set(on: bool) {
        ON.with(|c| c.set(on));
    }

    pub(crate) fn on() -> bool {
        ON.with(Cell::get)
    }
}

/// A lazy leaf's rows, for the functions that read them without decoding: row `i` is record
/// `at[i]` of `base` when `at[i] ≥ 0`, else `keys[i]` / `vals[i]`.
pub(crate) struct LazyView<'a> {
    pub(crate) base: &'a PackedLeaf,
    pub(crate) at: &'a [i32],
    pub(crate) keys: &'a [Vec<u8>],
    pub(crate) vals: &'a [Row],
    /// While the leaf's rows are its page's: per column, whether a replaced row stores it
    /// differently from the page. `None` once a row was inserted or removed.
    pub(crate) col_dirty: Option<&'a [bool]>,
}

impl<'a> LazyView<'a> {
    fn len(&self) -> usize {
        self.at.len()
    }

    fn key(&self, i: usize) -> &'a [u8] {
        match self.at[i] {
            at if at >= 0 => self.base.key(at as usize),
            _ => &self.keys[i],
        }
    }

    /// Row `i`: decoded from the page, or the one the leaf holds.
    pub(crate) fn row(&self, i: usize) -> Result<Cow<'a, [Value]>> {
        match self.at[i] {
            at if at >= 0 => Ok(Cow::Owned(self.base.row(at as usize)?)),
            _ => Ok(Cow::Borrowed(&self.vals[i])),
        }
    }
}

/// The length of the plain span a packed-array region's count byte stands for: nothing for NULL,
/// the 3-byte empty array, or an 11-byte header and `count` elements of width `w`.
fn arr_span_len(count: u8, w: usize) -> usize {
    match count {
        255 => 0,
        0 => 3,
        m => 11 + m as usize * w,
    }
}

impl PackedLeaf {
    /// Column `c`'s region extent in the payload, from the column directory.
    fn extent(&self, c: usize) -> (usize, usize) {
        let payload = self.payload();
        let at = self.dirs.col_dir + 2 * c;
        let word = |at: usize| u16::from_be_bytes([payload[at], payload[at + 1]]) as usize;
        (word(at), word(at + 2))
    }

    /// The stored span of variable-width value `(record i, column c)`: empty for NULL.
    fn stored_span(&self, c: usize, i: usize) -> &[u8] {
        self.dirs.var_span(self.payload(), c, i).bytes()
    }

    /// A stored fixed-width column's null bitmap and dense slots (rebuilt, for a base-plus-offsets
    /// region).
    fn dense(&self, c: usize) -> (&[u8], &[u8]) {
        let payload = self.payload();
        let bits = self.n.div_ceil(8);
        match &self.dirs.regions[c] {
            RegionDir::Fixed {
                width,
                bitmap,
                body,
            } => (
                &payload[*bitmap..*bitmap + bits],
                &payload[*body..*body + self.n * width],
            ),
            RegionDir::For { bitmap, slots, .. } => {
                if self.n == 0 {
                    return (&[], &[]);
                }
                self.dirs.slot(payload, c, 0); // rebuilds the slots on first use
                (
                    &payload[*bitmap..*bitmap + bits],
                    slots.get().expect("rebuilt above"),
                )
            }
            _ => unreachable!("not a stored fixed-width region"),
        }
    }

    /// [`record_weight`](Self::record_weight) for every record, read column by column from the
    /// directories, and their sum.
    pub(crate) fn record_weights(&self) -> (Vec<u32>, usize) {
        let payload = self.payload();
        let dirs = &self.dirs;
        let n = self.n;
        let mut w = vec![0u32; n];
        let mut fixed = 0u32;
        if dirs.key_len > 0 {
            fixed += dirs.key_len as u32;
        } else {
            for (i, wi) in w.iter_mut().enumerate() {
                *wi = dirs.key_len_at(payload, i) as u32;
            }
        }
        for region in &dirs.regions {
            match region {
                RegionDir::Elided { .. } => {}
                RegionDir::Fixed { width, .. } | RegionDir::For { width, .. } => {
                    fixed += *width as u32
                }
                RegionDir::Var { ends, .. } => {
                    let mut lo = 0;
                    for (i, wi) in w.iter_mut().enumerate() {
                        let hi = PaxDirs::end_offset(payload, *ends, i);
                        *wi += (hi - lo) as u32;
                        lo = hi;
                    }
                }
                RegionDir::Dict { ends, body, codes } => {
                    let mut lens = [0u32; 256];
                    let mut lo = 0;
                    for (e, len) in lens.iter_mut().enumerate().take((body - ends) / 2) {
                        let hi = PaxDirs::end_offset(payload, *ends, e);
                        *len = (hi - lo) as u32;
                        lo = hi;
                    }
                    for (i, wi) in w.iter_mut().enumerate() {
                        *wi += lens[payload[codes + i] as usize];
                    }
                }
                RegionDir::Arr { width, counts, .. } => {
                    for (i, wi) in w.iter_mut().enumerate() {
                        *wi += arr_span_len(payload[counts + i], *width) as u32;
                    }
                }
            }
        }
        let mut total = 0usize;
        for wi in w.iter_mut() {
            *wi += fixed;
            total += *wi as usize;
        }
        #[cfg(test)]
        if crate::pmap::leaf_sizes_check::on() {
            for (i, &wi) in w.iter().enumerate() {
                assert_eq!(
                    wi,
                    self.record_weight(i),
                    "BUG: record {i} weighs differently by columns and by record"
                );
            }
        }
        (w, total)
    }

    /// The bytes column `c`'s region saves on this page: its plain size less its stored size, 0
    /// for a plain region.
    pub(crate) fn region_saving(&self, c: usize) -> usize {
        let payload = self.payload();
        let n = self.n;
        let (start, end) = self.extent(c);
        let stored = end - start;
        let plain = match &self.dirs.regions[c] {
            RegionDir::Elided { .. } | RegionDir::Fixed { .. } | RegionDir::Var { .. } => return 0,
            RegionDir::For { width, .. } => 1 + n.div_ceil(8) + n * width,
            RegionDir::Dict { ends, body, codes } => {
                let mut lens = [0usize; 256];
                let mut lo = 0;
                for (e, len) in lens.iter_mut().enumerate().take((body - ends) / 2) {
                    let hi = PaxDirs::end_offset(payload, *ends, e);
                    *len = hi - lo;
                    lo = hi;
                }
                1 + 2 * n
                    + payload[*codes..*codes + n]
                        .iter()
                        .map(|&code| lens[code as usize])
                        .sum::<usize>()
            }
            RegionDir::Arr { width, counts, .. } => {
                1 + 2 * n
                    + payload[*counts..*counts + n]
                        .iter()
                        .map(|&count| arr_span_len(count, *width))
                        .sum::<usize>()
            }
        };
        plain - stored
    }

    /// Whether every variable-width value of the page is NULL or stored in the inline form this
    /// writer writes: tag 0x05 and the bytes for a string, tag 0x00 and the body for any other
    /// type, or an element of a packed-array region. Such a page holds no pointer to an overflow
    /// chain and no compressed block, and the plan of each of its records is all-inline.
    pub(crate) fn inline_only(&self) -> bool {
        *self.inline.get_or_init(|| {
            let payload = self.payload();
            self.shape.col_types.iter().enumerate().all(|(c, ty)| {
                // A dictionary's directory and blob are its entries'; a plain region's its values'.
                let (ends, body, count) = match &self.dirs.regions[c] {
                    RegionDir::Var { ends, body } => (*ends, *body, self.n),
                    RegionDir::Dict { ends, body, .. } => (*ends, *body, (body - ends) / 2),
                    _ => return true,
                };
                let want = if string_payload(ty) {
                    super::TAG_INLINE_BARE
                } else {
                    0x00
                };
                let mut lo = 0;
                (0..count).all(|e| {
                    let hi = PaxDirs::end_offset(payload, ends, e);
                    let ok = hi == lo || payload[body + lo] == want;
                    lo = hi;
                    ok
                })
            })
        })
    }

    /// Whether any value of the page is an external pointer: its bytes are on an overflow chain.
    pub(crate) fn has_chains(&self) -> bool {
        let payload = self.payload();
        self.dirs.regions.iter().any(|r| {
            // A dictionary's directory and blob are its entries'; a plain region's its values'.
            // No other region kind holds a pointer.
            let (ends, body, count) = match r {
                RegionDir::Var { ends, body } => (*ends, *body, self.n),
                RegionDir::Dict { ends, body, .. } => (*ends, *body, (body - ends) / 2),
                _ => return false,
            };
            let mut lo = 0;
            (0..count).any(|e| {
                let hi = PaxDirs::end_offset(payload, ends, e);
                let tag = (hi > lo).then(|| payload[body + lo]);
                lo = hi;
                matches!(tag, Some(super::TAG_EXTERNAL | super::TAG_EXTERNAL_COMP))
            })
        })
    }

    /// Whether the leaf encoder, writing `v` as column `c` of an all-inline record, would write
    /// exactly the bytes this page stores for record `i` of that column. It answers false whenever
    /// that is not certain.
    ///
    /// An UPDATE's unassigned variable-width value reaches the write as the deferred handle the
    /// scan read from this page, so its body is the page's own bytes; otherwise the bodies are
    /// compared, which also covers a value assigned its old content. A fixed-width value is
    /// compared with the dense slot.
    fn stored_same(&self, c: usize, i: usize, v: &Value) -> bool {
        let payload = self.payload();
        if matches!(self.dirs.regions[c], RegionDir::Elided { .. }) {
            return true; // rebuilt from the key, which is the same
        }
        let null = self.dirs.is_null(payload, c, i);
        if matches!(v, Value::Null) || null {
            return matches!(v, Value::Null) && null;
        }
        let ty = &self.shape.col_types[c];
        if let Some(w) = fixed_value_width(ty) {
            if matches!(v, Value::Unfetched(_)) {
                return false;
            }
            let slot = self.dirs.slot(payload, c, i);
            if for_eligible(ty) {
                return be_uint(slot) == for_slot(ty, w, v);
            }
            return encode_value(ty, v)[1..] == *slot;
        }
        let span = self.stored_span(c, i);
        // The inline form this writer writes: a bare string, or the tag and the body.
        let string = string_payload(ty);
        let want = if string { super::TAG_INLINE_BARE } else { 0x00 };
        if span.first() != Some(&want) {
            return false;
        }
        let old = &span[1..];
        match v {
            // What the encoder writes after the tag for an inline-deferred value: a string's
            // content, any other body whole (`encode_disposed_value`).
            Value::Unfetched(Unfetched::Inline {
                block,
                off,
                len,
                bare,
                ..
            }) => {
                let body = &block[*off as usize..*off as usize + *len as usize];
                let body = if string && !*bare {
                    &body[2.min(body.len())..]
                } else {
                    body
                };
                body.len() == old.len()
                    && (std::ptr::eq(body.as_ptr(), old.as_ptr()) || body == old)
            }
            // A compressed or external form: its plan is not known to be inline.
            Value::Unfetched(_) => false,
            v if string => payload_bytes(ty, v).as_ref() == old,
            v => encode_value(ty, v)[1..] == *old,
        }
    }
}

/// Which columns of record `i` of `base` the row `val` stores differently (pushed to `changed`,
/// one per column), and whether `val` is stored all-inline: its all-inline size fits `RECORD_MAX`,
/// so no disposition plan runs (the test `all_inline_size` makes).
pub(crate) fn replaced_columns(
    base: &PackedLeaf,
    shape: &LeafShape,
    cap: Cap,
    i: usize,
    key: &[u8],
    val: &[Value],
    changed: &mut Vec<bool>,
) -> bool {
    let payload = base.payload();
    let mut size = key.len() + shape.fixed_bytes;
    for (c, ty) in shape.col_types.iter().enumerate() {
        let same = base.stored_same(c, i, &val[c]);
        changed.push(!same);
        if fixed_value_width(ty).is_none() {
            size += if same {
                base.dirs.value_len(payload, c, i)
            } else {
                value_facts(ty, &val[c]).0
            };
        }
    }
    let inline = size <= record_max(cap.c, shape.col_types.len());
    debug_assert_eq!(inline, all_inline_size(shape, key, val, cap).is_some());
    inline
}

/// Whether a record is stored all-inline (see [`replaced_columns`]).
pub(crate) fn row_all_inline(shape: &LeafShape, cap: Cap, key: &[u8], row: &[Value]) -> bool {
    all_inline_size(shape, key, row, cap).is_some()
}

// --- size facts from the page ---

/// The span and count of base-plus-offsets column `fc`'s present values over a lazy leaf's rows:
/// dense slots for the rows still on the page, values for the others.
fn for_range_of(view: &LazyView<'_>, shape: &LeafShape, fc: &ForCol) -> (ForRange, usize) {
    let ty = &shape.col_types[fc.col];
    let (bitmap, slots) = view.base.dense(fc.col);
    let w = fc.w;
    let mut r = ForRange::default();
    let mut count = 0;
    for (i, &at) in view.at.iter().enumerate() {
        if at < 0 {
            let v = &view.vals[i][fc.col];
            if !matches!(v, Value::Null) {
                r.add(for_slot(ty, w, v));
                count += 1;
            }
            continue;
        }
        let at = at as usize;
        if bitmap[at / 8] & (0x80 >> (at % 8)) != 0 {
            continue;
        }
        r.add(be_uint(&slots[at * w..(at + 1) * w]));
        count += 1;
    }
    (r, count)
}

/// Measure item column `j` over a lazy leaf's rows. A row still on the page is classified from its
/// stored span, with no value decoded; where the page stores the column as a dictionary, the rows
/// are counted by code and each entry is added once.
fn item_acc_of(view: &LazyView<'_>, shape: &LeafShape, cap: Cap, j: usize) -> ColAcc<'static> {
    let ic = &shape.item_cols[j];
    let ty = &shape.col_types[ic.col];
    let base = view.base;
    let payload = base.payload();
    let dict = match &base.dirs.regions[ic.col] {
        RegionDir::Dict { ends, body, codes } if ic.arr_w == 0 => Some((*ends, *body, *codes)),
        _ => None,
    };
    let mut acc = ColAcc::default();
    let mut counts = [0usize; 256];
    let mut items = Vec::new();
    for (i, &at) in view.at.iter().enumerate() {
        if at < 0 {
            items.clear();
            row_items(shape, cap, view.key(i), &view.vals[i], &mut items);
            acc.add(ic, &items[j], |p| Cow::Owned(p.to_vec()));
        } else if let Some((_, _, codes)) = dict {
            counts[payload[codes + at as usize] as usize] += 1;
        } else {
            let it = span_item(ty, base.stored_span(ic.col, at as usize));
            acc.add(ic, &it, |p| Cow::Owned(p.to_vec()));
        }
    }
    if let Some((ends, body, _)) = dict {
        for (e, &k) in counts.iter().enumerate().take((body - ends) / 2) {
            if k > 0 {
                let (lo, hi) = PaxDirs::dir_span(payload, ends, e);
                acc.dict
                    .add_n(&span_item(ty, &payload[body + lo..body + hi]), k);
            }
        }
        acc.f.d = acc.dict.d;
        acc.f.dup = acc.dict.dup;
    }
    acc
}

/// The columns [`LeafSizes::replace_cols`] leaves for [`LeafSizes::remeasure`].
#[derive(Default)]
pub(crate) struct Redo {
    fors: Vec<usize>,
    items: Vec<usize>,
}

fn same_item(a: &Item<'_>, b: &Item<'_>) -> bool {
    match (a, b) {
        (Item::Null, Item::Null) | (Item::Own, Item::Own) => true,
        (Item::Shared(p), Item::Shared(q)) => p.as_ref() == q.as_ref(),
        _ => false,
    }
}

impl LeafSizes {
    /// A lazy leaf's size facts.
    ///
    /// While the leaf's rows are its page's, a column no replacement changed is frozen: its saving
    /// is taken to be what the page shows, [`PackedLeaf::region_saving`]. That is the size rule's
    /// saving because the writer stores a region in a compact form exactly when the rule says that
    /// form is smaller, and the form's stored size is the plain size less the rule's saving (the
    /// module's assumption; `leaf_sizes_check` compares the two at every use).
    pub(crate) fn measure_lazy(shape: &LeafShape, cap: Cap, view: &LazyView<'_>) -> LeafSizes {
        let mut sz = LeafSizes::empty(shape);
        sz.n = view.len();
        let frozen = |c: usize| view.col_dirty.is_some_and(|dirty| !dirty[c]);
        for (j, fc) in shape.for_cols.iter().enumerate() {
            if frozen(fc.col) {
                sz.for_frozen[j] = true;
                sz.for_save[j] = view.base.region_saving(fc.col);
                sz.frozen += 1;
            } else {
                (sz.fors[j], sz.for_cnt[j]) = for_range_of(view, shape, fc);
            }
        }
        for (j, ic) in shape.item_cols.iter().enumerate() {
            if frozen(ic.col) {
                sz.item_frozen[j] = true;
                sz.item_save[j] = view.base.region_saving(ic.col);
                sz.frozen += 1;
            } else {
                sz.accs[j] = item_acc_of(view, shape, cap, j);
            }
        }
        sz
    }

    /// Measure a packed leaf's rows from its page, with no value decoded and no column frozen. It
    /// equals [`measure`](Self::measure) over the decoded rows.
    #[cfg(test)]
    pub(crate) fn measure_packed(shape: &LeafShape, cap: Cap, p: &PackedLeaf) -> LeafSizes {
        let at: Vec<i32> = (0..p.len() as i32).collect();
        LeafSizes::measure_lazy(
            shape,
            cap,
            &LazyView {
                base: p,
                at: &at,
                keys: &[],
                vals: &[],
                col_dirty: None,
            },
        )
    }

    /// [`refresh`](Self::refresh) over a lazy leaf's rows.
    pub(crate) fn refresh_lazy(&mut self, shape: &LeafShape, view: &LazyView<'_>) {
        if !self.stale {
            return;
        }
        for (j, fc) in shape.for_cols.iter().enumerate() {
            if self.for_stale[j] {
                (self.fors[j], self.for_cnt[j]) = for_range_of(view, shape, fc);
                self.for_stale[j] = false;
            }
        }
        self.stale = false;
    }

    /// Follow the replacement of row `i` (not yet applied to `view`) by `val` in a leaf whose rows
    /// are its page's. `changed[c]` says the new row stores column `c` differently from the page;
    /// `was_lazy` that the row being replaced is still the page's. A frozen column the replacement
    /// changes is thawed; a measured column takes the old value out and the new one in. The
    /// columns to measure again are returned for [`remeasure`](Self::remeasure), which runs once
    /// the row is in place. `None` when the facts cannot be kept.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn replace_cols(
        &mut self,
        shape: &LeafShape,
        cap: Cap,
        view: &LazyView<'_>,
        i: usize,
        key: &[u8],
        val: &[Value],
        changed: &[bool],
        was_lazy: bool,
    ) -> Option<Redo> {
        // A measured column is untouched only when the old row is the page's and the new one
        // stores the same bytes there.
        let touched = |c: usize| changed[c] || !was_lazy;
        let need_fors = shape
            .for_cols
            .iter()
            .enumerate()
            .any(|(j, fc)| !self.for_frozen[j] && touched(fc.col));
        let need_items = shape
            .item_cols
            .iter()
            .enumerate()
            .any(|(j, ic)| !self.item_frozen[j] && touched(ic.col));
        let mut redo = Redo::default();
        if need_fors || need_items {
            let old = view.row(i).ok()?;
            for (j, fc) in shape.for_cols.iter().enumerate() {
                if self.for_frozen[j] || !touched(fc.col) {
                    continue;
                }
                let ty = &shape.col_types[fc.col];
                let (ov, nv) = (&old[fc.col], &val[fc.col]);
                if !matches!(ov, Value::Null) {
                    self.for_cnt[j] -= 1;
                    if self.for_cnt[j] == 0 {
                        self.fors[j] = ForRange::default();
                        self.for_stale[j] = false;
                    } else {
                        let s = for_slot(ty, fc.w, ov);
                        if s == self.fors[j].lo || s == self.fors[j].hi {
                            self.for_stale[j] = true;
                            self.stale = true;
                        }
                    }
                }
                if !matches!(nv, Value::Null) {
                    self.fors[j].add(for_slot(ty, fc.w, nv));
                    self.for_cnt[j] += 1;
                }
            }
            if need_items {
                let mut a = Vec::with_capacity(shape.item_cols.len());
                let mut b = Vec::with_capacity(shape.item_cols.len());
                row_items(shape, cap, key, &old, &mut a);
                row_items(shape, cap, key, val, &mut b);
                for (j, ic) in shape.item_cols.iter().enumerate() {
                    if self.item_frozen[j] || !touched(ic.col) || same_item(&a[j], &b[j]) {
                        continue;
                    }
                    if self.accs[j].remove(ic, &a[j]) {
                        self.accs[j].add(ic, &b[j], |p| Cow::Owned(p.to_vec()));
                    } else {
                        redo.items.push(j);
                    }
                }
            }
        }
        for (j, fc) in shape.for_cols.iter().enumerate() {
            if self.for_frozen[j] && changed[fc.col] {
                self.for_frozen[j] = false;
                self.frozen -= 1;
                redo.fors.push(j);
            }
        }
        for (j, ic) in shape.item_cols.iter().enumerate() {
            if self.item_frozen[j] && changed[ic.col] {
                self.item_frozen[j] = false;
                self.frozen -= 1;
                redo.items.push(j);
            }
        }
        Some(redo)
    }

    /// Measure the columns [`replace_cols`](Self::replace_cols) left, over the leaf's rows as they
    /// now are.
    pub(crate) fn remeasure(
        &mut self,
        shape: &LeafShape,
        cap: Cap,
        view: &LazyView<'_>,
        redo: Redo,
    ) {
        for j in redo.fors {
            (self.fors[j], self.for_cnt[j]) = for_range_of(view, shape, &shape.for_cols[j]);
            self.for_stale[j] = false;
        }
        for j in redo.items {
            self.accs[j] = item_acc_of(view, shape, cap, j);
        }
    }
}

// --- patch encode ---

/// A dirty lazy leaf's payload: built from its page when it can be ([`patch_leaf`]), else the full
/// encoder's over its rows.
pub(super) fn encode_lazy_leaf(
    node: &Node,
    shape: &LeafShape,
    seal: Sealer<'_>,
    take: &mut dyn FnMut() -> u32,
    ovf: &mut Vec<(u32, Vec<u8>)>,
    src: &mut ChainSource<'_>,
    memo: Option<&CompressMemo>,
) -> Result<Vec<u8>> {
    let patch = patch_leaf(node, shape, seal, src);
    #[cfg(not(test))]
    if let Some(patch) = patch {
        return Ok(patch);
    }
    // Under the test cross-check the full encoder runs as well, and the two must agree.
    #[cfg(test)]
    let patch = match patch {
        Some(patch) if !leaf_patch_check::on() => return Ok(patch),
        patch => patch,
    };
    let rows = node.decoded_rows()?;
    let rows: Vec<&[Value]> = rows.iter().map(Vec::as_slice).collect();
    let keys: Vec<&[u8]> = (0..node.len()).map(|i| node.key_at(i)).collect();
    // A leaf the patch encoder took holds only inline records, so encoding it again takes no page
    // and keeps no chain.
    let full = encode_leaf_pax(shape, &keys, &rows, seal, take, ovf, src, memo)?;
    #[cfg(test)]
    if let Some(patch) = patch {
        let at = patch.iter().zip(&full).take_while(|(a, b)| a == b).count();
        assert!(
            patch == full,
            "BUG: a lazy leaf of {} records encoded from its page differs from the full \
             encoder's ({} and {} bytes) at byte {at}",
            rows.len(),
            patch.len(),
            full.len(),
        );
    }
    Ok(full)
}

/// Whether a leaf decoded for shape `a` has the layout and value codecs of shape `b`
/// ([`super::same_leaf_shape`]).
fn same_shape(a: &LeafShape, b: &LeafShape) -> bool {
    std::ptr::eq(a, b)
        || (a.key_len == b.key_len
            && a.key_at == b.key_at
            && same_leaf_codecs(&a.col_types, &b.col_types))
}

/// Encode a lazy leaf from its page, or `None` when the full encoder must run.
///
/// It applies when every record of the page and every row written since is stored all-inline (so
/// no overflow chain is kept, shared or written, and no disposition plan is consulted) and the
/// column codecs are the page's. A row still on the page then contributes its stored spans,
/// undecoded, and a row written since its newly encoded values. While the leaf's rows are the
/// page's (rows were only replaced) the key section is the page's and a column no replacement
/// changed keeps the page's region; otherwise the key section and every region are built again.
///
/// This is the full encoder's output because that encoder, given a page's own rows, writes the
/// page again: an inline value read from a page is written from its stored bytes, and a region is
/// a function of its values' bytes alone (`encode_region`, which both encoders call).
fn patch_leaf(
    node: &Node,
    shape: &LeafShape,
    seal: Sealer<'_>,
    src: &mut ChainSource<'_>,
) -> Option<Vec<u8>> {
    let lz = node.lazy.as_deref()?;
    let base = lz.base.packed.as_ref()?;
    let n = node.len();
    if !lz.patch_ok
        || (lz.same_rows && n != base.n)
        || !same_shape(&base.shape, shape)
        || !base.inline_only()
    {
        return None;
    }
    let k = shape.k();
    let payload = base.payload();
    let dirs = &base.dirs;
    let mut out = Vec::with_capacity(dirs.end + 64);
    if lz.same_rows {
        out.extend_from_slice(&payload[..dirs.col_dir]);
    } else {
        let keys: Vec<&[u8]> = (0..n).map(|i| node.key_at(i)).collect();
        encode_leaf_keys(shape, &keys, &mut out).ok()?; // the full encoder reports it
    }
    let dir = out.len();
    out.resize(dir + 2 * (k + 1), 0);
    // An all-inline value allocates no page.
    let mut no_take = || -> u32 { unreachable!("an inline value takes no page") };
    let mut no_ovf = Vec::new();
    let (mut body, mut ends, mut nulls) = (Vec::new(), Vec::new(), Vec::new());
    for (c, ty) in shape.col_types.iter().enumerate() {
        let end = out.len();
        put16(&mut out, dir + 2 * c, end);
        let fixed = fixed_value_width(ty);
        if let Some(key_at) = shape.key_at[c] {
            // Not stored. A row written since must still hold the value its key gives.
            let w = fixed.expect("an elided column is fixed-width");
            for i in (0..n).filter(|&i| lz.at[i] < 0) {
                let v = &node.vals[i][c];
                if matches!(v, Value::Null)
                    || node.keys[i].get(key_at..key_at + w) != Some(&encode_value(ty, v)[1..])
                {
                    return None; // the full encoder reports it
                }
            }
            continue;
        }
        if lz.same_rows && !lz.col_dirty[c] {
            // No row stores this column differently: the page's region.
            let (lo, hi) = base.extent(c);
            out.extend_from_slice(&payload[lo..hi]);
            continue;
        }
        body.clear();
        ends.clear();
        nulls.clear();
        for (i, &at) in lz.at.iter().enumerate() {
            // The value's bytes as the full encoder lays them out: a fixed-width slot (zeros for
            // NULL) or a tagged span (nothing for NULL).
            let is_null = if at >= 0 {
                let at = at as usize;
                let is_null = dirs.is_null(payload, c, at);
                match fixed {
                    Some(w) if is_null => body.resize(body.len() + w, 0),
                    Some(_) => body.extend_from_slice(dirs.slot(payload, c, at)),
                    None => body.extend_from_slice(base.stored_span(c, at)),
                }
                is_null
            } else {
                let v = &node.vals[i][c];
                let is_null = matches!(v, Value::Null);
                match fixed {
                    Some(w) if is_null => body.resize(body.len() + w, 0),
                    Some(_) => body.extend_from_slice(&encode_value(ty, v)[1..]),
                    None if is_null => {}
                    None => body.extend_from_slice(
                        &encode_disposed_value(
                            ty,
                            v,
                            &Disp::Inline,
                            0,
                            seal,
                            &mut no_take,
                            &mut no_ovf,
                            src,
                        )
                        .ok()?,
                    ),
                }
                is_null
            };
            nulls.push(is_null);
            if fixed.is_none() {
                ends.push(body.len() as u32);
            }
        }
        encode_region(shape, c, n, &body, &ends, &nulls, &mut out);
    }
    let end = out.len();
    put16(&mut out, dir + 2 * k, end);
    debug_assert!(no_ovf.is_empty());
    Some(out)
}
