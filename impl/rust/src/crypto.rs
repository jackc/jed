//! Encryption at rest (spec/design/encryption.md; the bytes in spec/fileformat/encrypted.md): the
//! XAES-256-GCM page codec, the subkeys, and the key block that wraps a file's data key.
//!
//! The codec sits in the core above the block seam (hosts.md §6): format.rs builds each page's
//! plaintext exactly as the plaintext layout would (with payload capacity `C = page_size − 56`) and
//! hands it here to be sealed; a reader that follows a bound pointer hands the stored bytes and the
//! pointer's tag back to be checked and opened. The hosts never see plaintext.
//!
//! Every primitive comes from RustCrypto (CLAUDE.md §14 clause 3). Only XAES-256-GCM's key derivation
//! (https://c2sp.org/XAES-256-GCM — two AES blocks and a GF(2^128) doubling) is written here, over the
//! library AES, and checked against the C2SP vectors in spec/fileformat/encryption_vectors.toml.

use std::sync::{Arc, Mutex};

use aes::Aes256;
use aes::cipher::{BlockCipherEncrypt, KeyInit};
use aes_gcm::Aes256Gcm;
use aes_gcm::aead::AeadInOut;
use hkdf::Hkdf;
use sha2::Sha256;

use crate::error::{EngineError, Result, SqlState};
use crate::seam::RandomSource;

/// The encrypted variant's suite byte (meta offset 6, key block offset 6): XAES-256-GCM pages,
/// HKDF-SHA-256 subkeys, an AES-256-GCM key wrap (encrypted.md §1).
pub(crate) const SUITE_XAES: u8 = 1;
/// Reserved bytes at the end of every sealed page: the 24-byte nonce, then the 16-byte tag.
pub(crate) const RESERVE: usize = NONCE_LEN + TAG_LEN;
/// The bytes a bound pointer adds to a page index (encrypted.md §2).
pub(crate) const TAG_LEN: usize = 16;
const NONCE_LEN: usize = 24;
/// Pages 0–1 are the meta slots and 2–3 the key block, so body pages start at 4.
pub(crate) const FIRST_BODY_PAGE: u32 = 4;
/// The smallest page size an encrypted file may have (encrypted.md §1).
pub(crate) const MIN_PAGE_SIZE: usize = 1024;

/// A page's 16-byte AEAD tag — what a bound pointer carries. Zero in a pointer to no page, and in
/// every pointer of a plaintext file (which stores none).
pub(crate) type Tag = [u8; TAG_LEN];
pub(crate) const NO_TAG: Tag = [0; TAG_LEN];

/// The credential that unlocks an encrypted database: a passphrase (the KEK is Argon2id of it) or a
/// 32-byte key the host keeps elsewhere (the KEK is HKDF of it). It is an argument of
/// `create`/`open`/`attach`, never a setting, and its `Debug` form never shows it
/// (spec/design/encryption.md §5).
#[derive(Clone)]
pub enum Credential {
    Passphrase(String),
    Key([u8; 32]),
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Credential::Passphrase(_) => f.write_str("Credential::Passphrase(<redacted>)"),
            Credential::Key(_) => f.write_str("Credential::Key(<redacted>)"),
        }
    }
}

/// Argon2id parameters for a passphrase credential at `create` (encrypted.md §5). The default is
/// RFC 9106's second recommendation; any value must stay within the format's bounds, which a reader
/// checks before running the KDF.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Argon2Params {
    pub memory_kib: u32,
    pub iterations: u32,
    pub parallelism: u32,
}

impl Default for Argon2Params {
    fn default() -> Self {
        Argon2Params {
            memory_kib: 65536,
            iterations: 3,
            parallelism: 4,
        }
    }
}

impl Argon2Params {
    fn valid(&self) -> bool {
        (1..=16).contains(&self.parallelism)
            && (1..=16).contains(&self.iterations)
            && (8 * self.parallelism..=1 << 20).contains(&self.memory_kib)
    }
}

fn corrupt(msg: &str) -> EngineError {
    EngineError::new(SqlState::DataCorrupted, msg)
}

/// An XAES-256-GCM key: the AES key schedule and its CMAC subkey `K1`, both computed once (C2SP
/// XAES-256-GCM: `L = AES_K(0)`, `K1 = L·x` in GF(2^128)).
struct XaesKey {
    aes: Aes256,
    k1: [u8; 16],
}

impl XaesKey {
    fn new(key: &[u8; 32]) -> XaesKey {
        let aes = Aes256::new(key.into());
        let mut l = [0u8; 16].into();
        aes.encrypt_block(&mut l);
        let l: [u8; 16] = l.into();
        let mut k1 = [0u8; 16];
        for i in 0..16 {
            k1[i] = (l[i] << 1) | if i < 15 { l[i + 1] >> 7 } else { 0 };
        }
        // Branch-free: XOR 0x87 into the last byte when the shifted-out bit was set.
        k1[15] ^= 0x87 & (l[0] >> 7).wrapping_neg();
        XaesKey { aes, k1 }
    }

    /// The per-nonce AES-256-GCM key `Kx = AES_K(M1 ⊕ K1) ‖ AES_K(M2 ⊕ K1)` with `Mi = 00 0i 58 00 ‖
    /// N[0:12]`.
    fn derive(&self, nonce: &[u8; NONCE_LEN]) -> Aes256Gcm {
        let mut kx = [0u8; 32];
        for (i, half) in kx.chunks_exact_mut(16).enumerate() {
            let mut m = [0u8; 16];
            m[1] = i as u8 + 1;
            m[2] = b'X';
            m[4..].copy_from_slice(&nonce[..12]);
            for (b, k) in m.iter_mut().zip(self.k1) {
                *b ^= k;
            }
            let mut block = m.into();
            self.aes.encrypt_block(&mut block);
            half.copy_from_slice(&<[u8; 16]>::from(block));
        }
        Aes256Gcm::new(&kx.into())
    }

