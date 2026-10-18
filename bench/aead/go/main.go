// Public fixed inputs for measurement only, never a production nonce scheme.
package main

import (
	"bytes"
	"crypto/aes"
	"crypto/cipher"
	"encoding/binary"
	"encoding/hex"
	"encoding/json"
	"os"
	"strconv"
	"time"

	"golang.org/x/crypto/chacha20poly1305"
)

var sink byte

func check(err error) {
	if err != nil {
		panic(err)
	}
}
func emit(v any) { check(json.NewEncoder(os.Stdout).Encode(v)) }
func measure(op func(), samples, millis int) []float64 {
	values := make([]float64, 0, samples)
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
		ns := float64(time.Since(start).Nanoseconds()) / float64(count)
		if sample > 0 {
			values = append(values, ns)
		}
	}
	return values
}
func run(algorithm string, makeCipher func([]byte) (cipher.AEAD, error), samples, millis int) {
	key := make([]byte, 32)
	wrong := make([]byte, 32)
	aad := make([]byte, 32)
	for i := range key {
		key[i] = byte(i)
		wrong[i] = byte(i + 1)
		aad[i] = byte(0xa0 + i)
	}
	c, err := makeCipher(key)
	check(err)
	wrongCipher, err := makeCipher(wrong)
	check(err)
	for _, size := range []int{4096, 8192, 16384, 65536} {
		plain := make([]byte, size)
		for i := range plain {
			plain[i] = byte(i*31 + 7)
		}
		nonce := make([]byte, 12)
		binary.BigEndian.PutUint32(nonce, uint32(size))
		sealed := c.Seal(nil, nonce, plain, aad)
		out, err := c.Open(nil, nonce, sealed, aad)
		check(err)
		if !bytes.Equal(out, plain) {
			panic("round trip")
		}
		for _, idx := range []int{0, len(sealed) - 1} {
			bad := bytes.Clone(sealed)
			bad[idx] ^= 1
			if _, err = c.Open(nil, nonce, bad, aad); err == nil {
				panic("accepted corruption")
			}
		}
		badAAD := bytes.Clone(aad)
		badAAD[0] ^= 1
		if _, err = c.Open(nil, nonce, sealed, badAAD); err == nil {
			panic("accepted AAD")
		}
		if _, err = wrongCipher.Open(nil, nonce, sealed, aad); err == nil {
			panic("accepted key")
		}
		emit(map[string]any{"kind": "fixture", "backend": "go", "algorithm": algorithm, "bytes": size, "hex": hex.EncodeToString(sealed)})
		if samples == 0 {
			continue
		}
		counter := uint64(0)
		for _, mode := range []string{"allocating", "reused-in-place"} {
			for _, operation := range []string{"seal", "open"} {
				buffer := make([]byte, size+16)
				var result []byte
				times := measure(func() {
					if operation == "seal" {
						counter++
						binary.BigEndian.PutUint64(nonce[4:], counter)
						if mode == "allocating" {
							result = c.Seal(nil, nonce, plain, aad)
						} else {
							copy(buffer, plain)
							result = c.Seal(buffer[:0], nonce, buffer[:size], aad)
						}
					} else {
						binary.BigEndian.PutUint64(nonce[4:], 0)
						if mode == "allocating" {
							result, err = c.Open(nil, nonce, sealed, aad)
						} else {
							copy(buffer, sealed)
							result, err = c.Open(buffer[:0], nonce, buffer, aad)
						}
						check(err)
					}
					sink ^= result[0]
				}, samples, millis)
				if operation == "seal" {
					out, err = c.Open(nil, nonce, result, aad)
					check(err)
				} else {
					out = result
				}
				if !bytes.Equal(out, plain) {
					panic("timed output")
				}
				emit(map[string]any{"kind": "timing", "backend": "go", "algorithm": algorithm, "bytes": size, "mode": mode, "operation": operation, "ns_samples": times})
			}
		}
	}
}
func main() {
	samples, err := strconv.Atoi(os.Args[1])
	check(err)
	millis, err := strconv.Atoi(os.Args[2])
	check(err)
	run("aes-256-gcm", func(key []byte) (cipher.AEAD, error) {
		block, err := aes.NewCipher(key)
		if err != nil {
			return nil, err
		}
		return cipher.NewGCM(block)
	}, samples, millis)
	run("chacha20-poly1305", chacha20poly1305.New, samples, millis)
}
