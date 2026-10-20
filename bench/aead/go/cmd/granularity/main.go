// Key-granularity probe for encryption.md §3.1: per-page versus per-commit-attempt AES-256-GCM
// keys derived with HKDF-SHA-256 from random salts. The data key, purpose label and fixture salt
// are public benchmark inputs; nothing here is an encrypted page format or a production protocol.
package main

import (
	"bytes"
	"crypto/aes"
	"crypto/cipher"
	"crypto/hkdf"
	"crypto/rand"
	"crypto/sha256"
	"encoding/binary"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"os"
	"strconv"
	"time"
)

const (
	label    = "jed bench page key v0" // versioned purpose label (benchmark only)
	saltLen  = 32
	working  = 1024 // distinct pages in every seal/open working set
	coldCap  = 2    // cache bound for commit-ordered scans: below every commit count, so each commit misses
	hitCap   = 1024 // cache bound for the all-hit case: holds every commit's key
	hitPages = 16   // commit size of the all-hit case
)

var (
	sink    byte
	dataKey = seq(0x00, 32)
	sizes   = []int{4096, 8192, 16384}
	commits = []int{1, 4, 16, 256}
)

func must(err error) {
	if err != nil {
		panic(err)
	}
}

func seq(start byte, n int) []byte {
	out := make([]byte, n)
	for i := range out {
		out[i] = start + byte(i)
	}
	return out
}

func unhex(s string) []byte {
	out, err := hex.DecodeString(s)
	must(err)
	return out
}

func emit(row map[string]any) { must(json.NewEncoder(os.Stdout).Encode(row)) }

// layout derives a 32-byte AES key from a page or commit salt.
type layout struct {
	name   string
	derive func(salt []byte) []byte
}

func layouts() []layout {
	// One extract of the data key at unlock (HKDF-Extract with an empty salt), then a per-key
	// expand with the salt appended to the purpose label in the info.
	prk, err := hkdf.Extract(sha256.New, dataKey, nil)
	must(err)
	return []layout{
		{"extract-per-key", func(salt []byte) []byte {
			key, err := hkdf.Key(sha256.New, dataKey, salt, label, 32)
			must(err)
			return key
		}},
		{"expand-per-key", func(salt []byte) []byte {
			info := make([]byte, 0, len(label)+len(salt))
			info = append(append(info, label...), salt...)
			key, err := hkdf.Expand(sha256.New, prk, string(info), 32)
			must(err)
			return key
		}},
	}
}

func newAEAD(key []byte) cipher.AEAD {
	block, err := aes.NewCipher(key)
	must(err)
	a, err := cipher.NewGCM(block)
	must(err)
	return a
}

// Per-page keys seal one message each, so the nonce is fixed. Per-commit keys use the page's
// ordinal within its commit attempt: 8 zero bytes then a big-endian u32.
func ordinalNonce(nonce []byte, ordinal uint32) []byte {
	clear(nonce)
	binary.BigEndian.PutUint32(nonce[8:], ordinal)
	return nonce
}

// keyCache is a bounded FIFO of derived ciphers keyed by salt.
type keyCache struct {
	limit           int
	entries         map[[saltLen]byte]cipher.AEAD
	ring            [][saltLen]byte
	next            int
	lookups, misses uint64
	derive          func([]byte) []byte
}

func newCache(limit int, derive func([]byte) []byte) *keyCache {
	return &keyCache{limit: limit, entries: make(map[[saltLen]byte]cipher.AEAD, limit), derive: derive}
}

func (c *keyCache) get(salt *[saltLen]byte) cipher.AEAD {
	c.lookups++
	if a, ok := c.entries[*salt]; ok {
		return a
	}
	c.misses++
	a := newAEAD(c.derive(salt[:]))
	if len(c.ring) < c.limit {
		c.ring = append(c.ring, *salt)
	} else {
		delete(c.entries, c.ring[c.next])
		c.ring[c.next] = *salt
		c.next = (c.next + 1) % c.limit
	}
	c.entries[*salt] = a
	return a
}

