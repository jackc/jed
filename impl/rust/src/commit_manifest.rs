//! Format v33 commit identities, with v53's narrow CRC-32 entries and page stamp. See
//! spec/design/validated-cow.md and spec/design/manifest-crc32.md.
use super::*;

const HEADER: usize = 64;
/// An encrypted meta's inline manifest starts after the catalog-root and free-list tags
/// (spec/fileformat/encrypted.md §6).
const ENCRYPTED_HEADER: usize = 96;
const OVERFLOW_HEADER: usize = 32;
pub(super) const PAGE_MANIFEST: u8 = 8;

/// A manifest entry's bytes: the page id and the page's CRC-32 (format v53; manifest-crc32.md §2).
pub(crate) const ENTRY: usize = 8;

/// A listed page's manifest identity: a plaintext page's own header checksum -- copied, not
/// recomputed -- and an encrypted page's CRC-32 of its ciphertext.
pub(crate) fn page_digest(page: &[u8], encrypted: bool) -> u64 {
    if encrypted {
        u64::from(crate::format::crc32_extend(0, page))
    } else {
        u64::from(u32_at(page, 12))
    }
}

/// Extend the manifest-overflow chain digest, a CRC-32 in the low 4 bytes of meta offset 48.
pub(crate) fn chain_extend(crc: u64, bytes: &[u8]) -> u64 {
    u64::from(crate::format::crc32_extend(crc as u32, bytes))
}

/// The stamp a commit at `txid` writes at header offset 2 of every page it seals (manifest-crc32.md
/// §2.1): the txid's low 16 bits.
pub(crate) fn stamp_of(txid: u64) -> u16 {
    txid as u16
}

/// The txid window a page listed by a manifest must carry a stamp from: `(floor, newest]`. Under
/// `synchronous = on` the window is the one commit; under `synchronous = off` a cumulative manifest
/// lists every page since the durability floor, each stamped by its own commit. An empty or inverted
/// window, or one of 2^16 or more, admits every stamp, leaving the CRC as the only check.
#[derive(Clone, Copy, Default)]
pub(crate) struct StampWindow {
    pub(crate) floor: u64,
    pub(crate) newest: u64,
}

impl StampWindow {
    /// No floor is known: every stamp is admitted.
    pub(crate) const NONE: StampWindow = StampWindow {
        floor: 0,
        newest: 0,
    };

    pub(crate) fn contains(self, stamp: u16) -> bool {
        if self.newest <= self.floor {
            return true;
        }
        let span = self.newest - self.floor;
        if span >= 1 << 16 {
            return true;
        }
        let d = u64::from(stamp.wrapping_sub(self.floor as u16));
        d >= 1 && d <= span
    }
}

/// A manifest-overflow page's header bytes 1-3 as the commit at `txid` wrote them: flags zero and the
/// commit's own stamp. A manifest page is always written by the commit whose meta names it, so this is
/// an exact equality.
pub(crate) fn manifest_page_header_ok(page: &[u8], txid: u64) -> bool {
    page[1] == 0 && u16::from_be_bytes([page[2], page[3]]) == stamp_of(txid)
}

/// Whether a listed page still carries the identity its manifest entry records. A plaintext entry is
/// a copy of the page's own header checksum, so the page must also still verify against that checksum
/// (manifest-crc32.md §2); an encrypted page's ciphertext digest is the whole keyless check.
fn listed_page_matches(page: &[u8], encrypted: bool, expected: u64) -> bool {
    page_digest(page, encrypted) == expected
        && (encrypted || crate::format::page_crc(page) == u32_at(page, 12))
}

fn read_entry(e: &[u8]) -> (u32, u64) {
    (u32_at(e, 0), u64::from(u32_at(e, 4)))
}

const CRC64_POLYNOMIAL: u64 = 0x42F0_E1EB_A9EA_3693;

