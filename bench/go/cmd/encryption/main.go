// Standalone encryption-at-rest benchmark worker (spec/design/encryption.md §9, *Integrated
// performance*); mise-tasks/bench/encryption alternates the variants and reports. Mirrors
// bench/rust/src/bin/encryption.rs.
//
// `encryption plain|encrypted DIR ROWS` builds a table of ROWS rows (`id i64 PRIMARY KEY`, a 100-byte
// text) in batches of 1000 committed rows, then reopens it with a 1 MiB page cache and runs a full
// scan and random point lookups whose leaves mostly miss the cache, then the same lookups with a
// cache that holds the whole table. Each phase prints one `phase\tmillis\tchecksum` line. Commits
// skip fsync, so the commit phase measures sealing rather than the disk.
package main

import (
	"context"
	"fmt"
	"os"
	"path/filepath"
	"strconv"
	"time"

	jed "github.com/jackc/jed/impl/go"
)

const lookups = 20_000

func main() {
	if err := run(); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}

func run() error {
	if len(os.Args) != 4 || (os.Args[1] != "plain" && os.Args[1] != "encrypted") {
		return fmt.Errorf("usage: encryption plain|encrypted DIR ROWS")
	}
	var credential *jed.Credential
	if os.Args[1] == "encrypted" {
		var key [32]byte
		for i := range key {
			key[i] = 0x4A
		}
		credential = jed.KeyCredential(key)
	}
	path := filepath.Join(os.Args[2], os.Args[1]+".jed")
	_ = os.Remove(path)
	rows, err := strconv.ParseInt(os.Args[3], 10, 64)
	if err != nil {
		return err
	}
	ctx := context.Background()

	db, err := jed.CreateDatabase(jed.CreateOptions{Path: path, SkipFsync: true, Locking: jed.LockingNone, Credential: credential})
	if err != nil {
		return err
	}
	s := db.Session(jed.SessionOptions{})
	if _, err := s.Exec(ctx, "CREATE TABLE t (id i64 PRIMARY KEY, v text)"); err != nil {
		return err
	}
	start := time.Now()
	for lo := int64(1); lo <= rows; {
		hi := min(lo+999, rows)
		sql := fmt.Sprintf("INSERT INTO t SELECT g, repeat(md5(g::text), 3) || 'abcd' FROM generate_series(%d, %d) g", lo, hi)
		if _, err := s.Exec(ctx, sql); err != nil {
			return err
		}
		lo = hi + 1
	}
	report("commit_1000_row_batches", start, rows)
	s.Close()
	if err := db.Close(); err != nil {
		return err
	}

	for _, phase := range []struct {
		name  string
		cache int
	}{{"cold", 1 << 20}, {"hot", 1 << 30}} {
		db, err := jed.OpenDatabaseWithOptions(path, jed.OpenOptions{SkipFsync: true, Locking: jed.LockingNone, CacheBytes: phase.cache, Credential: credential})
		if err != nil {
			return err
		}
		s := db.Session(jed.SessionOptions{})
		if phase.name == "cold" {
			start := time.Now()
			var count, sum int64
			if err := s.QueryRow(ctx, "SELECT count(*), sum(length(v)) FROM t").Scan(&count, &sum); err != nil {
				return err
			}
			report("full_scan_cold", start, sum)
		} else {
			// Warm the cache so every lookup is a hit.
			var count int64
			if err := s.QueryRow(ctx, "SELECT count(*) FROM t").Scan(&count); err != nil {
				return err
			}
		}
		stmt, err := s.Prepare("SELECT length(v) FROM t WHERE id = $1")
		if err != nil {
			return err
		}
		start := time.Now()
		var checksum int64
		x := uint64(0x9E3779B97F4A7C15)
		for range lookups {
			x ^= x << 13
			x ^= x >> 7
			x ^= x << 17
			id := int64(x%uint64(rows)) + 1
			r, err := s.QueryPrepared(ctx, stmt, id)
			if err != nil {
				return err
			}
			for r.Next() {
				n, err := r.Int(0)
				if err == nil {
					checksum += n + id
				}
			}
			if err := r.Err(); err != nil {
				return err
			}
			_ = r.Close()
		}
		report("point_lookups_"+phase.name, start, checksum)
		s.Close()
		if err := db.Close(); err != nil {
			return err
		}
	}
	info, err := os.Stat(path)
	if err != nil {
		return err
	}
	fmt.Printf("file_bytes\t0\t%d\n", info.Size())
	return os.Remove(path)
}

func report(phase string, start time.Time, checksum int64) {
	fmt.Printf("%s\t%.3f\t%d\n", phase, float64(time.Since(start).Nanoseconds())/1e6, checksum)
}