func measure(samples, millis int, op func()) []float64 {
	times := []float64{}
	for sample := 0; sample <= samples; sample++ {
		start := time.Now()
		count := 0
		for {
			for i := 0; i < 64; i++ {
				op()
			}
			count += 64
			if time.Since(start) >= time.Duration(millis)*time.Millisecond {
				break
			}
		}
		if sample > 0 {
			times = append(times, float64(time.Since(start).Nanoseconds())/float64(count))
		}
	}
	return times
}

type pageSet struct {
	size  int
	plain [][]byte
	aad   [][]byte
}

func newPageSet(size int) pageSet {
	s := pageSet{size: size}
	for p := 0; p < working; p++ {
		plain := make([]byte, size)
		for i := range plain {
			plain[i] = byte(31*i + 7 + p)
		}
		aad := seq(0xa0, 32)
		binary.BigEndian.PutUint64(aad, uint64(p)) // stands in for the page identity binding
		s.plain = append(s.plain, plain)
		s.aad = append(s.aad, aad)
	}
	return s
}

func checkHKDF() {
	// RFC 5869 test cases 1 and 3 check the library invocation, not a custom HKDF.
	ikm := bytes.Repeat([]byte{0x0b}, 22)
	for _, tc := range []struct{ salt, info, prk, okm string }{
		{"000102030405060708090a0b0c", "f0f1f2f3f4f5f6f7f8f9",
			"077709362c2e32df0ddc3f0dc47bba6390b6c73bb50f9c3122ec844ad7c2b3e5",
			"3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865"},
		{"", "",
			"19ef24a32c717b167f33a91d6f648bdf96596776afdb6377ac434c1c293ccb04",
			"8da4e775a563c18f715f802a063c5a31b8a11f5c5ee1879ec3454e5f3c738d2d9d201395faa4b61a96c8"},
	} {
		prk, err := hkdf.Extract(sha256.New, ikm, unhex(tc.salt))
		must(err)
		okm, err := hkdf.Expand(sha256.New, prk, string(unhex(tc.info)), 42)
		must(err)
		key, err := hkdf.Key(sha256.New, ikm, unhex(tc.salt), string(unhex(tc.info)), 42)
		must(err)
		if hex.EncodeToString(prk) != tc.prk || hex.EncodeToString(okm) != tc.okm || !bytes.Equal(key, okm) {
			panic("HKDF RFC 5869 vector")
		}
	}
}

// validate checks round trips and rejection of every tampered input, and emits fixed-salt
// fixtures for cross-backend comparison. It runs before any timing.
func validate(ls []layout) {
	checkHKDF()
	salt := seq(0x40, saltLen)
	keys := map[string][]byte{}
	for _, l := range ls {
		keys[l.name] = l.derive(salt)
		emit(map[string]any{"kind": "fixture", "fixture": "key", "layout": l.name, "bytes": 0, "hex": hex.EncodeToString(keys[l.name])})
	}
	if bytes.Equal(keys["extract-per-key"], keys["expand-per-key"]) {
		panic("layouts derive the same key")
	}
	otherSalt := bytes.Clone(salt)
	otherSalt[saltLen-1] ^= 1
	for _, size := range sizes {
		set := newPageSet(size)
		for _, l := range ls {
			a := newAEAD(keys[l.name])
			for _, scheme := range []string{"per-page", "per-commit"} {
				// Page 1 at commit ordinal 1 under per-commit keys; ordinal is ignored per page.
				ordinal := uint32(0)
				if scheme == "per-commit" {
					ordinal = 1
				}
				nonce := ordinalNonce(make([]byte, 12), ordinal)
				sealed := a.Seal(nil, nonce, set.plain[1], set.aad[1])
				emit(map[string]any{"kind": "fixture", "fixture": scheme, "layout": l.name, "bytes": size, "hex": hex.EncodeToString(sealed)})
				out, err := a.Open(nil, nonce, sealed, set.aad[1])
				must(err)
				if !bytes.Equal(out, set.plain[1]) {
					panic("round trip")
				}
				reject := func(what string, aead cipher.AEAD, n, ct, aad []byte) {
					if _, err := aead.Open(nil, n, ct, aad); err == nil {
						panic("accepted tampered " + what)
					}
				}
				for _, index := range []int{0, size - 1, size, size + 15} { // ciphertext and tag bytes
					bad := bytes.Clone(sealed)
					bad[index] ^= 1
					reject("ciphertext/tag", a, nonce, bad, set.aad[1])
				}
				badAAD := bytes.Clone(set.aad[1])
				badAAD[7] ^= 1
				reject("aad", a, nonce, sealed, badAAD)
				reject("salt", newAEAD(l.derive(otherSalt)), nonce, sealed, set.aad[1])
				for _, other := range ls {
					if other.name != l.name {
						reject("layout", newAEAD(keys[other.name]), nonce, sealed, set.aad[1])
					}
				}
				reject("nonce", a, ordinalNonce(make([]byte, 12), ordinal+1), sealed, set.aad[1])
			}
		}
	}
}