    fn seal(&self, nonce: &[u8; NONCE_LEN], buf: &mut [u8], aad: &[u8]) -> Tag {
        let n: [u8; 12] = nonce[12..].try_into().unwrap();
        let tag = self
            .derive(nonce)
            .encrypt_inout_detached(&n.into(), aad, buf.into())
            .expect("AES-GCM seals any page-sized buffer");
        tag.into()
    }

    fn open(&self, nonce: &[u8; NONCE_LEN], buf: &mut [u8], aad: &[u8], tag: &Tag) -> bool {
        let n: [u8; 12] = nonce[12..].try_into().unwrap();
        self.derive(nonce)
            .decrypt_inout_detached(&n.into(), aad, buf.into(), tag.into())
            .is_ok()
    }
}

fn hkdf(ikm: &[u8], salt: &[u8], info: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    Hkdf::<Sha256>::new(Some(salt), ikm)
        .expand(info.as_bytes(), &mut out)
        .expect("32 bytes is a valid HKDF-SHA-256 length");
    out
}

/// The 8-byte AD prefix of every seal: `"JEDB" ‖ suite ‖ role ‖ u32 position` (encrypted.md §3).
fn ad_prefix(role: u8, position: u32) -> [u8; 10] {
    let mut ad = [0u8; 10];
    ad[..4].copy_from_slice(b"JEDB");
    ad[4] = SUITE_XAES;
    ad[5] = role;
    ad[6..].copy_from_slice(&position.to_be_bytes());
    ad
}

const ROLE_BODY: u8 = 0x01;
const ROLE_META: u8 = 0x02;

/// One open encrypted database's keys and nonce source: everything a commit needs to seal and a
/// reader needs to open. Shared (`Arc`) by the database's paging context.
pub(crate) struct PageCodec {
    page: XaesKey,
    meta: XaesKey,
    /// The database's key-entropy stream (encrypted.md §8): one per database.
    rng: Arc<dyn RandomSource>,
    /// The data key, kept so a credential change can rewrap it (encrypted.md §5).
    dek: [u8; 32],
    /// The selected key block copy, kept for compaction, which writes it to both pages of the new
    /// file, and replaced by a credential change (encrypted.md §5).
    key_block: Mutex<Vec<u8>>,
}

impl PageCodec {
    fn new(dek: &[u8; 32], rng: Arc<dyn RandomSource>, key_block: Vec<u8>) -> PageCodec {
        PageCodec {
            page: XaesKey::new(&hkdf(dek, &[], "jed/v1/page")),
            meta: XaesKey::new(&hkdf(dek, &[], "jed/v1/meta")),
            rng,
            dek: *dek,
            key_block: Mutex::new(key_block),
        }
    }

    fn nonce(&self) -> Result<[u8; NONCE_LEN]> {
        let mut n = [0u8; NONCE_LEN];
        self.rng.fill(&mut n)?;
        Ok(n)
    }

    /// The database's key-entropy stream: a rotation draws the new data key from it.
    pub(crate) fn rng(&self) -> Arc<dyn RandomSource> {
        Arc::clone(&self.rng)
    }

    /// The selected key-block copy (128 bytes; the rest of its page is zero).
    pub(crate) fn key_block(&self) -> Vec<u8> {
        self.key_block.lock().expect("key block lock").clone()
    }

    /// The selected copy as a whole key-block page of `page_size` bytes.
    pub(crate) fn key_block_page(&self, page_size: usize) -> Vec<u8> {
        let mut page = vec![0u8; page_size];
        page[..KEY_BLOCK_LEN].copy_from_slice(&self.key_block());
        page
    }

    /// The key block for a credential change (encrypted.md §5): the same DEK wrapped for `cred`
    /// under a fresh KDF salt and wrap nonce, drawn in that order from the key-entropy stream, at
    /// the next generation. Runs the KDF, so a caller derives it before taking the writer gate.
    pub(crate) fn rewrap(&self, cred: &Credential, params: Argon2Params) -> Result<Vec<u8>> {
        let current = self.key_block();
        let generation = u64::from_be_bytes(current[8..16].try_into().unwrap());
        let generation = generation
            .checked_add(1)
            .ok_or_else(|| corrupt("key block generation exhausted"))?;
        wrap_key_block(&self.dek, generation, cred, params, self.rng.as_ref())
    }

    /// Whether `block` is the next generation of the selected copy: a credential change derived
    /// before the writer gate fails `55006` when another change won the race (encrypted.md §5).
    pub(crate) fn follows(&self, block: &[u8]) -> bool {
        let generation = |b: &[u8]| u64::from_be_bytes(b[8..16].try_into().unwrap());
        generation(block) == generation(&self.key_block()).wrapping_add(1)
    }

    /// Adopt `block` as the selected copy once a credential change has written both pages.
    pub(crate) fn set_key_block(&self, block: Vec<u8>) {
        *self.key_block.lock().expect("key block lock") = block;
    }

    /// Seal body page `index` in place (encrypted.md §4): `page` is the full page, its first
    /// `page_size − 40` bytes the plaintext (header with a zero CRC field, payload, zero fill). Draws
    /// one nonce and returns the tag a pointer to the page binds.
    pub(crate) fn seal_body(&self, index: u32, page: &mut [u8]) -> Result<Tag> {
        let nonce = self.nonce()?;
        let split = page.len() - RESERVE;
        let (body, reserve) = page.split_at_mut(split);
        let tag = self.page.seal(&nonce, body, &ad_prefix(ROLE_BODY, index));
        reserve[..NONCE_LEN].copy_from_slice(&nonce);
        reserve[NONCE_LEN..].copy_from_slice(&tag);
        Ok(tag)
    }

