//! Cross-check: the hand-written page-service codec must reproduce spec/pageservice/fixtures.toml
//! byte for byte, decode it back, and reject every malformed frame with 08P01 (CLAUDE.md §8). The
//! constants and the codec's message table are checked against messages.toml. TOML is a test-time
//! only dependency.

use jed::SqlState;
use jed::pageservice::codec::*;
use std::path::Path;

fn spec(rel: &str) -> toml::Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../spec/pageservice")
        .join(rel);
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    toml::from_str(&text).unwrap()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Vec<u8> {
    assert!(text.len().is_multiple_of(2), "odd hex {text:?}");
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
        .collect()
}

fn str_field<'a>(row: &'a toml::Value, key: &str) -> &'a str {
    row.get(key)
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("fixture {:?} lacks {key}", row.get("name")))
}

/// Reads the fixture's `f_<field>` values in the canonical text form (fixtures.toml header),
/// recording each field name read so the test can check it read exactly the fixture's fields.
struct Fields<'a> {
    row: &'a toml::Value,
    page_size: usize,
    read: Vec<String>,
}

impl Fields<'_> {
    fn text(&mut self, name: &str) -> &str {
        self.read.push(name.to_string());
        str_field(self.row, &format!("f_{name}"))
    }

    fn u8(&mut self, name: &str) -> u8 {
        self.text(name).parse().unwrap()
    }

    fn u16(&mut self, name: &str) -> u16 {
        self.text(name).parse().unwrap()
    }

    fn u32(&mut self, name: &str) -> u32 {
        self.text(name).parse().unwrap()
    }

    fn u64(&mut self, name: &str) -> u64 {
        self.text(name).parse().unwrap()
    }

    fn bool(&mut self, name: &str) -> bool {
        match self.text(name) {
            "true" => true,
            "false" => false,
            other => panic!("bool {other:?}"),
        }
    }

    fn id16(&mut self, name: &str) -> Id16 {
        unhex(self.text(name)).try_into().unwrap()
    }

    fn string(&mut self, name: &str) -> String {
        self.text(name).to_string()
    }

    fn page(&mut self, name: &str) -> Vec<u8> {
        let size = self.page_size;
        parse_page(self.text(name), size)
    }

    fn u32_list(&mut self, name: &str) -> Vec<u32> {
        let text = self.text(name);
        if text.is_empty() {
            return Vec::new();
        }
        text.split(',').map(|v| v.parse().unwrap()).collect()
    }

    fn page_list(&mut self, name: &str) -> Vec<Vec<u8>> {
        let size = self.page_size;
        let text = self.text(name);
        if text.is_empty() {
            return Vec::new();
        }
        text.split(';').map(|p| parse_page(p, size)).collect()
    }

    fn indexed_page_list(&mut self, name: &str) -> Vec<IndexedPage> {
        let size = self.page_size;
        let text = self.text(name);
        if text.is_empty() {
            return Vec::new();
        }
        text.split(';')
            .map(|item| {
                let (index, page) = item.split_once('=').unwrap();
                IndexedPage {
                    index: index.parse().unwrap(),
                    page: parse_page(page, size),
                }
            })
            .collect()
    }
}

/// `<prefix hex>/<fill hex>`: the prefix bytes, then the fill byte up to `page_size`.
fn parse_page(text: &str, page_size: usize) -> Vec<u8> {
    let (prefix, fill) = text.split_once('/').unwrap();
    let mut page = unhex(prefix);
    assert!(page.len() <= page_size, "page prefix longer than page_size");
    page.resize(page_size, u8::from_str_radix(fill, 16).unwrap());
    page
}

