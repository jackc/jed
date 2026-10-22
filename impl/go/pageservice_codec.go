package jed

// The page-service wire codec (spec/design/page-service.md phase 1; the byte contract is
// spec/pageservice/README.md). Hand-written from spec/pageservice/messages.toml and tested against
// spec/pageservice/fixtures.toml; nothing here is generated, and no core reads the TOML at runtime.
//
// Every frame is `length u32 | code u8 | request_id u32 | body`, big-endian. A decoder rejects a
// frame the Ruby reference codec (spec/pageservice/verify.rb) rejects, with SQLSTATE 08P01. Counts
// are checked against the remaining body before anything is allocated, so a forged count cannot
// make the decoder allocate more than the frame holds.

import (
	"encoding/binary"
	"fmt"
	"hash/crc32"
	"io"
	"unicode/utf8"
)

// Framing and protocol constants (messages.toml top-level keys).
const (
	psProtocolVersion           = 1
	psMaxFrameLength            = 16777216 // the most bytes `length` may count
	psHeaderLength              = 9        // length u32 + code u8 + request_id u32
	psMaxReadPagesCap           = 1024     // the cap in hello_ok's max_read_pages formula
	psMaxInvalidationEntries    = 65536    // the longest exact read_begun index list
	psInvalidationWindowCommits = 256      // commits whose written-index sets an owner retains
	psEndpointMaxPathBytes      = 100      // the longest socket path an owner binds
	psOwnerRecordMagic          = "JEDOWNER"
	psOwnerRecordVersion        = 1
)

// Message codes (messages.toml). Requests are 0x01-0x3F, replies 0x40-0x7D, then notice and error.
const (
	psCodeHello             byte = 0x01
	psCodeBeginRead         byte = 0x02
	psCodeReadPages         byte = 0x03
	psCodeEndRead           byte = 0x04
	psCodeBeginWrite        byte = 0x05
	psCodeCommitBegin       byte = 0x06
	psCodeCommitPages       byte = 0x07
	psCodeCommitEnd         byte = 0x08
	psCodeAbort             byte = 0x09
	psCodeCommitStatus      byte = 0x0A
	psCodeRequestOwnership  byte = 0x0B
	psCodeSnapshotStream    byte = 0x0C // reserved (README.md §3.6): decoded as an unknown code
	psCodeSubscribe         byte = 0x0D // reserved (phase 7): decoded as an unknown code
	psCodeHelloOK           byte = 0x41
	psCodeReadBegun         byte = 0x42
	psCodePages             byte = 0x43
	psCodeWriteBegun        byte = 0x45
	psCodeCommitted         byte = 0x48
	psCodeCommitStatusReply byte = 0x4A
	psCodeOwnershipReply    byte = 0x4B
	psCodeNotice            byte = 0x7E
	psCodeError             byte = 0x7F
)

// Flag bits (README.md §3.1, §3.6, §5.1). The flags fields stay raw uint8; these name the bits.
const (
	psHelloFlagReadOnly        uint8 = 1 << 0 // hello: the client asks for a read-only connection
	psHelloOKFlagReadOnly      uint8 = 1 << 0 // hello_ok: the connection is read-only
	psHelloOKFlagOwnerReadOnly uint8 = 1 << 1 // hello_ok: the owner's own handle is read-only
	psHelloOKFlagOwnerYields   uint8 = 1 << 2 // hello_ok: the owner's ownership preference is yield
	psErrorFlagFatal           uint8 = 1 << 0 // error: the owner closes the connection after it
	psOwnerFlagServing         uint8 = 1 << 0 // owner record: the owner serves
)

// read_begun / write_begun `invalidate` values.
const (
	psInvalidateExact uint8 = 0 // the index list is exact
	psInvalidateAll   uint8 = 1 // drop the whole page cache
)

// commit_status_reply `status` values.
const (
	psCommitStatusCommitted    uint8 = 0
	psCommitStatusNotCommitted uint8 = 1
	psCommitStatusUnknown      uint8 = 2
)

// ownership_reply `status` values.
const (
	psOwnershipYielding uint8 = 0
	psOwnershipRefused  uint8 = 1
)

