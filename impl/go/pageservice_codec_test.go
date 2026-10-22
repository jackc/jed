package jed

// Cross-checks the page-service wire codec (pageservice_codec.go) against the shared contract:
// spec/pageservice/messages.toml (constants, codes, field lists) and spec/pageservice/fixtures.toml
// (byte fixtures and malformed frames). Byte-level checks are out of the corpus's reach (CLAUDE.md
// §10), so they live here.

import (
	"bytes"
	"encoding/hex"
	"errors"
	"hash/crc32"
	"io"
	"os"
	"reflect"
	"strconv"
	"strings"
	"testing"
)

const (
	psMessagesTOML = "../../spec/pageservice/messages.toml"
	psFixturesTOML = "../../spec/pageservice/fixtures.toml"
)

// psCatalogEntry is the Go side of one messages.toml message: its code, its field list in order,
// and a builder from a fixture row's f_* text form.
type psCatalogEntry struct {
	code   byte
	fields []string
	build  func(f psFixtureFields) psMessage
}

// psCatalog lists every non-reserved catalog message the codec implements.
var psCatalog = map[string]psCatalogEntry{
	"hello": {
		psCodeHello,
		[]string{"protocol_min:u16", "protocol_max:u16", "format_version:u16", "flags:u8", "client_name:string", "credential:credential"},
		func(f psFixtureFields) psMessage {
			return psHello{protocolMin: f.u16("protocol_min"), protocolMax: f.u16("protocol_max"), formatVersion: f.u16("format_version"), flags: f.u8("flags"), clientName: f.str("client_name"), credential: f.credential("credential")}
		},
	},
	"begin_read": {
		psCodeBeginRead,
		[]string{"incarnation:id16", "last_txid:u64"},
		func(f psFixtureFields) psMessage {
			return psBeginRead{incarnation: f.id("incarnation"), lastTxid: f.u64("last_txid")}
		},
	},
	"read_pages": {
		psCodeReadPages,
		[]string{"txid:u64", "indices:u32_list"},
		func(f psFixtureFields) psMessage {
			return psReadPages{txid: f.u64("txid"), indices: f.u32List("indices")}
		},
	},
	"end_read": {
		psCodeEndRead,
		[]string{"txid:u64"},
		func(f psFixtureFields) psMessage { return psEndRead{txid: f.u64("txid")} },
	},
	"begin_write": {
		psCodeBeginWrite,
		[]string{"incarnation:id16", "last_txid:u64", "lock_timeout_ms:u64"},
		func(f psFixtureFields) psMessage {
			return psBeginWrite{incarnation: f.id("incarnation"), lastTxid: f.u64("last_txid"), lockTimeoutMs: f.u64("lock_timeout_ms")}
		},
	},
	"commit_begin": {
		psCodeCommitBegin,
		[]string{"txid:u64", "meta:page", "manifest_pages:indexed_page_list"},
		func(f psFixtureFields) psMessage {
			return psCommitBegin{txid: f.u64("txid"), meta: f.page("meta"), manifestPages: f.indexedPageList("manifest_pages")}
		},
	},
	"commit_pages": {
		psCodeCommitPages,
		[]string{"pages:indexed_page_list"},
		func(f psFixtureFields) psMessage { return psCommitPages{pages: f.indexedPageList("pages")} },
	},
	"commit_end":        {psCodeCommitEnd, []string{}, func(psFixtureFields) psMessage { return psCommitEnd{} }},
	"abort":             {psCodeAbort, []string{}, func(psFixtureFields) psMessage { return psAbort{} }},
	"request_ownership": {psCodeRequestOwnership, []string{}, func(psFixtureFields) psMessage { return psRequestOwnership{} }},
	"commit_status": {
		psCodeCommitStatus,
		[]string{"txid:u64", "digest:u64"},
		func(f psFixtureFields) psMessage { return psCommitStatus{txid: f.u64("txid"), digest: f.u64("digest")} },
	},
	"hello_ok": {
		psCodeHelloOK,
		[]string{"protocol:u16", "format_version:u16", "page_size:u32", "connection_id:u64", "flags:u8", "max_read_pages:u32"},
		func(f psFixtureFields) psMessage {
			return psHelloOK{protocol: f.u16("protocol"), formatVersion: f.u16("format_version"), pageSize: f.u32("page_size"), connectionID: f.u64("connection_id"), flags: f.u8("flags"), maxReadPages: f.u32("max_read_pages")}
		},
	},
	"read_begun": {
		psCodeReadBegun,
		[]string{"incarnation:id16", "txid:u64", "invalidate:u8", "indices:u32_list", "meta:page"},
		func(f psFixtureFields) psMessage {
			return psReadBegun{incarnation: f.id("incarnation"), txid: f.u64("txid"), invalidate: f.u8("invalidate"), indices: f.u32List("indices"), meta: f.page("meta")}
		},
	},
	"pages": {
		psCodePages,
		[]string{"pages:page_list"},
		func(f psFixtureFields) psMessage { return psPages{pages: f.pageList("pages")} },
	},
	"write_begun": {
		psCodeWriteBegun,
		[]string{"incarnation:id16", "txid:u64", "invalidate:u8", "indices:u32_list", "reuse_grant:bool", "meta:page"},
		func(f psFixtureFields) psMessage {
			return psWriteBegun{incarnation: f.id("incarnation"), txid: f.u64("txid"), invalidate: f.u8("invalidate"), indices: f.u32List("indices"), reuseGrant: f.boolean("reuse_grant"), meta: f.page("meta")}
		},
	},
	"committed": {
		psCodeCommitted,
		[]string{"txid:u64"},
		func(f psFixtureFields) psMessage { return psCommitted{txid: f.u64("txid")} },
	},
	"commit_status_reply": {
		psCodeCommitStatusReply,
		[]string{"status:u8"},
		func(f psFixtureFields) psMessage { return psCommitStatusReply{status: f.u8("status")} },
	},
	"ownership_reply": {
		psCodeOwnershipReply,
		[]string{"status:u8"},
		func(f psFixtureFields) psMessage { return psOwnershipReply{status: f.u8("status")} },
	},
	"notice": {
		psCodeNotice,
		[]string{"kind:u8", "connection_id:u64"},
		func(f psFixtureFields) psMessage {
			return psNotice{kind: f.u8("kind"), connectionID: f.u64("connection_id")}
		},
	},
	"error": {
		psCodeError,
		[]string{"sqlstate:sqlstate", "message:string", "detail:string", "flags:u8"},
		func(f psFixtureFields) psMessage {
			return psError{sqlstate: f.str("sqlstate"), message: f.str("message"), detail: f.str("detail"), flags: f.u8("flags")}
		},
	},
}

