package jed

// GIN `jsonb_ops` operator class (spec/design/gin.md §2.1/§4.1): PostgreSQL's default jsonb GIN
// opclass term-for-term (gin_extract_jsonb / gin_extract_jsonb_query), with jed's own byte form per
// term. The inverted core (entry tree, gather, ALL/ANY combine, residual recheck, cost) is shared with
// array_ops; only term extraction lives here.

import (
	"bytes"
	"slices"
)

// The jsonb_ops term flag bytes (PostgreSQL's JGINFLAG_KEY / NULL / BOOL / NUM / STR) and the
// truncation bit (JGINFLAG_HASHED's role): a key / number / string body source longer than
// ginMaxTermBytes keeps only its first ginMaxTermBytes bytes and sets the bit.
const (
	ginJsonbFlagKey       byte = 0x01
	ginJsonbFlagNull      byte = 0x02
	ginJsonbFlagBool      byte = 0x03
	ginJsonbFlagNum       byte = 0x04
	ginJsonbFlagStr       byte = 0x05
	ginJsonbFlagTruncated byte = 0x10
)

// ginJsonbFramed is a key / number / string term: flag ‖ text-terminated-escape (encoding.md §2.4)
// of the first min(n, 128) bytes of content, the flag's 0x10 bit set when content was cut (at a byte,
// never a character boundary — a term is bytes, gin.md §4.1).
func ginJsonbFramed(flag byte, content []byte) []byte {
	if len(content) > ginMaxTermBytes {
		flag |= ginJsonbFlagTruncated
		content = content[:ginMaxTermBytes]
	}
	return append([]byte{flag}, encodeTerminated(content)...)
}

// ginJsonbKeyTerm is the KEY term of an object key, a string array element, a top-level string, or a
// `?` / `?|` / `?&` query key.
func ginJsonbKeyTerm(key string) []byte { return ginJsonbFramed(ginJsonbFlagKey, []byte(key)) }

// ginJsonbScalarTerm is the term of a scalar node; asKey marks a string that is an array element (or
// the top-level scalar), which PostgreSQL indexes as a KEY rather than a string value.
func ginJsonbScalarTerm(n *JsonNode, asKey bool) []byte {
	switch n.Kind {
	case JNull:
		return []byte{ginJsonbFlagNull}
	case JBool:
		if n.B {
			return []byte{ginJsonbFlagBool, 0x01}
		}
		return []byte{ginJsonbFlagBool, 0x00}
	case JNumber:
		// The value's decimal-order-preserving key (encoding.md §2.5): 1.5 and 1.50 share a term.
		return ginJsonbFramed(ginJsonbFlagNum, n.Num.EncodeKey())
	case JString:
		if asKey {
			return ginJsonbKeyTerm(n.S)
		}
		return ginJsonbFramed(ginJsonbFlagStr, []byte(n.S))
	default:
		panic("ginJsonbScalarTerm: a container is not a scalar")
	}
}

// ginJsonbWalk appends the terms of a container's contents, at every nesting level: a KEY term per
// object key, a KEY term per string array element, and a value term per other scalar. Containers
// themselves produce no term.
func ginJsonbWalk(n *JsonNode, out [][]byte) [][]byte {
	switch n.Kind {
	case JObject:
		for i := range n.Obj {
			out = append(out, ginJsonbKeyTerm(n.Obj[i].Key))
			v := &n.Obj[i].Val
			if jsonIsContainer(v) {
				out = ginJsonbWalk(v, out)
			} else {
				out = append(out, ginJsonbScalarTerm(v, false))
			}
		}
	case JArray:
		for i := range n.Arr {
			e := &n.Arr[i]
			if jsonIsContainer(e) {
				out = ginJsonbWalk(e, out)
			} else {
				out = append(out, ginJsonbScalarTerm(e, e.Kind == JString))
			}
		}
	}
	return out
}

// ginJsonbIndexTerms is extract_index_terms(doc) for jsonb_ops (gin.md §2.1): the document's
// DISTINCT terms, in ascending byte order. A top-level scalar is PostgreSQL's one-element raw-scalar
// array (so a top-level string is a KEY term). `{}`, `[]`, and a document of only empty containers
// yield none. Also the `@>` query terms of a containment query Q (mode ALL).
func ginJsonbIndexTerms(doc *JsonNode) [][]byte {
	var terms [][]byte
	if jsonIsContainer(doc) {
		terms = ginJsonbWalk(doc, nil)
	} else {
		terms = [][]byte{ginJsonbScalarTerm(doc, doc.Kind == JString)}
	}
	slices.SortFunc(terms, bytes.Compare)
	return slices.CompactFunc(terms, bytes.Equal)
}