    /// Open the stored body page `index` that a pointer binding `expected` names: check the stored
    /// tag against the pointer's, then decrypt. Returns the plaintext page (`page_size − 40` bytes)
    /// with its CRC field stamped as the plaintext layout's per-page CRC, so the ordinary page parse
    /// accepts it. A mismatch or an authentication failure is `XX001`; so is a plaintext whose CRC
    /// field is not zero.
    pub(crate) fn open_body(&self, index: u32, stored: &[u8], expected: &Tag) -> Result<Vec<u8>> {
        if stored.len() < MIN_PAGE_SIZE {
            return Err(corrupt("short encrypted page"));
        }
        let split = stored.len() - RESERVE;
        let nonce: [u8; NONCE_LEN] = stored[split..split + NONCE_LEN].try_into().unwrap();
        let tag: Tag = stored[split + NONCE_LEN..].try_into().unwrap();
        if &tag != expected {
            return Err(corrupt("page does not match the pointer that names it"));
        }
        let mut body = stored[..split].to_vec();
        if !self
            .page
            .open(&nonce, &mut body, &ad_prefix(ROLE_BODY, index), &tag)
        {
            return Err(corrupt("page fails authentication"));
        }
        if body[12..16] != [0; 4] {
            return Err(corrupt("encrypted page carries a CRC"));
        }
        let crc = crate::format::page_crc(&body);
        body[12..16].copy_from_slice(&crc.to_be_bytes());
        Ok(body)
    }

    /// Seal meta slot `slot` in place (encrypted.md §6): every field and the inline manifest are
    /// written; `manifest` is the commit's manifest-overflow chain in chain order. Writes the nonce
    /// and tag into the reserve, then the meta CRC.
    pub(crate) fn seal_meta(
        &self,
        slot: u32,
        meta: &mut [u8],
        manifest: &[(u32, Vec<u8>)],
    ) -> Result<()> {
        let nonce = self.nonce()?;
        let ps = meta.len();
        meta[32..36].fill(0);
        meta[ps - RESERVE..].fill(0);
        let aad = meta_aad(slot, meta, manifest);
        let tag = self.meta.seal(&nonce, &mut [], &aad);
        meta[ps - RESERVE..ps - TAG_LEN].copy_from_slice(&nonce);
        meta[ps - TAG_LEN..].copy_from_slice(&tag);
        let crc = crate::format::meta_crc(meta);
        meta[32..36].copy_from_slice(&crc.to_be_bytes());
        Ok(())
    }

    /// Check the stored meta slot `slot` against its tag; `manifest` is its manifest-overflow chain
    /// as stored. A failure is `XX001` (never a fallback — encrypted.md §6).
    pub(crate) fn verify_meta(
        &self,
        slot: u32,
        stored: &[u8],
        manifest: &[(u32, Vec<u8>)],
    ) -> Result<()> {
        let ps = stored.len();
        let nonce: [u8; NONCE_LEN] = stored[ps - RESERVE..ps - TAG_LEN].try_into().unwrap();
        let tag: Tag = stored[ps - TAG_LEN..].try_into().unwrap();
        let mut zeroed = stored.to_vec();
        zeroed[32..36].fill(0);
        zeroed[ps - RESERVE..].fill(0);
        let aad = meta_aad(slot, &zeroed, manifest);
        if self.meta.open(&nonce, &mut [], &aad, &tag) {
            Ok(())
        } else {
            Err(corrupt("meta page fails authentication"))
        }
    }
}

fn meta_aad(slot: u32, zeroed: &[u8], manifest: &[(u32, Vec<u8>)]) -> Vec<u8> {
    let mut aad = Vec::with_capacity(10 + zeroed.len() * (1 + manifest.len()));
    aad.extend_from_slice(&ad_prefix(ROLE_META, slot));
    aad.extend_from_slice(zeroed);
    for (id, page) in manifest {
        aad.extend_from_slice(&id.to_be_bytes());
        aad.extend_from_slice(page);
    }
    aad
}

// --- the key block (pages 2 and 3, encrypted.md §5) --------------------------------------------

const KEY_MAGIC: &[u8; 4] = b"JEDK";
const KEY_BLOCK_VERSION: u16 = 1;
/// The key block's encoded length; the rest of its page is zero.
pub(crate) const KEY_BLOCK_LEN: usize = 128;
const KDF_ARGON2ID: u8 = 1;
const KDF_HOST_KEY: u8 = 2;

/// The KEK for one key-block copy (encrypted.md §3). The copy's parameters were bounds-checked by
/// [`parse_key_block`] before this runs.
fn kek(block: &[u8], cred: &Credential) -> Result<Option<[u8; 32]>> {
    let salt = &block[32..64];
    match (block[7], cred) {
        (KDF_HOST_KEY, Credential::Key(key)) => Ok(Some(hkdf(key, salt, "jed/v1/kek"))),
        (KDF_ARGON2ID, Credential::Passphrase(pass)) => {
            let u = |at: usize| u32::from_be_bytes(block[at..at + 4].try_into().unwrap());
            let params = argon2::Params::new(u(16), u(20), u(24), Some(32))
                .map_err(|_| corrupt("invalid key-block KDF parameters"))?;
            let mut out = [0u8; 32];
            argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params)
                .hash_password_into(pass.as_bytes(), salt, &mut out)
                .map_err(|_| corrupt("invalid key-block KDF parameters"))?;
            Ok(Some(out))
        }
        // The wrong kind of credential cannot unwrap this copy.
        _ => Ok(None),
    }
}

fn wrong_credential() -> EngineError {
    EngineError::new(
        SqlState::InvalidPassword,
        "the credential does not unlock this database",
    )
}

/// Build a new key block for `cred`: draws the DEK, the KDF salt and the wrap nonce from `rng`, in
/// that order (encrypted.md §8). Returns the 128-byte block and the DEK.
fn new_key_block(
    cred: &Credential,
    params: Argon2Params,
    rng: &dyn RandomSource,
) -> Result<(Vec<u8>, [u8; 32])> {
    let mut dek = [0u8; 32];
    rng.fill(&mut dek)?;
    let block = wrap_key_block(&dek, 1, cred, params, rng)?;
    Ok((block, dek))
}

