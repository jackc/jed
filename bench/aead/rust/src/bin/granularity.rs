//! Key-granularity probe for encryption.md §3.1: per-page versus per-commit-attempt AES-256-GCM
//! keys derived with HKDF-SHA-256 from random salts. The data key, purpose label and fixture salt
//! are public benchmark inputs; nothing here is an encrypted page format or a production protocol.
use aes_gcm::Aes256Gcm;
use aes_gcm::aead::{Aead, AeadInOut, Generate, KeyInit, Payload};
use hkdf::Hkdf;
use sha2::Sha256;
use std::collections::HashMap;
use std::hint::black_box;
use std::time::{Duration, Instant};

const LABEL: &[u8] = b"jed bench page key v0"; // versioned purpose label (benchmark only)
const SALT_LEN: usize = 32;
const WORKING: usize = 1024; // distinct pages in every seal/open working set
const COLD_CAP: usize = 2; // below every commit count, so each commit's first page misses
const HIT_CAP: usize = 1024; // holds every commit's key
const HIT_PAGES: usize = 16; // commit size of the all-hit case
const SIZES: [usize; 3] = [4096, 8192, 16384];
const COMMITS: [usize; 4] = [1, 4, 16, 256];

type Salt = [u8; SALT_LEN];

fn seq(start: u8, n: usize) -> Vec<u8> {
    (0..n).map(|i| start.wrapping_add(i as u8)).collect()
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}
fn random_salt() -> Salt {
    Salt::generate()
}

/// Derives a 32-byte AES key from a page or commit salt.
#[derive(Clone, Copy, PartialEq)]
enum Layout {
    /// HKDF with the random salt as the extract salt, per key.
    ExtractPerKey,
    /// One extract of the data key at unlock; per key, expand with the salt in the info.
    ExpandPerKey,
}
const LAYOUTS: [Layout; 2] = [Layout::ExtractPerKey, Layout::ExpandPerKey];

impl Layout {
    fn name(self) -> &'static str {
        match self {
            Layout::ExtractPerKey => "extract-per-key",
            Layout::ExpandPerKey => "expand-per-key",
        }
    }
}

struct Deriver {
    data_key: Vec<u8>,
    unlocked: Hkdf<Sha256>, // HKDF-Extract(empty salt, data key), computed once
}

impl Deriver {
    fn new() -> Self {
        let data_key = seq(0, 32);
        let unlocked = Hkdf::<Sha256>::new(None, &data_key);
        Deriver { data_key, unlocked }
    }
    fn derive(&self, layout: Layout, salt: &[u8]) -> [u8; 32] {
        let mut key = [0u8; 32];
        match layout {
            Layout::ExtractPerKey => Hkdf::<Sha256>::new(Some(salt), &self.data_key)
                .expand(LABEL, &mut key)
                .unwrap(),
            Layout::ExpandPerKey => self
                .unlocked
                .expand_multi_info(&[LABEL, salt], &mut key)
                .unwrap(),
        }
        key
    }
    fn aead(&self, layout: Layout, salt: &[u8]) -> Aes256Gcm {
        Aes256Gcm::new_from_slice(&self.derive(layout, salt)).unwrap()
    }
}

/// Per-page keys seal one message each, so the nonce is fixed. Per-commit keys use the page's
/// ordinal within its commit attempt: 8 zero bytes then a big-endian u32.
fn ordinal_nonce(ordinal: u32) -> [u8; 12] {
    let mut nonce = [0u8; 12];
    nonce[8..].copy_from_slice(&ordinal.to_be_bytes());
    nonce
}

/// A bounded FIFO of derived ciphers keyed by salt.
struct KeyCache<'a> {
    limit: usize,
    entries: HashMap<Salt, Aes256Gcm>,
    ring: Vec<Salt>,
    next: usize,
    lookups: u64,
    misses: u64,
    deriver: &'a Deriver,
    layout: Layout,
}