// notice `kind` values.
const (
	psNoticeYieldBegun     uint8 = 1
	psNoticeYieldCancelled uint8 = 2
	psNoticeShutdownBegun  uint8 = 3
)

// id16 is an owner incarnation id: 16 raw bytes.
type id16 [16]byte

// psIndexedPage is one indexed_page_list item: a page index and its page bytes.
type psIndexedPage struct {
	index uint32
	page  []byte
}

// psMessage is one decoded page-service message. A `page` field holds exactly the connection's
// page_size bytes; pages a decoder returns alias the frame buffer it was given.
type psMessage interface {
	psCode() byte
	psEncodeBody(w *psWriter)
}

// --- messages --------------------------------------------------------------------------------------

type psHello struct {
	protocolMin   uint16
	protocolMax   uint16
	formatVersion uint16
	flags         uint8
	clientName    string
	// credential is sent only over the stream transport, for a serving end that must own the
	// database (spec/pageservice/README.md §3.1, §5.3); nil otherwise.
	credential *Credential
}

type psBeginRead struct {
	incarnation id16
	lastTxid    uint64
}

type psReadPages struct {
	txid    uint64
	indices []uint32
}

type psEndRead struct {
	txid uint64
}

type psBeginWrite struct {
	incarnation   id16
	lastTxid      uint64
	lockTimeoutMs uint64
}

type psCommitBegin struct {
	txid          uint64
	meta          []byte
	manifestPages []psIndexedPage
}

type psCommitPages struct {
	pages []psIndexedPage
}

type psCommitEnd struct{}

type psAbort struct{}

type psCommitStatus struct {
	txid   uint64
	digest uint64
}

type psRequestOwnership struct{}

type psHelloOK struct {
	protocol      uint16
	formatVersion uint16
	pageSize      uint32
	connectionID  uint64
	flags         uint8
	maxReadPages  uint32
}

type psReadBegun struct {
	incarnation id16
	txid        uint64
	invalidate  uint8
	indices     []uint32
	meta        []byte
}

type psPages struct {
	pages [][]byte
}

type psWriteBegun struct {
	incarnation id16
	txid        uint64
	invalidate  uint8
	indices     []uint32
	reuseGrant  bool
	meta        []byte
}

type psCommitted struct {
	txid uint64
}

type psCommitStatusReply struct {
	status uint8
}

type psOwnershipReply struct {
	status uint8
}

type psNotice struct {
	kind         uint8
	connectionID uint64
}

type psError struct {
	sqlstate string
	message  string
	detail   string
	flags    uint8
}

func (psHello) psCode() byte             { return psCodeHello }
func (psBeginRead) psCode() byte         { return psCodeBeginRead }
func (psReadPages) psCode() byte         { return psCodeReadPages }
func (psEndRead) psCode() byte           { return psCodeEndRead }
func (psBeginWrite) psCode() byte        { return psCodeBeginWrite }
func (psCommitBegin) psCode() byte       { return psCodeCommitBegin }
func (psCommitPages) psCode() byte       { return psCodeCommitPages }
func (psCommitEnd) psCode() byte         { return psCodeCommitEnd }
func (psAbort) psCode() byte             { return psCodeAbort }
func (psCommitStatus) psCode() byte      { return psCodeCommitStatus }
func (psRequestOwnership) psCode() byte  { return psCodeRequestOwnership }
func (psHelloOK) psCode() byte           { return psCodeHelloOK }
func (psReadBegun) psCode() byte         { return psCodeReadBegun }
func (psPages) psCode() byte             { return psCodePages }
func (psWriteBegun) psCode() byte        { return psCodeWriteBegun }
func (psCommitted) psCode() byte         { return psCodeCommitted }
func (psCommitStatusReply) psCode() byte { return psCodeCommitStatusReply }
func (psOwnershipReply) psCode() byte    { return psCodeOwnershipReply }
func (psNotice) psCode() byte            { return psCodeNotice }
func (psError) psCode() byte             { return psCodeError }

// --- encoding --------------------------------------------------------------------------------------

// psWriter appends a frame body. The first malformed value (a bad sqlstate, invalid UTF-8, or a
// page whose length differs from the frame's other pages) records err; later writes are ignored.
type psWriter struct {
	buf      []byte
	pageSize int // the length of the first page written; 0 until then
	err      error
}

