//! The compact leaf (format_version 52 — spec/fileformat/format.md "Leaf node" and "Fan-out";
//! spec/design/leaf-structural.md): a leaf's schema-derived shape, and the content-dependent part
//! of its size.
//!
//! A leaf's payload is `Σ record_size + leaf_overhead(N, shape) − saving(leaf)`. The saving is a
//! function of the leaf's keys and rows alone: the shared key prefix, the base-plus-offsets form of
//! an integer-like column, the dictionary form of a string column, and the bit-packed form of an
//! integer-array column. The encoder ([`super::encode_leaf_pax`]) makes each choice by the same
//! arithmetic this module measures with, so a leaf measured from its decoded rows and the page it
//! is written to agree.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;

use super::{
    Cap, ColType, Disp, ScalarType, Unfetched, Value, all_inline_size, fixed_value_width,
    never_inline, plan_dispositions, planning_payload,
};

/// Leaf flags bit 0: every key of the tree has one length, stored once; no key directory.
pub(crate) const LEAF_FLAG_FIXED_KEY: u8 = 1;
/// Leaf flags bit 1: the leaf stores its keys' shared prefix once; the key blob holds suffixes.
pub(crate) const LEAF_FLAG_PREFIX: u8 = 2;

/// The presence tag of an inline-plain `text` / `bytea` / `json` value in a leaf: the rest of the
/// value's span is its content.
pub(crate) const TAG_INLINE_BARE: u8 = 0x05;

/// Region kind 1: base plus offsets.
pub(crate) const REGION_FOR: u8 = 1;
/// Region kind 2: dictionary.
pub(crate) const REGION_DICT: u8 = 2;
/// Region kind 4: packed integer arrays. (Kinds 3 and 5 are reserved.)
pub(crate) const REGION_ARR: u8 = 4;

/// A column a leaf may write as base plus offsets.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ForCol {
    pub(crate) col: usize,
    pub(crate) w: usize,
}

/// A variable-width column whose content decides a leaf's size: a string column (dictionary) or an
/// integer-array column (`arr_w` is its element width; 0 for a string column).
#[derive(Clone, Copy, Debug)]
pub(crate) struct ItemCol {
    pub(crate) col: usize,
    pub(crate) arr_w: usize,
}

/// A tree's leaf shape: what the leaf codec and the B+tree's size arithmetic need from the schema.
/// Built once per store and shared (`Arc`) with every resident leaf of it.
#[derive(Debug)]
pub(crate) struct LeafShape {
    /// The value-column types, in declaration order (empty for an index tree).
    pub(crate) col_types: Arc<Vec<ColType>>,
    /// Stored fixed-width columns.
    pub(crate) fixed: usize,
    /// Stored variable-width columns.
    pub(crate) var: usize,
    /// The one length of every key of the tree, or 0 when keys vary.
    pub(crate) key_len: usize,
    /// Per column: the key offset of an elided column's value body.
    pub(crate) key_at: Vec<Option<usize>>,
    pub(crate) for_cols: Vec<ForCol>,
    pub(crate) item_cols: Vec<ItemCol>,
    /// The bytes the stored fixed-width columns add to every record.
    pub(crate) fixed_bytes: usize,
}

/// Whether a column's content payload is a resident value's string body verbatim.
pub(crate) fn string_payload(ty: &ColType) -> bool {
    matches!(ty, ColType::Scalar(s) if s.is_text() || s.is_bytea() || s.is_json())
}

/// The element width of an `i16[]` / `i32[]` / `i64[]` column, or 0.
pub(crate) fn arr_elem_width(ty: &ColType) -> usize {
    match ty {
        ColType::Array(e) => match **e {
            ColType::Scalar(ScalarType::Int16) => 2,
            ColType::Scalar(ScalarType::Int32) => 4,
            ColType::Scalar(ScalarType::Int64) => 8,
            _ => 0,
        },
        _ => 0,
    }
}

/// Whether a primary-key member of this type has a value body that is, byte for byte, its key
/// encoding. Floats and `interval` are not: their keys canonicalize.
fn key_derivable(ty: &ColType) -> bool {
    matches!(
        ty,
        ColType::Scalar(
            ScalarType::Int16
                | ScalarType::Int32
                | ScalarType::Int64
                | ScalarType::Bool
                | ScalarType::Uuid
                | ScalarType::Timestamp
                | ScalarType::Timestamptz
                | ScalarType::Date
        )
    )
}

/// The width of a scalar's fixed-width key encoding, or `None` when its key varies in length.
fn fixed_key_width(ty: &ColType) -> Option<usize> {
    match ty {
        ColType::Scalar(s) => match s {
            ScalarType::Int16 => Some(2),
            ScalarType::Int32 | ScalarType::Date | ScalarType::Float32 => Some(4),
            ScalarType::Int64
            | ScalarType::Timestamp
            | ScalarType::Timestamptz
            | ScalarType::Float64 => Some(8),
            ScalarType::Bool => Some(1),
            ScalarType::Uuid | ScalarType::Interval => Some(16),
            _ => None,
        },
        _ => None,
    }
}

/// Whether a stored fixed-width column's value body is the sign-flipped big-endian integer body.
pub(super) fn for_eligible(ty: &ColType) -> bool {
    matches!(
        ty,
        ColType::Scalar(
            ScalarType::Int16
                | ScalarType::Int32
                | ScalarType::Int64
                | ScalarType::Timestamp
                | ScalarType::Timestamptz
                | ScalarType::Date
        ) | ColType::FixedDecimal { .. }
    )
}

impl LeafShape {
    /// The shape of a tree with these value columns and no key facts: keys vary in length and no
    /// column is elided. An index tree (no columns) has this shape.
    pub(crate) fn plain(col_types: Vec<ColType>) -> LeafShape {
        LeafShape::build(Arc::new(col_types), None)
    }