fn build_message(name: &str, f: &mut Fields<'_>) -> Message {
    match name {
        "hello" => Message::Hello(Hello {
            protocol_min: f.u16("protocol_min"),
            protocol_max: f.u16("protocol_max"),
            format_version: f.u16("format_version"),
            flags: f.u8("flags"),
            client_name: f.string("client_name"),
        }),
        "hello_ok" => Message::HelloOk(HelloOk {
            protocol: f.u16("protocol"),
            format_version: f.u16("format_version"),
            page_size: f.u32("page_size"),
            connection_id: f.u64("connection_id"),
            flags: f.u8("flags"),
            max_read_pages: f.u32("max_read_pages"),
        }),
        "begin_read" => Message::BeginRead(BeginRead {
            incarnation: f.id16("incarnation"),
            last_txid: f.u64("last_txid"),
        }),
        "read_begun" => Message::ReadBegun(ReadBegun {
            incarnation: f.id16("incarnation"),
            txid: f.u64("txid"),
            invalidate: f.u8("invalidate"),
            indices: f.u32_list("indices"),
            meta: f.page("meta"),
        }),
        "read_pages" => Message::ReadPages(ReadPages {
            txid: f.u64("txid"),
            indices: f.u32_list("indices"),
        }),
        "pages" => Message::Pages(Pages {
            pages: f.page_list("pages"),
        }),
        "end_read" => Message::EndRead(EndRead {
            txid: f.u64("txid"),
        }),
        "begin_write" => Message::BeginWrite(BeginWrite {
            incarnation: f.id16("incarnation"),
            last_txid: f.u64("last_txid"),
            lock_timeout_ms: f.u64("lock_timeout_ms"),
        }),
        "write_begun" => Message::WriteBegun(WriteBegun {
            incarnation: f.id16("incarnation"),
            txid: f.u64("txid"),
            invalidate: f.u8("invalidate"),
            indices: f.u32_list("indices"),
            reuse_grant: f.bool("reuse_grant"),
            meta: f.page("meta"),
        }),
        "commit_begin" => Message::CommitBegin(CommitBegin {
            txid: f.u64("txid"),
            meta: f.page("meta"),
            manifest_pages: f.indexed_page_list("manifest_pages"),
        }),
        "commit_pages" => Message::CommitPages(CommitPages {
            pages: f.indexed_page_list("pages"),
        }),
        "commit_end" => Message::CommitEnd,
        "committed" => Message::Committed(Committed {
            txid: f.u64("txid"),
        }),
        "abort" => Message::Abort,
        "commit_status" => Message::CommitStatus(CommitStatus {
            txid: f.u64("txid"),
            digest: f.u64("digest"),
        }),
        "commit_status_reply" => Message::CommitStatusReply(CommitStatusReply {
            status: f.u8("status"),
        }),
        "request_ownership" => Message::RequestOwnership,
        "ownership_reply" => Message::OwnershipReply(OwnershipReply {
            status: f.u8("status"),
        }),
        "notice" => Message::Notice(Notice {
            kind: f.u8("kind"),
            connection_id: f.u64("connection_id"),
        }),
        "error" => Message::Error(ErrorReply {
            sqlstate: f.string("sqlstate"),
            message: f.string("message"),
            detail: f.string("detail"),
            flags: f.u8("flags"),
        }),
        other => panic!("no codec message {other}"),
    }
}