func (w *psWriter) fail(format string, args ...any) {
	if w.err == nil {
		w.err = newError(ProtocolViolation, "page-service encode: "+fmt.Sprintf(format, args...))
	}
}

func (w *psWriter) u8(v uint8)   { w.buf = append(w.buf, v) }
func (w *psWriter) u16(v uint16) { w.buf = binary.BigEndian.AppendUint16(w.buf, v) }
func (w *psWriter) u32(v uint32) { w.buf = binary.BigEndian.AppendUint32(w.buf, v) }
func (w *psWriter) u64(v uint64) { w.buf = binary.BigEndian.AppendUint64(w.buf, v) }

func (w *psWriter) boolean(v bool) {
	if v {
		w.u8(1)
	} else {
		w.u8(0)
	}
}

func (w *psWriter) id(v id16) { w.buf = append(w.buf, v[:]...) }

func (w *psWriter) sqlstate(v string) {
	if !psValidSqlstate(v) {
		w.fail("sqlstate %q is not 5 ASCII digits or upper-case letters", v)
		return
	}
	w.buf = append(w.buf, v...)
}

func (w *psWriter) str(v string) {
	if !utf8.ValidString(v) {
		w.fail("string is not valid UTF-8")
		return
	}
	if uint64(len(v)) > psMaxFrameLength {
		w.fail("string of %d bytes exceeds the frame limit", len(v))
		return
	}
	w.u32(uint32(len(v)))
	w.buf = append(w.buf, v...)
}

// credential encodes the credential type: kind 0 for none, else the kind and its secret.
func (w *psWriter) credential(c *Credential) {
	switch {
	case c == nil:
		w.u8(0)
	case c.kind == credentialPassphrase:
		w.u8(credentialPassphrase)
		w.str(c.passphrase)
	default:
		w.u8(credentialKey)
		w.buf = append(w.buf, c.key[:]...)
	}
}

// page writes one page. Every page in a frame must be the same nonzero length: the connection's
// page_size, which the caller guarantees.
func (w *psWriter) page(p []byte) {
	if len(p) == 0 {
		w.fail("empty page")
		return
	}
	if w.pageSize == 0 {
		w.pageSize = len(p)
	} else if len(p) != w.pageSize {
		w.fail("page of %d bytes in a frame of %d-byte pages", len(p), w.pageSize)
		return
	}
	w.buf = append(w.buf, p...)
}

// count writes a list count, refusing one no frame could hold.
func (w *psWriter) count(n int) {
	if uint64(n) > psMaxFrameLength {
		w.fail("list of %d items exceeds the frame limit", n)
		return
	}
	w.u32(uint32(n))
}

func (w *psWriter) u32List(v []uint32) {
	w.count(len(v))
	for _, x := range v {
		w.u32(x)
	}
}

func (w *psWriter) pageList(v [][]byte) {
	w.count(len(v))
	for _, p := range v {
		w.page(p)
	}
}

func (w *psWriter) indexedPageList(v []psIndexedPage) {
	w.count(len(v))
	for _, ip := range v {
		w.u32(ip.index)
		w.page(ip.page)
	}
}

func (m psHello) psEncodeBody(w *psWriter) {
	w.u16(m.protocolMin)
	w.u16(m.protocolMax)
	w.u16(m.formatVersion)
	w.u8(m.flags)
	w.str(m.clientName)
	w.credential(m.credential)
}

func (m psBeginRead) psEncodeBody(w *psWriter) {
	w.id(m.incarnation)
	w.u64(m.lastTxid)
}

func (m psReadPages) psEncodeBody(w *psWriter) {
	w.u64(m.txid)
	w.u32List(m.indices)
}

func (m psEndRead) psEncodeBody(w *psWriter) { w.u64(m.txid) }

func (m psBeginWrite) psEncodeBody(w *psWriter) {
	w.id(m.incarnation)
	w.u64(m.lastTxid)
	w.u64(m.lockTimeoutMs)
}

func (m psCommitBegin) psEncodeBody(w *psWriter) {
	w.u64(m.txid)
	w.page(m.meta)
	w.indexedPageList(m.manifestPages)
}

