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
use std::collections::HashSet;
use std::sync::Arc;

use super::{
    Cap, ColType, Disp, ScalarType, Unfetched, Value, all_inline_size, fixed_value_width,
    plan_dispositions, planning_payload,
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
fn for_eligible(ty: &ColType) -> bool {
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
    fn content_cols(&self) -> bool {
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
fn for_slot(ty: &ColType, w: usize, v: &Value) -> u64 {
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
#[derive(Default)]
pub(crate) struct DictAcc<'a> {
    seen: HashSet<Cow<'a, [u8]>>,
    pub(crate) d: usize,
    pub(crate) dup: usize,
    has_null: bool,
}

impl<'a> DictAcc<'a> {
    /// Take one value; `keep` makes the set's copy of a payload not seen before.
    pub(crate) fn add(&mut self, it: &Item<'_>, keep: impl FnOnce(&[u8]) -> Cow<'a, [u8]>) {
        match it {
            Item::Null => {
                if !self.has_null {
                    self.has_null = true;
                    self.d += 1;
                }
            }
            Item::Own => self.d += 1,
            Item::Shared(p) => {
                if self.seen.contains(p.as_ref()) {
                    self.dup += 1 + p.len(); // the span: its tag and content
                } else {
                    self.seen.insert(keep(p));
                    self.d += 1;
                }
            }
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
struct ColFacts {
    d: usize,
    dup: usize,
    arr: ArrAcc,
}

fn item_saving(ic: &ItemCol, n: usize, f: &ColFacts) -> usize {
    if ic.arr_w != 0 {
        arr_saving(n, ic.arr_w, &f.arr)
    } else {
        dict_saving(n, f.d, f.dup)
    }
}

/// One item column's accumulator: its dictionary set, and its facts so far.
#[derive(Default)]
struct ColAcc<'a> {
    dict: DictAcc<'a>,
    f: ColFacts,
}

impl<'a> ColAcc<'a> {
    fn add(&mut self, ic: &ItemCol, it: &Item<'_>, keep: impl FnOnce(&[u8]) -> Cow<'a, [u8]>) {
        if ic.arr_w != 0 {
            self.f.arr.add(it, ic.arr_w);
        } else {
            self.dict.add(it, keep);
            self.f.d = self.dict.d;
            self.f.dup = self.dict.dup;
        }
    }
}

/// Classify a decoded row's item-column values into `out` (one per `shape.item_cols`). A value is
/// inline-plain when the record's disposition plan leaves it so; the plan is computed only when the
/// all-inline record does not fit `RECORD_MAX`.
fn row_items<'a>(
    shape: &LeafShape,
    cap: Cap,
    key: &[u8],
    row: &'a [Value],
    out: &mut Vec<Item<'a>>,
) {
    let plan = match all_inline_size(shape, key, row, cap) {
        Some(_) => None,
        None => Some(plan_dispositions(shape, key, row, cap, None)),
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

/// The content-dependent size facts of a dirty decoded leaf, kept with the node so an insert
/// updates them for one row instead of measuring the leaf again: each base-plus-offsets column's
/// value span and each item column's dictionary or packed-array facts. All only grow under
/// inserts; any other change to the node's rows drops the cache.
pub(crate) struct LeafSizes {
    n: usize,
    fors: Vec<ForRange>,
    accs: Vec<ColAcc<'static>>,
}

impl LeafSizes {
    /// Measure a leaf of `n` records.
    pub(crate) fn measure<'a>(
        shape: &LeafShape,
        cap: Cap,
        n: usize,
        key: &dyn Fn(usize) -> &'a [u8],
        row: &dyn Fn(usize) -> &'a [Value],
    ) -> LeafSizes {
        let mut sz = LeafSizes {
            n: 0,
            fors: vec![ForRange::default(); shape.for_cols.len()],
            accs: shape.item_cols.iter().map(|_| ColAcc::default()).collect(),
        };
        for i in 0..n {
            sz.add_row(shape, cap, key(i), row(i));
        }
        sz
    }

    pub(crate) fn add_row(&mut self, shape: &LeafShape, cap: Cap, key: &[u8], row: &[Value]) {
        self.n += 1;
        for (r, fc) in self.fors.iter_mut().zip(&shape.for_cols) {
            let v = &row[fc.col];
            if !matches!(v, Value::Null) {
                r.add(for_slot(&shape.col_types[fc.col], fc.w, v));
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

    /// The record count these facts cover.
    pub(crate) fn len(&self) -> usize {
        self.n
    }

    /// The bytes the leaf's content-dependent regions save.
    pub(crate) fn saving(&self, shape: &LeafShape) -> usize {
        let fors: usize = self
            .fors
            .iter()
            .zip(&shape.for_cols)
            .map(|(r, fc)| for_saving(self.n, fc.w, *r))
            .sum();
        let items: usize = self
            .accs
            .iter()
            .zip(&shape.item_cols)
            .map(|(acc, ic)| item_saving(ic, self.n, &acc.f))
            .sum();
        fors + items
    }
}

/// The bytes a decoded leaf's content-dependent encodings save: the shared key prefix and its
/// regions' base-plus-offsets, dictionary and packed-array forms.
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
                let mut fwd = ColAcc::default();
                for i in 0..n {
                    let it = &items[i * cols + j];
                    fwd.add(ic, it, |_| item_payload(it));
                    pre[i + 1] = fwd.f;
                }
                let mut suf = vec![ColFacts::default(); n + 1];
                let mut back = ColAcc::default();
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

/// A borrowed copy of an item's payload, for an accumulator that the items outlive.
fn item_payload<'s>(it: &'s Item<'_>) -> Cow<'s, [u8]> {
    match it {
        Item::Shared(p) => Cow::Borrowed(p.as_ref()),
        _ => unreachable!("only an inline-plain value has a payload"),
    }
}