/// The codec's view of the catalog: every message with a body in protocol 1, its code, and its
/// field names in wire order (the order `build_message` and the codec read them).
const CODEC_MESSAGES: &[(&str, u8, &[&str])] = &[
    (
        "hello",
        CODE_HELLO,
        &[
            "protocol_min",
            "protocol_max",
            "format_version",
            "flags",
            "client_name",
        ],
    ),
    ("begin_read", CODE_BEGIN_READ, &["incarnation", "last_txid"]),
    ("read_pages", CODE_READ_PAGES, &["txid", "indices"]),
    ("end_read", CODE_END_READ, &["txid"]),
    (
        "begin_write",
        CODE_BEGIN_WRITE,
        &["incarnation", "last_txid", "lock_timeout_ms"],
    ),
    (
        "commit_begin",
        CODE_COMMIT_BEGIN,
        &["txid", "meta", "manifest_pages"],
    ),
    ("commit_pages", CODE_COMMIT_PAGES, &["pages"]),
    ("commit_end", CODE_COMMIT_END, &[]),
    ("abort", CODE_ABORT, &[]),
    ("commit_status", CODE_COMMIT_STATUS, &["txid", "digest"]),
    ("request_ownership", CODE_REQUEST_OWNERSHIP, &[]),
    (
        "hello_ok",
        CODE_HELLO_OK,
        &[
            "protocol",
            "format_version",
            "page_size",
            "connection_id",
            "flags",
            "max_read_pages",
        ],
    ),
    (
        "read_begun",
        CODE_READ_BEGUN,
        &["incarnation", "txid", "invalidate", "indices", "meta"],
    ),
    ("pages", CODE_PAGES, &["pages"]),
    (
        "write_begun",
        CODE_WRITE_BEGUN,
        &[
            "incarnation",
            "txid",
            "invalidate",
            "indices",
            "reuse_grant",
            "meta",
        ],
    ),
    ("committed", CODE_COMMITTED, &["txid"]),
    ("commit_status_reply", CODE_COMMIT_STATUS_REPLY, &["status"]),
    ("ownership_reply", CODE_OWNERSHIP_REPLY, &["status"]),
    ("notice", CODE_NOTICE, &["kind", "connection_id"]),
    (
        "error",
        CODE_ERROR,
        &["sqlstate", "message", "detail", "flags"],
    ),
];

/// The reserved messages: a code but no body in protocol 1.
const RESERVED: &[(&str, u8)] = &[
    ("snapshot_stream", CODE_SNAPSHOT_STREAM),
    ("subscribe", CODE_SUBSCRIBE),
];

fn assert_violation(result: jed::Result<impl std::fmt::Debug>, what: &str) {
    match result {
        Ok(v) => panic!("{what}: decoded without error: {v:?}"),
        Err(e) => assert_eq!(e.state, SqlState::ProtocolViolation, "{what}: {e:?}"),
    }
}

#[test]
fn constants_match_catalog() {
    let c = spec("messages.toml");
    let int = |k: &str| c[k].as_integer().unwrap();
    assert_eq!(int("protocol_version"), i64::from(PROTOCOL_VERSION));
    assert_eq!(int("max_frame_length"), i64::from(MAX_FRAME_LENGTH));
    assert_eq!(int("header_length"), HEADER_LENGTH as i64);
    assert_eq!(int("max_read_pages_cap"), i64::from(MAX_READ_PAGES_CAP));
    assert_eq!(
        int("max_invalidation_entries"),
        i64::from(MAX_INVALIDATION_ENTRIES)
    );
    assert_eq!(
        int("invalidation_window_commits"),
        i64::from(INVALIDATION_WINDOW_COMMITS)
    );
    assert_eq!(
        int("endpoint_max_path_bytes"),
        ENDPOINT_MAX_PATH_BYTES as i64
    );
    assert_eq!(
        c["owner_record_magic"].as_str().unwrap().as_bytes(),
        OWNER_RECORD_MAGIC
    );
    assert_eq!(int("owner_record_version"), i64::from(OWNER_RECORD_VERSION));
    // README.md §3.1's formula at a small and the default page size.
    assert_eq!(max_read_pages(4096), 1024);
    assert_eq!(max_read_pages(65536), (16_777_216 - 9) / 65536);
}