func (m psCommitPages) psEncodeBody(w *psWriter) { w.indexedPageList(m.pages) }

func (psCommitEnd) psEncodeBody(*psWriter)        {}
func (psAbort) psEncodeBody(*psWriter)            {}
func (psRequestOwnership) psEncodeBody(*psWriter) {}

func (m psCommitStatus) psEncodeBody(w *psWriter) {
	w.u64(m.txid)
	w.u64(m.digest)
}

func (m psHelloOK) psEncodeBody(w *psWriter) {
	w.u16(m.protocol)
	w.u16(m.formatVersion)
	w.u32(m.pageSize)
	w.u64(m.connectionID)
	w.u8(m.flags)
	w.u32(m.maxReadPages)
}

func (m psReadBegun) psEncodeBody(w *psWriter) {
	w.id(m.incarnation)
	w.u64(m.txid)
	w.u8(m.invalidate)
	w.u32List(m.indices)
	w.page(m.meta)
}

func (m psPages) psEncodeBody(w *psWriter) { w.pageList(m.pages) }

func (m psWriteBegun) psEncodeBody(w *psWriter) {
	w.id(m.incarnation)
	w.u64(m.txid)
	w.u8(m.invalidate)
	w.u32List(m.indices)
	w.boolean(m.reuseGrant)
	w.page(m.meta)
}

func (m psCommitted) psEncodeBody(w *psWriter)         { w.u64(m.txid) }
func (m psCommitStatusReply) psEncodeBody(w *psWriter) { w.u8(m.status) }
func (m psOwnershipReply) psEncodeBody(w *psWriter)    { w.u8(m.status) }

func (m psNotice) psEncodeBody(w *psWriter) {
	w.u8(m.kind)
	w.u64(m.connectionID)
}

func (m psError) psEncodeBody(w *psWriter) {
	w.sqlstate(m.sqlstate)
	w.str(m.message)
	w.str(m.detail)
	w.u8(m.flags)
}

// encodePSFrame encodes one complete frame, length prefix included. It fails (08P01) on a value the
// wire cannot carry: a bad sqlstate, invalid UTF-8, pages of differing or zero length, or a frame
// longer than psMaxFrameLength.
func encodePSFrame(requestID uint32, m psMessage) ([]byte, error) {
	w := &psWriter{buf: make([]byte, psHeaderLength, 64)}
	m.psEncodeBody(w)
	if w.err != nil {
		return nil, w.err
	}
	length := len(w.buf) - 4
	if length > psMaxFrameLength {
		return nil, newError(ProtocolViolation, fmt.Sprintf("page-service encode: frame length %d exceeds %d", length, psMaxFrameLength))
	}
	binary.BigEndian.PutUint32(w.buf[0:4], uint32(length))
	w.buf[4] = m.psCode()
	binary.BigEndian.PutUint32(w.buf[5:9], requestID)
	return w.buf, nil
}

// --- decoding --------------------------------------------------------------------------------------

// psMalformed is the decoder's error: a protocol violation naming the fault.
func psMalformed(format string, args ...any) error {
	return newError(ProtocolViolation, "page-service frame: "+fmt.Sprintf(format, args...))
}

// psReader walks one frame body. The first fault records err and every later read returns zero
// values, so a decode function reads its fields straight through and checks err once.
type psReader struct {
	b        []byte
	pos      int
	pageSize uint32
	err      error
}

func (r *psReader) fail(format string, args ...any) {
	if r.err == nil {
		r.err = psMalformed(format, args...)
	}
}

func (r *psReader) remaining() int { return len(r.b) - r.pos }

func (r *psReader) take(n int) []byte {
	if r.err != nil {
		return nil
	}
	if n > r.remaining() {
		r.fail("truncated: need %d bytes at offset %d, have %d", n, r.pos, r.remaining())
		return nil
	}
	out := r.b[r.pos : r.pos+n : r.pos+n]
	r.pos += n
	return out
}

func (r *psReader) u8() uint8 {
	b := r.take(1)
	if b == nil {
		return 0
	}
	return b[0]
}