    /// The shape of a table's data tree: `pk` is the primary key's member ordinals in key order
    /// (empty for a table keyed by its synthetic rowid).
    pub(crate) fn table(col_types: Vec<ColType>, pk: &[usize]) -> LeafShape {
        LeafShape::build(Arc::new(col_types), Some(pk))
    }

    fn build(col_types: Arc<Vec<ColType>>, pk: Option<&[usize]>) -> LeafShape {
        let k = col_types.len();
        let mut key_at: Vec<Option<usize>> = vec![None; k];
        let mut key_len = 0usize;
        if let Some(pk) = pk {
            // The leading run of key-derivable members is elided; offsets after the first member
            // that is not are no longer fixed.
            let mut at = 0usize;
            for &c in pk {
                if c >= k || !key_derivable(&col_types[c]) {
                    break;
                }
                key_at[c] = Some(at);
                at += fixed_value_width(&col_types[c]).expect("a key-derivable column is fixed");
            }
            key_len = if pk.is_empty() {
                8 // the synthetic rowid
            } else {
                pk.iter()
                    .map(|&c| col_types.get(c).and_then(fixed_key_width))
                    .sum::<Option<usize>>()
                    .filter(|&total| total <= 0xFFFF)
                    .unwrap_or(0)
            };
        }
        let (mut fixed, mut elided, mut fixed_bytes) = (0, 0, 0);
        let mut for_cols = Vec::new();
        let mut item_cols = Vec::new();
        for (c, ty) in col_types.iter().enumerate() {
            if key_at[c].is_some() {
                elided += 1;
            } else if let Some(w) = fixed_value_width(ty) {
                fixed += 1;
                fixed_bytes += w;
                if for_eligible(ty) {
                    for_cols.push(ForCol { col: c, w });
                }
            } else if string_payload(ty) {
                item_cols.push(ItemCol { col: c, arr_w: 0 });
            } else if arr_elem_width(ty) != 0 {
                item_cols.push(ItemCol {
                    col: c,
                    arr_w: arr_elem_width(ty),
                });
            }
        }
        LeafShape {
            fixed,
            var: k - fixed - elided,
            key_len,
            key_at,
            for_cols,
            item_cols,
            fixed_bytes,
            col_types,
        }
    }

    pub(crate) fn k(&self) -> usize {
        self.col_types.len()
    }

    /// Whether a leaf stores a region for column `c`.
    pub(crate) fn stored(&self, c: usize) -> bool {
        self.key_at[c].is_none()
    }

    /// Whether a leaf's size depends on its rows (beyond the key prefix).
    pub(crate) fn content_cols(&self) -> bool {
        !self.for_cols.is_empty() || !self.item_cols.is_empty()
    }
}

/// The shape of an index tree — and of any in-memory map that is never serialized.
pub(crate) fn index_shape() -> &'static Arc<LeafShape> {
    static SHAPE: std::sync::LazyLock<Arc<LeafShape>> =
        std::sync::LazyLock::new(|| Arc::new(LeafShape::plain(Vec::new())));
    &SHAPE
}

/// The bytes a leaf's payload carries beyond `Σ record_size`, before the content-dependent savings
/// (format.md "Page model"): the leaf flags byte, the key directory (or a fixed-key tree's key
/// length), the column directory, and each stored region's kind byte plus its null bitmap
/// (fixed-width) or value directory (variable-width).
///
/// `leaf_overhead(N, cols) = 1 + keyDir(N) + 2·(K+1) + F·(1 + ceil(N/8)) + V·(1 + 2·N)`
pub(crate) fn leaf_overhead(n: usize, shape: &LeafShape) -> usize {
    let key_dir = if shape.key_len > 0 { 2 } else { 2 * n };
    1 + key_dir + 2 * (shape.k() + 1) + shape.fixed * (1 + n.div_ceil(8)) + shape.var * (1 + 2 * n)
}

/// The length of the prefix a leaf of `n` keys stores once: the common prefix of its first and
/// last key (capped at 65535) when storing it saves bytes (`n·p > p + 2`), else 0.
pub(crate) fn leaf_prefix_len(n: usize, first: &[u8], last: &[u8]) -> usize {
    if n < 2 {
        return 0;
    }
    let p = first
        .iter()
        .zip(last.iter())
        .take_while(|(a, b)| a == b)
        .count()
        .min(0xFFFF);
    if n * p <= p + 2 { 0 } else { p }
}

/// The bytes the shared prefix removes from a leaf of `n` keys.
pub(crate) fn leaf_prefix_saving(n: usize, first: &[u8], last: &[u8]) -> usize {
    match leaf_prefix_len(n, first, last) {
        0 => 0,
        p => n * p - p - 2,
    }
}

/// The span of a column's present value bodies, read as unsigned big-endian integers.
#[derive(Clone, Copy, Default, Debug)]
pub(crate) struct ForRange {
    pub(crate) lo: u64,
    pub(crate) hi: u64,
    pub(crate) any: bool,
}

impl ForRange {
    pub(crate) fn add(&mut self, v: u64) {
        if !self.any {
            *self = ForRange {
                lo: v,
                hi: v,
                any: true,
            };
            return;
        }
        self.lo = self.lo.min(v);
        self.hi = self.hi.max(v);
    }
}

/// The offset width for present values spanning `r` in a column of width `w`, or `w` when no
/// narrower width holds them.
pub(crate) fn for_width(w: usize, r: ForRange) -> usize {
    let d = if r.any { r.hi - r.lo } else { 0 };
    let bw = if d == 0 {
        0
    } else if d < 1 << 8 {
        1
    } else if d < 1 << 16 {
        2
    } else if d < 1 << 32 {
        4
    } else {
        8
    };
    bw.min(w)
}

/// The bytes a base-plus-offsets region saves in a leaf of `n` records; 0 when the plain region is
/// no larger.
pub(crate) fn for_saving(n: usize, w: usize, r: ForRange) -> usize {
    (n * (w - for_width(w, r))).saturating_sub(w + 1)
}

