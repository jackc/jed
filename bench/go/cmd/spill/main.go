// Standalone spill benchmark worker. SQL comes from the shared Ruby workload driver.
package main

import (
	"bufio"
	"context"
	"encoding/json"
	"fmt"
	"os"
	"strconv"
	"strings"
	"time"

	jed "github.com/jackc/jed/impl/go"
)

func main() {
	if err := run(); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}

func run() error {
	if len(os.Args) != 6 {
		return fmt.Errorf("usage: spill create|open database work_mem sql.tsv cache_bytes")
	}
	budget, err := strconv.Atoi(os.Args[3])
	if err != nil || budget < 0 {
		return fmt.Errorf("invalid work_mem")
	}
	cache, err := strconv.Atoi(os.Args[5])
	if err != nil || cache <= 0 {
		return fmt.Errorf("invalid cache_bytes")
	}
	var db *jed.Database
	if os.Args[1] == "create" {
		db, err = jed.CreateDatabase(jed.CreateOptions{Path: os.Args[2], SkipFsync: true, Locking: jed.LockingNone})
	} else {
		db, err = jed.OpenDatabaseWithOptions(os.Args[2], jed.OpenOptions{CacheBytes: cache, Locking: jed.LockingNone})
	}
	if err != nil {
		return err
	}
	defer db.Close()
	sess := db.Session(jed.SessionOptions{})
	defer sess.Close()
	sess.SetWorkMem(budget)
	f, err := os.Open(os.Args[4])
	if err != nil {
		return err
	}
	defer f.Close()
	scan := bufio.NewScanner(f)
	scan.Buffer(make([]byte, 4096), 1<<20)
	for scan.Scan() {
		name, sql, ok := strings.Cut(scan.Text(), "\t")
		if !ok {
			return fmt.Errorf("invalid workload line")
		}
		start := time.Now()
		rows, err := sess.Query(context.Background(), sql)
		if err != nil {
			return err
		}
		hash, count := uint64(14695981039346656037), 0
		add := func(s string) {
			for i := 0; i < len(s); i++ {
				hash ^= uint64(s[i])
				hash *= 1099511628211
			}
		}
		for rows.Next() {
			count++
			for _, value := range rows.Row() {
				s := value.Render()
				add(strconv.Itoa(len(s)) + ":")
				add(s)
			}
			add("\n")
		}
		cost := rows.Cost()
		err = rows.Err()
		rows.Close()
		if err != nil {
			return err
		}
		if name != "-" {
			json.NewEncoder(os.Stdout).Encode(map[string]any{"name": name, "rows": count, "cost": cost, "checksum": fmt.Sprintf("%016x", hash), "ms": float64(time.Since(start).Microseconds()) / 1000})
		}
	}
	return scan.Err()
}