/// The eight slicing tables of the format's CRC-64/ECMA-182 (unreflected, so the first byte of a
/// word is its most significant): `tables[0]` is the byte table, and `tables[k]` advances a byte
/// past `k` further zero bytes. Implementation machinery; the checksum's parameters stay canonical
/// in spec/design/validated-cow.md.
const fn crc64_tables() -> [[u64; 256]; 8] {
    let mut tables = [[0u64; 256]; 8];
    let mut i = 0;
    while i < 256 {
        let mut crc = (i as u64) << 56;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & (1 << 63) != 0 {
                (crc << 1) ^ CRC64_POLYNOMIAL
            } else {
                crc << 1
            };
            bit += 1;
        }
        tables[0][i] = crc;
        i += 1;
    }
    let mut slice = 1;
    while slice < 8 {
        i = 0;
        while i < 256 {
            let crc = tables[slice - 1][i];
            tables[slice][i] = (crc << 8) ^ tables[0][(crc >> 56) as usize];
            i += 1;
        }
        slice += 1;
    }
    tables
}
const CRC64: [[u64; 256]; 8] = crc64_tables();

/// Fold `bytes` into a running CRC-64 register, eight bytes at a time (slicing-by-8) and the tail
/// byte by byte.
pub(super) fn extend(mut crc: u64, bytes: &[u8]) -> u64 {
    let mut words = bytes.chunks_exact(8);
    for word in &mut words {
        crc ^= u64::from_be_bytes(word.try_into().unwrap());
        crc = CRC64[7][(crc >> 56) as usize]
            ^ CRC64[6][(crc >> 48) as usize & 0xFF]
            ^ CRC64[5][(crc >> 40) as usize & 0xFF]
            ^ CRC64[4][(crc >> 32) as usize & 0xFF]
            ^ CRC64[3][(crc >> 24) as usize & 0xFF]
            ^ CRC64[2][(crc >> 16) as usize & 0xFF]
            ^ CRC64[1][(crc >> 8) as usize & 0xFF]
            ^ CRC64[0][crc as usize & 0xFF];
    }
    for &byte in words.remainder() {
        crc = (crc << 8) ^ CRC64[0][((crc >> 56) as u8 ^ byte) as usize];
    }
    crc
}
fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(b[at..at + 4].try_into().unwrap())
}
fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_be_bytes(b[at..at + 8].try_into().unwrap())
}

/// The byte range of a meta page of `ps` bytes that holds its inline manifest: after the header,
/// before an encrypted meta's reserve.
fn inline_area(ps: usize, encrypted: bool) -> (usize, usize) {
    // The remap region takes the area's last third, whether or not it holds entries (format.md
    // *Remap list*).
    let reserve = remap::reserve(ps, encrypted);
    if encrypted {
        (ENCRYPTED_HEADER, ps - crate::crypto::RESERVE - reserve)
    } else {
        (HEADER, ps - reserve)
    }
}

/// The inline (meta page) and per-overflow-page entry capacities for a page of `ps` bytes.
pub(crate) fn capacities(ps: usize, cap: Cap) -> (usize, usize) {
    let (lo, hi) = inline_area(ps, cap.encrypted_variant());
    ((hi - lo) / ENTRY, (ps - OVERFLOW_HEADER) / ENTRY)
}

pub(super) fn overflow_needed(ps: usize, cap: Cap, entries: usize) -> usize {
    let (inline, per) = capacities(ps, cap);
    entries.saturating_sub(inline).div_ceil(per)
}

/// The manifest entries -- `(page id, its manifest identity)` -- of written pages ([`page_digest`]).
pub(super) fn entries_of(pages: &[(u32, Vec<u8>)], encrypted: bool) -> Vec<(u32, u64)> {
    pages
        .iter()
        .map(|(id, bytes)| (*id, page_digest(bytes, encrypted)))
        .collect()
}

