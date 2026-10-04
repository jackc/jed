//! Research-only, one-barrier validated COW commits. Not a jed file format.
//!
//! The existing COW allocator must preserve the previous committed snapshot.
//! Unlike a WAL, body pages go directly to their final addresses and are never
//! copied again. Two reserved descriptor slots contain the proposed meta image
//! and a checksum of every body write. Recovery accepts a descriptor only when
//! those writes are present in full. An acknowledged previous commit supplies
//! the durable inherited pages; the checksums validate the new dependencies.
//!
//! A descriptor can mention a page no longer reachable from its own root (for
//! example allocation slack). Before overwriting ANY of the preceding
//! descriptor's body ranges, checkpoint its already-durable meta and sync it.
//! This provides a durable fallback even if that preceding descriptor can no
//! longer validate. This prototype therefore measures actual checkpoint costs,
//! rather than assuming an indefinitely append-only allocator.
//!
//! Assumptions: single writer; externally enforced reader/reuse rules; durable
//! initial image and preallocated descriptor region; successful sync flushes all
//! previous writes. Checksums detect accidents probabilistically, not malicious
//! modification. Opening for further writes must use `resume`, whose extra sync
//! protects against a second crash after a process-only restart.

use super::io::{Device, Write};
use std::io::{self, Error, ErrorKind};

const MAGIC: &[u8; 8] = b"JEDCOW01";
const HEADER: usize = 56;
const ENTRY: usize = 24;
const CHECKSUM_AT: usize = 48;

#[derive(Clone, Debug)]
struct Extent {
    offset: u64,
    len: u64,
    checksum: u64,
}

#[derive(Clone, Debug)]
struct Manifest {
    seq: u64,
    meta: Write,
    body: Vec<Extent>,
}

#[derive(Clone, Debug)]
pub struct Recovered {
    pub seq: u64,
    pub meta: Write,
}

pub struct ValidatedCow {
    manifest_offset: u64,
    slot_bytes: usize,
    seq: u64,
    latest: Option<Manifest>,
    poisoned: bool,
    pub checkpoint_count: u64,
}

fn invalid(message: &str) -> Error {
    Error::new(ErrorKind::InvalidData, message)
}

fn end(offset: u64, len: u64) -> io::Result<u64> {
    offset
        .checked_add(len)
        .ok_or_else(|| invalid("extent overflow"))
}

fn overlaps(a: u64, alen: u64, b: u64, blen: u64) -> bool {
    a < b.saturating_add(blen) && b < a.saturating_add(alen)
}

// Deliberately dependency-free research checksum. Production would specify a
// shared, fast page-identity checksum; do not mistake this for cryptography.
fn checksum(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325u64, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
    })
}

fn put(buf: &mut [u8], at: usize, value: u64) {
    buf[at..at + 8].copy_from_slice(&value.to_le_bytes());
}

fn get(buf: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(buf[at..at + 8].try_into().unwrap())
}

impl Manifest {
    fn encode(&self, slot_bytes: usize) -> io::Result<Vec<u8>> {
        let len = HEADER
            .checked_add(
                self.body
                    .len()
                    .checked_mul(ENTRY)
                    .ok_or_else(|| invalid("manifest too large"))?,
            )
            .and_then(|n| n.checked_add(self.meta.bytes.len()))
            .ok_or_else(|| invalid("manifest too large"))?;
        if len > slot_bytes {
            return Err(invalid("commit exceeds reserved COW manifest capacity"));
        }
        // Write just the live descriptor, not all reserved capacity. Its length
        // and checksum make leftover bytes from older descriptors irrelevant.
        let mut bytes = vec![0; len];
        bytes[..8].copy_from_slice(MAGIC);
        put(&mut bytes, 8, len as u64);
        put(&mut bytes, 16, self.seq);
        put(&mut bytes, 24, self.meta.offset);
        put(&mut bytes, 32, self.meta.bytes.len() as u64);
        put(&mut bytes, 40, self.body.len() as u64);
        for (i, entry) in self.body.iter().enumerate() {
            let at = HEADER + i * ENTRY;
            put(&mut bytes, at, entry.offset);
            put(&mut bytes, at + 8, entry.len);
            put(&mut bytes, at + 16, entry.checksum);
        }
        bytes[HEADER + self.body.len() * ENTRY..].copy_from_slice(&self.meta.bytes);
        let digest = checksum(&bytes);
        put(&mut bytes, CHECKSUM_AT, digest);
        Ok(bytes)
    }