// psReservedCodes are the catalog messages with no body yet; the decoder rejects them.
var psReservedCodes = map[string]byte{"snapshot_stream": psCodeSnapshotStream, "subscribe": psCodeSubscribe}

// psFixtureFields reads a fixture row's f_* values in fixtures.toml's text form.
type psFixtureFields struct {
	t        *testing.T
	row      tomlRow
	pageSize int
}

func (f psFixtureFields) str(name string) string { return f.row.str("f_" + name) }

func (f psFixtureFields) uint(name string, bits int) uint64 {
	n, err := strconv.ParseUint(f.str(name), 10, bits)
	if err != nil {
		f.t.Fatalf("f_%s: %v", name, err)
	}
	return n
}

// credential parses "", "passphrase:<text>", or "key:<64 hex digits>".
func (f psFixtureFields) credential(name string) *Credential {
	text := f.str(name)
	if text == "" {
		return nil
	}
	kind, rest, _ := strings.Cut(text, ":")
	switch kind {
	case "passphrase":
		return PassphraseCredential(rest)
	case "key":
		b, err := hex.DecodeString(rest)
		if err != nil || len(b) != 32 {
			f.t.Fatalf("f_%s: not 64 hex digits", name)
		}
		var key [32]byte
		copy(key[:], b)
		return KeyCredential(key)
	}
	f.t.Fatalf("f_%s: credential kind %q", name, kind)
	return nil
}