func timing(row map[string]any, times []float64) {
	row["kind"] = "timing"
	row["backend"] = "go"
	row["ns_samples"] = times
	emit(row)
}

func breakdown(samples, millis int, ls []layout) {
	salt := make([]byte, saltLen)
	key := ls[0].derive(seq(0x40, saltLen))
	cases := []struct {
		name string
		op   func()
	}{
		{"salt-random32", func() { rand.Read(salt); sink ^= salt[0] }},
		{"aes-gcm-key-setup", func() { a := newAEAD(key); sink ^= byte(a.Overhead()) }},
	}
	for _, l := range ls {
		cases = append(cases,
			struct {
				name string
				op   func()
			}{"hkdf-" + l.name, func() { sink ^= l.derive(salt)[0] }},
			struct {
				name string
				op   func()
			}{"derive-" + l.name, func() { rand.Read(salt); a := newAEAD(l.derive(salt)); sink ^= byte(a.Overhead()) }},
		)
	}
	for _, c := range cases {
		timing(map[string]any{"scheme": "key-setup", "operation": c.name, "bytes": 0}, measure(samples, millis, c.op))
	}
}

func sealCases(samples, millis int, set pageSet, ls []layout) {
	size := set.size
	buf := make([]byte, size, size+16)
	nonce := make([]byte, 12)
	var lastAEAD cipher.AEAD
	lastPage := 0
	seal := func(a cipher.AEAD, p int, n []byte) {
		copy(buf[:size], set.plain[p])
		out := a.Seal(buf[:0], n, buf[:size], set.aad[p])
		sink ^= out[size]
		lastAEAD, lastPage = a, p
	}
	// The reusable output holds each case's final sealed page; check that it opens.
	check := func(row map[string]any, times []float64) {
		out, err := lastAEAD.Open(nil, nonce, buf[:size+16], set.aad[lastPage])
		must(err)
		if !bytes.Equal(out, set.plain[lastPage]) {
			panic("seal output")
		}
		timing(row, times)
	}
	// Reference: one long-lived key, unique counter nonces, no derivation.
	fixed := newAEAD(ls[0].derive(seq(0x40, saltLen)))
	var counter uint64
	times := measure(samples, millis, func() {
		counter++
		clear(nonce)
		binary.BigEndian.PutUint64(nonce[4:], counter)
		seal(fixed, int(counter%working), nonce)
	})
	check(map[string]any{"scheme": "fixed-key", "operation": "seal", "bytes": size}, times)
	salt := make([]byte, saltLen)
	for _, l := range ls {
		p := 0
		times := measure(samples, millis, func() {
			rand.Read(salt)
			seal(newAEAD(l.derive(salt)), p, ordinalNonce(nonce, 0))
			p = (p + 1) % working
		})
		check(map[string]any{"scheme": "per-page", "layout": l.name, "operation": "seal", "bytes": size}, times)
		for _, n := range commits {
			var a cipher.AEAD
			var count, derivations uint64
			times := measure(samples, millis, func() {
				ordinal := uint32(count % uint64(n))
				if ordinal == 0 {
					rand.Read(salt)
					a = newAEAD(l.derive(salt))
					derivations++
				}
				seal(a, int(count%working), ordinalNonce(nonce, ordinal))
				count++
			})
			if derivations != (count+uint64(n)-1)/uint64(n) {
				panic("per-commit derivation count")
			}
			check(map[string]any{"scheme": "per-commit", "layout": l.name, "commit_pages": n, "operation": "seal", "bytes": size}, times)
		}
	}
}