pub(crate) fn be_uint(b: &[u8]) -> u64 {
    b.iter().fold(0u64, |v, &x| v << 8 | x as u64)
}

/// Write the low `b.len()` bytes of `v`, big-endian.
pub(crate) fn put_be_uint(b: &mut [u8], mut v: u64) {
    for x in b.iter_mut().rev() {
        *x = v as u8;
        v >>= 8;
    }
}

/// The unsigned image of a present value of a base-plus-offsets column: its value body read as a
/// big-endian integer (the sign bit flipped at the column's width).
pub(super) fn for_slot(ty: &ColType, w: usize, v: &Value) -> u64 {
    let x = match (ty, v) {
        (_, Value::Int(n)) => *n,
        (_, Value::Timestamp(m) | Value::Timestamptz(m)) => *m,
        (_, Value::Date(d)) => *d as i64,
        (ColType::FixedDecimal { scale, .. }, Value::Decimal(d)) => d
            .unscaled_i64(*scale as u32)
            .expect("BUG: a decimal outside its column's typmod reached the leaf codec"),
        (_, other) => panic!("BUG: {other:?} stored in an integer-bodied column"),
    };
    match w {
        2 => (x as u16 ^ 0x8000) as u64,
        4 => (x as u32 ^ 0x8000_0000) as u64,
        _ => x as u64 ^ (1 << 63),
    }
}

/// One value as the dictionary and packed-array forms see it.
pub(crate) enum Item<'a> {
    /// A NULL: every NULL of a dictionary shares the empty entry.
    Null,
    /// An inline-plain value, with its content payload (a string's bytes; an array's body).
    Shared(Cow<'a, [u8]>),
    /// A compressed or external value: always its own dictionary entry.
    Own,
}

/// Classify a stored variable-column span.
pub(crate) fn span_item<'a>(ty: &ColType, span: &'a [u8]) -> Item<'a> {
    match span.split_first() {
        None => Item::Null,
        Some((&TAG_INLINE_BARE, rest)) if string_payload(ty) => Item::Shared(Cow::Borrowed(rest)),
        Some((0x00, rest)) if !string_payload(ty) => Item::Shared(Cow::Borrowed(rest)),
        _ => Item::Own,
    }
}

/// A dictionary region's facts: `d` entries, and `dup` bytes of spans that repeat an earlier entry.
#[derive(Clone, Default)]
pub(crate) struct DictAcc<'a> {
    /// The rows holding each shared payload.
    seen: HashMap<Cow<'a, [u8]>, usize>,
    pub(crate) d: usize,
    pub(crate) dup: usize,
    nulls: usize,
    /// No value is ever removed. More than 256 entries then rule the dictionary out for good
    /// ([`dict_saving`]), so the rest are not counted.
    add_only: bool,
    /// Every shared payload counted occurs once and `seen` is not kept (a leaf measured by
    /// [`LeafScan`]). Removing a value then needs no lookup; a shared value cannot be added (the
    /// writer drops the facts first).
    distinct: bool,
}

impl<'a> DictAcc<'a> {
    /// Take one value; `keep` makes the set's copy of a payload not seen before.
    pub(crate) fn add(&mut self, it: &Item<'_>, keep: impl FnOnce(&[u8]) -> Cow<'a, [u8]>) {
        if self.add_only && self.d > 256 {
            return;
        }
        match it {
            Item::Null => {
                if self.nulls == 0 {
                    self.d += 1;
                }
                self.nulls += 1;
            }
            Item::Own => self.d += 1,
            Item::Shared(p) => {
                assert!(
                    !self.distinct,
                    "BUG: a value added to a dictionary kept as distinct"
                );
                if let Some(rows) = self.seen.get_mut(p.as_ref()) {
                    *rows += 1;
                    self.dup += 1 + p.len(); // the span: its tag and content
                } else {
                    self.seen.insert(keep(p), 1);
                    self.d += 1;
                }
            }
        }
    }

    /// [`add`](Self::add) for `k` values with the same item (`k ≥ 1`).
    pub(super) fn add_n(&mut self, it: &Item<'_>, k: usize) {
        match it {
            Item::Null => {
                if self.nulls == 0 {
                    self.d += 1;
                }
                self.nulls += k;
            }
            Item::Own => self.d += k,
            Item::Shared(p) => {
                assert!(
                    !self.distinct,
                    "BUG: a value added to a dictionary kept as distinct"
                );
                if let Some(rows) = self.seen.get_mut(p.as_ref()) {
                    *rows += k;
                    self.dup += k * (1 + p.len());
                } else {
                    self.seen.insert(Cow::Owned(p.to_vec()), k);
                    self.d += 1;
                    self.dup += (k - 1) * (1 + p.len());
                }
            }
        }
    }

    /// Take out one value [`add`](Self::add) was given. The facts are those of the remaining
    /// values in any order: an entry is counted once however many rows share it.
    fn remove(&mut self, it: &Item<'_>) {
        match it {
            Item::Null => {
                self.nulls -= 1;
                if self.nulls == 0 {
                    self.d -= 1;
                }
            }
            Item::Own => self.d -= 1,
            Item::Shared(_) if self.distinct => self.d -= 1,
            Item::Shared(p) => match self.seen.get_mut(p.as_ref()) {
                Some(rows) if *rows > 1 => {
                    *rows -= 1;
                    self.dup -= 1 + p.len();
                }
                Some(_) => {
                    self.seen.remove(p.as_ref());
                    self.d -= 1;
                }
                None => panic!("BUG: a value removed from a dictionary that never took it"),
            },
        }
    }
}

/// The bytes a dictionary saves in a region of `n` values; 0 when the plain region is no larger or
/// the entries exceed a one-byte code.
pub(crate) fn dict_saving(n: usize, d: usize, dup: usize) -> usize {
    if d > 256 {
        return 0;
    }
    (n + dup).saturating_sub(2 + 2 * d)
}

