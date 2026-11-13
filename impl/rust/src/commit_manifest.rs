//! Format v33 commit identities. See spec/design/validated-cow.md.
use super::*;

const HEADER: usize = 64;
/// An encrypted meta's inline manifest starts after the catalog-root and free-list tags
/// (spec/fileformat/encrypted.md §6).
const ENCRYPTED_HEADER: usize = 96;
const OVERFLOW_HEADER: usize = 32;
pub(super) const PAGE_MANIFEST: u8 = 8;

// --- PROTOTYPE: the v53 layout ahead of v53 (spec/design/manifest-crc32.md, deferred-interior.md §9).
// The Go core writes it behind three environment switches and no version bump; this core reads the
// same switches so the simulator can hold the two to byte-identical images. At v53 each becomes
// unconditional and the version byte decides.

fn switch_on(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| v == "on")
}

/// `JED_MANIFEST_CRC32=on`: this process writes narrow manifests (manifest-crc32.md §2).
pub(crate) fn manifest_crc32() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| switch_on("JED_MANIFEST_CRC32"))
}

/// `JED_PAGE_TXID_STAMP=on`: pages carry the writing commit's stamp (manifest-crc32.md §2.1).
pub(crate) fn page_txid_stamp() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| switch_on("JED_PAGE_TXID_STAMP"))
}

/// `JED_DEFER_INTERIOR=on`: the deferred-interior layout and commit (deferred-interior.md §9).
pub(crate) fn defer_interior() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| switch_on("JED_DEFER_INTERIOR"))
}

/// The bit of a meta's dirty-count field (offset 40) that marks narrow entries. Self-describing, so a
/// reader needs no switch to read either width.
pub(crate) const NARROW_FLAG: u32 = 1 << 31;

/// A manifest entry's bytes: the page id and a CRC-32 (narrow) or CRC-64 (wide).
pub(crate) fn entry_size(narrow: bool) -> usize {
    if narrow { 8 } else { 12 }
}

/// A meta's manifest entry count and whether its entries are narrow.
pub(crate) fn count_of(meta: &[u8]) -> (usize, bool) {
    let raw = u32_at(meta, 40);
    ((raw & !NARROW_FLAG) as usize, raw & NARROW_FLAG != 0)
}

/// A listed page's manifest identity: the CRC-64 of its bytes (wide); narrow, a plaintext page's own
/// header checksum -- copied, not recomputed -- and an encrypted page's CRC-32 of its ciphertext.
pub(crate) fn page_digest(page: &[u8], encrypted: bool, narrow: bool) -> u64 {
    match (narrow, encrypted) {
        (false, _) => extend(0, page),
        (true, true) => u64::from(crate::format::crc32_extend(0, page)),
        (true, false) => u64::from(u32_at(page, 12)),
    }
}

/// Extend the manifest-overflow chain digest: CRC-64 (wide) or CRC-32 (narrow).
pub(crate) fn chain_extend(crc: u64, bytes: &[u8], narrow: bool) -> u64 {
    if narrow {
        u64::from(crate::format::crc32_extend(crc as u32, bytes))
    } else {
        extend(crc, bytes)
    }
}

fn read_entry(e: &[u8], narrow: bool) -> (u32, u64) {
    let digest = if narrow {
        u64::from(u32_at(e, 4))
    } else {
        u64_at(e, 4)
    };
    (u32_at(e, 0), digest)
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
    if encrypted {
        (ENCRYPTED_HEADER, ps - crate::crypto::RESERVE)
    } else {
        (HEADER, ps)
    }
}

/// The inline (meta page) and per-overflow-page entry capacities for a page of `ps` bytes, as this
/// process writes manifests.
pub(super) fn capacities(ps: usize, cap: Cap) -> (usize, usize) {
    capacities_for(ps, cap, manifest_crc32())
}

/// The capacities for entries of the given width, as a meta declaring that width lays them out.
pub(crate) fn capacities_for(ps: usize, cap: Cap, narrow: bool) -> (usize, usize) {
    let (lo, hi) = inline_area(ps, cap.encrypted_variant());
    let entry = entry_size(narrow);
    ((hi - lo) / entry, (ps - OVERFLOW_HEADER) / entry)
}

pub(super) fn overflow_needed(ps: usize, cap: Cap, entries: usize) -> usize {
    overflow_needed_for(ps, cap, entries, manifest_crc32())
}

fn overflow_needed_for(ps: usize, cap: Cap, entries: usize, narrow: bool) -> usize {
    let (inline, per) = capacities_for(ps, cap, narrow);
    entries.saturating_sub(inline).div_ceil(per)
}