impl<'a> KeyCache<'a> {
    fn new(limit: usize, deriver: &'a Deriver, layout: Layout) -> Self {
        KeyCache {
            limit,
            entries: HashMap::with_capacity(limit),
            ring: Vec::with_capacity(limit),
            next: 0,
            lookups: 0,
            misses: 0,
            deriver,
            layout,
        }
    }
    fn get(&mut self, salt: &Salt) -> &Aes256Gcm {
        self.lookups += 1;
        if !self.entries.contains_key(salt) {
            self.misses += 1;
            let cipher = self.deriver.aead(self.layout, salt);
            if self.ring.len() < self.limit {
                self.ring.push(*salt);
            } else {
                self.entries.remove(&self.ring[self.next]);
                self.ring[self.next] = *salt;
                self.next = (self.next + 1) % self.limit;
            }
            self.entries.insert(*salt, cipher);
        }
        &self.entries[salt]
    }
}

fn measure(samples: usize, millis: u64, mut op: impl FnMut()) -> Vec<f64> {
    let mut values = Vec::new();
    for sample in 0..=samples {
        let start = Instant::now();
        let mut count = 0u64;
        loop {
            for _ in 0..64 {
                op();
            }
            count += 64;
            if start.elapsed() >= Duration::from_millis(millis) {
                break;
            }
        }
        if sample > 0 {
            values.push(start.elapsed().as_secs_f64() * 1e9 / count as f64);
        }
    }
    values
}

/// Prints one timing row; `fields` is a JSON object body without braces.
fn timing(fields: &str, times: &[f64]) {
    println!(
        "{{\"kind\":\"timing\",\"backend\":\"rustcrypto\",{fields},\"ns_samples\":{times:?}}}"
    );
}

struct PageSet {
    size: usize,
    plain: Vec<Vec<u8>>,
    aad: Vec<Vec<u8>>,
}

impl PageSet {
    fn new(size: usize) -> Self {
        let mut set = PageSet {
            size,
            plain: Vec::new(),
            aad: Vec::new(),
        };
        for p in 0..WORKING {
            set.plain
                .push((0..size).map(|i| (31 * i + 7 + p) as u8).collect());
            let mut aad = seq(0xa0, 32);
            aad[..8].copy_from_slice(&(p as u64).to_be_bytes()); // stands in for the page identity binding
            set.aad.push(aad);
        }
        set
    }
}

fn seal(cipher: &Aes256Gcm, nonce: &[u8; 12], msg: &[u8], aad: &[u8]) -> Vec<u8> {
    cipher.encrypt(nonce.into(), Payload { msg, aad }).unwrap()
}
fn open(cipher: &Aes256Gcm, nonce: &[u8; 12], msg: &[u8], aad: &[u8]) -> Option<Vec<u8>> {
    cipher.decrypt(nonce.into(), Payload { msg, aad }).ok()
}

fn check_hkdf() {
    // RFC 5869 test cases 1 and 3 check the library invocation, not a custom HKDF.
    let ikm = [0x0bu8; 22];
    for (salt, info, prk, okm) in [
        (
            "000102030405060708090a0b0c",
            "f0f1f2f3f4f5f6f7f8f9",
            "077709362c2e32df0ddc3f0dc47bba6390b6c73bb50f9c3122ec844ad7c2b3e5",
            "3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865",
        ),
        (
            "",
            "",
            "19ef24a32c717b167f33a91d6f648bdf96596776afdb6377ac434c1c293ccb04",
            "8da4e775a563c18f715f802a063c5a31b8a11f5c5ee1879ec3454e5f3c738d2d9d201395faa4b61a96c8",
        ),
    ] {
        let (got_prk, hk) = Hkdf::<Sha256>::extract(Some(&unhex(salt)), &ikm);
        let mut got = [0u8; 42];
        hk.expand(&unhex(info), &mut got).unwrap();
        assert_eq!(hex(&got_prk), prk, "HKDF RFC 5869 PRK");
        assert_eq!(hex(&got), okm, "HKDF RFC 5869 OKM");
        let mut again = [0u8; 42];
        Hkdf::<Sha256>::from_prk(&got_prk)
            .unwrap()
            .expand(&unhex(info), &mut again)
            .unwrap();
        assert_eq!(got, again);
    }
}