/// Return the final meta and overflow pages for the manifest `entries` (the commit's body and
/// free-list pages, and under batched durability the pending set — validated-cow.md §4; never the
/// manifest itself). The allocator has already reserved `ids`.
pub(super) fn encode(
    mut meta: Vec<u8>,
    cap: Cap,
    mut entries: Vec<(u32, u64)>,
    ids: &[u32],
) -> (Vec<u8>, Vec<(u32, Vec<u8>)>) {
    let ps = meta.len();
    let entry = ENTRY;
    entries.sort_unstable_by_key(|e| e.0);
    let (header, _) = inline_area(ps, cap.encrypted_variant());
    let inline = entries.len().min(capacities(ps, cap).0);
    let put_entries = |out: &mut [u8], entries: &[(u32, u64)]| {
        for (chunk, (id, hash)) in out.chunks_exact_mut(entry).zip(entries) {
            chunk[..4].copy_from_slice(&id.to_be_bytes());
            chunk[4..].copy_from_slice(&(*hash as u32).to_be_bytes());
        }
    };
    put_entries(
        &mut meta[header..header + inline * entry],
        &entries[..inline],
    );
    let mut pages = Vec::with_capacity(ids.len());
    let per = (ps - OVERFLOW_HEADER) / entry;
    let mut digest = 0;
    for (ordinal, &id) in ids.iter().enumerate() {
        let lo = (inline + ordinal * per).min(entries.len());
        let hi = (lo + per).min(entries.len());
        let mut payload = vec![0; 16 + (hi - lo) * entry];
        payload[..8].copy_from_slice(&meta[12..20]);
        payload[8..12].copy_from_slice(&(ordinal as u32).to_be_bytes());
        put_entries(&mut payload[16..], &entries[lo..hi]);
        let page = make_stamped_page(
            ps,
            PAGE_MANIFEST,
            (hi - lo) as u32,
            ids.get(ordinal + 1).copied().unwrap_or(0),
            &payload,
            stamp_of(u64_at(&meta, 12)),
        );
        digest = chain_extend(chain_extend(digest, &id.to_be_bytes()), &page);
        pages.push((id, page));
    }
    meta[36..40].copy_from_slice(&ids.first().copied().unwrap_or(0).to_be_bytes());
    let count = entries.len() as u32;
    meta[40..44].copy_from_slice(&count.to_be_bytes());
    meta[44..48].copy_from_slice(&(ids.len() as u32).to_be_bytes());
    meta[48..56].copy_from_slice(&digest.to_be_bytes());
    let crc = meta_crc(&meta);
    meta[32..36].copy_from_slice(&crc.to_be_bytes());
    (meta, pages)
}

/// The manifest-overflow chain a valid meta names, in chain order — what an encrypted meta's tag
/// covers (encrypted.md §6). Reads only the pages the meta counts; a malformed chain was already
/// refused by [`validate`].
pub(super) fn chain_pages(
    meta: &[u8],
    mut read: impl FnMut(u32) -> Result<Vec<u8>>,
) -> Result<Vec<(u32, Vec<u8>)>> {
    let mut out = Vec::new();
    let mut next = u32_at(meta, 36);
    for _ in 0..u32_at(meta, 44) {
        let page = read(next)?;
        let after = parse_page(&page)?.next_page;
        out.push((next, page));
        next = after;
    }
    Ok(out)
}

