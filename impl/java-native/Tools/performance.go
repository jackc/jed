// Diagnostic transport only. Compiled against the experiment's frozen Go core;
// it does not implement or generate any database behavior.
package main

import (
	"bufio"
	"context"
	"encoding/json"
	"fmt"
	jed "github.com/jackc/jed/impl/go"
	"os"
	"path/filepath"
	"runtime"
	"strconv"
	"strings"
	"sync"
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
type checksum struct {
	value uint64
	cost  int64
}

func (c *checksum) text(s string) {
	for i := range len(s) {
		c.value = (c.value ^ uint64(s[i])) * 1099511628211
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
	sum.cost += rows.Cost()
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
func parallel(db *jed.Database, sql string, sum *checksum) int64 {
	var workers sync.WaitGroup
	sums := make([]checksum, 4)
	counts := make([]int64, 4)
	for i := range 4 {
		workers.Add(1)
		go func(index int) {
			defer workers.Done()
			s := db.Session(jed.SessionOptions{})
			defer s.Close()
			p, err := s.Prepare(sql)
			must(err)
			sums[index] = checksum{value: 14695981039346656037}
			for range 64 {
				counts[index] += prepared(s, p, &sums[index])
			}
		}(i)
	}
	workers.Wait()
	var count int64
	for i := range 4 {
		sum.text(fmt.Sprintf("%016x", sums[i].value))
		sum.text("\x1d")
		sum.cost += sums[i].cost
		count += counts[i]
	}
	return count
}
func main() {
	if len(os.Args) == 2 && os.Args[1] == "ready" {
		db, err := jed.CreateDatabase(jed.CreateOptions{})
		must(err)
		s := db.Session(jed.SessionOptions{})
		sum := checksum{value: 14695981039346656037}
		query(s, "SELECT 1", &sum)
		s.Close()
		must(db.Close())
		fmt.Println("ready")
		return
	}
	if len(os.Args) != 3 {
		panic("usage: INPUT DIRECTORY | ready")
	}
	file, err := os.Open(os.Args[1])
	must(err)
	var in input
	scanner := bufio.NewScanner(file)
	scanner.Buffer(make([]byte, 4096), 1024*1024)
	for scanner.Scan() {
		fields := strings.SplitN(scanner.Text(), "\t", 6)
		if len(fields) == 2 && fields[0] == "setup" {
			in.Setup = append(in.Setup, fields[1])
		} else if len(fields) == 6 && fields[0] == "case" {
			warmup, err := strconv.Atoi(fields[3])
			must(err)
			iterations, err := strconv.Atoi(fields[4])
			must(err)
			if warmup < 0 || iterations <= 0 {
				panic("invalid samples")
			}
			in.Cases = append(in.Cases, workload{Name: fields[1], Mode: fields[2], Warmup: warmup, Iterations: iterations, SQL: fields[5]})
		} else {
			panic("invalid benchmark input")
		}
	}
	must(scanner.Err())
	must(file.Close())
	directory := os.Args[2]
	must(os.MkdirAll(directory, 0700))
	for _, w := range in.Cases {
		path := filepath.Join(directory, w.Name+".jed")
		setup, err := jed.CreateDatabase(jed.CreateOptions{Path: path})
		must(err)
		session := setup.Session(jed.SessionOptions{})
		discarded := checksum{value: 14695981039346656037}
		for _, sql := range in.Setup {
			query(session, sql, &discarded)
		}
		session.Close()
		must(setup.Close())
		var db *jed.Database
		var s *jed.Session
		var p *jed.PreparedStatement
		if w.Mode != "cold_open" {
			db, err = jed.OpenDatabaseWithOptions(path, jed.OpenOptions{CacheBytes: 256 * 1024 * 1024, WorkMem: 256 * 1024 * 1024})
			must(err)
			s = db.Session(jed.SessionOptions{})
			if w.Mode != "parallel" {
				p, err = s.Prepare(w.SQL)
				must(err)
			}
		}
		sum := checksum{value: 14695981039346656037}
		var count int64
		samples := make([]int64, w.Iterations)
		warmups := make([]int64, w.Warmup)
		var before, after runtime.MemStats
		for i := 0; i < w.Warmup+w.Iterations; i++ {
			if i == w.Warmup {
				runtime.ReadMemStats(&before)
			}
			sink := &sum
			dummy := checksum{value: 14695981039346656037}
			if i < w.Warmup {
				sink = &dummy
			}
			start := time.Now()
			var rows int64
			if w.Mode == "cold_open" {
				opened, err := jed.OpenDatabaseWithOptions(path, jed.OpenOptions{CacheBytes: 256 * 1024 * 1024, WorkMem: 256 * 1024 * 1024})
				must(err)
				session := opened.Session(jed.SessionOptions{})
				statement, err := session.Prepare(w.SQL)
				must(err)
				rows = prepared(session, statement, sink)
				session.Close()
				must(opened.Close())
			} else if w.Mode == "parallel" {
				rows = parallel(db, w.SQL, sink)
			} else if w.Mode == "rollback" {
				query(s, "BEGIN", &dummy)
				rows = prepared(s, p, sink)
				query(s, "ROLLBACK", &dummy)
			} else {
				rows = prepared(s, p, sink)
			}
			elapsed := time.Since(start).Nanoseconds()
			if i < w.Warmup {
				warmups[i] = elapsed
			} else {
				samples[i-w.Warmup] = elapsed
				count += rows
			}
		}
		runtime.ReadMemStats(&after)
		if w.Mode == "rollback" || w.Mode == "durable" {
			count += query(s, "SELECT count(*), sum(v) FROM t", &sum)
		}
		must(json.NewEncoder(os.Stdout).Encode(map[string]any{"name": w.Name, "mode": w.Mode, "runtime": runtime.Version(), "warmup": w.Warmup, "iterations": w.Iterations, "rows": count, "checksum": fmt.Sprintf("%016x", sum.value), "cost": sum.cost, "warmup_ns": warmups, "allocated_bytes": after.TotalAlloc - before.TotalAlloc, "gc_collections": after.NumGC - before.NumGC, "gc_pause_ns": after.PauseTotalNs - before.PauseTotalNs, "heap_before": before.HeapAlloc, "heap_after": after.HeapAlloc, "samples_ns": samples}))
		if s != nil {
			s.Close()
			must(db.Close())
		}
	}
}