/// The 128-byte key block wrapping `dek` for `cred` at `generation`: draws the KDF salt, then the
/// wrap nonce, from `rng` (encrypted.md §5, §8).
fn wrap_key_block(
    dek: &[u8; 32],
    generation: u64,
    cred: &Credential,
    params: Argon2Params,
    rng: &dyn RandomSource,
) -> Result<Vec<u8>> {
    let mut block = vec![0u8; KEY_BLOCK_LEN];
    block[..4].copy_from_slice(KEY_MAGIC);
    block[4..6].copy_from_slice(&KEY_BLOCK_VERSION.to_be_bytes());
    block[6] = SUITE_XAES;
    block[8..16].copy_from_slice(&generation.to_be_bytes());
    match cred {
        Credential::Passphrase(_) => {
            if !params.valid() {
                return Err(EngineError::new(
                    SqlState::InvalidParameterValue,
                    "Argon2id parameters are out of range",
                ));
            }
            block[7] = KDF_ARGON2ID;
            block[16..20].copy_from_slice(&params.memory_kib.to_be_bytes());
            block[20..24].copy_from_slice(&params.iterations.to_be_bytes());
            block[24..28].copy_from_slice(&params.parallelism.to_be_bytes());
        }
        Credential::Key(_) => block[7] = KDF_HOST_KEY,
    }
    rng.fill(&mut block[32..76])?;
    let kek = kek(&block, cred)?.expect("the block's KDF matches its credential");
    let nonce: [u8; 12] = block[64..76].try_into().unwrap();
    let mut wrapped = *dek;
    let tag = Aes256Gcm::new(&kek.into())
        .encrypt_inout_detached(&nonce.into(), &block[..76], (&mut wrapped[..]).into())
        .expect("AES-GCM wraps 32 bytes");
    block[76..108].copy_from_slice(&wrapped);
    block[108..124].copy_from_slice(&tag);
    let crc = crate::format::crc32_ieee(&block[..124]);
    block[124..128].copy_from_slice(&crc.to_be_bytes());
    Ok(block)
}

/// One key-block copy, validated (encrypted.md §5): `None` when it is not a valid copy.
fn parse_key_block(page: &[u8]) -> Option<(&[u8], u64)> {
    if page.len() < KEY_BLOCK_LEN {
        return None;
    }
    let b = &page[..KEY_BLOCK_LEN];
    let u = |at: usize| u32::from_be_bytes(b[at..at + 4].try_into().unwrap());
    let generation = u64::from_be_bytes(b[8..16].try_into().unwrap());
    let params = Argon2Params {
        memory_kib: u(16),
        iterations: u(20),
        parallelism: u(24),
    };
    let kdf_ok = match b[7] {
        KDF_ARGON2ID => params.valid(),
        KDF_HOST_KEY => b[16..28] == [0; 12],
        _ => false,
    };
    let valid = &b[..4] == KEY_MAGIC
        && b[4..6] == KEY_BLOCK_VERSION.to_be_bytes()
        && b[6] == SUITE_XAES
        && generation >= 1
        && kdf_ok
        && u(28) == 0
        && u(124) == crate::format::crc32_ieee(&b[..124])
        && page[KEY_BLOCK_LEN..].iter().all(|&x| x == 0);
    valid.then_some((b, generation))
}

/// Select the key-block copy to unwrap (encrypted.md §5): the valid copy with the greater
/// generation; two valid copies of one generation must be identical; no valid copy is `XX001`.
fn select_key_block<'a>(page2: &'a [u8], page3: &'a [u8]) -> Result<&'a [u8]> {
    match (parse_key_block(page2), parse_key_block(page3)) {
        (None, None) => Err(corrupt("no valid key block")),
        (Some((b, _)), None) | (None, Some((b, _))) => Ok(b),
        (Some((a, ga)), Some((b, gb))) => {
            if ga == gb && a != b {
                Err(corrupt("key block copies disagree"))
            } else if gb > ga {
                Ok(b)
            } else {
                Ok(a)
            }
        }
    }
}

/// The selected key-block copy of `page2` and `page3` as a whole page — what a backup writes to
/// both pages (encrypted.md §13).
pub(crate) fn selected_key_page(page2: &[u8], page3: &[u8]) -> Result<Vec<u8>> {
    let block = select_key_block(page2, page3)?;
    let mut page = vec![0u8; page2.len()];
    page[..KEY_BLOCK_LEN].copy_from_slice(block);
    Ok(page)
}

/// The key-block page a credential change writes first (encrypted.md §5): the copy that is not the
/// selected one — the older or invalid copy, page 3 when both are equal — so the only valid copy is
/// never the first overwritten.
pub(crate) fn unselected_key_page(page2: &[u8], page3: &[u8]) -> u32 {
    match (parse_key_block(page2), parse_key_block(page3)) {
        (Some((_, ga)), Some((_, gb))) if gb > ga => 2,
        (None, Some(_)) => 2,
        _ => 3,
    }
}

/// Unwrap the selected copy's DEK with `cred`: `28P01` when it does not unwrap.
fn unwrap_dek(block: &[u8], cred: &Credential) -> Result<[u8; 32]> {
    let kek = kek(block, cred)?.ok_or_else(wrong_credential)?;
    let nonce: [u8; 12] = block[64..76].try_into().unwrap();
    let tag: Tag = block[108..124].try_into().unwrap();
    let mut dek: [u8; 32] = block[76..108].try_into().unwrap();
    Aes256Gcm::new(&kek.into())
        .decrypt_inout_detached(
            &nonce.into(),
            &block[..76],
            (&mut dek[..]).into(),
            &tag.into(),
        )
        .map_err(|_| wrong_credential())?;
    Ok(dek)
}

/// A new encrypted database's codec and key-block copy (`create`).
pub(crate) fn create(
    cred: &Credential,
    params: Argon2Params,
    rng: Arc<dyn RandomSource>,
) -> Result<Arc<PageCodec>> {
    let (block, dek) = new_key_block(cred, params, rng.as_ref())?;
    Ok(Arc::new(PageCodec::new(&dek, rng, block)))
}

