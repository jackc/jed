//! Format v33 commit identities. See spec/design/validated-cow.md.
use super::*;

const HEADER: usize = 64;
const ENTRY: usize = 12;
const OVERFLOW_HEADER: usize = 32;
const PAGE_MANIFEST: u8 = 8;

const fn crc64_table() -> [u64; 256] {
    let mut table = [0; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = (i as u64) << 56;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & (1 << 63) != 0 {
                (crc << 1) ^ 0x42F0_E1EB_A9EA_3693
            } else {
                crc << 1
            };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}
const CRC64: [u64; 256] = crc64_table();
fn extend(mut crc: u64, bytes: &[u8]) -> u64 {
    for &byte in bytes {
        crc = (crc << 8) ^ CRC64[((crc >> 56) as u8 ^ byte) as usize];
    }
    crc
}
fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(b[at..at + 4].try_into().unwrap())
}
fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_be_bytes(b[at..at + 8].try_into().unwrap())
}

pub(super) fn overflow_needed(ps: usize, entries: usize) -> usize {
    entries
        .saturating_sub((ps - HEADER) / ENTRY)
        .div_ceil((ps - OVERFLOW_HEADER) / ENTRY)
}

/// Return the final meta and overflow pages. The allocator has already reserved `ids`.
pub(super) fn encode(
    mut meta: Vec<u8>,
    written: &[(u32, Vec<u8>)],
    auxiliary: &[(u32, Vec<u8>)],
    ids: &[u32],
) -> (Vec<u8>, Vec<(u32, Vec<u8>)>) {
    let ps = meta.len();
    let mut entries: Vec<_> = written
        .iter()
        .chain(auxiliary.iter())
        .map(|(id, bytes)| (*id, extend(0, bytes)))
        .collect();
    entries.sort_unstable_by_key(|e| e.0);
    let inline = entries.len().min((ps - HEADER) / ENTRY);
    let put_entries = |out: &mut [u8], entries: &[(u32, u64)]| {
        for (chunk, (id, hash)) in out.chunks_exact_mut(ENTRY).zip(entries) {
            chunk[..4].copy_from_slice(&id.to_be_bytes());
            chunk[4..].copy_from_slice(&hash.to_be_bytes());
        }
    };
    put_entries(
        &mut meta[HEADER..HEADER + inline * ENTRY],
        &entries[..inline],
    );
    let mut pages = Vec::with_capacity(ids.len());
    let per = (ps - OVERFLOW_HEADER) / ENTRY;
    let mut digest = 0;
    for (ordinal, &id) in ids.iter().enumerate() {
        let lo = (inline + ordinal * per).min(entries.len());
        let hi = (lo + per).min(entries.len());
        let mut payload = vec![0; 16 + (hi - lo) * ENTRY];
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
        digest = extend(extend(digest, &id.to_be_bytes()), &page);
        pages.push((id, page));
    }
    meta[36..40].copy_from_slice(&ids.first().copied().unwrap_or(0).to_be_bytes());
    meta[40..44].copy_from_slice(&(entries.len() as u32).to_be_bytes());
    meta[44..48].copy_from_slice(&(ids.len() as u32).to_be_bytes());
    meta[48..56].copy_from_slice(&digest.to_be_bytes());
    let crc = meta_crc(&meta);
    meta[32..36].copy_from_slice(&crc.to_be_bytes());
    (meta, pages)
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
    let count = u32_at(meta, 40) as usize;
    let n = u32_at(meta, 44) as usize;
    let mut next = u32_at(meta, 36);
    let bad = || corrupt("invalid commit manifest");
    if pc > physical
        || pc < 3
        || count as u64 + n as u64 > u64::from(pc - 2)
        || meta[60..64].iter().any(|b| *b != 0)
        || (n == 0) != (next == 0)
        || (count == 0 && (n != 0 || u64_at(meta, 48) != 0))
        || overflow_needed(meta.len(), count) > n
    {
        return Err(bad());
    }
    let mut protected = HashSet::new();
    let mut last = 1;
    let mut remaining = count;
    let mut check_entries = |bytes: &[u8], number: usize| -> Result<()> {
        if bytes[number * ENTRY..].iter().any(|b| *b != 0) {
            return Err(bad());
        }
        for e in bytes[..number * ENTRY].chunks_exact(ENTRY) {
            let id = u32_at(e, 0);
            if id <= last || id >= pc {
                return Err(bad());
            }
            last = id;
            let page = read(id)?;
            if extend(0, &page) != u64_at(e, 4) {
                return Err(bad());
            }
            let p = parse_page(&page)?;
            if !(1..=7).contains(&p.page_type) {
                return Err(bad());
            }
            protected.insert(id);
        }
        Ok(())
    };
    let inline = count.min((meta.len() - HEADER) / ENTRY);
    check_entries(&meta[HEADER..], inline)?;
    remaining -= inline;
    // End the closure's borrow so chain pages can be read through the same host.
    drop(check_entries);
    let mut digest = 0;
    for ordinal in 0..n {
        if next < 2
            || next >= pc
            || next == u32_at(meta, 20)
            || next == u32_at(meta, 28)
            || !protected.insert(next)
        {
            return Err(bad());
        }
        let id = next;
        let page = read(id)?;
        digest = extend(extend(digest, &id.to_be_bytes()), &page);
        let p = parse_page(&page)?;
        let number = remaining.min((meta.len() - OVERFLOW_HEADER) / ENTRY);
        if p.page_type != PAGE_MANIFEST
            || p.item_count as usize != number
            || page[1..4] != [0; 3]
            || u64_at(&page, 16) != u64_at(meta, 12)
            || u32_at(&page, 24) as usize != ordinal
            || u32_at(&page, 28) != 0
            || page[OVERFLOW_HEADER + number * ENTRY..]
                .iter()
                .any(|b| *b != 0)
        {
            return Err(bad());
        }
        for e in page[OVERFLOW_HEADER..OVERFLOW_HEADER + number * ENTRY].chunks_exact(ENTRY) {
            let id = u32_at(e, 0);
            if id <= last || id >= pc || !protected.insert(id) {
                return Err(bad());
            }
            last = id;
            let data = read(id)?;
            if extend(0, &data) != u64_at(e, 4) || !(1..=7).contains(&parse_page(&data)?.page_type)
            {
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn crc64_vector() {
        assert_eq!(extend(0, b"123456789"), 0x6C40_DF5F_0B49_7347);
    }
}