/// The manifest entries -- `(page id, its manifest identity)` -- of written pages, as this process
/// writes them ([`page_digest`]).
pub(super) fn entries_of(pages: &[(u32, Vec<u8>)], encrypted: bool) -> Vec<(u32, u64)> {
    let narrow = manifest_crc32();
    pages
        .iter()
        .map(|(id, bytes)| (*id, page_digest(bytes, encrypted, narrow)))
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
    let narrow = manifest_crc32();
    let entry = entry_size(narrow);
    entries.sort_unstable_by_key(|e| e.0);
    let (header, _) = inline_area(ps, cap.encrypted_variant());
    let inline = entries.len().min(capacities(ps, cap).0);
    let put_entries = |out: &mut [u8], entries: &[(u32, u64)]| {
        for (chunk, (id, hash)) in out.chunks_exact_mut(entry).zip(entries) {
            chunk[..4].copy_from_slice(&id.to_be_bytes());
            if narrow {
                chunk[4..].copy_from_slice(&(*hash as u32).to_be_bytes());
            } else {
                chunk[4..].copy_from_slice(&hash.to_be_bytes());
            }
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
        let page = make_page(
            ps,
            PAGE_MANIFEST,
            (hi - lo) as u32,
            ids.get(ordinal + 1).copied().unwrap_or(0),
            &payload,
        );
        digest = chain_extend(
            chain_extend(digest, &id.to_be_bytes(), narrow),
            &page,
            narrow,
        );
        pages.push((id, page));
    }
    meta[36..40].copy_from_slice(&ids.first().copied().unwrap_or(0).to_be_bytes());
    let count = entries.len() as u32 | if narrow { NARROW_FLAG } else { 0 };
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
    mut read: impl FnMut(u32) -> Result<Vec<u8>>,
) -> Result<Option<HashSet<u32>>> {
    match validate_inner(meta, physical, &mut read) {
        Ok(ids) => Ok(Some(ids)),
        Err(e) if e.state == SqlState::DataCorrupted => Ok(None),
        Err(e) => Err(e),
    }
}
fn validate_inner(
    meta: &[u8],
    physical: u32,
    read: &mut impl FnMut(u32) -> Result<Vec<u8>>,
) -> Result<HashSet<u32>> {
    let pc = u32_at(meta, 24);
    let (count, narrow) = count_of(meta);
    let entry = entry_size(narrow);
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
        || meta[60..64].iter().any(|b| *b != 0)
        || (n == 0) != (next == 0)
        || (count == 0 && (n != 0 || u64_at(meta, 48) != 0))
        || overflow_needed_for(meta.len(), cap, count, narrow) > n
    {
        return Err(bad());
    }
    let typed = |page: &[u8]| -> Result<bool> {
        Ok(encrypted || (1..=7).contains(&parse_page(page)?.page_type))
    };
    let mut protected = HashSet::new();
    let mut last = 1;
    let mut remaining = count;
    let mut check_entries = |bytes: &[u8], number: usize| -> Result<()> {
        if bytes[number * entry..].iter().any(|b| *b != 0) {
            return Err(bad());
        }
        for e in bytes[..number * entry].chunks_exact(entry) {
            let (id, expected) = read_entry(e, narrow);
            if id < first || id <= last || id >= pc {
                return Err(bad());
            }
            last = id;
            let page = read(id)?;
            if page_digest(&page, encrypted, narrow) != expected || !typed(&page)? {
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
        digest = chain_extend(
            chain_extend(digest, &id.to_be_bytes(), narrow),
            &page,
            narrow,
        );
        let p = parse_page(&page)?;
        let number = remaining.min((meta.len() - OVERFLOW_HEADER) / entry);
        if p.page_type != PAGE_MANIFEST
            || p.item_count as usize != number
            || page[1..4] != [0; 3]
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
            let (id, expected) = read_entry(e, narrow);
            if id < first || id <= last || id >= pc || !protected.insert(id) {
                return Err(bad());
            }
            last = id;
            let data = read(id)?;
            if page_digest(&data, encrypted, narrow) != expected || !typed(&data)? {
                return Err(bad());
            }
        }
        remaining -= number;
        next = p.next_page;
    }
    if next != 0
        || remaining != 0
        || digest != u64_at(meta, 48)
        || (count != 0
            && (!protected.contains(&u32_at(meta, 20))
                || (u32_at(meta, 28) != 0 && !protected.contains(&u32_at(meta, 28)))))
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
    let (count, narrow) = count_of(meta);
    let entry = entry_size(narrow);
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
            let (id, expected) = read_entry(e, narrow);
            protected.insert(id);
            if id < first || id >= physical {
                bad.push(id);
                continue;
            }
            if page_digest(&read(id)?, encrypted, narrow) != expected {
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
        digest = chain_extend(
            chain_extend(digest, &next.to_be_bytes(), narrow),
            &page,
            narrow,
        );
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
