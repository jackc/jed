// Isolated candidate-cost probe. Public keys; no durable codec or production protocol.
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
	"golang.org/x/crypto/chacha20poly1305"
	"os"
	"sort"
	"strconv"
	"time"
)

var sink byte

func must(err error) {
	if err != nil {
		panic(err)
	}
}
func main() {
	samples, err := strconv.Atoi(os.Args[1])
	must(err)
	millis, err := strconv.Atoi(os.Args[2])
	must(err)
	// RFC 5869 test case 1 checks the library invocation, not a custom HKDF.
	ikm := bytes.Repeat([]byte{0x0b}, 22)
	salt, _ := hex.DecodeString("000102030405060708090a0b0c")
	info, _ := hex.DecodeString("f0f1f2f3f4f5f6f7f8f9")
	out, err := hkdf.Key(sha256.New, ikm, salt, string(info), 42)
	must(err)
	if hex.EncodeToString(out) != "3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865" {
		panic("HKDF vector")
	}
	measure := func(label string, size int, op func()) {
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
		sorted := append([]float64{}, times...)
		sort.Float64s(sorted)
		must(json.NewEncoder(os.Stdout).Encode(map[string]any{"backend": "go", "operation": label, "bytes": size, "ns_samples": times, "median_ns": sorted[len(sorted)/2]}))
	}
	master := make([]byte, 32)
	for i := range master {
		master[i] = byte(i)
	}
	epochSalt := make([]byte, 32)
	derive := func() cipher.AEAD {
		rand.Read(epochSalt)
		key, e := hkdf.Key(sha256.New, master, epochSalt, "jed benchmark writer key, not a format", 32)
		must(e)
		block, e := aes.NewCipher(key)
		must(e)
		a, e := cipher.NewGCM(block)
		must(e)
		return a
	}
	var epoch cipher.AEAD
	measure("random32+hkdf+aes-key", 0, func() { epoch = derive(); sink ^= byte(epoch.NonceSize()) })
	for _, size := range []int{8192, 65536} {
		plain := make([]byte, size)
		for i := range plain {
			plain[i] = byte(i*31 + 7)
		}
		aad := make([]byte, 32)
		for i := range aad {
			aad[i] = byte(i + 0xa0)
		}
		nonce := make([]byte, 12)
		buf := make([]byte, 0, size+16)
		for _, rotate := range []uint64{0, 1, 16, 256} {
			epoch = derive()
			var count uint64
			measure("aes-counter-rotate-"+strconv.FormatUint(rotate, 10), size, func() {
				if rotate > 0 && count%rotate == 0 {
					epoch = derive()
				}
				count++
				binary.BigEndian.PutUint64(nonce[4:], count)
				buf = epoch.Seal(buf[:0], nonce, plain, aad)
				sink ^= buf[0]
			})
			recovered, e := epoch.Open(nil, nonce, buf, aad)
			must(e)
			if !bytes.Equal(recovered, plain) {
				panic("AES round trip")
			}
		}
		x, e := chacha20poly1305.NewX(master)
		must(e)
		n := make([]byte, x.NonceSize())
		binary.BigEndian.PutUint32(n, uint32(size))
		// Emit a fixed ciphertext for byte-for-byte comparison with browser/Rust fixtures.
		fixed := x.Seal(nil, n, plain, aad)
		must(json.NewEncoder(os.Stdout).Encode(map[string]any{"kind": "fixture", "algorithm": "xchacha20-poly1305", "bytes": size, "hex": hex.EncodeToString(fixed)}))
		measure("xchacha-random24-seal", size, func() { rand.Read(n); buf = x.Seal(buf[:0], n, plain, aad); sink ^= buf[0] })
		recovered, e := x.Open(nil, n, buf, aad)
		must(e)
		if !bytes.Equal(recovered, plain) {
			panic("XChaCha round trip")
		}
		sealed := bytes.Clone(buf)
		measure("xchacha-open", size, func() { out, e := x.Open(buf[:0], n, sealed, aad); must(e); sink ^= out[0] })
	}
}