func openCases(samples, millis int, set pageSet, ls []layout) {
	size := set.size
	buf := make([]byte, size+16)
	nonce := make([]byte, 12)
	open := func(a cipher.AEAD, p int, n, sealed []byte) {
		copy(buf, sealed)
		out, err := a.Open(buf[:0], n, buf, set.aad[p])
		must(err)
		sink ^= out[0]
	}
	sealed := make([][]byte, working)
	// Reference: one long-lived key; page p sealed at nonce p.
	fixed := newAEAD(ls[0].derive(seq(0x40, saltLen)))
	for p := range sealed {
		sealed[p] = fixed.Seal(nil, ordinalNonce(nonce, uint32(p)), set.plain[p], set.aad[p])
	}
	p := 0
	times := measure(samples, millis, func() {
		open(fixed, p, ordinalNonce(nonce, uint32(p)), sealed[p])
		p = (p + 1) % working
	})
	timing(map[string]any{"scheme": "fixed-key", "operation": "open", "bytes": size}, times)
	salts := make([][saltLen]byte, working)
	for _, l := range ls {
		// Per page: every page has its own salt and every open derives its key.
		for p := range sealed {
			_, _ = rand.Read(salts[p][:])
			sealed[p] = newAEAD(l.derive(salts[p][:])).Seal(nil, ordinalNonce(nonce, 0), set.plain[p], set.aad[p])
		}
		p := 0
		times := measure(samples, millis, func() {
			open(newAEAD(l.derive(salts[p][:])), p, ordinalNonce(nonce, 0), sealed[p])
			p = (p + 1) % working
		})
		timing(map[string]any{"scheme": "per-page", "layout": l.name, "operation": "open", "bytes": size}, times)
		// Per commit: pages grouped into working/n commits, read in commit order through a
		// cache too small to keep earlier commits, so each commit's first page misses.
		for _, n := range commits {
			for c := 0; c < working/n; c++ {
				_, _ = rand.Read(salts[c][:])
				a := newAEAD(l.derive(salts[c][:]))
				for o := 0; o < n; o++ {
					p := c*n + o
					sealed[p] = a.Seal(nil, ordinalNonce(nonce, uint32(o)), set.plain[p], set.aad[p])
				}
			}
			cache := newCache(coldCap, l.derive)
			p := 0
			times := measure(samples, millis, func() {
				open(cache.get(&salts[p/n]), p, ordinalNonce(nonce, uint32(p%n)), sealed[p])
				p = (p + 1) % working
			})
			if cache.misses != (cache.lookups+uint64(n)-1)/uint64(n) {
				panic(fmt.Sprintf("cache misses %d of %d at %d pages per commit", cache.misses, cache.lookups, n))
			}
			timing(map[string]any{"scheme": "per-commit", "layout": l.name, "commit_pages": n, "cache": "miss-per-commit", "operation": "open", "bytes": size}, times)
		}
	}
	// All hits: every commit's key already cached; layouts do not matter.
	n := hitPages
	cache := newCache(hitCap, ls[0].derive)
	for c := 0; c < working/n; c++ {
		_, _ = rand.Read(salts[c][:])
		a := cache.get(&salts[c])
		for o := 0; o < n; o++ {
			p := c*n + o
			sealed[p] = a.Seal(nil, ordinalNonce(nonce, uint32(o)), set.plain[p], set.aad[p])
		}
	}
	warm := cache.misses
	p = 0
	times = measure(samples, millis, func() {
		open(cache.get(&salts[p/n]), p, ordinalNonce(nonce, uint32(p%n)), sealed[p])
		p = (p + 1) % working
	})
	if cache.misses != warm {
		panic("all-hit cache missed")
	}
	timing(map[string]any{"scheme": "per-commit", "commit_pages": n, "cache": "all-hit", "operation": "open", "bytes": size}, times)
	if !bytes.Equal(buf[:size], set.plain[(p+working-1)%working]) {
		panic("open output")
	}
}

func main() {
	samples, err := strconv.Atoi(os.Args[1])
	must(err)
	millis, err := strconv.Atoi(os.Args[2])
	must(err)
	ls := layouts()
	validate(ls)
	if samples == 0 {
		return
	}
	breakdown(samples, millis, ls)
	for _, size := range sizes {
		set := newPageSet(size)
		sealCases(samples, millis, set, ls)
		openCases(samples, millis, set, ls)
	}
}