#[test]
fn message_table_matches_catalog() {
    let c = spec("messages.toml");
    let catalog = c["message"].as_array().unwrap();
    assert_eq!(catalog.len(), CODEC_MESSAGES.len() + RESERVED.len());
    for m in catalog {
        let name = m["name"].as_str().unwrap();
        let code = m["code"].as_integer().unwrap();
        let fields: Vec<&str> = m["fields"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f.as_str().unwrap().split_once(':').unwrap().0)
            .collect();
        if m.get("reply").and_then(|r| r.as_str()) == Some("reserved") {
            let (_, ours) = RESERVED
                .iter()
                .find(|(n, _)| *n == name)
                .unwrap_or_else(|| panic!("reserved {name} missing from the codec table"));
            assert_eq!(i64::from(*ours), code, "{name} code");
            continue;
        }
        let (_, ours, our_fields) = CODEC_MESSAGES
            .iter()
            .find(|(n, _, _)| *n == name)
            .unwrap_or_else(|| panic!("{name} missing from the codec table"));
        assert_eq!(i64::from(*ours), code, "{name} code");
        assert_eq!(*our_fields, fields.as_slice(), "{name} fields");
    }
}

#[test]
fn frame_fixtures_encode_and_decode() {
    let fx = spec("fixtures.toml");
    let frames = fx["frame"].as_array().unwrap();
    let mut seen = std::collections::BTreeSet::new();
    for row in frames {
        let name = str_field(row, "name");
        let message_name = str_field(row, "message");
        let page_size: u32 = str_field(row, "page_size").parse().unwrap();
        let request_id: u32 = str_field(row, "request_id").parse().unwrap();
        let mut fields = Fields {
            row,
            page_size: page_size as usize,
            read: Vec::new(),
        };
        let message = build_message(message_name, &mut fields);
        seen.insert(message_name.to_string());

        // The builder read exactly the fixture's fields, in the codec table's order.
        let (_, code, table_fields) = CODEC_MESSAGES
            .iter()
            .find(|(n, _, _)| *n == message_name)
            .unwrap();
        assert_eq!(fields.read, *table_fields, "{name}: field order");
        let mut fixture_keys: Vec<&str> = row
            .as_table()
            .unwrap()
            .keys()
            .filter_map(|k| k.strip_prefix("f_"))
            .collect();
        let mut read: Vec<&str> = fields.read.iter().map(String::as_str).collect();
        fixture_keys.sort_unstable();
        read.sort_unstable();
        assert_eq!(fixture_keys, read, "{name}: fixture fields");
        assert_eq!(message.code(), *code, "{name}: code");
        assert_eq!(message.name(), message_name, "{name}: name");

        let bytes = encode_frame(request_id, &message).unwrap();
        assert_eq!(hex(&bytes), str_field(row, "hex"), "{name}: encoding");
        let (got_id, got) = decode_frame(&bytes, page_size).unwrap();
        assert_eq!(got_id, request_id, "{name}: request id");
        assert_eq!(got, message, "{name}: decoding");
    }
    // Every message with a body has at least one fixture.
    assert_eq!(seen.len(), CODEC_MESSAGES.len());
}

#[test]
fn owner_record_fixtures_encode_and_decode() {
    let fx = spec("fixtures.toml");
    for row in fx["owner_record"].as_array().unwrap() {
        let name = str_field(row, "name");
        let mut f = Fields {
            row,
            page_size: 0,
            read: Vec::new(),
        };
        let flags = f.u8("flags");
        assert!(flags <= 1, "{name}: flags");
        let rec = OwnerRecord {
            serving: flags == 1,
            uid: f.u32("uid"),
            incarnation: f.id16("incarnation"),
            pid: f.u32("pid"),
            endpoint: f.string("endpoint"),
        };
        let bytes = encode_owner_record(&rec).unwrap();
        assert_eq!(hex(&bytes), str_field(row, "hex"), "{name}: encoding");
        assert_eq!(
            decode_owner_record(&bytes).unwrap(),
            rec,
            "{name}: decoding"
        );
    }
}