func (f psFixtureFields) u8(name string) uint8   { return uint8(f.uint(name, 8)) }
func (f psFixtureFields) u16(name string) uint16 { return uint16(f.uint(name, 16)) }
func (f psFixtureFields) u32(name string) uint32 { return uint32(f.uint(name, 32)) }
func (f psFixtureFields) u64(name string) uint64 { return f.uint(name, 64) }

func (f psFixtureFields) boolean(name string) bool {
	switch f.str(name) {
	case "true":
		return true
	case "false":
		return false
	}
	f.t.Fatalf("f_%s: not a bool", name)
	return false
}

func (f psFixtureFields) id(name string) id16 {
	b, err := hex.DecodeString(f.str(name))
	if err != nil || len(b) != 16 {
		f.t.Fatalf("f_%s: not 32 hex digits", name)
	}
	var out id16
	copy(out[:], b)
	return out
}

// pageText parses "<prefix hex>/<fill hex>": the prefix, then the fill byte up to the page size.
func (f psFixtureFields) pageText(text string) []byte {
	prefixHex, fillHex, ok := strings.Cut(text, "/")
	fill, err := hex.DecodeString(fillHex)
	if !ok || err != nil || len(fill) != 1 {
		f.t.Fatalf("page %q: expected <prefix hex>/<fill hex>", text)
	}
	prefix, err := hex.DecodeString(prefixHex)
	if err != nil || len(prefix) > f.pageSize {
		f.t.Fatalf("page %q: bad prefix for page size %d", text, f.pageSize)
	}
	return append(prefix, bytes.Repeat(fill, f.pageSize-len(prefix))...)
}

func (f psFixtureFields) page(name string) []byte { return f.pageText(f.str(name)) }

func (f psFixtureFields) u32List(name string) []uint32 {
	out := []uint32{}
	if f.str(name) == "" {
		return out
	}
	for _, s := range strings.Split(f.str(name), ",") {
		n, err := strconv.ParseUint(s, 10, 32)
		if err != nil {
			f.t.Fatalf("f_%s: %v", name, err)
		}
		out = append(out, uint32(n))
	}
	return out
}

func (f psFixtureFields) pageList(name string) [][]byte {
	out := [][]byte{}
	if f.str(name) == "" {
		return out
	}
	for _, p := range strings.Split(f.str(name), ";") {
		out = append(out, f.pageText(p))
	}
	return out
}

func (f psFixtureFields) indexedPageList(name string) []psIndexedPage {
	out := []psIndexedPage{}
	if f.str(name) == "" {
		return out
	}
	for _, item := range strings.Split(f.str(name), ";") {
		idx, p, ok := strings.Cut(item, "=")
		n, err := strconv.ParseUint(idx, 10, 32)
		if !ok || err != nil {
			f.t.Fatalf("f_%s: bad item %q", name, item)
		}
		out = append(out, psIndexedPage{index: uint32(n), page: f.pageText(p)})
	}
	return out
}

func psFixtureHex(t *testing.T, row tomlRow) []byte {
	b, err := hex.DecodeString(row.str("hex"))
	if err != nil {
		t.Fatalf("%s: bad hex: %v", row.str("name"), err)
	}
	return b
}

// psFieldKeys returns a row's f_* keys without the prefix, so a fixture naming a field the message
// lacks fails.
func psFieldKeys(row tomlRow) []string {
	var out []string
	for k := range row.vals {
		if name, ok := strings.CutPrefix(k, "f_"); ok {
			out = append(out, name)
		}
	}
	return out
}

