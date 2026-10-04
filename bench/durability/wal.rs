//! Experimental page-redo journal, deliberately outside the database format.
//!
//! A commit appends one complete checksummed frame and performs one sync. The
//! journal can be another file or a reserved extent of the database file. Every
//! frame occupies whole 4096-byte pages so a later append never rewrites a
//! previously committed sector. Recovery stops at the first incomplete frame.
//!
//! Checkpointing requires exclusive access and no readers whose pages are being
//! replaced. Keep the complete journal until checkpoint body and meta syncs have
//! both succeeded. Its next generation then starts at offset zero; the durable
//! database meta txid rejects every old frame without a third reset sync. This
//! is an experiment, not a locking protocol or a production file-format change.

use super::io::{Device, Write};
use std::io::{self, ErrorKind};

pub const FRAME_ALIGNMENT: usize = 4096;
pub const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;
const HEADER_BYTES: usize = 48;
const CHECKSUM_BYTES: usize = 8;
const MAGIC: &[u8; 8] = b"JWAL0001";

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(ErrorKind::InvalidInput, message)
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

const fn crc64_table() -> [u64; 256] {
    let mut table = [0; 256];
    let mut index = 0;
    while index < 256 {
        let mut remainder = (index as u64) << 56;
        let mut bit = 0;
        while bit < 8 {
            remainder = if remainder & (1 << 63) != 0 {
                (remainder << 1) ^ 0x42f0_e1eb_a9ea_3693
            } else {
                remainder << 1
            };
            bit += 1;
        }
        table[index] = remainder;
        index += 1;
    }
    table
}

const CRC64_TABLE: [u64; 256] = crc64_table();

/// CRC-64/ECMA-182. Detects accidental tears; not an authentication mechanism.
pub fn checksum(bytes: &[u8]) -> u64 {
    let mut crc = 0;
    for byte in bytes {
        crc = CRC64_TABLE[((crc >> 56) as u8 ^ byte) as usize] ^ (crc << 8);
    }
    crc
}

pub fn encoded_len(writes: &[Write]) -> io::Result<usize> {
    u32::try_from(writes.len()).map_err(|_| invalid("too many journal records"))?;
    let mut length = HEADER_BYTES + CHECKSUM_BYTES;
    for write in writes {
        write
            .offset
            .checked_add(write.bytes.len() as u64)
            .ok_or_else(|| invalid("journal write offset overflows"))?;
        length = length
            .checked_add(16)
            .and_then(|n| n.checked_add(write.bytes.len()))
            .ok_or_else(|| invalid("journal frame length overflows"))?;
    }
    length = length
        .checked_add(FRAME_ALIGNMENT - 1)
        .ok_or_else(|| invalid("journal frame length overflows"))?
        / FRAME_ALIGNMENT
        * FRAME_ALIGNMENT;
    if length > MAX_FRAME_BYTES {
        return Err(invalid("journal frame exceeds 64 MiB experiment limit"));
    }
    Ok(length)
}

/// Inputs must pass `encoded_len` and have a nonzero txid.
pub fn encode_frame(txid: u64, writes: &[Write]) -> Vec<u8> {
    assert!(txid != 0, "journal txid must be nonzero");
    let length = encoded_len(writes).expect("invalid journal frame");
    let mut frame = vec![0; length];
    frame[..8].copy_from_slice(MAGIC);
    frame[8..16].copy_from_slice(&(length as u64).to_le_bytes());
    frame[16..24].copy_from_slice(&txid.to_le_bytes());
    frame[24..32].copy_from_slice(&(txid - 1).to_le_bytes());
    frame[40..44].copy_from_slice(&(writes.len() as u32).to_le_bytes());
    let mut cursor = HEADER_BYTES;
    for write in writes {
        frame[cursor..cursor + 8].copy_from_slice(&write.offset.to_le_bytes());
        frame[cursor + 8..cursor + 16].copy_from_slice(&(write.bytes.len() as u64).to_le_bytes());
        cursor += 16;
        frame[cursor..cursor + write.bytes.len()].copy_from_slice(&write.bytes);
        cursor += write.bytes.len();
    }
    frame[32..40].copy_from_slice(&(cursor as u64).to_le_bytes());
    let footer = length - CHECKSUM_BYTES;
    let crc = checksum(&frame[..footer]);
    frame[footer..].copy_from_slice(&crc.to_le_bytes());
    frame
}