    fn decode(bytes: &mut [u8]) -> Option<Self> {
        if bytes.len() < HEADER || &bytes[..8] != MAGIC {
            return None;
        }
        let len = usize::try_from(get(bytes, 8)).ok()?;
        if len < HEADER || len > bytes.len() {
            return None;
        }
        let expected = get(bytes, CHECKSUM_AT);
        put(bytes, CHECKSUM_AT, 0);
        let actual = checksum(&bytes[..len]);
        put(bytes, CHECKSUM_AT, expected);
        if expected != actual {
            return None;
        }
        let count = usize::try_from(get(bytes, 40)).ok()?;
        let meta_len = usize::try_from(get(bytes, 32)).ok()?;
        let body_end = HEADER.checked_add(count.checked_mul(ENTRY)?)?;
        if body_end.checked_add(meta_len)? != len {
            return None;
        }
        let meta_offset = get(bytes, 24);
        meta_offset.checked_add(meta_len as u64)?;
        let mut body = Vec::with_capacity(count);
        for i in 0..count {
            let at = HEADER + i * ENTRY;
            let offset = get(bytes, at);
            let len = get(bytes, at + 8);
            offset.checked_add(len)?;
            body.push(Extent {
                offset,
                len,
                checksum: get(bytes, at + 16),
            });
        }
        Some(Self {
            seq: get(bytes, 16),
            meta: Write {
                offset: meta_offset,
                bytes: bytes[body_end..len].to_vec(),
            },
            body,
        })
    }

