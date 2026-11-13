// Diagnostic transport only. Compiled against the experiment's frozen Go core;
// it does not implement or generate any database behavior.
package main

import (
	"context"
	"encoding/json"
	"fmt"
	jed "github.com/jackc/jed/impl/go"
	"os"
	"path/filepath"
	"runtime"
	"time"
)

type workload struct {
	Name, SQL, Mode    string
	Warmup, Iterations int
}
type input struct {
	Setup []string
	Cases []workload
}
type checksum uint64

func (c *checksum) text(s string) {
	for i := range len(s) {
		*c = (*c ^ checksum(s[i])) * 1099511628211
	}
}
func must(err error) {
	if err != nil {
		panic(err)
	}
}
func drain(s *jed.Session, rows *jed.Rows, err error, sum *checksum) int64 {
	must(err)
	defer rows.Close()
	var count int64
	for rows.Next() {
		for _, v := range rows.Row() {
			text, err := s.Render(v)
			must(err)
			sum.text(text)
			sum.text("\x1f")
		}
		sum.text("\x1e")
		count++
	}
	must(rows.Err())
	return count
}
func query(s *jed.Session, sql string, sum *checksum) int64 {
	rows, err := s.Query(context.Background(), sql)
	return drain(s, rows, err, sum)
}
func prepared(s *jed.Session, p *jed.PreparedStatement, sum *checksum) int64 {
	rows, err := s.QueryPrepared(context.Background(), p)
	return drain(s, rows, err, sum)
}
func main() {
	if len(os.Args) == 2 && os.Args[1] == "performance-ready" {
		db, err := jed.CreateDatabase(jed.CreateOptions{})
		must(err)
		s := db.Session(jed.SessionOptions{})
		sum := checksum(14695981039346656037)
		query(s, "SELECT 1", &sum)
		s.Close()
		must(db.Close())
		fmt.Println("ready")
		return
	}
	if len(os.Args) != 3 || os.Args[1] != "performance" {
		panic("usage: performance <input> | performance-ready")
	}
	bytes, err := os.ReadFile(os.Args[2])
	must(err)
	var in input
	must(json.Unmarshal(bytes, &in))
	directory, err := os.MkdirTemp("", "jed-go-performance-")
	must(err)
	defer os.RemoveAll(directory)
	for _, w := range in.Cases {
		path := filepath.Join(directory, w.Name+".jed")
		setup, err := jed.CreateDatabase(jed.CreateOptions{Path: path})
		must(err)
		session := setup.Session(jed.SessionOptions{})
		discarded := checksum(14695981039346656037)
		for _, sql := range in.Setup {
			query(session, sql, &discarded)
		}
		session.Close()
		must(setup.Close())
		var db *jed.Database
		var s *jed.Session
		var p *jed.PreparedStatement
		if w.Mode != "cold_open" {
			db, err = jed.OpenDatabaseWithOptions(path, jed.OpenOptions{})
			must(err)
			s = db.Session(jed.SessionOptions{})
			p, err = s.Prepare(w.SQL)
			must(err)
		}
		sum := checksum(14695981039346656037)
		var count int64
		samples := make([]int64, w.Iterations)
		var before, after runtime.MemStats
		for i := 0; i < w.Warmup+w.Iterations; i++ {
			if i == w.Warmup {
				runtime.ReadMemStats(&before)
			}
			sink := &sum
			dummy := checksum(14695981039346656037)
			if i < w.Warmup {
				sink = &dummy
			}
			start := time.Now()
			var rows int64
			if w.Mode == "cold_open" {
				opened, err := jed.OpenDatabaseWithOptions(path, jed.OpenOptions{})
				must(err)
				session := opened.Session(jed.SessionOptions{})
				statement, err := session.Prepare(w.SQL)
				must(err)
				rows = prepared(session, statement, sink)
				session.Close()
				must(opened.Close())
			} else if w.Mode == "rollback" {
				query(s, "BEGIN", &dummy)
				rows = prepared(s, p, sink)
				query(s, "ROLLBACK", &dummy)
			} else {
				rows = prepared(s, p, sink)
			}
			if i >= w.Warmup {
				samples[i-w.Warmup] = time.Since(start).Nanoseconds()
				count += rows
			}
		}
		runtime.ReadMemStats(&after)
		if w.Mode == "rollback" || w.Mode == "durable" {
			count += query(s, "SELECT count(*), sum(v) FROM t", &sum)
		}
		must(json.NewEncoder(os.Stdout).Encode(map[string]any{"name": w.Name, "mode": w.Mode, "runtime": runtime.Version(), "warmup": w.Warmup, "iterations": w.Iterations, "rows": count, "checksum": fmt.Sprintf("%016x", uint64(sum)), "allocated_bytes": after.TotalAlloc - before.TotalAlloc, "gc_collections": []uint32{after.NumGC - before.NumGC}, "samples_ns": samples}))
		if s != nil {
			s.Close()
			must(db.Close())
		}
	}
}