/// A packed-array region's facts.
#[derive(Clone, Copy, Default, Debug)]
pub(crate) struct ArrAcc {
    pub(crate) empties: usize,
    pub(crate) arrays: usize,
    /// Elements over all arrays.
    pub(crate) elems: usize,
    pub(crate) r: ForRange,
    /// A value the packed form cannot hold.
    pub(crate) bad: bool,
}

impl ArrAcc {
    pub(crate) fn add(&mut self, it: &Item<'_>, w: usize) {
        match it {
            Item::Null => {}
            Item::Own => self.bad = true,
            Item::Shared(body) => match arr_elems(body, w) {
                None => self.bad = true,
                Some([]) => self.empties += 1,
                Some(elems) => {
                    self.arrays += 1;
                    self.elems += elems.len() / w;
                    for e in elems.chunks_exact(w) {
                        self.r.add(be_uint(e));
                    }
                }
            },
        }
    }

    /// Take out one value [`add`](Self::add) was given; `false` when the facts cannot be kept
    /// exact (the value made the region unpackable, or held an end of the element range).
    fn remove(&mut self, it: &Item<'_>, w: usize) -> bool {
        match it {
            Item::Null => true,
            Item::Own => false,
            Item::Shared(body) => match arr_elems(body, w) {
                None => false,
                Some([]) => {
                    self.empties -= 1;
                    true
                }
                Some(elems) => {
                    if elems.chunks_exact(w).any(|e| {
                        let v = be_uint(e);
                        v == self.r.lo || v == self.r.hi
                    }) {
                        return false;
                    }
                    self.arrays -= 1;
                    self.elems -= elems.len() / w;
                    true
                }
            },
        }
    }
}

/// The element bytes of an array body the packed form holds (empty for the empty array): one
/// dimension, lower bound 1, no NULL element, 1 to 254 elements of width `w`.
pub(crate) fn arr_elems(body: &[u8], w: usize) -> Option<&[u8]> {
    if body == [0, 0] {
        return Some(&[]);
    }
    if body.len() < 10 || body[0] != 1 || body[1] != 0 {
        return None;
    }
    let m = be_uint(&body[2..6]) as usize;
    if !(1..=254).contains(&m) || be_uint(&body[6..10]) != 1 || body.len() != 10 + m * w {
        return None;
    }
    Some(&body[10..])
}

/// The bit width of a packed-array region's elements.
pub(crate) fn arr_bits(r: ForRange) -> usize {
    if !r.any {
        return 0;
    }
    (64 - (r.hi - r.lo).leading_zeros()) as usize
}

/// The bytes the packed form saves in a region of `n` values of element width `w`.
pub(crate) fn arr_saving(n: usize, w: usize, a: &ArrAcc) -> usize {
    if a.bad {
        return 0;
    }
    let plain = 2 * n + 3 * a.empties + 11 * a.arrays + a.elems * w;
    let packed = n + w + 1 + (a.elems * arr_bits(a.r)).div_ceil(8);
    plain.saturating_sub(packed)
}

/// Read `bw` bits at bit offset `at`, MSB-first.
pub(crate) fn get_bits(b: &[u8], at: usize, bw: usize) -> u64 {
    let mut v = 0u64;
    for p in at..at + bw {
        v = v << 1 | (b[p / 8] >> (7 - p % 8) & 1) as u64;
    }
    v
}

/// Write the low `bw` bits of `v` at bit offset `at`, MSB-first, into zeroed bytes.
pub(crate) fn put_bits(b: &mut [u8], at: usize, bw: usize, v: u64) {
    for i in 0..bw {
        if v >> (bw - 1 - i) & 1 != 0 {
            let p = at + i;
            b[p / 8] |= 0x80 >> (p % 8);
        }
    }
}

/// The copyable facts of one item column, which split planning records at every boundary.
#[derive(Clone, Copy, Default, Debug)]
pub(super) struct ColFacts {
    pub(super) d: usize,
    pub(super) dup: usize,
    pub(super) arr: ArrAcc,
}

fn item_saving(ic: &ItemCol, n: usize, f: &ColFacts) -> usize {
    if ic.arr_w != 0 {
        arr_saving(n, ic.arr_w, &f.arr)
    } else {
        dict_saving(n, f.d, f.dup)
    }
}

/// One item column's accumulator: its dictionary set, and its facts so far.
#[derive(Clone, Default)]
pub(super) struct ColAcc<'a> {
    pub(super) dict: DictAcc<'a>,
    pub(super) f: ColFacts,
}

impl<'a> ColAcc<'a> {
    pub(super) fn add(
        &mut self,
        ic: &ItemCol,
        it: &Item<'_>,
        keep: impl FnOnce(&[u8]) -> Cow<'a, [u8]>,
    ) {
        if ic.arr_w != 0 {
            self.f.arr.add(it, ic.arr_w);
        } else {
            self.dict.add(it, keep);
            self.f.d = self.dict.d;
            self.f.dup = self.dict.dup;
        }
    }

    /// Take out one value [`add`](Self::add) was given; `false` when the facts cannot be kept exact.
    pub(super) fn remove(&mut self, ic: &ItemCol, it: &Item<'_>) -> bool {
        if ic.arr_w != 0 {
            return self.f.arr.remove(it, ic.arr_w);
        }
        self.dict.remove(it);
        self.f.d = self.dict.d;
        self.f.dup = self.dict.dup;
        true
    }

    /// An accumulator that only ever adds (split planning).
    fn add_only() -> ColAcc<'a> {
        let mut acc = ColAcc::default();
        acc.dict.add_only = true;
        acc
    }
}