/// A throwaway codec for a rebuild's scratch domain (spec/design/encryption.md §5): a random key
/// from the OS CSPRNG, never persisted. The scratch pages it seals are discarded, so they never
/// touch a database's key-entropy stream.
pub(crate) fn scratch_codec() -> Result<Arc<PageCodec>> {
    let rng = crate::seam::os_random_source();
    let mut key = [0u8; 32];
    rng.fill(&mut key)?;
    create(&Credential::Key(key), Argon2Params::default(), rng)
}

/// Unlock an encrypted file from its key-block pages: select the copy, unwrap its DEK with `cred`.
pub(crate) fn unlock(
    page2: &[u8],
    page3: &[u8],
    cred: &Credential,
    rng: Arc<dyn RandomSource>,
) -> Result<Arc<PageCodec>> {
    let block = select_key_block(page2, page3)?;
    let dek = unwrap_dek(block, cred)?;
    Ok(Arc::new(PageCodec::new(&dek, rng, block.to_vec())))
}

/// The suite a file's meta slots declare (offset 6, with the reserve byte at 7 — encrypted.md §6),
/// read from the CRC-valid slots before any key is available: `0` plaintext, [`SUITE_XAES`]
/// encrypted. Slots that disagree, or an unknown suite, are `XX001`.
pub(crate) fn file_suite(slot0: &[u8], slot1: &[u8]) -> Result<u8> {
    let mut suite = None;
    for m in [slot0, slot1] {
        if m.len() < 64
            || u32::from_be_bytes(m[32..36].try_into().unwrap()) != crate::format::meta_crc(m)
        {
            continue;
        }
        let s = match (m[6], m[7] as usize) {
            (0, 0) => 0,
            (SUITE_XAES, RESERVE) => SUITE_XAES,
            _ => return Err(corrupt("unknown encryption suite")),
        };
        if suite.is_some_and(|x| x != s) {
            return Err(corrupt("meta slots disagree on encryption"));
        }
        suite = Some(s);
    }
    // Neither slot is valid: slot selection reports the corruption.
    Ok(suite.unwrap_or(0))
}

// --- scratch encryption (spec/design/encryption.md §6) ------------------------------------------

/// The plaintext bytes of one sealed scratch block; each is stored as the block's ciphertext, its
/// 24-byte nonce and its 16-byte tag (spec/fileformat/encrypted.md §11).
pub(crate) const SCRATCH_BLOCK: usize = 4096;
const SCRATCH_STORED: usize = SCRATCH_BLOCK + RESERVE;
const ROLE_SCRATCH: u8 = 0x03;

/// A spill host whose every scratch file is sealed (encryption.md §6): each file gets a random key
/// and id from the handle's scratch stream, never persisted, and stores its bytes in sealed 4 KiB
/// blocks whose associated data binds the file id, the block ordinal and the block's write counter,
/// so a replayed older version of a rewritten block fails. Results and cost do not change.
pub(crate) fn encrypting_spill_host(
    inner: Arc<dyn crate::spillhost::SpillHost>,
    rng: Arc<dyn RandomSource>,
) -> Arc<dyn crate::spillhost::SpillHost> {
    Arc::new(EncryptingSpillHost { inner, rng })
}

struct EncryptingSpillHost {
    inner: Arc<dyn crate::spillhost::SpillHost>,
    rng: Arc<dyn RandomSource>,
}

impl crate::spillhost::SpillHost for EncryptingSpillHost {
    fn create(&self, kind: &str) -> Result<Box<dyn crate::spillhost::SpillFile>> {
        let mut key = [0u8; 32];
        self.rng.fill(&mut key)?;
        let mut id = [0u8; 16];
        self.rng.fill(&mut id)?;
        Ok(Box::new(SealedSpillFile {
            inner: self.inner.create(kind)?,
            key: XaesKey::new(&key),
            id,
            rng: Arc::clone(&self.rng),
            state: Mutex::new(SealedState {
                size: 0,
                counters: Vec::new(),
                live: Vec::new(),
                cached: None,
            }),
        }))
    }

    fn encrypts(&self) -> bool {
        true
    }
}

struct SealedSpillFile {
    inner: Box<dyn crate::spillhost::SpillFile>,
    key: XaesKey,
    id: [u8; 16],
    rng: Arc<dyn RandomSource>,
    state: Mutex<SealedState>,
}

struct SealedState {
    /// The logical (plaintext) size.
    size: u64,
    /// Each block's write counter, never reset (truncation included), so no sealed version of a
    /// block can authenticate again once a newer one was sealed.
    counters: Vec<u64>,
    /// Whether each block holds written bytes; a block never written, or cut by a truncation,
    /// reads as zeros.
    live: Vec<bool>,
    /// The block last written, kept in plaintext until another block is written, so a run of small
    /// writes into one block seals it once.
    cached: Option<(usize, Vec<u8>)>,
}

fn io_err(e: EngineError) -> std::io::Error {
    std::io::Error::other(e.message.clone())
}

impl SealedSpillFile {
    /// The associated data of block `ordinal` at write counter `counter`.
    fn ad(&self, ordinal: usize, counter: u64) -> Vec<u8> {
        let mut ad = Vec::with_capacity(42);
        ad.extend_from_slice(&ad_prefix(ROLE_SCRATCH, 0)[..6]);
        ad.extend_from_slice(&self.id);
        ad.extend_from_slice(&(ordinal as u64).to_be_bytes());
        ad.extend_from_slice(&counter.to_be_bytes());
        ad
    }

    /// Block `ordinal`'s plaintext: the cached block, zeros for a block never written, else the
    /// stored block opened against its counter.
    fn load(&self, state: &SealedState, ordinal: usize) -> std::io::Result<Vec<u8>> {
        if let Some((at, block)) = &state.cached
            && *at == ordinal
        {
            return Ok(block.clone());
        }
        if !state.live.get(ordinal).copied().unwrap_or(false) {
            return Ok(vec![0u8; SCRATCH_BLOCK]);
        }
        let counter = state.counters[ordinal];
        let mut stored = vec![0u8; SCRATCH_STORED];
        crate::spillhost::read_exact_at(
            self.inner.as_ref(),
            (ordinal * SCRATCH_STORED) as u64,
            &mut stored,
        )?;
        let nonce: [u8; NONCE_LEN] = stored[SCRATCH_BLOCK..SCRATCH_BLOCK + NONCE_LEN]
            .try_into()
            .unwrap();
        let tag: Tag = stored[SCRATCH_BLOCK + NONCE_LEN..].try_into().unwrap();
        stored.truncate(SCRATCH_BLOCK);
        if !self
            .key
            .open(&nonce, &mut stored, &self.ad(ordinal, counter), &tag)
        {
            return Err(std::io::Error::other(
                "a scratch block fails authentication",
            ));
        }
        Ok(stored)
    }

