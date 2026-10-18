//! Isolated crypto-only WASM probe. No engine changes, bindings generator, or raw pointers.
//! JS copies bytes through exported linear-memory offsets, between calls (no live Rust borrow).
//! Fixed public keys are benchmark inputs, never production secrets.
use aes_gcm::{
    Aes256Gcm,
    aead::{AeadInOut, KeyInit},
};
use chacha20poly1305::XChaCha20Poly1305;
use std::cell::RefCell;

enum Cipher {
    Aes(Aes256Gcm),
    XChaCha(XChaCha20Poly1305),
}
struct State {
    cipher: Cipher,
    buffer: Vec<u8>,
    nonce: Vec<u8>,
    aad: Vec<u8>,
}
thread_local! { static STATE: RefCell<Option<State>> = const { RefCell::new(None) }; }

#[unsafe(no_mangle)]
pub extern "C" fn setup(algorithm: u32, size: u32, wrong_key: u32) {
    let key: Vec<u8> = (0..32).map(|i| i + wrong_key as u8).collect();
    let cipher = if algorithm == 0 {
        Cipher::Aes(Aes256Gcm::new_from_slice(&key).unwrap())
    } else {
        Cipher::XChaCha(XChaCha20Poly1305::new_from_slice(&key).unwrap())
    };
    STATE.with_borrow_mut(|s| {
        *s = Some(State {
            cipher,
            buffer: vec![0; size as usize + 16],
            nonce: vec![0; if algorithm == 0 { 12 } else { 24 }],
            aad: vec![0; 32],
        })
    });
}

#[unsafe(no_mangle)]
pub extern "C" fn buffer_ptr() -> usize {
    STATE.with_borrow(|s| s.as_ref().unwrap().buffer.as_ptr() as usize)
}
#[unsafe(no_mangle)]
pub extern "C" fn nonce_ptr() -> usize {
    STATE.with_borrow(|s| s.as_ref().unwrap().nonce.as_ptr() as usize)
}
#[unsafe(no_mangle)]
pub extern "C" fn aad_ptr() -> usize {
    STATE.with_borrow(|s| s.as_ref().unwrap().aad.as_ptr() as usize)
}

#[unsafe(no_mangle)]
pub extern "C" fn prepare(len: u32) {
    STATE.with_borrow_mut(|s| {
        let s = s.as_mut().unwrap();
        assert!(len as usize <= s.buffer.capacity());
        s.buffer.resize(len as usize, 0);
    });
}

#[unsafe(no_mangle)]
pub extern "C" fn crypt(open: u32, len: u32) -> u32 {
    STATE.with_borrow_mut(|state| {
        let s = state.as_mut().unwrap();
        if len as usize > s.buffer.capacity() {
            return 0;
        }
        s.buffer.resize(len as usize, 0);
        let result = match &s.cipher {
            Cipher::Aes(c) => {
                let n = s.nonce.as_slice().try_into().unwrap();
                if open == 0 {
                    c.encrypt_in_place(n, &s.aad, &mut s.buffer)
                } else {
                    c.decrypt_in_place(n, &s.aad, &mut s.buffer)
                }
            }
            Cipher::XChaCha(c) => {
                let n = s.nonce.as_slice().try_into().unwrap();
                if open == 0 {
                    c.encrypt_in_place(n, &s.aad, &mut s.buffer)
                } else {
                    c.decrypt_in_place(n, &s.aad, &mut s.buffer)
                }
            }
        };
        if result.is_ok() {
            s.buffer.len() as u32
        } else {
            0
        }
    })
}