/// Classify a decoded row's item-column values into `out` (one per `shape.item_cols`). A value is
/// inline-plain when the record's disposition plan leaves it so; the plan is computed only when the
/// all-inline record does not fit `RECORD_MAX`.
pub(super) fn row_items<'a>(
    shape: &LeafShape,
    cap: Cap,
    key: &[u8],
    row: &'a [Value],
    out: &mut Vec<Item<'a>>,
) {
    let plan = match all_inline_size(shape, key, row, cap) {
        Some(_) => None,
        // Every item value is NULL or too large to stay inline-plain under any plan: the
        // classification is known without planning — no compression, no memo lookup.
        None if shape.item_cols.iter().all(|ic| {
            matches!(row[ic.col], Value::Null) || never_inline(shape, row, cap, ic.col)
        }) =>
        {
            debug_assert!({
                let plan = plan_dispositions(shape, key, row, cap, None);
                shape.item_cols.iter().all(|ic| {
                    matches!(row[ic.col], Value::Null) || !matches!(plan.disp[ic.col], Disp::Inline)
                })
            });
            out.extend(shape.item_cols.iter().map(|ic| match row[ic.col] {
                Value::Null => Item::Null,
                _ => Item::Own,
            }));
            return;
        }
        // Large values: the plan compresses them, from the writing store's memo when it lent one.
        None => Some(crate::compress_memo::plan_with_lent(|memo| {
            plan_dispositions(shape, key, row, cap, memo)
        })),
    };
    for ic in &shape.item_cols {
        let v = &row[ic.col];
        let ty = &shape.col_types[ic.col];
        out.push(match (v, &plan) {
            (Value::Null, _) => Item::Null,
            (_, Some(plan)) if !matches!(plan.disp[ic.col], Disp::Inline) => Item::Own,
            // A stored row's deferred values equal its own plan's output (large-values.md §15), so
            // a record whose plan is all-inline holds no large form.
            (Value::Unfetched(u), _) if !matches!(u, Unfetched::Inline { .. }) => {
                panic!("BUG: a stored large form in a record whose plan is all-inline")
            }
            _ => Item::Shared(planning_payload(ty, v)),
        });
    }
}

/// The content-dependent size facts of a dirty leaf, kept with the node so an insert,
/// delete or row replacement updates them for one row instead of measuring the leaf again: each
/// base-plus-offsets column's value span and each item column's dictionary or packed-array facts.
/// [`add_row`](Self::add_row) and [`remove_row`](Self::remove_row) keep them equal to a fresh
/// measurement of the node's rows.
#[derive(Clone)]
pub(crate) struct LeafSizes {
    pub(super) n: usize,
    pub(super) fors: Vec<ForRange>,
    /// Each base-plus-offsets column's present values.
    pub(super) for_cnt: Vec<usize>,
    /// A column whose range lost a value at one of its ends: `fors` then only contains the true
    /// range, until [`refresh`](Self::refresh) rescans the column. `stale` says any column is.
    pub(super) for_stale: Vec<bool>,
    pub(super) stale: bool,
    pub(super) accs: Vec<ColAcc<'static>>,
    /// A frozen column (leaf_lazy.rs): its saving is the one its leaf's page shows (`for_save` /
    /// `item_save`), and its other facts are unset. `frozen` counts them. Only a lazy leaf whose
    /// rows are its page's has any.
    pub(super) for_frozen: Vec<bool>,
    pub(super) for_save: Vec<usize>,
    pub(super) item_frozen: Vec<bool>,
    pub(super) item_save: Vec<usize>,
    pub(super) frozen: usize,
    /// Some column's dictionary is kept as distinct ([`LeafScan::sizes`]): the facts are dropped
    /// before a row is added.
    distinct: bool,
}

impl LeafSizes {
    /// The facts of a leaf of no records.
    pub(super) fn empty(shape: &LeafShape) -> LeafSizes {
        let (fors, items) = (shape.for_cols.len(), shape.item_cols.len());
        LeafSizes {
            n: 0,
            fors: vec![ForRange::default(); fors],
            for_cnt: vec![0; fors],
            for_stale: vec![false; fors],
            stale: false,
            accs: shape.item_cols.iter().map(|_| ColAcc::default()).collect(),
            for_frozen: vec![false; fors],
            for_save: vec![0; fors],
            item_frozen: vec![false; items],
            item_save: vec![0; items],
            frozen: 0,
            distinct: false,
        }
    }

    /// Whether some dictionary is kept as distinct: such facts cannot take a row, so the writer
    /// drops them before an insert or replacement.
    pub(crate) fn distinct(&self) -> bool {
        self.distinct
    }

    /// How many columns are frozen at their page's saving.
    pub(crate) fn frozen(&self) -> usize {
        self.frozen
    }