fn frame_length(header: &[u8], remaining: usize) -> Option<usize> {
    if header.len() < HEADER_BYTES || &header[..8] != MAGIC {
        return None;
    }
    let length = usize::try_from(u64_at(header, 8)).ok()?;
    (length >= FRAME_ALIGNMENT
        && length <= MAX_FRAME_BYTES
        && length <= remaining
        && length % FRAME_ALIGNMENT == 0)
        .then_some(length)
}

fn decode_frame(frame: &[u8], previous_txid: u64) -> Option<(u64, Vec<Write>)> {
    let length = frame_length(frame, frame.len())?;
    if length != frame.len() || frame[44..48] != [0; 4] {
        return None;
    }
    let txid = previous_txid.checked_add(1)?;
    if u64_at(frame, 16) != txid || u64_at(frame, 24) != previous_txid {
        return None;
    }
    let footer = length - CHECKSUM_BYTES;
    if checksum(&frame[..footer]) != u64_at(frame, footer) {
        return None;
    }
    let payload_end = usize::try_from(u64_at(frame, 32)).ok()?;
    if !(HEADER_BYTES..=footer).contains(&payload_end) {
        return None;
    }
    let count = u32::from_le_bytes(frame[40..44].try_into().unwrap()) as usize;
    if count > (payload_end - HEADER_BYTES) / 16 {
        return None;
    }
    // No allocations based on unchecked counts, offsets, or lengths.
    let mut writes = Vec::with_capacity(count);
    let mut cursor = HEADER_BYTES;
    for _ in 0..count {
        if cursor.checked_add(16)? > payload_end {
            return None;
        }
        let offset = u64_at(frame, cursor);
        let byte_count = usize::try_from(u64_at(frame, cursor + 8)).ok()?;
        cursor += 16;
        let end = cursor.checked_add(byte_count)?;
        offset.checked_add(byte_count as u64)?;
        if end > payload_end {
            return None;
        }
        writes.push(Write {
            offset,
            bytes: frame[cursor..end].to_vec(),
        });
        cursor = end;
    }
    if cursor != payload_end || frame[payload_end..footer].iter().any(|byte| *byte != 0) {
        return None;
    }
    Some((txid, writes))
}

#[derive(Debug, Default)]
pub struct Recovery {
    pub frames: Vec<(u64, Vec<Write>)>,
    pub valid_bytes: usize,
}

/// `checkpoint_txid` comes from a valid, durable database meta page.
pub fn recover(bytes: &[u8], checkpoint_txid: u64) -> Recovery {
    let mut recovery = Recovery::default();
    let mut previous_txid = checkpoint_txid;
    while let Some(length) = frame_length(
        &bytes[recovery.valid_bytes..],
        bytes.len() - recovery.valid_bytes,
    ) {
        let Some(frame) = decode_frame(
            &bytes[recovery.valid_bytes..recovery.valid_bytes + length],
            previous_txid,
        ) else {
            break;
        };
        previous_txid = frame.0;
        recovery.frames.push(frame);
        recovery.valid_bytes += length;
    }
    recovery
}

/// Read one bounded frame at a time; never allocate the entire reserved region.
pub fn read_recovery(
    device: &mut dyn Device,
    base: u64,
    capacity: usize,
    checkpoint_txid: u64,
) -> io::Result<Recovery> {
    base.checked_add(capacity as u64)
        .ok_or_else(|| invalid("journal region overflows"))?;
    let mut recovery = Recovery::default();
    let mut previous_txid = checkpoint_txid;
    while capacity - recovery.valid_bytes >= HEADER_BYTES {
        let position = base + recovery.valid_bytes as u64;
        let header = match device.read(position, HEADER_BYTES) {
            Ok(header) => header,
            Err(error) if error.kind() == ErrorKind::UnexpectedEof => break,
            Err(error) => return Err(error),
        };
        let Some(length) = frame_length(&header, capacity - recovery.valid_bytes) else {
            break;
        };
        let frame = match device.read(position, length) {
            Ok(frame) => frame,
            Err(error) if error.kind() == ErrorKind::UnexpectedEof => break,
            Err(error) => return Err(error),
        };
        let Some(decoded) = decode_frame(&frame, previous_txid) else {
            break;
        };
        previous_txid = decoded.0;
        recovery.frames.push(decoded);
        recovery.valid_bytes += length;
    }
    Ok(recovery)
}