func (r *psReader) u16() uint16 {
	b := r.take(2)
	if b == nil {
		return 0
	}
	return binary.BigEndian.Uint16(b)
}

func (r *psReader) u32() uint32 {
	b := r.take(4)
	if b == nil {
		return 0
	}
	return binary.BigEndian.Uint32(b)
}

func (r *psReader) u64() uint64 {
	b := r.take(8)
	if b == nil {
		return 0
	}
	return binary.BigEndian.Uint64(b)
}

func (r *psReader) boolean() bool {
	v := r.u8()
	if r.err == nil && v > 1 {
		r.fail("bool byte %d", v)
	}
	return v == 1
}

func (r *psReader) id() id16 {
	var out id16
	copy(out[:], r.take(16))
	return out
}

func (r *psReader) sqlstate() string {
	b := r.take(5)
	if b == nil {
		return ""
	}
	if !psValidSqlstate(string(b)) {
		r.fail("sqlstate %q", b)
		return ""
	}
	return string(b)
}

func (r *psReader) str() string {
	n := r.u32()
	if r.err != nil {
		return ""
	}
	if uint64(n) > uint64(r.remaining()) {
		r.fail("string length %d exceeds the %d remaining bytes", n, r.remaining())
		return ""
	}
	b := r.take(int(n))
	if !utf8.Valid(b) {
		r.fail("string is not valid UTF-8")
		return ""
	}
	return string(b)
}

// credential decodes the credential type: a kind byte, then a passphrase string or a 32-byte key.
func (r *psReader) credential() *Credential {
	switch kind := r.u8(); {
	case r.err != nil:
		return nil
	case kind == 0:
		return nil
	case kind == credentialPassphrase:
		text := r.str()
		if r.err != nil {
			return nil
		}
		return PassphraseCredential(text)
	case kind == credentialKey:
		b := r.take(32)
		if r.err != nil {
			return nil
		}
		var key [32]byte
		copy(key[:], b)
		return KeyCredential(key)
	default:
		r.fail("credential kind %d names no credential", kind)
		return nil
	}
}

func (r *psReader) page() []byte {
	if r.err != nil {
		return nil
	}
	if r.pageSize == 0 {
		r.fail("page field with no connection page size")
		return nil
	}
	return r.take(int(r.pageSize))
}

// count reads a list count and checks count × itemSize against the remaining body before the
// caller allocates. itemSize 0 means a page list with no connection page size.
func (r *psReader) count(itemSize uint64) int {
	n := r.u32()
	if r.err != nil {
		return 0
	}
	if n > 0 && itemSize == 0 {
		r.fail("page list with no connection page size")
		return 0
	}
	if uint64(n)*itemSize > uint64(r.remaining()) {
		r.fail("count %d of %d-byte items exceeds the %d remaining bytes", n, itemSize, r.remaining())
		return 0
	}
	return int(n)
}

func (r *psReader) u32List() []uint32 {
	n := r.count(4)
	if r.err != nil {
		return nil
	}
	out := make([]uint32, n)
	for i := range out {
		out[i] = r.u32()
	}
	return out
}

func (r *psReader) pageList() [][]byte {
	n := r.count(uint64(r.pageSize))
	if r.err != nil {
		return nil
	}
	out := make([][]byte, n)
	for i := range out {
		out[i] = r.page()
	}
	return out
}

func (r *psReader) indexedPageList() []psIndexedPage {
	itemSize := uint64(0)
	if r.pageSize > 0 {
		itemSize = 4 + uint64(r.pageSize)
	}
	n := r.count(itemSize)
	if r.err != nil {
		return nil
	}
	out := make([]psIndexedPage, n)
	for i := range out {
		out[i].index = r.u32()
		out[i].page = r.page()
	}
	return out
}