    /// Measure a leaf of `n` records.
    pub(crate) fn measure<'a>(
        shape: &LeafShape,
        cap: Cap,
        n: usize,
        key: &dyn Fn(usize) -> &'a [u8],
        row: &dyn Fn(usize) -> &'a [Value],
    ) -> LeafSizes {
        let mut sz = LeafSizes::empty(shape);
        for i in 0..n {
            sz.add_row(shape, cap, key(i), row(i));
        }
        sz
    }

    pub(crate) fn add_row(&mut self, shape: &LeafShape, cap: Cap, key: &[u8], row: &[Value]) {
        debug_assert_eq!(self.frozen, 0, "a frozen column takes no row");
        self.n += 1;
        for (j, fc) in shape.for_cols.iter().enumerate() {
            let v = &row[fc.col];
            if !matches!(v, Value::Null) {
                self.fors[j].add(for_slot(&shape.col_types[fc.col], fc.w, v));
                self.for_cnt[j] += 1;
            }
        }
        if shape.item_cols.is_empty() {
            return;
        }
        let mut items = Vec::with_capacity(shape.item_cols.len());
        row_items(shape, cap, key, row, &mut items);
        for ((acc, ic), it) in self.accs.iter_mut().zip(&shape.item_cols).zip(&items) {
            acc.add(ic, it, |p| Cow::Owned(p.to_vec()));
        }
    }

    /// Take one of the measured rows out. `false` when the facts cannot be kept exact this way (a
    /// packed array that is unpackable or holds an end of its element range); the facts are then
    /// unusable, and the caller drops them so the leaf is measured again.
    pub(crate) fn remove_row(
        &mut self,
        shape: &LeafShape,
        cap: Cap,
        key: &[u8],
        row: &[Value],
    ) -> bool {
        debug_assert_eq!(self.frozen, 0, "a frozen column gives up no row");
        if !shape.item_cols.is_empty() {
            let mut items = Vec::with_capacity(shape.item_cols.len());
            row_items(shape, cap, key, row, &mut items);
            for ((acc, ic), it) in self.accs.iter_mut().zip(&shape.item_cols).zip(&items) {
                if !acc.remove(ic, it) {
                    return false;
                }
            }
        }
        self.n -= 1;
        for (j, fc) in shape.for_cols.iter().enumerate() {
            let v = &row[fc.col];
            if matches!(v, Value::Null) {
                continue;
            }
            self.for_cnt[j] -= 1;
            if self.for_cnt[j] == 0 {
                self.fors[j] = ForRange::default();
                self.for_stale[j] = false;
            } else {
                let s = for_slot(&shape.col_types[fc.col], fc.w, v);
                if s == self.fors[j].lo || s == self.fors[j].hi {
                    self.for_stale[j] = true;
                    self.stale = true;
                }
            }
        }
        true
    }

    /// Whether a column's range is stale: [`saving`](Self::saving) is then an upper bound.
    pub(crate) fn stale(&self) -> bool {
        self.stale
    }

    /// Rescan the columns whose range went stale. `row` reads the node's `len()` rows.
    pub(crate) fn refresh<'a>(&mut self, shape: &LeafShape, row: &dyn Fn(usize) -> &'a [Value]) {
        if !self.stale {
            return;
        }
        for (j, fc) in shape.for_cols.iter().enumerate() {
            if !self.for_stale[j] {
                continue;
            }
            let ty = &shape.col_types[fc.col];
            let mut r = ForRange::default();
            for i in 0..self.n {
                let v = &row(i)[fc.col];
                if !matches!(v, Value::Null) {
                    r.add(for_slot(ty, fc.w, v));
                }
            }
            self.fors[j] = r;
            self.for_stale[j] = false;
        }
        self.stale = false;
    }

    /// The record count these facts cover.
    pub(crate) fn len(&self) -> usize {
        self.n
    }

    /// The bytes the leaf's content-dependent regions save. While a column's range is stale the
    /// result is an upper bound: the column is taken at its narrowest offsets.
    pub(crate) fn saving(&self, shape: &LeafShape) -> usize {
        const NARROWEST: ForRange = ForRange {
            lo: 0,
            hi: 0,
            any: true,
        };
        let fors: usize = shape
            .for_cols
            .iter()
            .enumerate()
            .map(|(j, fc)| {
                if self.for_frozen[j] {
                    return self.for_save[j];
                }
                let r = if self.for_stale[j] {
                    NARROWEST
                } else {
                    self.fors[j]
                };
                for_saving(self.n, fc.w, r)
            })
            .sum();
        let items: usize = self
            .accs
            .iter()
            .zip(&shape.item_cols)
            .enumerate()
            .map(|(j, (acc, ic))| {
                if self.item_frozen[j] {
                    self.item_save[j]
                } else {
                    item_saving(ic, self.n, &acc.f)
                }
            })
            .sum();
        fors + items
    }
}

/// The bytes a decoded leaf's content-dependent encodings save: the shared key prefix and its
/// regions' base-plus-offsets, dictionary and packed-array forms, measured afresh.
#[cfg(test)]
pub(crate) fn leaf_saving<'a>(
    shape: &LeafShape,
    cap: Cap,
    n: usize,
    key: &dyn Fn(usize) -> &'a [u8],
    row: &dyn Fn(usize) -> &'a [Value],
) -> usize {
    if n == 0 {
        return 0;
    }
    let mut saving = leaf_prefix_saving(n, key(0), key(n - 1));
    if shape.content_cols() {
        saving += LeafSizes::measure(shape, cap, n, key, row).saving(shape);
    }
    saving
}

/// A leaf's content facts over every prefix and every suffix of its records, so the split-point
/// scan measures each candidate half in `O(columns)` (format.md "Leaf split").
pub(crate) struct SplitFacts<'a, 'k> {
    n: usize,
    key: &'k dyn Fn(usize) -> &'a [u8],
    /// `fors[j][m]`: column `j`'s span over records `[0, m)` (`pre`) or `[m, N)` (`suf`).
    for_pre: Vec<Vec<ForRange>>,
    for_suf: Vec<Vec<ForRange>>,
    item_pre: Vec<Vec<ColFacts>>,
    item_suf: Vec<Vec<ColFacts>>,
}