func psWantProtocolViolation(t *testing.T, what string, err error) {
	t.Helper()
	var ee *EngineError
	if !errors.As(err, &ee) || ee.Code() != "08P01" {
		t.Fatalf("%s: want 08P01, got %v", what, err)
	}
}

// readTomlTopLevel reads the scalar keys before the first table header.
func readTomlTopLevel(t *testing.T, path string) map[string]string {
	t.Helper()
	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("read %s: %v", path, err)
	}
	out := map[string]string{}
	for _, raw := range strings.Split(string(data), "\n") {
		line := strings.TrimSpace(raw)
		if line == "" || strings.HasPrefix(line, "#") {
			continue
		}
		if strings.HasPrefix(line, "[") {
			break
		}
		key, val, ok := strings.Cut(line, "=")
		if ok {
			out[strings.TrimSpace(key)] = unquote(strings.TrimSpace(stripComment(val)))
		}
	}
	return out
}

func TestPageServiceConstantsMatchCatalog(t *testing.T) {
	top := readTomlTopLevel(t, psMessagesTOML)
	ints := map[string]int64{
		"protocol_version":            psProtocolVersion,
		"max_frame_length":            psMaxFrameLength,
		"header_length":               psHeaderLength,
		"max_read_pages_cap":          psMaxReadPagesCap,
		"max_invalidation_entries":    psMaxInvalidationEntries,
		"invalidation_window_commits": psInvalidationWindowCommits,
		"endpoint_max_path_bytes":     psEndpointMaxPathBytes,
		"owner_record_version":        psOwnerRecordVersion,
	}
	for key, want := range ints {
		got, err := strconv.ParseInt(top[key], 10, 64)
		if err != nil || got != want {
			t.Errorf("%s: messages.toml %q, Go %d", key, top[key], want)
		}
	}
	if top["owner_record_magic"] != psOwnerRecordMagic {
		t.Errorf("owner_record_magic: messages.toml %q, Go %q", top["owner_record_magic"], psOwnerRecordMagic)
	}
}

func TestPageServiceCatalogMatchesCodec(t *testing.T) {
	rows := readTomlTables(t, psMessagesTOML, "message")
	seen := map[string]bool{}
	for _, row := range rows {
		name := row.str("name")
		code, err := strconv.ParseUint(row.str("code"), 0, 8)
		if err != nil {
			t.Fatalf("%s: code %q: %v", name, row.str("code"), err)
		}
		seen[name] = true
		if reserved, ok := psReservedCodes[name]; ok {
			if row.str("reply") != "reserved" || byte(code) != reserved {
				t.Errorf("%s: expected a reserved message at 0x%02x", name, reserved)
			}
			continue
		}
		entry, ok := psCatalog[name]
		if !ok {
			t.Errorf("%s: no Go codec", name)
			continue
		}
		if byte(code) != entry.code {
			t.Errorf("%s: messages.toml code 0x%02x, Go 0x%02x", name, code, entry.code)
		}
		if got := row.strs("fields"); !reflect.DeepEqual(append([]string{}, got...), entry.fields) {
			t.Errorf("%s: messages.toml fields %v, Go %v", name, got, entry.fields)
		}
	}
	for name := range psCatalog {
		if !seen[name] {
			t.Errorf("%s: in the Go codec but not messages.toml", name)
		}
	}
}