/// A candidate is either complete, structurally invalid, or unreadable. Only the second permits
/// fallback. Bounds are checked before host reads; actual host failures must propagate.
pub(super) fn validate(
    meta: &[u8],
    physical: u32,
    window: StampWindow,
    mut read: impl FnMut(u32) -> Result<Vec<u8>>,
) -> Result<Option<HashSet<u32>>> {
    match validate_inner(meta, physical, window, &mut read) {
        Ok(ids) => Ok(Some(ids)),
        Err(e) if e.state == SqlState::DataCorrupted => Ok(None),
        Err(e) => Err(e),
    }
}
fn validate_inner(
    meta: &[u8],
    physical: u32,
    window: StampWindow,
    read: &mut impl FnMut(u32) -> Result<Vec<u8>>,
) -> Result<HashSet<u32>> {
    let pc = u32_at(meta, 24);
    let count = u32_at(meta, 40) as usize;
    let entry = ENTRY;
    let n = u32_at(meta, 44) as usize;
    let mut next = u32_at(meta, 36);
    let bad = || corrupt("invalid commit manifest");
    // An encrypted meta (suite byte 6, checked by `parse_meta`): body pages start past the key block,
    // the inline manifest after the tags, and a listed page is ciphertext, so only its digest is
    // checked (encrypted.md §6).
    let encrypted = meta[6] != 0;
    let cap = Cap::of(meta.len() as u32, encrypted);
    let first = cap.first_body();
    if pc > physical
        || pc <= first
        || count as u64 + n as u64 > u64::from(pc - first)
        || (n == 0) != (next == 0)
        || (count == 0 && (n != 0 || u64_at(meta, 48) != 0))
        || overflow_needed(meta.len(), cap, count) > n
    {
        return Err(bad());
    }
    // A plaintext page must also carry a stamp from the window (manifest-crc32.md §2.1); an encrypted
    // page's stamp is sealed, so its ciphertext digest is the whole keyless check.
    let typed = |page: &[u8]| -> Result<bool> {
        Ok(encrypted
            || ((1..=7).contains(&parse_page(page)?.page_type)
                && window.contains(u16::from_be_bytes([page[2], page[3]]))))
    };
    let mut protected = HashSet::new();
    let mut last = 1;
    let mut remaining = count;
    let mut check_entries = |bytes: &[u8], number: usize| -> Result<()> {
        if bytes[number * entry..].iter().any(|b| *b != 0) {
            return Err(bad());
        }
        for e in bytes[..number * entry].chunks_exact(entry) {
            let (id, expected) = read_entry(e);
            if id < first || id <= last || id >= pc {
                return Err(bad());
            }
            last = id;
            let page = read(id)?;
            if page_digest(&page, encrypted) != expected || !typed(&page)? {
                return Err(bad());
            }
            protected.insert(id);
        }
        Ok(())
    };
    let (lo, hi) = inline_area(meta.len(), encrypted);
    let inline = count.min((hi - lo) / entry);
    check_entries(&meta[lo..hi], inline)?;
    remaining -= inline;
    // End the closure's borrow so chain pages can be read through the same host.
    drop(check_entries);
    let mut digest = 0;
    for ordinal in 0..n {
        if next < first
            || next >= pc
            || next == u32_at(meta, 20)
            || next == u32_at(meta, 28)
            || !protected.insert(next)
        {
            return Err(bad());
        }
        let id = next;
        let page = read(id)?;
        digest = chain_extend(chain_extend(digest, &id.to_be_bytes()), &page);
        let p = parse_page(&page)?;
        let number = remaining.min((meta.len() - OVERFLOW_HEADER) / entry);
        if p.page_type != PAGE_MANIFEST
            || p.item_count as usize != number
            || !manifest_page_header_ok(&page, u64_at(meta, 12))
            || u64_at(&page, 16) != u64_at(meta, 12)
            || u32_at(&page, 24) as usize != ordinal
            || u32_at(&page, 28) != 0
            || page[OVERFLOW_HEADER + number * entry..]
                .iter()
                .any(|b| *b != 0)
        {
            return Err(bad());
        }
        for e in page[OVERFLOW_HEADER..OVERFLOW_HEADER + number * entry].chunks_exact(entry) {
            let (id, expected) = read_entry(e);
            if id < first || id <= last || id >= pc || !protected.insert(id) {
                return Err(bad());
            }
            last = id;
            let data = read(id)?;
            if page_digest(&data, encrypted) != expected || !typed(&data)? {
                return Err(bad());
            }
        }
        remaining -= number;
        next = p.next_page;
    }
    if next != 0
        || remaining != 0
        || digest != u64_at(meta, 48)
        // The catalog root need not be listed -- a commit may keep its chain (format.md *Deferral*) --
        // but a nonzero free-list head must be.
        || (count != 0 && u32_at(meta, 28) != 0 && !protected.contains(&u32_at(meta, 28)))
    {
        return Err(bad());
    }
    Ok(protected)
}