impl<'a, 'k> SplitFacts<'a, 'k> {
    pub(crate) fn new(
        shape: &LeafShape,
        cap: Cap,
        n: usize,
        key: &'k dyn Fn(usize) -> &'a [u8],
        row: &dyn Fn(usize) -> &'a [Value],
    ) -> SplitFacts<'a, 'k> {
        let mut for_pre = Vec::with_capacity(shape.for_cols.len());
        let mut for_suf = Vec::with_capacity(shape.for_cols.len());
        for fc in &shape.for_cols {
            let ty = &shape.col_types[fc.col];
            let slots: Vec<Option<u64>> = (0..n)
                .map(|i| match &row(i)[fc.col] {
                    Value::Null => None,
                    v => Some(for_slot(ty, fc.w, v)),
                })
                .collect();
            let mut pre = vec![ForRange::default(); n + 1];
            for i in 0..n {
                pre[i + 1] = pre[i];
                if let Some(s) = slots[i] {
                    pre[i + 1].add(s);
                }
            }
            let mut suf = vec![ForRange::default(); n + 1];
            for i in (0..n).rev() {
                suf[i] = suf[i + 1];
                if let Some(s) = slots[i] {
                    suf[i].add(s);
                }
            }
            for_pre.push(pre);
            for_suf.push(suf);
        }
        let mut item_pre = Vec::with_capacity(shape.item_cols.len());
        let mut item_suf = Vec::with_capacity(shape.item_cols.len());
        if !shape.item_cols.is_empty() {
            // items[i·J + j] is record i's item for item column j.
            let cols = shape.item_cols.len();
            let mut items: Vec<Item<'a>> = Vec::with_capacity(n * cols);
            for i in 0..n {
                row_items(shape, cap, key(i), row(i), &mut items);
            }
            for (j, ic) in shape.item_cols.iter().enumerate() {
                let mut pre = vec![ColFacts::default(); n + 1];
                let mut fwd = ColAcc::add_only();
                for i in 0..n {
                    let it = &items[i * cols + j];
                    fwd.add(ic, it, |_| item_payload(it));
                    pre[i + 1] = fwd.f;
                }
                let mut suf = vec![ColFacts::default(); n + 1];
                let mut back = ColAcc::add_only();
                for i in (0..n).rev() {
                    let it = &items[i * cols + j];
                    back.add(ic, it, |_| item_payload(it));
                    suf[i] = back.f;
                }
                item_pre.push(pre);
                item_suf.push(suf);
            }
        }
        SplitFacts {
            n,
            key,
            for_pre,
            for_suf,
            item_pre,
            item_suf,
        }
    }

    fn regions(
        shape: &LeafShape,
        count: usize,
        at: usize,
        fors: &[Vec<ForRange>],
        items: &[Vec<ColFacts>],
    ) -> usize {
        let f: usize = shape
            .for_cols
            .iter()
            .zip(fors)
            .map(|(fc, spans)| for_saving(count, fc.w, spans[at]))
            .sum();
        let i: usize = shape
            .item_cols
            .iter()
            .zip(items)
            .map(|(ic, facts)| item_saving(ic, count, &facts[at]))
            .sum();
        f + i
    }

    /// The saving of a left half holding records `[0, m)`.
    pub(crate) fn left(&self, shape: &LeafShape, m: usize) -> usize {
        leaf_prefix_saving(m, (self.key)(0), (self.key)(m - 1))
            + Self::regions(shape, m, m, &self.for_pre, &self.item_pre)
    }

    /// The saving of a right half holding records `[m, N)`.
    pub(crate) fn right(&self, shape: &LeafShape, m: usize) -> usize {
        leaf_prefix_saving(self.n - m, (self.key)(m), (self.key)(self.n - 1))
            + Self::regions(shape, self.n - m, m, &self.for_suf, &self.item_suf)
    }
}

/// One dictionary column's values by row, from one pass over a leaf: whether each shared value is
/// the first, and the last, of its payload. `dups` says some payload repeats.
struct DictScan {
    first: Vec<bool>,
    last: Vec<bool>,
    dups: bool,
}

/// A leaf's rows classified once, with every dictionary column scanned: the whole leaf's size
/// facts, the two-way split point and each half's facts all come from it with no further hashing.
pub(crate) struct LeafScan<'a> {
    n: usize,
    /// `items[i·J + j]` is record `i`'s item for item column `j`.
    items: Vec<Item<'a>>,
    /// Per item column; `None` for a packed-array column.
    dicts: Vec<Option<DictScan>>,
    dups: bool,
}