    /// Seal and store the cached block, if any, at its next write counter.
    fn flush_cached(&self, state: &mut SealedState) -> std::io::Result<()> {
        let Some((ordinal, mut block)) = state.cached.take() else {
            return Ok(());
        };
        if state.counters.len() <= ordinal {
            state.counters.resize(ordinal + 1, 0);
            state.live.resize(ordinal + 1, false);
        }
        let counter = state.counters[ordinal] + 1;
        let mut nonce = [0u8; NONCE_LEN];
        self.rng.fill(&mut nonce).map_err(io_err)?;
        let tag = self
            .key
            .seal(&nonce, &mut block, &self.ad(ordinal, counter));
        block.extend_from_slice(&nonce);
        block.extend_from_slice(&tag);
        self.inner
            .write_at((ordinal * SCRATCH_STORED) as u64, &block)?;
        state.counters[ordinal] = counter;
        state.live[ordinal] = true;
        Ok(())
    }
}

impl crate::spillhost::SpillFile for SealedSpillFile {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if offset >= state.size {
            return Ok(0);
        }
        let n = buf.len().min((state.size - offset) as usize);
        let mut done = 0;
        while done < n {
            let at = offset as usize + done;
            let (ordinal, within) = (at / SCRATCH_BLOCK, at % SCRATCH_BLOCK);
            let take = (SCRATCH_BLOCK - within).min(n - done);
            let block = self.load(&state, ordinal)?;
            buf[done..done + take].copy_from_slice(&block[within..within + take]);
            done += take;
        }
        Ok(n)
    }

    fn write_at(&self, offset: u64, bytes: &[u8]) -> std::io::Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut done = 0;
        while done < bytes.len() {
            let at = offset as usize + done;
            let (ordinal, within) = (at / SCRATCH_BLOCK, at % SCRATCH_BLOCK);
            let take = (SCRATCH_BLOCK - within).min(bytes.len() - done);
            if state.cached.as_ref().is_none_or(|(c, _)| *c != ordinal) {
                let block = if take == SCRATCH_BLOCK {
                    vec![0u8; SCRATCH_BLOCK]
                } else {
                    self.load(&state, ordinal)?
                };
                self.flush_cached(&mut state)?;
                state.cached = Some((ordinal, block));
            }
            let (_, block) = state.cached.as_mut().expect("cached above");
            block[within..within + take].copy_from_slice(&bytes[done..done + take]);
            done += take;
        }
        state.size = state.size.max(offset + bytes.len() as u64);
        Ok(())
    }

    fn size(&self) -> std::io::Result<u64> {
        Ok(self.state.lock().unwrap_or_else(|e| e.into_inner()).size)
    }

    fn truncate(&self, n: u64) -> std::io::Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let blocks = (n as usize).div_ceil(SCRATCH_BLOCK);
        if n < state.size {
            // Zero the kept part of a cut block, so growing again reads zeros past `n`.
            let within = n as usize % SCRATCH_BLOCK;
            if within != 0 {
                let ordinal = n as usize / SCRATCH_BLOCK;
                let mut block = self.load(&state, ordinal)?;
                block[within..].fill(0);
                if state.cached.as_ref().is_none_or(|(c, _)| *c != ordinal) {
                    self.flush_cached(&mut state)?;
                }
                state.cached = Some((ordinal, block));
            }
            if state.cached.as_ref().is_some_and(|(c, _)| *c >= blocks) {
                state.cached = None;
            }
            for live in state.live.iter_mut().skip(blocks) {
                *live = false;
            }
            self.inner.truncate((blocks * SCRATCH_STORED) as u64)?;
        }
        state.size = n;
        Ok(())
    }

    fn close(&self) {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).cached = None;
        self.inner.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn xaes_c2sp_vectors() {
        let nonce: [u8; 24] = *b"ABCDEFGHIJKLMNOPQRSTUVWX";
        for (key, aad, want) in [
            (
                [1u8; 32],
                &b""[..],
                "ce546ef63c9cc60765923609b33a9a1974e96e52daf2fcf7075e2271",
            ),
            (
                [3u8; 32],
                &b"c2sp.org/XAES-256-GCM"[..],
                "986ec1832593df5443a179437fd083bf3fdb41abd740a21f71eb769d",
            ),
        ] {
            let k = XaesKey::new(&key);
            let mut buf = b"XAES-256-GCM".to_vec();
            let tag = k.seal(&nonce, &mut buf, aad);
            buf.extend_from_slice(&tag);
            assert_eq!(buf, hex(want));
            let mut body = buf[..12].to_vec();
            assert!(k.open(&nonce, &mut body, aad, &tag));
            assert_eq!(body, b"XAES-256-GCM");
            assert!(!k.open(&nonce, &mut buf[..12].to_vec(), b"x", &tag));
        }
    }

    /// The shared vectors (spec/fileformat/encryption_vectors.toml), authored by the Ruby reference.
    #[test]
    fn shared_encryption_vectors() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../spec/fileformat/encryption_vectors.toml"
        );
        let doc: toml::Table = std::fs::read_to_string(path).unwrap().parse().unwrap();
        let field = |t: &toml::Table, k: &str| hex(t[k].as_str().unwrap());
        for v in doc["xaes"].as_array().unwrap() {
            let v = v.as_table().unwrap();
            let key: [u8; 32] = field(v, "key_hex").try_into().unwrap();
            let nonce: [u8; 24] = field(v, "nonce_hex").try_into().unwrap();
            let mut buf = field(v, "plaintext_hex");
            let tag = XaesKey::new(&key).seal(&nonce, &mut buf, &field(v, "aad_hex"));
            buf.extend_from_slice(&tag);
            assert_eq!(buf, field(v, "sealed_hex"), "{}", v["name"]);
        }
        let sub = doc["subkeys"].as_table().unwrap();
        let dek = field(sub, "dek_hex");
        assert_eq!(
            hkdf(&dek, &[], "jed/v1/page").to_vec(),
            field(sub, "page_key_hex")
        );
        assert_eq!(
            hkdf(&dek, &[], "jed/v1/meta").to_vec(),
            field(sub, "meta_key_hex")
        );
        let kek = doc["host_kek"].as_table().unwrap();
        let block = {
            let mut b = vec![0u8; KEY_BLOCK_LEN];
            b[7] = KDF_HOST_KEY;
            b[32..64].copy_from_slice(&field(kek, "kdf_salt_hex"));
            b
        };
        let key: [u8; 32] = field(kek, "key_hex").try_into().unwrap();
        assert_eq!(
            super::kek(&block, &Credential::Key(key))
                .unwrap()
                .unwrap()
                .to_vec(),
            field(kek, "kek_hex")
        );
        let kb = doc["key_block"].as_table().unwrap();
        let rng = crate::seam::seeded_random_source(kb["seed"].as_integer().unwrap() as u64);
        let (block, dek) =
            new_key_block(&Credential::Key(key), Argon2Params::default(), rng.as_ref()).unwrap();
        assert_eq!(block, field(kb, "block_hex"));
        assert_eq!(dek.to_vec(), field(kb, "dek_hex"));
        let rw = doc["rewrap"].as_table().unwrap();
        let mut page = vec![0u8; 1024];
        page[..KEY_BLOCK_LEN].copy_from_slice(&block);
        let rng = crate::seam::seeded_random_source(rw["seed"].as_integer().unwrap() as u64);
        let codec = unlock(&page, &page, &Credential::Key(key), rng).unwrap();
        let new_key: [u8; 32] = field(rw, "key_hex").try_into().unwrap();
        let rewrapped = codec
            .rewrap(&Credential::Key(new_key), Argon2Params::default())
            .unwrap();
        assert_eq!(rewrapped, field(rw, "block_hex"));
        let a = doc["argon2id"].as_table().unwrap();
        let params = argon2::Params::new(
            a["memory_kib"].as_integer().unwrap() as u32,
            a["iterations"].as_integer().unwrap() as u32,
            a["parallelism"].as_integer().unwrap() as u32,
            Some(32),
        )
        .unwrap();
        let mut out = [0u8; 32];
        argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params)
            .hash_password_into(
                a["password"].as_str().unwrap().as_bytes(),
                a["salt"].as_str().unwrap().as_bytes(),
                &mut out,
            )
            .unwrap();
        assert_eq!(out.to_vec(), field(a, "key_hex"));
    }

    #[test]
    fn argon2id_reference_vector() {
        let params = argon2::Params::new(65536, 2, 1, Some(32)).unwrap();
        let mut out = [0u8; 32];
        argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params)
            .hash_password_into(b"password", b"somesalt", &mut out)
            .unwrap();
        assert_eq!(
            out.to_vec(),
            hex("09316115d5cf24ed5a15a31a3ba326e5cf32edc24702987c02b6566f61913cf7")
        );
    }

    #[test]
    fn key_block_round_trip_and_wrong_credential() {
        let rng = crate::seam::seeded_random_source(7);
        let cred = Credential::Key([9; 32]);
        let codec = create(&cred, Argon2Params::default(), rng.clone()).unwrap();
        let page = codec.key_block_page(1024);
        assert!(unlock(&page, &page, &cred, rng.clone()).is_ok());
        let err = unlock(&page, &page, &Credential::Key([8; 32]), rng.clone())
            .err()
            .unwrap();
        assert_eq!(err.state, SqlState::InvalidPassword);
        let err = unlock(
            &page,
            &page,
            &Credential::Passphrase("x".into()),
            rng.clone(),
        )
        .err()
        .unwrap();
        assert_eq!(err.state, SqlState::InvalidPassword);
        let mut torn = page.clone();
        torn[40] ^= 1;
        assert!(unlock(&page, &torn, &cred, rng.clone()).is_ok());
        let err = unlock(&torn, &torn, &cred, rng).err().unwrap();
        assert_eq!(err.state, SqlState::DataCorrupted);
    }

    #[test]
    fn credential_change_writes_the_unselected_copy_first() {
        let rng = crate::seam::seeded_random_source(3);
        let cred = Credential::Key([9; 32]);
        let codec = create(&cred, Argon2Params::default(), rng.clone()).unwrap();
        let old = codec.key_block_page(1024);
        let mut new = vec![0u8; 1024];
        let next = codec
            .rewrap(&Credential::Key([5; 32]), Argon2Params::default())
            .unwrap();
        new[..KEY_BLOCK_LEN].copy_from_slice(&next);
        assert!(codec.follows(&next));
        codec.set_key_block(next.clone());
        assert!(
            !codec.follows(&next),
            "a second change from the same generation lost the race"
        );
        assert_eq!(unselected_key_page(&old, &old), 3);
        assert_eq!(unselected_key_page(&old, &new), 2);
        assert_eq!(unselected_key_page(&new, &old), 3);
        let mut torn = old.clone();
        torn[50] ^= 1;
        assert_eq!(unselected_key_page(&torn, &old), 2);
        assert_eq!(unselected_key_page(&old, &torn), 3);
        // The newer copy is selected: the new credential unlocks, the old one no longer does.
        assert!(unlock(&old, &new, &Credential::Key([5; 32]), rng.clone()).is_ok());
        let err = unlock(&old, &new, &cred, rng).err().unwrap();
        assert_eq!(err.state, SqlState::InvalidPassword);
    }

    /// An in-memory spill host whose files the test can read and overwrite underneath the seal.
    #[derive(Default)]
    struct MemSpill(Mutex<Vec<Arc<Mutex<Vec<u8>>>>>);
    struct MemFile(Arc<Mutex<Vec<u8>>>);

    impl crate::spillhost::SpillHost for Arc<MemSpill> {
        fn create(&self, _kind: &str) -> Result<Box<dyn crate::spillhost::SpillFile>> {
            let bytes = Arc::new(Mutex::new(Vec::new()));
            self.0.lock().unwrap().push(Arc::clone(&bytes));
            Ok(Box::new(MemFile(bytes)))
        }
    }

    impl crate::spillhost::SpillFile for MemFile {
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
            let b = self.0.lock().unwrap();
            let start = (offset as usize).min(b.len());
            let n = buf.len().min(b.len() - start);
            buf[..n].copy_from_slice(&b[start..start + n]);
            Ok(n)
        }
        fn write_at(&self, offset: u64, bytes: &[u8]) -> std::io::Result<()> {
            let mut b = self.0.lock().unwrap();
            let end = offset as usize + bytes.len();
            if b.len() < end {
                b.resize(end, 0);
            }
            b[offset as usize..end].copy_from_slice(bytes);
            Ok(())
        }
        fn size(&self) -> std::io::Result<u64> {
            Ok(self.0.lock().unwrap().len() as u64)
        }
        fn truncate(&self, n: u64) -> std::io::Result<()> {
            self.0.lock().unwrap().resize(n as usize, 0);
            Ok(())
        }
        fn close(&self) {}
    }

    #[test]
    fn sealed_scratch_round_trips_and_detects_replay() {
        let mem = Arc::new(MemSpill::default());
        let host = encrypting_spill_host(
            Arc::new(Arc::clone(&mem)),
            crate::seam::seeded_random_source(5),
        );
        assert!(host.encrypts());
        let file = host.create("hash").unwrap();
        let mut model = vec![0u8; 0];
        let mut write = |file: &Box<dyn crate::spillhost::SpillFile>, at: usize, bytes: &[u8]| {
            file.write_at(at as u64, bytes).unwrap();
            if model.len() < at + bytes.len() {
                model.resize(at + bytes.len(), 0);
            }
            model[at..at + bytes.len()].copy_from_slice(bytes);
            model.clone()
        };
        let secret = b"secret-scratch-".repeat(700);
        write(&file, 0, &secret);
        write(&file, 5000, b"patched");
        write(&file, 20000, b"far away");
        let expect = write(&file, 4090, b"straddles a block boundary");
        let read = |file: &Box<dyn crate::spillhost::SpillFile>| {
            let mut out = vec![0u8; file.size().unwrap() as usize];
            crate::spillhost::read_exact_at(file.as_ref(), 0, &mut out).unwrap();
            out
        };
        assert_eq!(read(&file), expect);
        let mut tail = [0u8; 4];
        assert_eq!(file.read_at(expect.len() as u64, &mut tail).unwrap(), 0);
        // Writing another block seals the cached one; nothing is stored in plaintext.
        write(&file, 30000, b"x");
        let stored = mem.0.lock().unwrap()[0].lock().unwrap().clone();
        assert!(!stored.windows(15).any(|w| w == b"secret-scratch-"));

        // Replay: put back an older sealed version of block 1, which was rewritten since.
        let block1 = stored[SCRATCH_STORED..2 * SCRATCH_STORED].to_vec();
        write(&file, 4100, b"newer");
        write(&file, 30001, b"y");
        assert_eq!(&read(&file)[4100..4105], b"newer");
        mem.0.lock().unwrap()[0].lock().unwrap()[SCRATCH_STORED..2 * SCRATCH_STORED]
            .copy_from_slice(&block1);
        let mut buf = [0u8; 5];
        assert!(
            file.read_at(4100, &mut buf).is_err(),
            "a replayed block must fail"
        );

        // A block sealed before a truncation does not authenticate after the block is rewritten.
        let file = host.create("rows").unwrap();
        let mem_file = mem.0.lock().unwrap().last().unwrap().clone();
        file.write_at(0, &[1u8; 4096]).unwrap();
        file.write_at(4096, &[2u8; 4096]).unwrap();
        file.write_at(8192, &[0u8; 1]).unwrap(); // seals block 1
        let sealed = mem_file.lock().unwrap()[SCRATCH_STORED..2 * SCRATCH_STORED].to_vec();
        file.truncate(4096).unwrap();
        file.write_at(4096, &[3u8; 4096]).unwrap();
        file.write_at(8192, &[0u8; 1]).unwrap();
        mem_file.lock().unwrap()[SCRATCH_STORED..2 * SCRATCH_STORED].copy_from_slice(&sealed);
        assert!(
            file.read_at(4096, &mut buf).is_err(),
            "a pre-truncation block must fail"
        );

        // Truncation zeroes the cut tail.
        let file = host.create("rows").unwrap();
        file.write_at(0, &[7u8; 9000]).unwrap();
        file.truncate(4500).unwrap();
        file.write_at(8999, &[1]).unwrap();
        let got = read(&file);
        assert_eq!(got.len(), 9000);
        assert!(got[..4500].iter().all(|&b| b == 7));
        assert!(got[4500..8999].iter().all(|&b| b == 0));
    }

    #[test]
    fn body_page_binds_index_and_tag() {
        let rng = crate::seam::seeded_random_source(1);
        let codec = create(&Credential::Key([1; 32]), Argon2Params::default(), rng).unwrap();
        let mut page = vec![0u8; 1024];
        page[0] = 2;
        page[16..20].copy_from_slice(b"rows");
        let plain = page[..1024 - RESERVE].to_vec();
        let tag = codec.seal_body(9, &mut page).unwrap();
        let opened = codec.open_body(9, &page, &tag).unwrap();
        assert_eq!(opened[16..], plain[16..]);
        assert!(codec.open_body(10, &page, &tag).is_err());
        assert!(codec.open_body(9, &page, &NO_TAG).is_err());
    }
}