pub struct Journal {
    pub base: u64,
    pub capacity: usize,
    pub next_offset: usize,
    pub next_txid: u64,
    poisoned: bool,
}

impl Journal {
    /// The region must already be durably allocated. Creation is outside timing.
    pub fn new(base: u64, capacity: usize, checkpoint_txid: u64) -> io::Result<Self> {
        if base % FRAME_ALIGNMENT as u64 != 0 || capacity % FRAME_ALIGNMENT != 0 {
            return Err(invalid("journal region must be page aligned"));
        }
        base.checked_add(capacity as u64)
            .ok_or_else(|| invalid("journal region overflows"))?;
        let next_txid = checkpoint_txid
            .checked_add(1)
            .ok_or_else(|| invalid("journal txid exhausted"))?;
        Ok(Self {
            base,
            capacity,
            next_offset: 0,
            next_txid,
            poisoned: false,
        })
    }

    /// Exactly one sync per successful commit. On I/O failure reopen/recover.
    pub fn append(&mut self, device: &mut dyn Device, writes: &[Write]) -> io::Result<u64> {
        if self.poisoned {
            return Err(invalid(
                "journal has an indeterminate I/O failure; reopen required",
            ));
        }
        let length = encoded_len(writes)?;
        if self
            .next_offset
            .checked_add(length)
            .filter(|end| *end <= self.capacity)
            .is_none()
        {
            return Err(io::Error::new(
                ErrorKind::StorageFull,
                "journal requires checkpoint",
            ));
        }
        let following_txid = self
            .next_txid
            .checked_add(1)
            .ok_or_else(|| invalid("journal txid exhausted"))?;
        let frame = encode_frame(self.next_txid, writes);
        self.poisoned = true;
        device.write(self.base + self.next_offset as u64, &frame)?;
        device.sync()?;
        self.poisoned = false;
        let txid = self.next_txid;
        self.next_txid = following_txid;
        self.next_offset += length;
        Ok(txid)
    }

    /// Call only after the database checkpoint meta has synced successfully.
    pub fn reset(&mut self, checkpoint_txid: u64) -> io::Result<()> {
        if self.poisoned || checkpoint_txid.checked_add(1) != Some(self.next_txid) {
            return Err(invalid(
                "journal reset requires its latest successful checkpoint",
            ));
        }
        self.next_offset = 0;
        Ok(())
    }
}