#[test]
fn owner_record_rejects_corruption() {
    let rec = OwnerRecord {
        serving: true,
        uid: 1000,
        incarnation: [7; 16],
        pid: 42,
        endpoint: "/tmp/jed-1000/jed-0707070707070707.sock".to_string(),
    };
    let good = encode_owner_record(&rec).unwrap();
    // The flags byte sits after the 8-byte magic and the 2-byte version.
    let reseal = |mut b: Vec<u8>| {
        let n = b.len() - 4;
        let crc = jed::format::crc32_ieee(&b[..n]);
        b[n..].copy_from_slice(&crc.to_be_bytes());
        b
    };
    let mut bad_magic = good.clone();
    bad_magic[0] = b'X';
    assert_violation(decode_owner_record(&reseal(bad_magic)), "magic");
    let mut bad_version = good.clone();
    bad_version[9] = 2;
    assert_violation(decode_owner_record(&reseal(bad_version)), "version");
    let mut reserved_flag = good.clone();
    reserved_flag[10] = 0x03;
    assert_violation(decode_owner_record(&reseal(reserved_flag)), "flags");
    let mut bad_crc = good.clone();
    *bad_crc.last_mut().unwrap() ^= 1;
    assert_violation(decode_owner_record(&bad_crc), "crc");
    let mut trailing = good.clone();
    trailing.push(0);
    assert_violation(decode_owner_record(&trailing), "trailing");
    assert_violation(decode_owner_record(&good[..good.len() - 1]), "truncated");
}

#[test]
fn malformed_fixtures_are_protocol_violations() {
    let fx = spec("fixtures.toml");
    for row in fx["malformed"].as_array().unwrap() {
        let name = str_field(row, "name");
        let page_size: u32 = str_field(row, "page_size").parse().unwrap();
        let bytes = unhex(str_field(row, "hex"));
        assert_violation(decode_frame(&bytes, page_size), name);
    }
}

#[test]
fn encode_rejects_unsendable_messages() {
    let bad_state = Message::Error(ErrorReply {
        sqlstate: "08p01".to_string(),
        message: String::new(),
        detail: String::new(),
        flags: 0,
    });
    assert_violation(encode_frame(1, &bad_state), "lower-case sqlstate");
    let mixed_pages = Message::Pages(Pages {
        pages: vec![vec![0; 256], vec![0; 512]],
    });
    assert_violation(encode_frame(1, &mixed_pages), "pages of differing size");
    let empty_meta = Message::CommitBegin(CommitBegin {
        txid: 1,
        meta: Vec::new(),
        manifest_pages: Vec::new(),
    });
    assert_violation(encode_frame(1, &empty_meta), "empty page");
    let too_big = Message::Pages(Pages {
        pages: vec![vec![0; 65536]; 257],
    });
    assert_violation(encode_frame(1, &too_big), "frame above max length");
}

#[test]
fn read_frame_streams_frames() {
    let fx = spec("fixtures.toml");
    let frames = fx["frame"].as_array().unwrap();
    let pick = |name: &str| {
        frames
            .iter()
            .find(|r| str_field(r, "name") == name)
            .map(|r| unhex(str_field(r, "hex")))
            .unwrap()
    };
    let mut stream = pick("read_begun_list");
    stream.extend(pick("pages"));
    let mut cursor = std::io::Cursor::new(stream);
    let (id, first) = read_frame(&mut cursor, 256).unwrap().unwrap();
    assert_eq!(id, 2);
    assert!(matches!(first, Message::ReadBegun(_)));
    let (id, second) = read_frame(&mut cursor, 256).unwrap().unwrap();
    assert_eq!(id, 5);
    assert!(matches!(second, Message::Pages(_)));
    assert!(read_frame(&mut cursor, 256).unwrap().is_none());

    // A length above the maximum is rejected before the body is read.
    let mut oversized = std::io::Cursor::new(unhex("01000001"));
    assert_violation(read_frame(&mut oversized, 0), "oversized length");

    // A stream that ends inside a frame is a lost connection, not a clean end.
    let end_read = pick("end_read");
    for cut in [2, 4, end_read.len() - 1] {
        let mut short = std::io::Cursor::new(end_read[..cut].to_vec());
        let err = read_frame(&mut short, 0).unwrap_err();
        assert_eq!(err.state, SqlState::ConnectionFailure, "cut at {cut}");
    }
}