/// The integrity check's reading of a meta's manifest (spec/design/integrity-check.md §3.1): the
/// pages the generation depends on, and those whose stored bytes no longer carry their recorded
/// identity, in manifest order. Unlike [`validate`] it does not stop at the first problem: every
/// listed page is read and compared. A manifest-overflow page that does not decode as one, or a
/// chain whose digest no longer matches, is reported as that overflow page (the chain's head when no
/// single page can be blamed), and ends the walk. Reads only pages inside `[first, physical)`.
pub(super) fn audit(
    meta: &[u8],
    physical: u32,
    mut read: impl FnMut(u32) -> Result<Vec<u8>>,
) -> Result<(HashSet<u32>, Vec<u32>)> {
    let encrypted = meta[6] != 0;
    let cap = Cap::of(meta.len() as u32, encrypted);
    let first = cap.first_body();
    let count = u32_at(meta, 40) as usize;
    let entry = ENTRY;
    let n = u32_at(meta, 44) as usize;
    let head = u32_at(meta, 36);
    let mut protected = HashSet::new();
    let mut bad = Vec::new();
    let entries = |bytes: &[u8],
                   number: usize,
                   protected: &mut HashSet<u32>,
                   bad: &mut Vec<u32>,
                   read: &mut dyn FnMut(u32) -> Result<Vec<u8>>|
     -> Result<()> {
        for e in bytes[..number * entry].chunks_exact(entry) {
            let (id, expected) = read_entry(e);
            protected.insert(id);
            if id < first || id >= physical {
                bad.push(id);
                continue;
            }
            if !listed_page_matches(&read(id)?, encrypted, expected) {
                bad.push(id);
            }
        }
        Ok(())
    };
    let (lo, hi) = inline_area(meta.len(), encrypted);
    let inline = count.min((hi - lo) / entry);
    entries(&meta[lo..hi], inline, &mut protected, &mut bad, &mut read)?;
    let mut remaining = count - inline;
    let mut digest = 0;
    let mut next = head;
    let mut chain_ok = true;
    for _ in 0..n {
        if next < first || next >= physical || !protected.insert(next) {
            bad.push(next);
            chain_ok = false;
            break;
        }
        let page = read(next)?;
        digest = chain_extend(chain_extend(digest, &next.to_be_bytes()), &page);
        let number = remaining.min((meta.len() - OVERFLOW_HEADER) / entry);
        let parsed = match parse_page(&page) {
            Ok(p) if p.page_type == PAGE_MANIFEST && p.item_count as usize == number => p,
            _ => {
                bad.push(next);
                chain_ok = false;
                break;
            }
        };
        entries(
            &page[OVERFLOW_HEADER..],
            number,
            &mut protected,
            &mut bad,
            &mut read,
        )?;
        remaining -= number;
        next = parsed.next_page;
    }
    if chain_ok && n > 0 && digest != u64_at(meta, 48) {
        bad.push(head);
    }
    Ok((protected, bad))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn crc64_vector() {
        assert_eq!(extend(0, b"123456789"), 0x6C40_DF5F_0B49_7347);
    }

    /// The sliced fold equals the one-byte-at-a-time definition at every length and split.
    #[test]
    fn crc64_slicing_matches_bytewise() {
        let bytewise = |mut crc: u64, bytes: &[u8]| {
            for &byte in bytes {
                crc = (crc << 8) ^ CRC64[0][((crc >> 56) as u8 ^ byte) as usize];
            }
            crc
        };
        let data: Vec<u8> = (0..300u32)
            .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
            .collect();
        for len in 0..data.len() {
            assert_eq!(
                extend(0, &data[..len]),
                bytewise(0, &data[..len]),
                "len {len}"
            );
            let split = len / 3;
            let chained = extend(
                extend(0x0123_4567_89AB_CDEF, &data[..split]),
                &data[split..len],
            );
            assert_eq!(
                chained,
                bytewise(0x0123_4567_89AB_CDEF, &data[..len]),
                "split {len}"
            );
        }
    }
}