/// Keep the WAL until both barriers succeed. Caller supplies coalesced body
/// writes and the final meta write; intermediate meta pages must be excluded.
/// The final meta must target the slot opposite the last checkpoint meta, even
/// when its txid parity would ordinarily target that checkpoint's slot. A torn
/// checkpoint meta must leave the WAL's starting checkpoint txid discoverable.
pub fn checkpoint(device: &mut dyn Device, body: &[Write], meta: &Write) -> io::Result<()> {
    for write in body {
        device.write(write.offset, &write.bytes)?;
    }
    device.sync()?;
    device.write(meta.offset, &meta.bytes)?;
    device.sync()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone)]
    struct MemoryDevice {
        bytes: Vec<u8>,
        durable: Vec<u8>,
        syncs: usize,
        fail_sync: bool,
    }

    impl MemoryDevice {
        fn new(length: usize) -> Self {
            Self {
                bytes: vec![0; length],
                durable: vec![0; length],
                syncs: 0,
                fail_sync: false,
            }
        }
    }

    impl Device for MemoryDevice {
        fn read(&mut self, offset: u64, length: usize) -> io::Result<Vec<u8>> {
            let start = offset as usize;
            self.bytes
                .get(start..start + length)
                .map(|bytes| bytes.to_vec())
                .ok_or_else(|| io::Error::from(ErrorKind::UnexpectedEof))
        }

        fn write(&mut self, offset: u64, bytes: &[u8]) -> io::Result<()> {
            let start = offset as usize;
            self.bytes[start..start + bytes.len()].copy_from_slice(bytes);
            Ok(())
        }

        fn sync(&mut self) -> io::Result<()> {
            if self.fail_sync {
                return Err(io::Error::other("injected sync failure"));
            }
            self.syncs += 1;
            self.durable.clone_from(&self.bytes);
            Ok(())
        }
    }

    fn page(offset: u64, byte: u8) -> Write {
        Write {
            offset,
            bytes: vec![byte; FRAME_ALIGNMENT],
        }
    }

    #[test]
    fn standard_crc64_vector() {
        assert_eq!(checksum(b"123456789"), 0x6c40_df5f_0b49_7347);
    }

    #[test]
    fn roundtrip_full_pages_and_stop_at_first_hole() {
        let one = encode_frame(1, &[page(8192, 7), page(4096, 1)]);
        let two = encode_frame(2, &[page(8192, 8), page(0, 2)]);
        let three = encode_frame(3, &[page(12288, 9), page(4096, 3)]);
        let bytes = [one.clone(), two.clone(), three].concat();
        let result = recover(&bytes, 0);
        assert_eq!(result.valid_bytes, bytes.len());
        assert_eq!(result.frames.len(), 3);
        assert_eq!(result.frames[1].1[0].bytes, vec![8; FRAME_ALIGNMENT]);
        for length in 0..two.len() {
            let truncated = [one.as_slice(), &two[..length]].concat();
            let result = recover(&truncated, 0);
            assert_eq!(result.frames.len(), 1);
            assert_eq!(result.valid_bytes, one.len());
        }
        let mut corrupted = bytes;
        corrupted[one.len() + HEADER_BYTES + 16 + 512] ^= 1;
        assert_eq!(recover(&corrupted, 0).frames.len(), 1);
    }

    #[test]
    fn header_length_count_and_offset_corruption_are_bounded() {
        let frame = encode_frame(1, &[page(8192, 7)]);
        for (offset, value) in [
            (8, u64::MAX),
            (32, u64::MAX),
            (40, u64::MAX),
            (48, u64::MAX),
            (56, u64::MAX),
        ] {
            let mut corrupted = frame.clone();
            corrupted[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
            let end = corrupted.len() - CHECKSUM_BYTES;
            let crc = checksum(&corrupted[..end]);
            corrupted[end..].copy_from_slice(&crc.to_le_bytes());
            assert!(recover(&corrupted, 0).frames.is_empty());
        }
    }

    #[test]
    fn durable_prefix_survives_every_subset_of_next_frames_sectors() {
        let first = encode_frame(1, &[page(8192, 0x31)]);
        let next = encode_frame(2, &[page(12288, 0xb7)]);
        assert_eq!(next.len(), 8192);
        let mut bytes = vec![0; first.len() + next.len()];
        bytes[..first.len()].copy_from_slice(&first);
        // Exhaust all 65,536 subsets of the sixteen 512-byte sectors. Accepting
        // an unsynced commit is legal only if the entire frame validates.
        for mask in 0u32..(1 << 16) {
            bytes[first.len()..].fill(0);
            for sector in 0..16 {
                if mask & (1 << sector) != 0 {
                    let start = sector * 512;
                    bytes[first.len() + start..first.len() + start + 512]
                        .copy_from_slice(&next[start..start + 512]);
                }
            }
            let result = recover(&bytes, 0);
            assert!(!result.frames.is_empty());
            assert_eq!(result.frames[0].1[0].bytes, vec![0x31; FRAME_ALIGNMENT]);
            if result.frames.len() == 2 {
                assert_eq!(&bytes[first.len()..], next.as_slice());
                assert_eq!(result.frames[1].1[0].bytes, vec![0xb7; FRAME_ALIGNMENT]);
            }
        }
    }

    #[test]
    fn checkpoint_reset_reuses_bounded_region_without_a_reset_sync() {
        let mut db = MemoryDevice::new(4 * FRAME_ALIGNMENT);
        let mut log = MemoryDevice::new(8 * FRAME_ALIGNMENT);
        let mut journal = Journal::new(0, log.bytes.len(), 0).unwrap();
        let mut checkpoint_txid = 0;
        for generation in 0..24 {
            for _ in 0..2 {
                let txid = journal.next_txid;
                let writes = [page(8192, txid as u8), page((txid & 1) * 4096, txid as u8)];
                assert_eq!(journal.append(&mut log, &writes).unwrap(), txid);
            }
            let recovery = read_recovery(&mut log, 0, journal.capacity, checkpoint_txid).unwrap();
            assert_eq!(recovery.frames.len(), 2);
            let (txid, writes) = recovery.frames.last().unwrap();
            checkpoint(&mut db, &writes[..1], &writes[1]).unwrap();
            checkpoint_txid = *txid;
            journal.reset(checkpoint_txid).unwrap();
            assert_eq!(journal.next_offset, 0);
            assert!(recover(&log.durable, checkpoint_txid).frames.is_empty());
            assert_eq!(db.durable[8192], checkpoint_txid as u8);
            assert_eq!(log.syncs, (generation + 1) * 2);
            assert_eq!(db.syncs, (generation + 1) * 2);
        }
    }

    #[test]
    fn shorter_new_generation_cannot_replay_a_stale_tail() {
        let mut log = MemoryDevice::new(16 * FRAME_ALIGNMENT);
        let mut journal = Journal::new(0, log.bytes.len(), 0).unwrap();
        for _ in 0..3 {
            journal
                .append(&mut log, &[page(8192, 1), page(12288, 2)])
                .unwrap();
        }
        journal.reset(3).unwrap(); // Models the completed, separately-tested checkpoint.
        journal.append(&mut log, &[page(8192, 4)]).unwrap();
        let result = recover(&log.durable, 3);
        assert_eq!(result.frames.len(), 1);
        assert_eq!(result.frames[0].0, 4);
        assert_eq!(result.valid_bytes, 8192);
    }

    #[test]
    fn every_sector_subset_during_reuse_rejects_the_old_generation() {
        let old = [
            encode_frame(1, &[page(8192, 0x11)]),
            encode_frame(2, &[page(12288, 0x22)]),
            encode_frame(3, &[page(8192, 0x33)]),
        ]
        .concat();
        let new = encode_frame(4, &[page(8192, 0x44)]);
        let mut bytes = old.clone();
        for mask in 0u32..(1 << 16) {
            bytes[..new.len()].copy_from_slice(&old[..new.len()]);
            for sector in 0..16 {
                if mask & (1 << sector) != 0 {
                    let start = sector * 512;
                    bytes[start..start + 512].copy_from_slice(&new[start..start + 512]);
                }
            }
            let result = recover(&bytes, 3);
            assert!(result.frames.len() <= 1);
            if let Some((txid, writes)) = result.frames.first() {
                assert_eq!(*txid, 4);
                assert_eq!(writes[0].bytes, vec![0x44; FRAME_ALIGNMENT]);
                assert_eq!(&bytes[..new.len()], new.as_slice());
            }
        }
    }

    #[test]
    fn embedded_region_has_identical_recovery_and_does_not_touch_body() {
        let mut device = MemoryDevice::new(12 * FRAME_ALIGNMENT);
        device.bytes[..4 * FRAME_ALIGNMENT].fill(0xa5);
        device.sync().unwrap();
        let mut journal =
            Journal::new((4 * FRAME_ALIGNMENT) as u64, 8 * FRAME_ALIGNMENT, 11).unwrap();
        journal.append(&mut device, &[page(8192, 12)]).unwrap();
        assert!(
            device.durable[..4 * FRAME_ALIGNMENT]
                .iter()
                .all(|byte| *byte == 0xa5)
        );
        let recovered = read_recovery(&mut device, journal.base, journal.capacity, 11).unwrap();
        assert_eq!(recovered.frames.len(), 1);
        assert_eq!(recovered.frames[0].0, 12);
    }

    #[test]
    fn indeterminate_sync_failure_poisoned_until_reopen() {
        let mut log = MemoryDevice::new(8 * FRAME_ALIGNMENT);
        let mut journal = Journal::new(0, log.bytes.len(), 0).unwrap();
        log.fail_sync = true;
        assert!(journal.append(&mut log, &[page(8192, 1)]).is_err());
        log.fail_sync = false;
        assert!(journal.append(&mut log, &[page(8192, 2)]).is_err());
        assert!(journal.reset(0).is_err());
        assert_eq!(log.syncs, 0);
    }

    fn toy_meta(offset: u64, txid: u64) -> Write {
        let mut write = page(offset, txid as u8);
        write.bytes[..8].copy_from_slice(&txid.to_le_bytes());
        let end = write.bytes.len() - CHECKSUM_BYTES;
        let crc = checksum(&write.bytes[..end]);
        write.bytes[end..].copy_from_slice(&crc.to_le_bytes());
        write
    }

    fn toy_checkpoint_txid(db: &[u8]) -> u64 {
        db[..2 * FRAME_ALIGNMENT]
            .chunks_exact(FRAME_ALIGNMENT)
            .filter(|page| {
                checksum(&page[..FRAME_ALIGNMENT - CHECKSUM_BYTES])
                    == u64_at(page, FRAME_ALIGNMENT - CHECKSUM_BYTES)
            })
            .map(|page| u64_at(page, 0))
            .max()
            .unwrap()
    }

    fn assert_checkpoint_recovers(mut db: Vec<u8>, journal: &[u8]) {
        let checkpoint_txid = toy_checkpoint_txid(&db);
        let recovery = recover(journal, checkpoint_txid);
        for (_, writes) in recovery.frames {
            for write in writes {
                let start = write.offset as usize;
                db[start..start + write.bytes.len()].copy_from_slice(&write.bytes);
            }
        }
        assert_eq!(toy_checkpoint_txid(&db), 3);
        assert_eq!(&db[8192..12288], vec![0x33; FRAME_ALIGNMENT]);
        assert_eq!(&db[12288..16384], vec![0x44; FRAME_ALIGNMENT]);
    }

    #[test]
    fn recovery_repairs_reused_baseline_pages_during_partial_checkpoint() {
        let mut baseline = MemoryDevice::new(4 * FRAME_ALIGNMENT);
        for write in [
            toy_meta(0, 0),
            toy_meta(4096, 1),
            page(8192, 0x11),
            page(12288, 0x12),
        ] {
            baseline.write(write.offset, &write.bytes).unwrap();
        }
        baseline.sync().unwrap();
        let journal = [
            encode_frame(2, &[page(8192, 0x22), toy_meta(0, 2)]),
            encode_frame(3, &[page(8192, 0x33), page(12288, 0x44), toy_meta(4096, 3)]),
        ]
        .concat();
        let bodies = [page(8192, 0x33), page(12288, 0x44)];
        // Final txid3 has the same parity as baseline txid1. Preserve slot1
        // until checkpoint succeeds by putting final meta in slot0 instead.
        let meta = toy_meta(0, 3);
        // Every subset of the sectors in either body write, with preceding
        // writes complete and later writes absent, plus deliberately reordered
        // sector persistence across both writes.
        for active_write in 0..2 {
            for mask in 0u32..256 {
                let mut crash = baseline.durable.clone();
                for write in &bodies[..active_write] {
                    let start = write.offset as usize;
                    crash[start..start + write.bytes.len()].copy_from_slice(&write.bytes);
                }
                let write = &bodies[active_write];
                for sector in 0..8 {
                    if mask & (1 << sector) != 0 {
                        let source = sector * 512;
                        let target = write.offset as usize + source;
                        crash[target..target + 512]
                            .copy_from_slice(&write.bytes[source..source + 512]);
                    }
                }
                assert_checkpoint_recovers(crash, &journal);
            }
        }
        for mask in [0xaaaa_u32, 0x5555, 0x8001, 0x7ffe, 0xffff] {
            let mut crash = baseline.durable.clone();
            for sector in 0..16 {
                if mask & (1 << sector) != 0 {
                    let write = &bodies[sector / 8];
                    let source = (sector % 8) * 512;
                    let target = write.offset as usize + source;
                    crash[target..target + 512].copy_from_slice(&write.bytes[source..source + 512]);
                }
            }
            assert_checkpoint_recovers(crash, &journal);
        }
        let mut body_durable = baseline.durable.clone();
        for write in &bodies {
            let start = write.offset as usize;
            body_durable[start..start + write.bytes.len()].copy_from_slice(&write.bytes);
        }
        for mask in 0u32..256 {
            let mut crash = body_durable.clone();
            for sector in 0..8 {
                if mask & (1 << sector) != 0 {
                    let start = sector * 512;
                    crash[start..start + 512].copy_from_slice(&meta.bytes[start..start + 512]);
                }
            }
            assert_checkpoint_recovers(crash, &journal);
        }
    }
}