/// Checks round trips and rejection of every tampered input, and emits fixed-salt fixtures for
/// cross-backend comparison. It runs before any timing.
fn validate(d: &Deriver) {
    check_hkdf();
    let salt = seq(0x40, SALT_LEN);
    let keys: Vec<[u8; 32]> = LAYOUTS.iter().map(|&l| d.derive(l, &salt)).collect();
    for (l, key) in LAYOUTS.iter().zip(&keys) {
        println!(
            "{{\"kind\":\"fixture\",\"fixture\":\"key\",\"layout\":\"{}\",\"bytes\":0,\"hex\":\"{}\"}}",
            l.name(),
            hex(key)
        );
    }
    assert_ne!(keys[0], keys[1], "layouts derive the same key");
    let mut other_salt = salt.clone();
    other_salt[SALT_LEN - 1] ^= 1;
    for size in SIZES {
        let set = PageSet::new(size);
        let (plain, aad) = (&set.plain[1], &set.aad[1]);
        for (li, &l) in LAYOUTS.iter().enumerate() {
            let cipher = Aes256Gcm::new_from_slice(&keys[li]).unwrap();
            for scheme in ["per-page", "per-commit"] {
                // Page 1 at commit ordinal 1 under per-commit keys; ordinal is ignored per page.
                let ordinal = if scheme == "per-commit" { 1 } else { 0 };
                let nonce = ordinal_nonce(ordinal);
                let sealed = seal(&cipher, &nonce, plain, aad);
                println!(
                    "{{\"kind\":\"fixture\",\"fixture\":\"{scheme}\",\"layout\":\"{}\",\"bytes\":{size},\"hex\":\"{}\"}}",
                    l.name(),
                    hex(&sealed)
                );
                assert_eq!(
                    open(&cipher, &nonce, &sealed, aad).as_ref(),
                    Some(plain),
                    "round trip"
                );
                for index in [0, size - 1, size, size + 15] {
                    let mut bad = sealed.clone();
                    bad[index] ^= 1;
                    assert!(
                        open(&cipher, &nonce, &bad, aad).is_none(),
                        "accepted tampered ciphertext/tag"
                    );
                }
                let mut bad_aad = aad.clone();
                bad_aad[7] ^= 1;
                assert!(
                    open(&cipher, &nonce, &sealed, &bad_aad).is_none(),
                    "accepted tampered aad"
                );
                assert!(
                    open(&d.aead(l, &other_salt), &nonce, &sealed, aad).is_none(),
                    "accepted wrong salt"
                );
                let other = Aes256Gcm::new_from_slice(&keys[1 - li]).unwrap();
                assert!(
                    open(&other, &nonce, &sealed, aad).is_none(),
                    "accepted wrong layout"
                );
                assert!(
                    open(&cipher, &ordinal_nonce(ordinal + 1), &sealed, aad).is_none(),
                    "accepted wrong nonce"
                );
            }
        }
    }
}

fn breakdown(samples: usize, millis: u64, d: &Deriver) {
    let key = d.derive(Layout::ExtractPerKey, &seq(0x40, SALT_LEN));
    let mut salt = random_salt();
    timing(
        "\"scheme\":\"key-setup\",\"operation\":\"salt-random32\",\"bytes\":0",
        &measure(samples, millis, || {
            salt = random_salt();
            black_box(&salt);
        }),
    );
    timing(
        "\"scheme\":\"key-setup\",\"operation\":\"aes-gcm-key-setup\",\"bytes\":0",
        &measure(samples, millis, || {
            black_box(Aes256Gcm::new_from_slice(black_box(&key)).unwrap());
        }),
    );
    for l in LAYOUTS {
        timing(
            &format!(
                "\"scheme\":\"key-setup\",\"operation\":\"hkdf-{}\",\"bytes\":0",
                l.name()
            ),
            &measure(samples, millis, || {
                black_box(d.derive(l, black_box(&salt)));
            }),
        );
        timing(
            &format!(
                "\"scheme\":\"key-setup\",\"operation\":\"derive-{}\",\"bytes\":0",
                l.name()
            ),
            &measure(samples, millis, || {
                black_box(d.aead(l, &random_salt()));
            }),
        );
    }
}

