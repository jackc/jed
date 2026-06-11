//! Cross-check: the Rust LZ4-block encoder must reproduce the byte-exact vectors in
//! spec/fileformat/lz4_vectors.toml (CLAUDE.md §8; spec/fileformat/lz4.md §4). The encoder is
//! pinned — a library would diverge (large-values.md §6) — so these vectors are what guarantee
//! the Rust, Go, TS, and Ruby codecs emit identical compressed bytes (which the goldens and the
//! deterministic cost both depend on). The decoder is checked by round-tripping each vector.

use jed::lz4::{compress, decompress};
use std::path::Path;

fn spec(rel: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../spec")
        .join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn unhex(s: &str) -> Vec<u8> {
    assert!(s.len() % 2 == 0, "odd hex length");
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn encoder_matches_the_pinned_vectors() {
    let v: toml::Value = toml::from_str(&spec("fileformat/lz4_vectors.toml")).unwrap();
    let vectors = v["vector"].as_array().expect("[[vector]] array");
    assert!(vectors.len() >= 10, "vector corpus unexpectedly small");
    for vec in vectors {
        let name = vec["name"].as_str().unwrap();
        let input = unhex(vec["input_hex"].as_str().unwrap());
        let expected = vec["compressed_hex"].as_str().unwrap();
        let comp = compress(&input);
        assert_eq!(hex(&comp), expected, "vector {name}: compressed bytes");
        let round = decompress(&comp, input.len()).unwrap_or_else(|e| {
            panic!("vector {name}: decompress failed: {e:?}");
        });
        assert_eq!(round, input, "vector {name}: round-trip");
    }
}