// decodePSFrame decodes ONE complete frame, length prefix included, whose `page` fields are
// pageSize bytes (0 before hello_ok: any page field is then malformed). A frame that is too short or
// too long for its length, names an unknown or reserved code, or has a malformed or trailing body
// fails 08P01. Returned pages alias frame.
func decodePSFrame(frame []byte, pageSize uint32) (uint32, psMessage, error) {
	if len(frame) < 4 {
		return 0, nil, psMalformed("truncated: frame shorter than its length field")
	}
	length := binary.BigEndian.Uint32(frame[0:4])
	if err := psCheckLength(length); err != nil {
		return 0, nil, err
	}
	rest := frame[4:]
	if uint64(len(rest)) > uint64(length) {
		return 0, nil, psMalformed("trailing: %d frame bytes beyond length %d", uint64(len(rest))-uint64(length), length)
	}
	if uint64(len(rest)) < uint64(length) {
		return 0, nil, psMalformed("truncated: length %d with %d bytes", length, len(rest))
	}
	code := rest[0]
	requestID := binary.BigEndian.Uint32(rest[1:5])
	r := &psReader{b: rest[5:], pageSize: pageSize}

	var m psMessage
	switch code {
	case psCodeHello:
		m = psHello{protocolMin: r.u16(), protocolMax: r.u16(), formatVersion: r.u16(), flags: r.u8(), clientName: r.str(), credential: r.credential()}
	case psCodeBeginRead:
		m = psBeginRead{incarnation: r.id(), lastTxid: r.u64()}
	case psCodeReadPages:
		m = psReadPages{txid: r.u64(), indices: r.u32List()}
	case psCodeEndRead:
		m = psEndRead{txid: r.u64()}
	case psCodeBeginWrite:
		m = psBeginWrite{incarnation: r.id(), lastTxid: r.u64(), lockTimeoutMs: r.u64()}
	case psCodeCommitBegin:
		m = psCommitBegin{txid: r.u64(), meta: r.page(), manifestPages: r.indexedPageList()}
	case psCodeCommitPages:
		m = psCommitPages{pages: r.indexedPageList()}
	case psCodeCommitEnd:
		m = psCommitEnd{}
	case psCodeAbort:
		m = psAbort{}
	case psCodeCommitStatus:
		m = psCommitStatus{txid: r.u64(), digest: r.u64()}
	case psCodeRequestOwnership:
		m = psRequestOwnership{}
	case psCodeHelloOK:
		m = psHelloOK{protocol: r.u16(), formatVersion: r.u16(), pageSize: r.u32(), connectionID: r.u64(), flags: r.u8(), maxReadPages: r.u32()}
	case psCodeReadBegun:
		m = psReadBegun{incarnation: r.id(), txid: r.u64(), invalidate: r.u8(), indices: r.u32List(), meta: r.page()}
	case psCodePages:
		m = psPages{pages: r.pageList()}
	case psCodeWriteBegun:
		m = psWriteBegun{incarnation: r.id(), txid: r.u64(), invalidate: r.u8(), indices: r.u32List(), reuseGrant: r.boolean(), meta: r.page()}
	case psCodeCommitted:
		m = psCommitted{txid: r.u64()}
	case psCodeCommitStatusReply:
		m = psCommitStatusReply{status: r.u8()}
	case psCodeOwnershipReply:
		m = psOwnershipReply{status: r.u8()}
	case psCodeNotice:
		m = psNotice{kind: r.u8(), connectionID: r.u64()}
	case psCodeError:
		m = psError{sqlstate: r.sqlstate(), message: r.str(), detail: r.str(), flags: r.u8()}
	default:
		// Unknown codes and the reserved snapshot_stream / subscribe codes alike.
		return 0, nil, psMalformed("code 0x%02x names no message", code)
	}
	if r.err != nil {
		return 0, nil, r.err
	}
	if r.remaining() != 0 {
		return 0, nil, psMalformed("trailing: %d bytes after the last field", r.remaining())
	}
	return requestID, m, nil
}

// psCheckLength rejects a frame length outside [5, psMaxFrameLength].
func psCheckLength(length uint32) error {
	if length < psHeaderLength-4 || length > psMaxFrameLength {
		return psMalformed("length %d is outside [%d, %d]", length, psHeaderLength-4, psMaxFrameLength)
	}
	return nil
}