fn seal_cases(samples: usize, millis: u64, set: &PageSet, d: &Deriver) {
    let size = set.size;
    let mut buf: Vec<u8> = Vec::with_capacity(size + 16);
    // Reusable output with the input copy timed, as in the main benchmark's in-place mode.
    let seal_into = |buf: &mut Vec<u8>, cipher: &Aes256Gcm, p: usize, nonce: &[u8; 12]| {
        buf.clear();
        buf.extend_from_slice(black_box(&set.plain[p]));
        cipher
            .encrypt_in_place(nonce.into(), &set.aad[p], buf)
            .unwrap();
        black_box(&*buf);
    };
    // The reusable output holds each case's final sealed page; check that it opens.
    let check = |buf: &Vec<u8>, cipher: &Aes256Gcm, p: usize, nonce: &[u8; 12]| {
        assert_eq!(
            open(cipher, nonce, buf, &set.aad[p]).as_ref(),
            Some(&set.plain[p]),
            "seal output"
        );
    };
    // Reference: one long-lived key, unique counter nonces, no derivation.
    let fixed = d.aead(Layout::ExtractPerKey, &seq(0x40, SALT_LEN));
    let mut counter = 0u64;
    let mut nonce = [0u8; 12];
    let times = measure(samples, millis, || {
        counter += 1;
        nonce[4..].copy_from_slice(&counter.to_be_bytes());
        seal_into(
            &mut buf,
            &fixed,
            (counter % WORKING as u64) as usize,
            &nonce,
        );
    });
    check(&buf, &fixed, (counter % WORKING as u64) as usize, &nonce);
    timing(
        &format!("\"scheme\":\"fixed-key\",\"operation\":\"seal\",\"bytes\":{size}"),
        &times,
    );
    for l in LAYOUTS {
        let mut p = 0;
        let mut last = None;
        let times = measure(samples, millis, || {
            let cipher = d.aead(l, &random_salt());
            seal_into(&mut buf, &cipher, p, &ordinal_nonce(0));
            last = Some((cipher, p));
            p = (p + 1) % WORKING;
        });
        let (cipher, lp) = last.take().unwrap();
        check(&buf, &cipher, lp, &ordinal_nonce(0));
        timing(
            &format!(
                "\"scheme\":\"per-page\",\"layout\":\"{}\",\"operation\":\"seal\",\"bytes\":{size}",
                l.name()
            ),
            &times,
        );
        for n in COMMITS {
            let mut cipher = d.aead(l, &random_salt());
            let (mut count, mut derivations) = (0u64, 0u64);
            let times = measure(samples, millis, || {
                let ordinal = (count % n as u64) as u32;
                if ordinal == 0 {
                    cipher = d.aead(l, &random_salt());
                    derivations += 1;
                }
                seal_into(
                    &mut buf,
                    &cipher,
                    (count % WORKING as u64) as usize,
                    &ordinal_nonce(ordinal),
                );
                count += 1;
            });
            assert_eq!(
                derivations,
                count.div_ceil(n as u64),
                "per-commit derivation count"
            );
            let last = count - 1;
            check(
                &buf,
                &cipher,
                (last % WORKING as u64) as usize,
                &ordinal_nonce((last % n as u64) as u32),
            );
            timing(
                &format!(
                    "\"scheme\":\"per-commit\",\"layout\":\"{}\",\"commit_pages\":{n},\"operation\":\"seal\",\"bytes\":{size}",
                    l.name()
                ),
                &times,
            );
        }
    }
}

