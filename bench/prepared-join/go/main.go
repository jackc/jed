package main

import (
	"bufio"
	"context"
	"encoding/json"
	"fmt"
	"os"
	"runtime"
	"runtime/pprof"
	"strconv"
	"strings"
	"time"

	jed "github.com/jackc/jed/impl/go"
)

func must(err error) {
	if err != nil {
		panic(err)
	}
}
func integer(s string) int { n, e := strconv.Atoi(s); must(e); return n }

func main() {
	if path := os.Getenv("JOIN_CPU_PROFILE"); path != "" {
		f, err := os.Create(path)
		must(err)
		must(pprof.StartCPUProfile(f))
		defer pprof.StopCPUProfile()
	}
	cycles, warmCycles := 1, 1
	if v := os.Getenv("JOIN_CYCLES"); v != "" {
		cycles = integer(v)
	}
	if v := os.Getenv("JOIN_WARM_CYCLES"); v != "" {
		warmCycles = integer(v)
	}
	db, err := jed.CreateDatabase(jed.CreateOptions{})
	must(err)
	defer db.Close()
	session := db.Session(jed.SessionOptions{})
	ctx := context.Background()
	file, err := os.Open(os.Args[1])
	must(err)
	defer file.Close()
	scanner := bufio.NewScanner(file)
	scanner.Buffer(make([]byte, 65536), 4*1024*1024)
	repetitions, validation := 0, 0
	verify := len(os.Args) > 2 && os.Args[2] == "--verify-only"
	for scanner.Scan() {
		f := strings.Split(scanner.Text(), "\t")
		switch f[0] {
		case "config":
			repetitions, validation = integer(f[1]), integer(f[2])
		case "setup":
			_, err = session.Exec(ctx, f[1])
			must(err)
		case "workload":
			name, iterations, warmup := f[1], integer(f[2]), integer(f[3])
			offset, step, span := integer(f[4]), integer(f[5]), integer(f[6])
			stmt, err := session.Prepare(f[7])
			must(err)
			count := max(validation, warmup, iterations)
			parameters := make([][]any, count)
			for i := range parameters {
				if span > 0 {
					parameters[i] = []any{int64(offset + (i*step)%span)}
				}
			}
			if len(os.Args) > 2 && os.Args[2] == "--diagnostics" {
				for _, param := range []any{int64(0), int64(17), int64(51), int64(999), int64(1000), nil} {
					for _, prefix := range []string{"", "EXPLAIN (ANALYZE, VERBOSE) "} {
						q := stmt
						if prefix != "" {
							q, err = session.Prepare(prefix + f[7])
							must(err)
						}
						rows, err := session.QueryPrepared(ctx, q, param)
						must(err)
						values := [][]string{}
						for rows.Next() {
							row := []string{}
							for _, v := range rows.Row() {
								row = append(row, v.Render())
							}
							values = append(values, row)
						}
						must(rows.Err())
						must(json.NewEncoder(os.Stdout).Encode(map[string]any{"workload": name, "binding": param, "explain": prefix != "", "types": rows.ColumnTypes(), "rows": values, "cost": rows.Cost()}))
						rows.Close()
					}
				}
				continue
			}
			hash := uint64(0xcbf29ce484222325)
			add := func(b byte) { hash = (hash ^ uint64(b)) * 0x100000001b3 }
			run := func(i int, checksum bool) int {
				rows, err := session.QueryPrepared(ctx, stmt, parameters[i]...)
				must(err)
				defer rows.Close()
				n := 0
				for rows.Next() {
					n++
					if checksum {
						for _, value := range rows.Row() {
							var text string
							switch value.Kind {
							case jed.ValNull:
								text = "NULL"
							case jed.ValInt:
								text = strconv.FormatInt(value.Int, 10)
							case jed.ValText:
								text = value.Render()
							default:
								panic("unexpected value kind")
							}
							for j := range len(text) {
								add(text[j])
							}
							add(0x1f)
						}
						add(0x1e)
					}
				}
				must(rows.Err())
				return n
			}
			validationRows := 0
			for i := range validation {
				validationRows += run(i, true)
			}
			samples := []int64{}
			totalRows := 0
			if !verify {
				for i := range warmup * warmCycles {
					run(i%warmup, false)
				}
				for range repetitions {
					n := 0
					start := time.Now()
					for i := range iterations * cycles {
						n += run(i%iterations, false)
					}
					samples = append(samples, time.Since(start).Nanoseconds())
					totalRows += n
				}
			}
			var before, after runtime.MemStats
			runtime.ReadMemStats(&before)
			for i := range iterations * cycles {
				run(i%iterations, false)
			}
			runtime.ReadMemStats(&after)
			must(json.NewEncoder(os.Stdout).Encode(map[string]any{"workload": name, "checksum": fmt.Sprintf("%016x", hash), "validation_rows": validationRows, "samples_ns": samples, "rows_total": totalRows, "iterations": iterations * cycles, "allocated_bytes": after.TotalAlloc - before.TotalAlloc, "allocations": after.Mallocs - before.Mallocs, "gc_cycles": after.NumGC - before.NumGC, "warmup": warmup, "runtime": runtime.Version()}))
		}
	}
	must(scanner.Err())
}
