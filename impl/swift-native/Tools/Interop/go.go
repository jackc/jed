// Opt-in interchange transport. All SQL/storage operations use the public API.
package main

import (
	"bufio"
	"context"
	"encoding/hex"
	"errors"
	"fmt"
	jed "github.com/jackc/jed/impl/go"
	"os"
	"path/filepath"
	"strings"
	"unicode/utf8"
)

func decode(s string) (string, error) {
	b, e := hex.DecodeString(s)
	if e != nil {
		return "", e
	}
	if !utf8.Valid(b) {
		return "", fmt.Errorf("invalid UTF-8")
	}
	return string(b), nil
}
func cred(kind, encoded string) (*jed.Credential, error) {
	switch kind {
	case "plain":
		return nil, nil
	case "pass":
		s, e := decode(encoded)
		if e != nil {
			return nil, e
		}
		return jed.PassphraseCredential(s), nil
	case "key":
		b, e := hex.DecodeString(encoded)
		if e != nil || len(b) != 32 {
			return nil, fmt.Errorf("invalid key")
		}
		var k [32]byte
		copy(k[:], b)
		return jed.KeyCredential(k), nil
	default:
		return nil, fmt.Errorf("invalid credential kind")
	}
}
func failure(e error) {
	var engine *jed.EngineError
	code := "XXXXX"
	if errors.As(e, &engine) {
		code = engine.Code()
	}
	fmt.Printf("ERR\t%s\t%s\n", code, hex.EncodeToString([]byte(e.Error())))
}
func main() {
	if e := run(); e != nil {
		failure(e)
		os.Exit(1)
	}
}
func run() error {
	a := os.Args[1:]
	if len(a) != 6 {
		return fmt.Errorf("expected action path fixtures credential-kind credential-hex environment")
	}
	b := jed.NewEnvironmentBuilder()
	if a[5] != "empty" {
		u, e := os.ReadFile(filepath.Join(a[2], "unicode.jucd"))
		if e != nil {
			return e
		}
		if e = b.LoadUnicodeData(u); e != nil {
			return e
		}
		z, e := os.ReadFile(filepath.Join(a[2], "tzdata.jtz"))
		if e != nil {
			return e
		}
		if e = b.LoadTimeZoneData(z); e != nil {
			return e
		}
	}
	if a[5] == "full" {
		f := jed.NewHostFunction("geo_hash", []string{"i64"}, "i64", func(v []jed.Value) (jed.Value, error) {
			n := v[0].Int
			if n > 922337203685477580 || n < -922337203685477580 {
				return jed.Value{}, fmt.Errorf("host overflow")
			}
			return jed.IntValue(n * 10), nil
		}).WithVolatility(jed.VolatilityImmutable).WithCrossCore(true).WithComponentID("com.example/geo_hash").WithSemanticVersion(1)
		if e := b.RegisterFunction(f); e != nil {
			return e
		}
	}
	b.SetInsecureSeededKeyEntropy(0x4a454442)
	env := b.Build()
	c, e := cred(a[3], a[4])
	if e != nil {
		return e
	}
	kdf := jed.Argon2Params{MemoryKiB: 32, Iterations: 1, Parallelism: 1}
	var db *jed.Database
	switch a[0] {
	case "create":
		db, e = jed.CreateDatabase(jed.CreateOptions{Path: a[1], PageSize: 1024, Environment: env, Credential: c, KDF: kdf, Locking: jed.LockingNone, ScratchEncryption: jed.ScratchEncryptionOn})
	case "open":
		db, e = jed.OpenDatabaseWithOptions(a[1], jed.OpenOptions{Environment: env, Credential: c, Locking: jed.LockingNone, ScratchEncryption: jed.ScratchEncryptionOn})
	default:
		return fmt.Errorf("invalid action")
	}
	if e != nil {
		return e
	}
	defer db.Close()
	fmt.Println("READY")
	input := bufio.NewScanner(os.Stdin)
	input.Buffer(make([]byte, 65536), 16*1024*1024)
	for input.Scan() {
		f := strings.Split(input.Text(), "\t")
		arity := map[string]int{"EXEC": 2, "QUERY": 2, "ATTACH": 4, "REWRAP": 3, "UPGRADE": 1, "CLOSE": 1}
		if arity[f[0]] != len(f) {
			failure(fmt.Errorf("invalid command arity"))
			continue
		}
		result := ""
		e = nil
		switch f[0] {
		case "EXEC":
			var s string
			s, e = decode(f[1])
			if e == nil {
				_, e = db.Exec(context.Background(), s)
			}
		case "QUERY":
			var s string
			s, e = decode(f[1])
			if e == nil {
				result, e = query(db, s)
			}
		case "ATTACH":
			if len(f) != 4 {
				e = fmt.Errorf("invalid attach")
				break
			}
			var p string
			var c *jed.Credential
			p, e = decode(f[1])
			if e == nil {
				c, e = cred(f[2], f[3])
			}
			if e == nil {
				e = db.Attach("aux", jed.AttachFileWithOptions(p, jed.AttachFileOptions{Locking: jed.LockingNone}).WithCredential(c), false)
			}
		case "REWRAP":
			var c *jed.Credential
			c, e = cred(f[1], f[2])
			if e == nil {
				e = db.ChangeCredential("main", c, kdf)
			}
		case "UPGRADE":
			_, e = db.UpgradeCollations()
		case "CLOSE":
			e = db.Close()
			if e != nil {
				return e
			}
			fmt.Println("OK\t")
			return nil
		default:
			e = fmt.Errorf("unknown interop command")
		}
		if e != nil {
			failure(e)
		} else {
			fmt.Printf("OK\t%s\n", result)
		}
	}
	return input.Err()
}
func query(db *jed.Database, sql string) (string, error) {
	rows, e := db.Query(context.Background(), sql)
	if e != nil {
		return "", e
	}
	defer rows.Close()
	var out []string
	for rows.Next() {
		v, e := rows.Values()
		if e != nil {
			return "", e
		}
		fields := make([]string, len(v))
		for i, value := range v {
			if value == nil {
				fields[i] = "-"
			} else if s, ok := value.(string); ok {
				fields[i] = hex.EncodeToString([]byte(s))
			} else {
				return "", fmt.Errorf("interop QUERY requires text fields, got %T", value)
			}
		}
		out = append(out, strings.Join(fields, ":"))
	}
	if e = rows.Err(); e != nil {
		return "", e
	}
	return fmt.Sprintf("%d;%s", len(out), strings.Join(out, ",")), nil
}