fn open_cases(samples: usize, millis: u64, set: &PageSet, d: &Deriver) {
    let size = set.size;
    let mut buf: Vec<u8> = Vec::with_capacity(size + 16);
    let open_into =
        |buf: &mut Vec<u8>, cipher: &Aes256Gcm, p: usize, nonce: &[u8; 12], sealed: &[u8]| {
            buf.clear();
            buf.extend_from_slice(black_box(sealed));
            cipher
                .decrypt_in_place(nonce.into(), &set.aad[p], buf)
                .unwrap();
            black_box(&*buf);
        };
    // Reference: one long-lived key; page p sealed at nonce p.
    let fixed = d.aead(Layout::ExtractPerKey, &seq(0x40, SALT_LEN));
    let mut sealed: Vec<Vec<u8>> = (0..WORKING)
        .map(|p| seal(&fixed, &ordinal_nonce(p as u32), &set.plain[p], &set.aad[p]))
        .collect();
    let mut p = 0;
    let times = measure(samples, millis, || {
        open_into(&mut buf, &fixed, p, &ordinal_nonce(p as u32), &sealed[p]);
        p = (p + 1) % WORKING;
    });
    timing(
        &format!("\"scheme\":\"fixed-key\",\"operation\":\"open\",\"bytes\":{size}"),
        &times,
    );
    let mut salts: Vec<Salt> = vec![[0; SALT_LEN]; WORKING];
    for l in LAYOUTS {
        // Per page: every page has its own salt and every open derives its key.
        for p in 0..WORKING {
            salts[p] = random_salt();
            sealed[p] = seal(
                &d.aead(l, &salts[p]),
                &ordinal_nonce(0),
                &set.plain[p],
                &set.aad[p],
            );
        }
        let mut p = 0;
        let times = measure(samples, millis, || {
            open_into(
                &mut buf,
                &d.aead(l, &salts[p]),
                p,
                &ordinal_nonce(0),
                &sealed[p],
            );
            p = (p + 1) % WORKING;
        });
        timing(
            &format!(
                "\"scheme\":\"per-page\",\"layout\":\"{}\",\"operation\":\"open\",\"bytes\":{size}",
                l.name()
            ),
            &times,
        );
        // Per commit: pages grouped into WORKING/n commits, read in commit order through a cache
        // too small to keep earlier commits, so each commit's first page misses.
        for n in COMMITS {
            for c in 0..WORKING / n {
                salts[c] = random_salt();
                let cipher = d.aead(l, &salts[c]);
                for o in 0..n {
                    let p = c * n + o;
                    sealed[p] = seal(
                        &cipher,
                        &ordinal_nonce(o as u32),
                        &set.plain[p],
                        &set.aad[p],
                    );
                }
            }
            let mut cache = KeyCache::new(COLD_CAP, d, l);
            let mut p = 0;
            let times = measure(samples, millis, || {
                open_into(
                    &mut buf,
                    cache.get(&salts[p / n]),
                    p,
                    &ordinal_nonce((p % n) as u32),
                    &sealed[p],
                );
                p = (p + 1) % WORKING;
            });
            assert_eq!(
                cache.misses,
                cache.lookups.div_ceil(n as u64),
                "cache misses at {n} pages per commit"
            );
            timing(
                &format!(
                    "\"scheme\":\"per-commit\",\"layout\":\"{}\",\"commit_pages\":{n},\"cache\":\"miss-per-commit\",\"operation\":\"open\",\"bytes\":{size}",
                    l.name()
                ),
                &times,
            );
        }
    }
    // All hits: every commit's key already cached; layouts do not matter.
    let n = HIT_PAGES;
    let mut cache = KeyCache::new(HIT_CAP, d, Layout::ExtractPerKey);
    for c in 0..WORKING / n {
        salts[c] = random_salt();
        let cipher = cache.get(&salts[c]).clone();
        for o in 0..n {
            let p = c * n + o;
            sealed[p] = seal(
                &cipher,
                &ordinal_nonce(o as u32),
                &set.plain[p],
                &set.aad[p],
            );
        }
    }
    let warm = cache.misses;
    let mut p = 0;
    let times = measure(samples, millis, || {
        open_into(
            &mut buf,
            cache.get(&salts[p / n]),
            p,
            &ordinal_nonce((p % n) as u32),
            &sealed[p],
        );
        p = (p + 1) % WORKING;
    });
    assert_eq!(cache.misses, warm, "all-hit cache missed");
    assert_eq!(buf, set.plain[(p + WORKING - 1) % WORKING], "open output");
    timing(
        &format!(
            "\"scheme\":\"per-commit\",\"commit_pages\":{n},\"cache\":\"all-hit\",\"operation\":\"open\",\"bytes\":{size}"
        ),
        &times,
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let samples: usize = args[1].parse().unwrap();
    let millis: u64 = args[2].parse().unwrap();
    let d = Deriver::new();
    validate(&d);
    if samples == 0 {
        return;
    }
    breakdown(samples, millis, &d);
    for size in SIZES {
        let set = PageSet::new(size);
        seal_cases(samples, millis, &set, &d);
        open_cases(samples, millis, &set, &d);
    }
}