impl<'a> LeafScan<'a> {
    pub(crate) fn new(
        shape: &LeafShape,
        cap: Cap,
        n: usize,
        key: &dyn Fn(usize) -> &'a [u8],
        row: &dyn Fn(usize) -> &'a [Value],
    ) -> LeafScan<'a> {
        let cols = shape.item_cols.len();
        let mut items: Vec<Item<'a>> = Vec::with_capacity(n * cols);
        if cols != 0 {
            for i in 0..n {
                row_items(shape, cap, key(i), row(i), &mut items);
            }
        }
        let mut dicts = Vec::with_capacity(cols);
        let mut dups = false;
        for (j, ic) in shape.item_cols.iter().enumerate() {
            if ic.arr_w != 0 {
                dicts.push(None);
                continue;
            }
            let mut sc = DictScan {
                first: vec![false; n],
                last: vec![false; n],
                dups: false,
            };
            // payload -> its latest row
            let mut at: HashMap<&[u8], usize> = HashMap::with_capacity(n);
            for i in 0..n {
                let Item::Shared(p) = &items[i * cols + j] else {
                    continue;
                };
                match at.insert(p.as_ref(), i) {
                    Some(prev) => {
                        sc.last[prev] = false;
                        sc.dups = true;
                    }
                    None => sc.first[i] = true,
                }
                sc.last[i] = true;
            }
            dups |= sc.dups;
            dicts.push(Some(sc));
        }
        LeafScan {
            n,
            items,
            dicts,
            dups,
        }
    }

    /// Whether any dictionary column repeats a payload.
    pub(crate) fn dups(&self) -> bool {
        self.dups
    }

    /// The size facts of records `[lo, hi)`. A dictionary column with no repeated payload is kept
    /// as distinct, with no set; one with repeats is counted into a set, which needs the whole
    /// leaf (`lo = 0`, `hi = N`).
    pub(crate) fn sizes(
        &self,
        shape: &LeafShape,
        row: &dyn Fn(usize) -> &'a [Value],
        lo: usize,
        hi: usize,
    ) -> LeafSizes {
        let cols = shape.item_cols.len();
        let mut sz = LeafSizes::empty(shape);
        sz.n = hi - lo;
        for (j, fc) in shape.for_cols.iter().enumerate() {
            let ty = &shape.col_types[fc.col];
            for i in lo..hi {
                let v = &row(i)[fc.col];
                if !matches!(v, Value::Null) {
                    sz.fors[j].add(for_slot(ty, fc.w, v));
                    sz.for_cnt[j] += 1;
                }
            }
        }
        let mut distinct = false;
        for (j, ic) in shape.item_cols.iter().enumerate() {
            let acc = &mut sz.accs[j];
            let col = (lo..hi).map(|i| &self.items[i * cols + j]);
            match &self.dicts[j] {
                None => col.for_each(|it| acc.f.arr.add(it, ic.arr_w)),
                Some(sc) if !sc.dups => {
                    let d = &mut acc.dict;
                    d.distinct = true;
                    distinct = true;
                    for it in col {
                        match it {
                            Item::Null => d.nulls += 1,
                            _ => d.d += 1,
                        }
                    }
                    if d.nulls > 0 {
                        d.d += 1;
                    }
                    acc.f.d = d.d;
                }
                Some(_) => {
                    assert!(
                        lo == 0 && hi == self.n,
                        "BUG: part of a leaf whose dictionary repeats a payload"
                    );
                    col.for_each(|it| acc.add(ic, it, |p| Cow::Owned(p.to_vec())));
                }
            }
        }
        sz.distinct = distinct;
        sz
    }

    /// The two-way split point of a leaf that does not fit (format.md "Split point"), over running
    /// facts instead of per-boundary tables: one pass up from the first record while the left half
    /// fits, one pass down from the last while the right half does. Both half sizes are monotone in
    /// `m`, so the passes stop at the first boundary that does not fit; the balanced boundary, when
    /// it lies past the last fitting one, is clamped to it either way. `prefix[m]` is the records'
    /// summed weight over `[0, m)` and `payload` the whole leaf's. `None` when no two halves fit.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn split_point(
        &self,
        shape: &LeafShape,
        cap_bytes: usize,
        key: &dyn Fn(usize) -> &'a [u8],
        row: &dyn Fn(usize) -> &'a [Value],
        prefix: &[usize],
        payload: usize,
        right_edge: bool,
    ) -> Option<usize> {
        let n = self.n;
        let total = prefix[n];
        let mut run = RunningFacts::new(shape);
        let (mut m_max, mut m_balanced) = (None, None);
        for m in 1..n {
            run.add(self, shape, row(m - 1), m - 1, true);
            let left = prefix[m] + leaf_overhead(m, shape)
                - leaf_prefix_saving(m, key(0), key(m - 1))
                - run.saving(shape, m);
            if left > cap_bytes {
                break;
            }
            m_max = Some(m);
            if m_balanced.is_none() && 2 * left >= payload {
                m_balanced = Some(m);
            }
        }
        let m_max = m_max?;
        let mut run = RunningFacts::new(shape);
        let mut m_min = None;
        for m in (1..n).rev() {
            run.add(self, shape, row(m), m, false);
            let right = total - prefix[m] + leaf_overhead(n - m, shape)
                - leaf_prefix_saving(n - m, key(m), key(n - 1))
                - run.saving(shape, n - m);
            if right > cap_bytes {
                break;
            }
            m_min = Some(m);
        }
        let m_min = m_min?;
        if m_min > m_max {
            return None;
        }
        let m = match m_balanced {
            Some(m) if !right_edge => m,
            _ => m_max,
        };
        Some(m.max(m_min))
    }
}

/// The content facts of a half that grows by one record at a time ([`LeafScan::split_point`]).
struct RunningFacts {
    fors: Vec<ForRange>,
    facts: Vec<ColFacts>,
    /// Per item column: the half holds a NULL.
    nulls: Vec<bool>,
}

impl RunningFacts {
    fn new(shape: &LeafShape) -> RunningFacts {
        RunningFacts {
            fors: vec![ForRange::default(); shape.for_cols.len()],
            facts: vec![ColFacts::default(); shape.item_cols.len()],
            nulls: vec![false; shape.item_cols.len()],
        }
    }

    /// Take record `i` (whose row is `row`) into the half; `up` says the half grows from the
    /// first record.
    fn add(&mut self, scan: &LeafScan<'_>, shape: &LeafShape, row: &[Value], i: usize, up: bool) {
        for (r, fc) in self.fors.iter_mut().zip(&shape.for_cols) {
            let v = &row[fc.col];
            if !matches!(v, Value::Null) {
                r.add(for_slot(&shape.col_types[fc.col], fc.w, v));
            }
        }
        let cols = shape.item_cols.len();
        for (j, ic) in shape.item_cols.iter().enumerate() {
            let it = &scan.items[i * cols + j];
            let f = &mut self.facts[j];
            let Some(sc) = &scan.dicts[j] else {
                f.arr.add(it, ic.arr_w);
                continue;
            };
            match it {
                Item::Null => {
                    if !self.nulls[j] {
                        self.nulls[j] = true;
                        f.d += 1;
                    }
                }
                Item::Own => f.d += 1,
                Item::Shared(p) => {
                    // A value repeats one already in the half exactly when it is not the first of
                    // its payload (growing up) or not the last (growing down).
                    let fresh = if up { sc.first[i] } else { sc.last[i] };
                    if fresh {
                        f.d += 1;
                    } else {
                        f.dup += 1 + p.len(); // the span: its tag and content
                    }
                }
            }
        }
    }

    /// The bytes the half's regions save, at `count` records.
    fn saving(&self, shape: &LeafShape, count: usize) -> usize {
        let fors: usize = shape
            .for_cols
            .iter()
            .zip(&self.fors)
            .map(|(fc, &r)| for_saving(count, fc.w, r))
            .sum();
        let items: usize = shape
            .item_cols
            .iter()
            .zip(&self.facts)
            .map(|(ic, f)| item_saving(ic, count, f))
            .sum();
        fors + items
    }
}

/// A borrowed copy of an item's payload, for an accumulator that the items outlive.
fn item_payload<'s>(it: &'s Item<'_>) -> Cow<'s, [u8]> {
    match it {
        Item::Shared(p) => Cow::Borrowed(p.as_ref()),
        _ => unreachable!("only an inline-plain value has a payload"),
    }
}
