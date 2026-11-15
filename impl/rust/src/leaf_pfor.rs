//! Format v54 (spec/fileformat/format.md "Leaf node", spec/design/leaf-write-cost.md §8–§9): a
//! base-plus-offsets region with exceptions. A fixed column eligible for base plus offsets may take
//! region kind 6:
//!
//! ```text
//! null bitmap | base (w) | offset width bw (u8) | E (u8) | E exception rows (u16, ascending)
//! | E exception bodies (w) | N offsets (bw)
//! ```
//!
//! A layout is a width `bw` narrower than kind 1's and a base among the present values; its
//! exceptions are the present values outside `[base, base + 2^(8·bw) − 1]`, at most 16, each stored
//! whole with offset zero. The region takes the layout of least size `(E + 1)·(w + 2) + N·bw`, the
//! narrower width and then the lower base on a tie, when it saves more than kind 1. The exceptions
//! are some of the 16 smallest and 16 largest present values, so a column's size facts are its 17
//! smallest and 17 largest ([`PforAcc`]). A reader reads the region in place: row `i` is its
//! exception's body when `i` is an exception row (a bucket mask rejects most rows before the
//! search), else the base plus offset `i` (format.rs `PaxDirs::for_value`).

use super::leaf_compact::{ForRange, be_uint, for_width, put_be_uint};

/// The region kind byte of a base-plus-offsets region with exceptions.
pub(crate) const REGION_PFOR: u8 = 6;

/// The most values a kind 6 region stores whole.
pub(crate) const PFOR_MAX_K: usize = 16;

/// How many of the smallest and of the largest present values the size facts keep.
const K1: usize = PFOR_MAX_K + 1;

/// The offset widths a layout may take, indexed as [`PforAcc`]'s cache.
const WIDTHS: [usize; 4] = [0, 1, 2, 4];

/// Test switches, per thread: `off` makes no kind 6 layout admissible (kind 1 alone), and
/// `written` counts the kind 6 regions written and the exceptions they store.
#[cfg(test)]
pub(crate) mod pfor_test {
    use std::cell::Cell;

    thread_local! {
        static OFF: Cell<bool> = const { Cell::new(false) };
        static WRITTEN: Cell<(u64, u64)> = const { Cell::new((0, 0)) };
    }

    pub(crate) fn set_off(off: bool) {
        OFF.with(|c| c.set(off));
    }

    pub(crate) fn off() -> bool {
        OFF.with(Cell::get)
    }

    pub(crate) fn written() -> (u64, u64) {
        WRITTEN.with(Cell::get)
    }

    pub(super) fn wrote(exceptions: usize) {
        WRITTEN.with(|c| {
            let (r, e) = c.get();
            c.set((r + 1, e + exceptions as u64));
        });
    }
}

/// A column's kind 6 size facts: its 17 smallest and 17 largest present values (as a multiset).
#[derive(Clone, Default, Debug)]
pub(crate) struct PforAcc {
    /// `lo[..nlo]`, ascending: the smallest present values.
    lo: [u64; K1],
    /// `hi[..nhi]`, ascending: the largest present values.
    hi: [u64; K1],
    nlo: u8,
    nhi: u8,
    /// Present values.
    np: usize,
    /// `lo` and `hi` each hold `K1` values and enough values lie between them that a value in
    /// `[skip_lo, skip_hi] = [lo[K1 − 1], hi[0]]` changes nothing.
    full: bool,
    skip_lo: u64,
    skip_hi: u64,
    /// The least exception count for each offset width 0, 1, 2, 4 and the lowest base giving it, a
    /// function of `lo` and `hi` alone (not of the row count), so a saving query is four products.
    cached: bool,
    ce: [u8; 4],
    cb: [u64; 4],
    cok: [bool; 4],
}

/// A kind 6 layout: offsets of width `bw` from `base`, `e` exceptions.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PforPlan {
    pub(crate) bw: usize,
    pub(crate) base: u64,
    pub(crate) e: usize,
}

impl PforAcc {
    /// Take present value `v`. A value between the kept extremes is rejected by two compares.
    #[inline]
    pub(crate) fn add(&mut self, v: u64) {
        self.np += 1;
        if self.full && v >= self.skip_lo && v <= self.skip_hi {
            return; // neither among the K1 smallest nor the K1 largest
        }
        self.add_slow(v);
    }

    #[inline(never)]
    fn add_slow(&mut self, v: u64) {
        if self.np <= 2 * K1 + 1 {
            self.cached = false; // whether more values lie beyond `hi` can still change
        } else {
            self.full = true;
        }
        let nlo = self.nlo as usize;
        if nlo < K1 || v < self.lo[K1 - 1] {
            let mut i = if nlo < K1 {
                self.nlo += 1;
                nlo
            } else {
                K1 - 1 // the largest kept value drops out
            };
            while i > 0 && self.lo[i - 1] > v {
                self.lo[i] = self.lo[i - 1];
                i -= 1;
            }
            self.lo[i] = v;
            self.cached = false;
        }
        let nhi = self.nhi as usize;
        if nhi < K1 {
            self.nhi += 1;
            let mut i = nhi;
            while i > 0 && self.hi[i - 1] > v {
                self.hi[i] = self.hi[i - 1];
                i -= 1;
            }
            self.hi[i] = v;
            self.cached = false;
        } else if v > self.hi[0] {
            let mut i = 0; // the smallest kept value drops out
            while i + 1 < K1 && self.hi[i + 1] < v {
                self.hi[i] = self.hi[i + 1];
                i += 1;
            }
            self.hi[i] = v;
            self.cached = false;
        }
        if self.full {
            (self.skip_lo, self.skip_hi) = (self.lo[K1 - 1], self.hi[0]);
        }
    }

