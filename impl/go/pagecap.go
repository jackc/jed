package jed

// A database's page capacities, bound page pointers, and the one place a body page's stored bytes are
// built (format.md *Page model*; the encrypted variant in spec/fileformat/encrypted.md §1–§4).

import "encoding/binary"

// pageCap is a database's page capacities — what the size rules read. c is the payload capacity C
// every split, record-cap and fit rule uses; tag is the bytes a bound pointer adds to a page index: 0
// in a plaintext file, 16 in an encrypted one, whose C also excludes the 40-byte reserve. Every
// pointer width and chain capacity derives from these.
type pageCap struct {
	c   int
	tag int
}

// plainCap is a plaintext file's (or an in-memory database's) capacities at pageSize.
func plainCap(pageSize uint32) pageCap { return pageCap{c: pagePayload(pageSize)} }

// encryptedCap is an encrypted file's capacities at pageSize (encrypted.md §1).
func encryptedCap(pageSize uint32) pageCap {
	return pageCap{c: pagePayload(pageSize) - cryptoReserve, tag: cryptoTagLen}
}

func capOf(pageSize uint32, encrypted bool) pageCap {
	if encrypted {
		return encryptedCap(pageSize)
	}
	return plainCap(pageSize)
}

func (c pageCap) encrypted() bool { return c.tag != 0 }

// ptr is the bytes one page pointer takes: a u32 index, plus its tag when bound.
func (c pageCap) ptr() int { return 4 + c.tag }

// chain is the content one chain page (catalog, overflow, free-list) holds after its successor's tag.
func (c pageCap) chain() int { return c.c - c.tag }

// firstBody is the first body page: 2, or 4 past an encrypted file's key block.
func (c pageCap) firstBody() uint32 {
	if c.encrypted() {
		return firstBodyEncrypted
	}
	return rootPage
}

// pageRef is a bound page pointer (encrypted.md §2): a page index and the tag of the stored bytes it
// names — noTag in a plaintext file, and in a pointer to no page (page == 0).
type pageRef struct {
	page uint32
	tag  pageTag
}

// plainRef is an unbound reference (a plaintext file's pointers carry no tag).
func plainRef(page uint32) pageRef { return pageRef{page: page} }

// appendTo appends this pointer in c's layout: the u32 index, then the tag when bound.
func (r pageRef) appendTo(out []byte, c pageCap) []byte {
	out = binary.BigEndian.AppendUint32(out, r.page)
	return append(out, r.tag[:c.tag]...)
}

// readPageRef reads a pointer in c's layout at *pos.
func readPageRef(buf []byte, pos *int, c pageCap) (pageRef, error) {
	page, err := readU32(buf, pos)
	if err != nil {
		return pageRef{}, err
	}
	return readPointerTag(buf, pos, page, c)
}

// readPointerTag reads the tag that ends a pointer in c's layout, after its u32 fields: a bound
// reference to page.
func readPointerTag(buf []byte, pos *int, page uint32, c pageCap) (pageRef, error) {
	raw, err := take(buf, pos, c.tag)
	if err != nil {
		return pageRef{}, err
	}
	r := pageRef{page: page}
	copy(r.tag[:], raw)
	if page == 0 && r.tag != noTag {
		return pageRef{}, newError(DataCorrupted, "a pointer to no page carries a tag")
	}
	return r, nil
}

// sealer builds a body page's stored bytes — the one place they are made (format.md *Page header*;
// encrypted.md §4). A plaintext page is the header + payload with its CRC; an encrypted page is the
// same plaintext over capacity C, with a zero CRC field and, on a chain page, its successor's tag
// leading the payload, sealed under the database's page key.
type sealer struct {
	ps    int
	cap   pageCap
	codec *pageCodec
}

func plainSealer(ps int) sealer { return sealer{ps: ps, cap: plainCap(uint32(ps))} }

// sealerOf is the sealer for a database whose paging context is paging (nil: an in-memory or temp
// store, always plaintext).
func sealerOf(ps int, paging *sharedPaging) sealer {
	var codec *pageCodec
	if paging != nil {
		codec = paging.codec()
	}
	return sealer{ps: ps, cap: capOf(uint32(ps), codec != nil), codec: codec}
}

// page builds a body page and returns its bytes and the tag a pointer to it binds. next is the
// successor of a chain page (catalog, overflow, free-list), else the zero pageRef.
func (s sealer) page(index uint32, pageType, flags byte, itemCount uint32, next pageRef, payload []byte) ([]byte, pageTag, error) {
	if s.codec == nil {
		p := makePage(s.ps, pageType, itemCount, next.page, payload)
		if flags != 0 {
			p[1] = flags
			binary.BigEndian.PutUint32(p[12:], pageCRC(p))
		}
		return p, noTag, nil
	}
	chain := pageType == pageCatalog || pageType == pageOverflow || pageType == pageFreelist
	lead := 0
	if chain {
		lead = s.cap.tag
	}
	if lead+len(payload) > s.cap.c {
		return nil, noTag, newError(DataCorrupted, "page payload overflows its page")
	}
	p := make([]byte, s.ps)
	p[0] = pageType
	p[1] = flags
	binary.BigEndian.PutUint32(p[4:], itemCount)
	binary.BigEndian.PutUint32(p[8:], next.page)
	if chain {
		copy(p[pageHeader:], next.tag[:])
	}
	copy(p[pageHeader+lead:], payload)
	tag, err := s.codec.sealBody(index, p)
	if err != nil {
		return nil, noTag, err
	}
	return p, tag, nil
}

// chainContent is the content of a parsed chain page (catalog, overflow, free-list) in c's layout: its
// successor pointer and the bytes after the successor's tag (encrypted.md §2).
func chainContent(pg page, c pageCap) (pageRef, []byte, error) {
	if len(pg.payload) < c.tag {
		return pageRef{}, nil, newError(DataCorrupted, "chain page too short")
	}
	next := pageRef{page: pg.nextPage}
	copy(next.tag[:], pg.payload[:c.tag])
	if pg.nextPage == 0 && next.tag != noTag {
		return pageRef{}, nil, newError(DataCorrupted, "the last chain page carries a successor tag")
	}
	return next, pg.payload[c.tag:], nil
}