func TestPageServiceFrameFixtures(t *testing.T) {
	covered := map[string]bool{}
	for _, row := range readTomlTables(t, psFixturesTOML, "frame") {
		name := row.str("name")
		entry, ok := psCatalog[row.str("message")]
		if !ok {
			t.Fatalf("%s: unknown message %q", name, row.str("message"))
		}
		covered[row.str("message")] = true
		pageSize, err := strconv.ParseUint(row.str("page_size"), 10, 32)
		if err != nil {
			t.Fatalf("%s: page_size: %v", name, err)
		}
		for _, k := range psFieldKeys(row) {
			found := false
			for _, f := range entry.fields {
				found = found || strings.HasPrefix(f, k+":")
			}
			if !found {
				t.Errorf("%s: f_%s is not a field of %s", name, k, row.str("message"))
			}
		}
		requestID, err := strconv.ParseUint(row.str("request_id"), 10, 32)
		if err != nil {
			t.Fatalf("%s: request_id: %v", name, err)
		}
		want := entry.build(psFixtureFields{t: t, row: row, pageSize: int(pageSize)})
		if want.psCode() != entry.code {
			t.Errorf("%s: psCode 0x%02x, catalog 0x%02x", name, want.psCode(), entry.code)
		}
		wantBytes := psFixtureHex(t, row)

		got, err := encodePSFrame(uint32(requestID), want)
		if err != nil {
			t.Fatalf("%s: encode: %v", name, err)
		}
		if !bytes.Equal(got, wantBytes) {
			t.Errorf("%s: encoded\n  %x\nwant\n  %x", name, got, wantBytes)
		}

		gotID, gotMsg, err := decodePSFrame(wantBytes, uint32(pageSize))
		if err != nil {
			t.Fatalf("%s: decode: %v", name, err)
		}
		if gotID != uint32(requestID) {
			t.Errorf("%s: decoded request_id %d, want %d", name, gotID, requestID)
		}
		if !reflect.DeepEqual(gotMsg, want) {
			t.Errorf("%s: decoded %#v\nwant %#v", name, gotMsg, want)
		}
	}
	for name := range psCatalog {
		if !covered[name] {
			t.Errorf("%s: no [[frame]] fixture", name)
		}
	}
}

func TestPageServiceMalformedFixtures(t *testing.T) {
	rows := readTomlTables(t, psFixturesTOML, "malformed")
	if len(rows) == 0 {
		t.Fatal("no [[malformed]] fixtures")
	}
	for _, row := range rows {
		pageSize, err := strconv.ParseUint(row.str("page_size"), 10, 32)
		if err != nil {
			t.Fatalf("%s: page_size: %v", row.str("name"), err)
		}
		_, m, err := decodePSFrame(psFixtureHex(t, row), uint32(pageSize))
		if err == nil {
			t.Errorf("%s: decoded %#v without error", row.str("name"), m)
			continue
		}
		psWantProtocolViolation(t, row.str("name"), err)
	}
}

func TestPageServiceOwnerRecordFixtures(t *testing.T) {
	rows := readTomlTables(t, psFixturesTOML, "owner_record")
	if len(rows) == 0 {
		t.Fatal("no [[owner_record]] fixtures")
	}
	for _, row := range rows {
		name := row.str("name")
		f := psFixtureFields{t: t, row: row}
		flags := f.u8("flags")
		if flags&^psOwnerFlagServing != 0 {
			t.Fatalf("%s: fixture sets reserved flag bits", name)
		}
		want := ownerRecord{serving: flags&psOwnerFlagServing != 0, uid: f.u32("uid"), incarnation: f.id("incarnation"), pid: f.u32("pid"), endpoint: f.str("endpoint")}
		wantBytes := psFixtureHex(t, row)
		if got := encodeOwnerRecord(want); !bytes.Equal(got, wantBytes) {
			t.Errorf("%s: encoded\n  %x\nwant\n  %x", name, got, wantBytes)
		}
		got, err := decodeOwnerRecord(wantBytes)
		if err != nil {
			t.Fatalf("%s: decode: %v", name, err)
		}
		if got != want {
			t.Errorf("%s: decoded %#v, want %#v", name, got, want)
		}

		// Damage the record each way the decoder must refuse.
		bad := map[string][]byte{
			"magic":    append([]byte("JEDOWNEX"), wantBytes[8:]...),
			"crc":      append(append([]byte{}, wantBytes[:len(wantBytes)-1]...), wantBytes[len(wantBytes)-1]^1),
			"trailing": append(append([]byte{}, wantBytes...), 0),
			"short":    wantBytes[:len(wantBytes)-1],
		}
		version := append([]byte{}, wantBytes...)
		version[9] = 2
		bad["version"] = version
		// A reserved flag bit with a matching CRC: rejected on the flag, not the checksum.
		reserved := encodeOwnerRecord(want)
		reserved[10] |= 0x02
		body := reserved[:len(reserved)-4]
		bad["reserved_flag"] = encodeOwnerRecordRaw(body)
		for what, b := range bad {
			if _, err := decodeOwnerRecord(b); err == nil {
				t.Errorf("%s: %s: decoded without error", name, what)
			} else {
				psWantProtocolViolation(t, name+" "+what, err)
			}
		}
	}
}