    /// Take out present value `v`: `true` when the facts may have changed, so the column must be
    /// measured again — while few values are present, or when `v` is one of the kept extremes.
    /// Otherwise the facts stay exact.
    pub(crate) fn remove(&mut self, v: u64) -> bool {
        if self.np <= 2 * K1 || v <= self.lo[K1 - 1] || v >= self.hi[0] {
            return true;
        }
        self.np -= 1;
        false
    }

    /// The smallest kind 6 layout of a `w`-byte column of `n` rows and its size: a width narrower
    /// than kind 1's, the narrower width and then the lower base on a tie. `None` when none is
    /// admissible.
    pub(crate) fn plan(&mut self, n: usize, w: usize) -> Option<(PforPlan, usize)> {
        if self.np == 0 {
            return None;
        }
        #[cfg(test)]
        if pfor_test::off() {
            return None;
        }
        if !self.cached {
            self.measure();
        }
        let span = ForRange {
            lo: self.lo[0],
            hi: self.hi[self.nhi as usize - 1],
            any: true,
        };
        let k1bw = for_width(w, span);
        let mut best: Option<(PforPlan, usize)> = None;
        for (x, &bw) in WIDTHS.iter().enumerate() {
            if bw >= k1bw {
                break;
            }
            if !self.cok[x] {
                continue;
            }
            let e = self.ce[x] as usize;
            let size = (e + 1) * (w + 2) + n * bw;
            if best.is_none_or(|(_, s)| size < s) {
                let plan = PforPlan {
                    bw,
                    base: self.cb[x],
                    e,
                };
                best = Some((plan, size));
            }
        }
        best
    }

    /// Fill the per-width cache: for each width, the least exception count over bases (at most
    /// [`PFOR_MAX_K`]) and the lowest base giving it. A base is one of the K1 smallest values (more
    /// below it would be too many exceptions); the values above its window are counted from `hi`,
    /// whose boundary moves only up as the base rises.
    fn measure(&mut self) {
        self.cached = true;
        let lo = &self.lo[..self.nlo as usize];
        let hi = &self.hi[..self.nhi as usize];
        for (x, &bw) in WIDTHS.iter().enumerate() {
            let r = if bw == 0 { 0 } else { (1u64 << (8 * bw)) - 1 };
            let mut best: Option<(usize, u64)> = None;
            let mut idx = 0; // hi[idx..] are the kept values above the window
            for j in 0..lo.len() {
                if j > 0 && lo[j] == lo[j - 1] {
                    continue;
                }
                let base = lo[j];
                let top = base.saturating_add(r);
                while idx < hi.len() && hi[idx] <= top {
                    idx += 1;
                }
                if idx == 0 && self.np > hi.len() {
                    continue; // more than PFOR_MAX_K above
                }
                let e = j + hi.len() - idx;
                if e > PFOR_MAX_K {
                    continue;
                }
                if best.is_none_or(|(be, _)| e < be) {
                    best = Some((e, base));
                }
            }
            match best {
                Some((e, base)) => {
                    (self.cok[x], self.ce[x], self.cb[x]) = (true, e as u8, base);
                }
                None => self.cok[x] = false,
            }
        }
    }

    /// The bytes a kind 6 region saves over the plain one (0 when it does not).
    pub(crate) fn saving(&mut self, n: usize, w: usize) -> usize {
        match self.plan(n, w) {
            Some((_, size)) => (n * w).saturating_sub(size),
            None => 0,
        }
    }
}

/// Append a kind 6 region of `n` fixed-width values of width `w` (`body` the dense slots, zeros
/// for NULL) under plan `p`: kind byte, null bitmap, base, offset width, exception count, the
/// exceptions' rows and bodies, then the offsets (an exception's and a NULL's zero).
pub(crate) fn encode_pfor(
    w: usize,
    n: usize,
    body: &[u8],
    nulls: &[bool],
    p: PforPlan,
    out: &mut Vec<u8>,
) {
    let r = if p.bw == 0 {
        0
    } else {
        (1u64 << (8 * p.bw)) - 1
    };
    let slot = |i: usize| be_uint(&body[i * w..(i + 1) * w]);
    let exc: Vec<usize> = (0..n)
        .filter(|&i| !nulls[i] && (slot(i) < p.base || slot(i) - p.base > r))
        .collect();
    assert_eq!(
        exc.len(),
        p.e,
        "BUG: a kind 6 plan's exception count differs from the encoded one"
    );
    #[cfg(test)]
    pfor_test::wrote(exc.len());
    out.push(REGION_PFOR);
    let bitmap = out.len();
    out.resize(bitmap + n.div_ceil(8), 0);
    for (i, &is_null) in nulls.iter().enumerate() {
        if is_null {
            out[bitmap + i / 8] |= 0x80 >> (i % 8);
        }
    }
    let at = out.len();
    out.resize(at + w + 2, 0);
    put_be_uint(&mut out[at..at + w], p.base);
    out[at + w] = p.bw as u8;
    out[at + w + 1] = p.e as u8;
    for &i in &exc {
        out.extend_from_slice(&(i as u16).to_be_bytes());
    }
    for &i in &exc {
        out.extend_from_slice(&body[i * w..(i + 1) * w]);
    }
    let at = out.len();
    out.resize(at + n * p.bw, 0);
    let mut k = 0;
    for i in 0..n {
        if k < exc.len() && exc[k] == i {
            k += 1;
            continue;
        }
        if !nulls[i] {
            put_be_uint(
                &mut out[at + i * p.bw..at + (i + 1) * p.bw],
                slot(i) - p.base,
            );
        }
    }
}