// readPSFrame reads and decodes one frame from a stream. A bad length is rejected (08P01) before any
// body byte is read. A clean EOF before the first byte returns io.EOF unwrapped (the peer closed the
// connection); EOF inside a frame returns io.ErrUnexpectedEOF. Other read errors pass through.
func readPSFrame(r io.Reader, pageSize uint32) (uint32, psMessage, error) {
	var head [4]byte
	if _, err := io.ReadFull(r, head[:]); err != nil {
		return 0, nil, err // io.EOF (nothing read) or io.ErrUnexpectedEOF (part of the length)
	}
	length := binary.BigEndian.Uint32(head[:])
	if err := psCheckLength(length); err != nil {
		return 0, nil, err
	}
	frame := make([]byte, 4+int(length))
	copy(frame, head[:])
	if _, err := io.ReadFull(r, frame[4:]); err != nil {
		if err == io.EOF {
			err = io.ErrUnexpectedEOF
		}
		return 0, nil, err
	}
	return decodePSFrame(frame, pageSize)
}

// psValidSqlstate reports whether s is 5 ASCII digits or upper-case letters.
func psValidSqlstate(s string) bool {
	if len(s) != 5 {
		return false
	}
	for i := 0; i < len(s); i++ {
		c := s[i]
		if !(c >= '0' && c <= '9') && !(c >= 'A' && c <= 'Z') {
			return false
		}
	}
	return true
}

// --- the owner record (README.md §5.1) ---------------------------------------------------------------

// ownerRecord is the `<path>.lock/owner` record: who owns the database and where it serves.
type ownerRecord struct {
	serving     bool   // flags bit 0
	uid         uint32 // the owner's numeric user id
	incarnation id16
	pid         uint32 // diagnostic only; never used for liveness
	endpoint    string // the absolute socket path; empty when not serving
}

// encodeOwnerRecord encodes r: magic, version, flags, uid, incarnation, pid, endpoint, then the
// CRC-32/IEEE of every preceding byte. The endpoint is a filesystem path the owner built, so it is
// written as is.
func encodeOwnerRecord(r ownerRecord) []byte {
	b := make([]byte, 0, 8+2+1+4+16+4+4+len(r.endpoint)+4)
	b = append(b, psOwnerRecordMagic...)
	b = binary.BigEndian.AppendUint16(b, psOwnerRecordVersion)
	flags := uint8(0)
	if r.serving {
		flags |= psOwnerFlagServing
	}
	b = append(b, flags)
	b = binary.BigEndian.AppendUint32(b, r.uid)
	b = append(b, r.incarnation[:]...)
	b = binary.BigEndian.AppendUint32(b, r.pid)
	b = binary.BigEndian.AppendUint32(b, uint32(len(r.endpoint)))
	b = append(b, r.endpoint...)
	return binary.BigEndian.AppendUint32(b, crc32.ChecksumIEEE(b))
}

// decodeOwnerRecord decodes an owner record. A wrong magic or version, a reserved flag bit, a
// malformed endpoint, a bad CRC, or trailing bytes fail 08P01; the caller treats any failure as an
// absent record.
func decodeOwnerRecord(b []byte) (ownerRecord, error) {
	r := &psReader{b: b}
	if string(r.take(8)) != psOwnerRecordMagic {
		return ownerRecord{}, newError(ProtocolViolation, "owner record: bad magic")
	}
	if r.u16() != psOwnerRecordVersion {
		return ownerRecord{}, newError(ProtocolViolation, "owner record: unsupported version")
	}
	flags := r.u8()
	var out ownerRecord
	out.uid = r.u32()
	out.incarnation = r.id()
	out.pid = r.u32()
	out.endpoint = r.str()
	crc := r.u32()
	if r.err != nil {
		return ownerRecord{}, newError(ProtocolViolation, "owner record: "+r.err.(*EngineError).Message)
	}
	if r.remaining() != 0 {
		return ownerRecord{}, newError(ProtocolViolation, "owner record: trailing bytes")
	}
	if crc != crc32.ChecksumIEEE(b[:len(b)-4]) {
		return ownerRecord{}, newError(ProtocolViolation, "owner record: bad CRC")
	}
	if flags&^psOwnerFlagServing != 0 {
		return ownerRecord{}, newError(ProtocolViolation, fmt.Sprintf("owner record: reserved flag bits 0x%02x", flags))
	}
	out.serving = flags&psOwnerFlagServing != 0
	return out, nil
}