// encodeOwnerRecordRaw appends a fresh CRC-32/IEEE to an owner-record body.
func encodeOwnerRecordRaw(body []byte) []byte {
	out := append([]byte{}, body...)
	sum := crc32.ChecksumIEEE(out)
	return append(out, byte(sum>>24), byte(sum>>16), byte(sum>>8), byte(sum))
}

func TestPageServiceReadFrameStream(t *testing.T) {
	rows := readTomlTables(t, psFixturesTOML, "frame")
	var first, second tomlRow
	for _, row := range rows {
		switch row.str("name") {
		case "hello":
			first = row
		case "end_read":
			second = row
		}
	}
	stream := append(psFixtureHex(t, first), psFixtureHex(t, second)...)
	r := bytes.NewReader(stream)

	id, m, err := readPSFrame(r, 0)
	if err != nil || id != 1 {
		t.Fatalf("first frame: id %d, err %v", id, err)
	}
	if _, ok := m.(psHello); !ok {
		t.Fatalf("first frame: got %#v, want psHello", m)
	}
	id, m, err = readPSFrame(r, 0)
	if err != nil || id != 6 || !reflect.DeepEqual(m, psEndRead{txid: 12}) {
		t.Fatalf("second frame: id %d, %#v, err %v", id, m, err)
	}
	if _, _, err = readPSFrame(r, 0); err != io.EOF {
		t.Fatalf("after the last frame: want io.EOF, got %v", err)
	}

	// A partial frame is an unexpected EOF, in the length and in the body.
	for _, cut := range []int{2, 6} {
		_, _, err = readPSFrame(bytes.NewReader(stream[:cut]), 0)
		if !errors.Is(err, io.ErrUnexpectedEOF) {
			t.Errorf("frame cut at %d bytes: want io.ErrUnexpectedEOF, got %v", cut, err)
		}
	}
}

func TestPageServiceReadFrameRejectsLengthBeforeBody(t *testing.T) {
	for _, row := range readTomlTables(t, psFixturesTOML, "malformed") {
		if row.str("kind") != "length" {
			continue
		}
		b := psFixtureHex(t, row)
		r := bytes.NewReader(b)
		_, _, err := readPSFrame(r, 0)
		psWantProtocolViolation(t, row.str("name"), err)
		if consumed := len(b) - r.Len(); consumed != 4 {
			t.Errorf("%s: read %d bytes, want only the 4-byte length", row.str("name"), consumed)
		}
	}
}

func TestPageServiceEncodeRejectsUnencodable(t *testing.T) {
	page := make([]byte, 256)
	cases := map[string]psMessage{
		"lower-case sqlstate": psError{sqlstate: "08p01"},
		"short sqlstate":      psError{sqlstate: "08P0"},
		"invalid UTF-8":       psHello{clientName: "\xc3\x28"},
		"empty page":          psReadBegun{meta: nil},
		"mixed page sizes":    psPages{pages: [][]byte{page, page[:128]}},
		"meta vs list size":   psCommitBegin{meta: page, manifestPages: []psIndexedPage{{index: 20, page: page[:255]}}},
		"oversized frame":     psPages{pages: [][]byte{make([]byte, psMaxFrameLength)}},
	}
	for what, m := range cases {
		_, err := encodePSFrame(1, m)
		psWantProtocolViolation(t, what, err)
	}
}
