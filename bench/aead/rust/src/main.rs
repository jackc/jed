//! Public fixed inputs for measurement only, never a production nonce scheme.
use aes_gcm::Aes256Gcm;
use aes_gcm::aead::{Aead, AeadInOut, KeyInit, Payload, consts::U12};
use chacha20poly1305::ChaCha20Poly1305;
use std::{
    hint::black_box,
    time::{Duration, Instant},
};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn measure(mut operation: impl FnMut(), samples: usize, millis: u64) -> Vec<f64> {
    let mut values = Vec::new();
    for sample in 0..=samples {
        let start = Instant::now();
        let mut count = 0u64;
        loop {
            for _ in 0..64 {
                operation();
            }
            count += 64;
            if start.elapsed() >= Duration::from_millis(millis) {
                break;
            }
        }
        let ns = start.elapsed().as_secs_f64() * 1e9 / count as f64;
        if sample > 0 {
            values.push(ns);
        }
    }
    values
}
fn run<C: Aead + AeadInOut + KeyInit + aes_gcm::aead::AeadCore<NonceSize = U12>>(
    algorithm: &str,
    samples: usize,
    millis: u64,
) {
    let key: Vec<u8> = (0..32).collect();
    let wrong: Vec<u8> = (1..33).collect();
    let cipher = C::new_from_slice(&key).unwrap();
    let wrong_cipher = C::new_from_slice(&wrong).unwrap();
    let aad: Vec<u8> = (0xa0..0xc0).collect();
    for size in [4096usize, 8192, 16384, 65536] {
        let plain: Vec<u8> = (0..size).map(|i| (i * 31 + 7) as u8).collect();
        let mut nonce = [0u8; 12];
        nonce[..4].copy_from_slice(&(size as u32).to_be_bytes());
        let sealed = cipher
            .encrypt(
                (&nonce).into(),
                Payload {
                    msg: &plain,
                    aad: &aad,
                },
            )
            .unwrap();
        assert_eq!(
            cipher
                .decrypt(
                    (&nonce).into(),
                    Payload {
                        msg: &sealed,
                        aad: &aad
                    }
                )
                .unwrap(),
            plain
        );
        for index in [0, sealed.len() - 1] {
            let mut bad = sealed.clone();
            bad[index] ^= 1;
            assert!(
                cipher
                    .decrypt(
                        (&nonce).into(),
                        Payload {
                            msg: &bad,
                            aad: &aad
                        }
                    )
                    .is_err()
            );
        }
        let mut bad_aad = aad.clone();
        bad_aad[0] ^= 1;
        assert!(
            cipher
                .decrypt(
                    (&nonce).into(),
                    Payload {
                        msg: &sealed,
                        aad: &bad_aad
                    }
                )
                .is_err()
        );
        assert!(
            wrong_cipher
                .decrypt(
                    (&nonce).into(),
                    Payload {
                        msg: &sealed,
                        aad: &aad
                    }
                )
                .is_err()
        );
        println!(
            "{{\"kind\":\"fixture\",\"backend\":\"rustcrypto\",\"algorithm\":\"{algorithm}\",\"bytes\":{size},\"hex\":\"{}\"}}",
            hex(&sealed)
        );
        if samples == 0 {
            continue;
        }
        let mut counter = 0u64;
        for mode in ["allocating", "reused-in-place"] {
            for operation in ["seal", "open"] {
                let mut buffer = Vec::with_capacity(size + 16);
                let times = measure(
                    || {
                        if operation == "seal" {
                            counter += 1;
                            nonce[4..].copy_from_slice(&counter.to_be_bytes());
                            if mode == "allocating" {
                                black_box(
                                    cipher
                                        .encrypt(
                                            (&nonce).into(),
                                            Payload {
                                                msg: black_box(&plain),
                                                aad: &aad,
                                            },
                                        )
                                        .unwrap(),
                                );
                            } else {
                                buffer.clear();
                                buffer.extend_from_slice(black_box(&plain));
                                cipher
                                    .encrypt_in_place((&nonce).into(), &aad, &mut buffer)
                                    .unwrap();
                                black_box(&buffer);
                            }
                        } else {
                            nonce[4..].fill(0);
                            if mode == "allocating" {
                                black_box(
                                    cipher
                                        .decrypt(
                                            (&nonce).into(),
                                            Payload {
                                                msg: black_box(&sealed),
                                                aad: &aad,
                                            },
                                        )
                                        .unwrap(),
                                );
                            } else {
                                buffer.clear();
                                buffer.extend_from_slice(black_box(&sealed));
                                cipher
                                    .decrypt_in_place((&nonce).into(), &aad, &mut buffer)
                                    .unwrap();
                                black_box(&buffer);
                            }
                        }
                    },
                    samples,
                    millis,
                );
                // Validate the final reusable-buffer result as well as the allocating fixture.
                if mode == "reused-in-place" {
                    if operation == "seal" {
                        assert_eq!(
                            cipher
                                .decrypt(
                                    (&nonce).into(),
                                    Payload {
                                        msg: &buffer,
                                        aad: &aad
                                    }
                                )
                                .unwrap(),
                            plain
                        );
                    } else {
                        assert_eq!(buffer, plain);
                    }
                }
                println!(
                    "{{\"kind\":\"timing\",\"backend\":\"rustcrypto\",\"algorithm\":\"{algorithm}\",\"bytes\":{size},\"mode\":\"{mode}\",\"operation\":\"{operation}\",\"ns_samples\":{times:?}}}"
                );
            }
        }
    }
}
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let samples = args[1].parse().unwrap();
    let millis = args[2].parse().unwrap();
    run::<Aes256Gcm>("aes-256-gcm", samples, millis);
    run::<ChaCha20Poly1305>("chacha20-poly1305", samples, millis);
}