    fn validate<D: Device>(&self, device: &mut D, manifest_offset: u64) -> io::Result<bool> {
        // This experiment reserves descriptors after the entire data region.
        // Reject hostile extent bounds before asking a host to allocate a read.
        if end(self.meta.offset, self.meta.bytes.len() as u64)? > manifest_offset {
            return Ok(false);
        }
        for extent in &self.body {
            if end(extent.offset, extent.len)? > manifest_offset
                || overlaps(
                    extent.offset,
                    extent.len,
                    self.meta.offset,
                    self.meta.bytes.len() as u64,
                )
            {
                return Ok(false);
            }
            let len = usize::try_from(extent.len).map_err(|_| invalid("extent too large"))?;
            let bytes = match device.read(extent.offset, len) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == ErrorKind::UnexpectedEof => return Ok(false),
                Err(error) => return Err(error),
            };
            if bytes.len() != len || checksum(&bytes) != extent.checksum {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

impl ValidatedCow {
    pub fn new(manifest_offset: u64, slot_bytes: usize, base_seq: u64) -> io::Result<Self> {
        if slot_bytes < HEADER || slot_bytes % 512 != 0 || manifest_offset % 512 != 0 {
            return Err(invalid(
                "COW manifest slots must be sector-aligned and nonempty",
            ));
        }
        end(
            manifest_offset,
            (slot_bytes as u64)
                .checked_mul(2)
                .ok_or_else(|| invalid("slot overflow"))?,
        )?;
        Ok(Self {
            manifest_offset,
            slot_bytes,
            seq: base_seq,
            latest: None,
            poisoned: false,
            checkpoint_count: 0,
        })
    }

    pub fn commit<D: Device>(
        &mut self,
        device: &mut D,
        seq: u64,
        body: &[Write],
        meta: &Write,
    ) -> io::Result<()> {
        if self.poisoned {
            return Err(invalid("COW writer poisoned by earlier I/O failure"));
        }
        if self.seq.checked_add(1) != Some(seq) {
            return Err(invalid("COW sequence must increase by one"));
        }
        for write in body.iter().chain(std::iter::once(meta)) {
            end(write.offset, write.bytes.len() as u64)?;
            if end(write.offset, write.bytes.len() as u64)? > self.manifest_offset {
                return Err(invalid("database write exceeds COW data region"));
            }
        }
        // A sequential trace can have overlapping writes. It must first be
        // normalized into final disjoint ranges, otherwise both checksums
        // could not simultaneously validate after a successful commit.
        for (i, a) in body.iter().enumerate() {
            if overlaps(
                a.offset,
                a.bytes.len() as u64,
                meta.offset,
                meta.bytes.len() as u64,
            ) || body[..i].iter().any(|b| {
                overlaps(
                    a.offset,
                    a.bytes.len() as u64,
                    b.offset,
                    b.bytes.len() as u64,
                )
            }) {
                return Err(invalid(
                    "COW body writes must be disjoint from each other and meta",
                ));
            }
        }
        let manifest = Manifest {
            seq,
            meta: meta.clone(),
            body: body
                .iter()
                .map(|write| Extent {
                    offset: write.offset,
                    len: write.bytes.len() as u64,
                    checksum: checksum(&write.bytes),
                })
                .collect(),
        };
        let bytes = manifest.encode(self.slot_bytes)?;
        let needs_checkpoint = self.latest.as_ref().is_some_and(|previous| {
            body.iter().any(|write| {
                previous.body.iter().any(|old| {
                    overlaps(write.offset, write.bytes.len() as u64, old.offset, old.len)
                })
            })
        });
        if needs_checkpoint {
            self.checkpoint(device)?;
        }
        // Until sync succeeds the old snapshot is the fallback. On any error
        // continuing with either presumed snapshot would be unsafe.
        self.poisoned = true;
        for write in body {
            device.write(write.offset, &write.bytes)?;
        }
        device.write(
            self.manifest_offset + (seq & 1) * self.slot_bytes as u64,
            &bytes,
        )?;
        device.sync()?;
        self.seq = seq;
        self.latest = Some(manifest);
        self.poisoned = false;
        Ok(())
    }

    /// Materialize the current meta for ordinary-image readers, or establish
    /// a fallback before descriptor dependencies may be reused. No body copy.
    pub fn checkpoint<D: Device>(&mut self, device: &mut D) -> io::Result<()> {
        if self.poisoned {
            return Err(invalid("COW writer poisoned by earlier I/O failure"));
        }
        if let Some(latest) = &self.latest {
            self.poisoned = true;
            device.write(latest.meta.offset, &latest.meta.bytes)?;
            device.sync()?;
            self.checkpoint_count += 1;
            self.latest = None;
            self.poisoned = false;
        }
        Ok(())
    }

    fn scan<D: Device>(
        device: &mut D,
        manifest_offset: u64,
        slot_bytes: usize,
        base_seq: u64,
    ) -> io::Result<Option<Manifest>> {
        Self::new(manifest_offset, slot_bytes, base_seq)?;
        let mut candidates = Vec::new();
        for slot in 0..2 {
            let mut bytes = device.read(manifest_offset + slot * slot_bytes as u64, slot_bytes)?;
            if let Some(manifest) = Manifest::decode(&mut bytes) {
                if manifest.seq > base_seq && manifest.seq & 1 == slot {
                    candidates.push(manifest);
                }
            }
        }
        candidates.sort_by(|a, b| b.seq.cmp(&a.seq));
        for manifest in candidates {
            if manifest.validate(device, manifest_offset)? {
                return Ok(Some(manifest));
            }
        }
        Ok(None)
    }

    /// `base_meta` must be the highest checksum-valid conventional meta slot.
    /// A read-only recovery performs no sync. Use `resume` before further writes.
    pub fn recover<D: Device>(
        device: &mut D,
        manifest_offset: u64,
        slot_bytes: usize,
        base_seq: u64,
        base_meta: &Write,
    ) -> io::Result<Recovered> {
        Ok(
            match Self::scan(device, manifest_offset, slot_bytes, base_seq)? {
                Some(manifest) => Recovered {
                    seq: manifest.seq,
                    meta: manifest.meta,
                },
                None => Recovered {
                    seq: base_seq,
                    meta: base_meta.clone(),
                },
            },
        )
    }

    /// Sync before allowing writes after recovery. A process-only crash can
    /// leave readable-but-not-durable data in the kernel cache; validation alone
    /// would not protect the fallback against a subsequent machine crash.
    pub fn resume<D: Device>(
        device: &mut D,
        manifest_offset: u64,
        slot_bytes: usize,
        base_seq: u64,
        base_meta: &Write,
    ) -> io::Result<(Self, Recovered)> {
        let latest = Self::scan(device, manifest_offset, slot_bytes, base_seq)?;
        let recovered = latest.as_ref().map_or_else(
            || Recovered {
                seq: base_seq,
                meta: base_meta.clone(),
            },
            |manifest| Recovered {
                seq: manifest.seq,
                meta: manifest.meta.clone(),
            },
        );
        device.sync()?;
        let mut writer = Self::new(manifest_offset, slot_bytes, recovered.seq)?;
        writer.latest = latest;
        Ok((writer, recovered))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REGION: u64 = 32 * 1024;
    const SLOT: usize = 1024;
    const CAPACITY: usize = 36 * 1024;

    #[derive(Clone)]
    struct Model {
        stable: Vec<u8>,
        pending: Vec<Write>,
        fail_sync: bool,
        syncs: usize,
    }

    impl Model {
        fn new() -> Self {
            Self {
                stable: vec![0; CAPACITY],
                pending: vec![],
                fail_sync: false,
                syncs: 0,
            }
        }

        fn apply(bytes: &mut [u8], write: &Write) {
            let at = write.offset as usize;
            bytes[at..at + write.bytes.len()].copy_from_slice(&write.bytes);
        }

        fn crash(&self, writes: &[Write]) -> Self {
            let mut crashed = Self::new();
            crashed.stable.clone_from(&self.stable);
            for write in writes {
                Self::apply(&mut crashed.stable, write);
            }
            crashed
        }
    }

    impl Device for Model {
        fn read(&mut self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
            let mut image = self.stable.clone();
            for write in &self.pending {
                Self::apply(&mut image, write);
            }
            Ok(image[offset as usize..offset as usize + len].to_vec())
        }

        fn write(&mut self, offset: u64, bytes: &[u8]) -> io::Result<()> {
            self.pending.push(Write {
                offset,
                bytes: bytes.to_vec(),
            });
            Ok(())
        }

        fn sync(&mut self) -> io::Result<()> {
            self.syncs += 1;
            if self.fail_sync {
                return Err(Error::new(
                    ErrorKind::Other,
                    "simulated crash before barrier",
                ));
            }
            for write in self.pending.drain(..) {
                Self::apply(&mut self.stable, &write);
            }
            Ok(())
        }
    }

    fn write(offset: u64, byte: u8, len: usize) -> Write {
        Write {
            offset,
            bytes: vec![byte; len],
        }
    }

    fn meta(seq: u8) -> Write {
        write(u64::from(seq & 1) * 512, seq, 512)
    }

    fn recovered(device: &mut Model, base_seq: u64) -> Recovered {
        ValidatedCow::recover(device, REGION, SLOT, base_seq, &meta(base_seq as u8)).unwrap()
    }

    #[test]
    fn one_barrier_and_no_body_rewrite() {
        let mut device = Model::new();
        let mut writer = ValidatedCow::new(REGION, SLOT, 0).unwrap();
        writer
            .commit(&mut device, 1, &[write(4096, 11, 512)], &meta(1))
            .unwrap();
        assert_eq!(device.syncs, 1);
        assert_eq!(recovered(&mut device, 0).seq, 1);
        writer.checkpoint(&mut device).unwrap();
        assert_eq!(device.syncs, 2);
        assert_eq!(&device.stable[4096..4608], &[11; 512]);
        assert_eq!(&device.stable[512..1024], &[1; 512]);
    }

    #[test]
    fn exhaustive_sector_subsets_and_torn_sectors_keep_valid_snapshot() {
        let mut device = Model::new();
        let mut writer = ValidatedCow::new(REGION, SLOT, 0).unwrap();
        writer
            .commit(&mut device, 1, &[write(4096, 11, 512)], &meta(1))
            .unwrap();
        device.fail_sync = true;
        assert!(
            writer
                .commit(&mut device, 2, &[write(8192, 22, 1024)], &meta(2))
                .is_err()
        );
        let mut sectors = Vec::new();
        for pending in &device.pending {
            for (i, chunk) in pending.bytes.chunks(512).enumerate() {
                sectors.push(Write {
                    offset: pending.offset + (i * 512) as u64,
                    bytes: chunk.to_vec(),
                });
            }
        }
        // Any subset represents arbitrary writeback order, not just prefixes.
        // Also tear each persisted sector at every possible byte boundary.
        for mask in 0..1usize << sectors.len() {
            let writes: Vec<_> = sectors
                .iter()
                .enumerate()
                .filter(|(i, _)| mask & (1 << i) != 0)
                .map(|(_, write)| write.clone())
                .collect();
            for tear in 0..=sectors.len() {
                let lengths: Vec<_> = if tear == sectors.len() {
                    vec![512]
                } else {
                    (0..=sectors[tear].bytes.len()).collect()
                };
                for len in lengths {
                    let mut crash_writes = writes.clone();
                    if tear < sectors.len() {
                        crash_writes.retain(|write| write.offset != sectors[tear].offset);
                        let mut torn = sectors[tear].clone();
                        torn.bytes.truncate(len);
                        crash_writes.push(torn);
                    }
                    let mut crashed = device.crash(&crash_writes);
                    let state = recovered(&mut crashed, 0);
                    assert!(state.seq == 1 || state.seq == 2);
                    assert_eq!(state.meta.bytes, meta(state.seq as u8).bytes);
                    let (at, value, size) = if state.seq == 2 {
                        (8192, 22, 1024)
                    } else {
                        (4096, 11, 512)
                    };
                    assert!(
                        crashed.stable[at..at + size]
                            .iter()
                            .all(|byte| *byte == value)
                    );
                }
            }
        }
    }

    #[test]
    fn stale_whole_page_rejected_even_when_individually_well_formed() {
        let mut device = Model::new();
        device.stable[8192..8704].fill(99);
        let mut writer = ValidatedCow::new(REGION, SLOT, 0).unwrap();
        writer
            .commit(&mut device, 1, &[write(4096, 11, 512)], &meta(1))
            .unwrap();
        device.fail_sync = true;
        assert!(
            writer
                .commit(&mut device, 2, &[write(8192, 22, 512)], &meta(2))
                .is_err()
        );
        let manifest = device.pending.last().unwrap().clone();
        let mut crashed = device.crash(&[manifest]);
        assert_eq!(recovered(&mut crashed, 0).seq, 1);
    }

    #[test]
    fn reuse_of_orphan_descriptor_dependency_checkpoints_first() {
        let mut device = Model::new();
        let mut writer = ValidatedCow::new(REGION, SLOT, 0).unwrap();
        // 8192 is recorded allocation slack: it is not reachable by meta(1).
        writer
            .commit(
                &mut device,
                1,
                &[write(4096, 11, 512), write(8192, 0, 512)],
                &meta(1),
            )
            .unwrap();
        writer
            .commit(&mut device, 2, &[write(8192, 22, 512)], &meta(2))
            .unwrap();
        assert_eq!(writer.checkpoint_count, 1);
        assert_eq!(device.syncs, 3);
        assert_eq!(&device.stable[512..1024], &[1; 512]);
        // Destroy the newer descriptor; older descriptor now cannot validate
        // its old slack, but the conventional checkpoint supplies the fallback.
        device.stable[REGION as usize..REGION as usize + SLOT].fill(0);
        assert_eq!(recovered(&mut device, 1).seq, 1);
    }

    #[test]
    fn resume_syncs_process_crash_cache_before_allowing_reuse() {
        let mut device = Model::new();
        let mut writer = ValidatedCow::new(REGION, SLOT, 0).unwrap();
        device.fail_sync = true;
        assert!(
            writer
                .commit(&mut device, 1, &[write(4096, 11, 512)], &meta(1))
                .is_err()
        );
        assert_eq!(recovered(&mut device, 0).seq, 1); // Visible in page cache only.
        assert_eq!(device.stable[4096], 0);
        device.fail_sync = false;
        let (_writer, state) =
            ValidatedCow::resume(&mut device, REGION, SLOT, 0, &meta(0)).unwrap();
        assert_eq!(state.seq, 1);
        assert_eq!(device.stable[4096], 11);
        assert!(device.pending.is_empty());
    }

    #[test]
    fn invalid_capacity_and_overlapping_ranges_fail_before_io() {
        let mut device = Model::new();
        let mut writer = ValidatedCow::new(REGION, SLOT, 0).unwrap();
        assert!(
            writer
                .commit(&mut device, 1, &[write(REGION, 1, 512)], &meta(1))
                .is_err()
        );
        assert!(
            writer
                .commit(
                    &mut device,
                    1,
                    &[write(4096, 1, 512), write(4096, 2, 512)],
                    &meta(1)
                )
                .is_err()
        );
        assert!(
            writer
                .commit(&mut device, 1, &[], &write(0, 1, SLOT))
                .is_err()
        );
        assert!(device.pending.is_empty());
        assert_eq!(device.syncs, 0);
    }

    #[test]
    fn failed_commit_poisoned_and_sequence_never_reused() {
        let mut device = Model::new();
        let mut writer = ValidatedCow::new(REGION, SLOT, 0).unwrap();
        device.fail_sync = true;
        assert!(
            writer
                .commit(&mut device, 1, &[write(4096, 11, 512)], &meta(1))
                .is_err()
        );
        device.fail_sync = false;
        assert!(
            writer
                .commit(&mut device, 2, &[write(8192, 22, 512)], &meta(2))
                .is_err()
        );
        assert_eq!(device.syncs, 1);
    }

    #[test]
    fn torn_checkpoint_keeps_manifest_even_when_meta_parity_repeats() {
        let mut device = Model::new();
        let mut writer = ValidatedCow::new(REGION, SLOT, 0).unwrap();
        writer
            .commit(&mut device, 1, &[write(4096, 11, 512)], &meta(1))
            .unwrap();
        writer.checkpoint(&mut device).unwrap();
        writer
            .commit(&mut device, 2, &[write(8192, 22, 512)], &meta(2))
            .unwrap();
        writer
            .commit(&mut device, 3, &[write(12288, 33, 512)], &meta(3))
            .unwrap();
        // The second checkpoint writes slot 1 again. A torn write there must
        // recover from the intact seq-3 descriptor, without opening the older
        // conventional image (whose pages may legally have been reclaimed).
        device.fail_sync = true;
        assert!(writer.checkpoint(&mut device).is_err());
        let checkpoint = device.pending[0].clone();
        for len in 0..=checkpoint.bytes.len() {
            let mut torn = checkpoint.clone();
            torn.bytes.truncate(len);
            let mut crashed = device.crash(&[torn]);
            assert_eq!(recovered(&mut crashed, 0).seq, 3);
            assert_eq!(crashed.stable[12288], 33);
        }
    }

    #[test]
    fn failed_adaptive_checkpoint_never_starts_dependency_reuse() {
        let mut device = Model::new();
        let mut writer = ValidatedCow::new(REGION, SLOT, 0).unwrap();
        writer
            .commit(
                &mut device,
                1,
                &[write(4096, 11, 512), write(8192, 0, 512)],
                &meta(1),
            )
            .unwrap();
        device.fail_sync = true;
        assert!(
            writer
                .commit(&mut device, 2, &[write(8192, 22, 512)], &meta(2))
                .is_err()
        );
        assert_eq!(device.pending.len(), 1);
        assert_eq!(device.pending[0], meta(1));
        for len in 0..=512 {
            let mut torn = meta(1);
            torn.bytes.truncate(len);
            let mut crashed = device.crash(&[torn]);
            assert_eq!(recovered(&mut crashed, 0).seq, 1);
            assert_eq!(crashed.stable[8192], 0);
        }
    }

    #[test]
    fn sequence_gaps_cannot_overwrite_the_only_fallback_descriptor() {
        let mut device = Model::new();
        let mut writer = ValidatedCow::new(REGION, SLOT, 0).unwrap();
        writer
            .commit(&mut device, 1, &[write(4096, 11, 512)], &meta(1))
            .unwrap();
        assert!(
            writer
                .commit(&mut device, 3, &[write(8192, 33, 512)], &meta(3))
                .is_err()
        );
        assert_eq!(device.syncs, 1);
        assert_eq!(recovered(&mut device, 0).seq, 1);
    }

    #[test]
    fn checksummed_but_out_of_region_extents_fail_before_reading() {
        for (offset, len) in [(REGION - 1, 4096), (u64::MAX - 4096, 4096), (512, 512)] {
            let mut device = Model::new();
            let malformed = Manifest {
                seq: 1,
                meta: meta(1),
                body: vec![Extent {
                    offset,
                    len,
                    checksum: 0,
                }],
            };
            let bytes = malformed.encode(SLOT).unwrap();
            device.write(REGION + SLOT as u64, &bytes).unwrap();
            device.sync().unwrap();
            // Model::read panics on an out-of-range read. These descriptors
            // must instead be rejected structurally, despite valid checksums.
            assert_eq!(recovered(&mut device, 0).seq, 0);
        }
    }
}
