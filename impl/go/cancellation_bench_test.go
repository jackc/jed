package jed

import (
	"context"
	"fmt"
	"math"
	"os"
	"sort"
	"strings"
	"sync/atomic"
	"testing"
	"time"
)

// Reuse the context and statement: these numbers exclude caller context/timer setup.
func BenchmarkCancellation(b *testing.B) {
	db := memDB()
	defer db.Close()
	ctx := context.Background()
	for _, sql := range []string{"CREATE TABLE t (id i32 PRIMARY KEY, v i32)", "INSERT INTO t SELECT i, i % 100 FROM generate_series(1, 10000) AS g(i)"} {
		if _, err := db.Exec(ctx, sql); err != nil {
			b.Fatal(err)
		}
	}
	for _, q := range []struct{ name, sql string }{
		{"cheap", "SELECT v FROM t WHERE id = 42"},
		{"stream", "SELECT v FROM t ORDER BY id LIMIT 10000"},
		{"aggregate", "SELECT sum(v), count(*) FROM t"},
		{"projection", "SELECT v FROM t"},
	} {
		stmt, err := db.Prepare(q.sql)
		if err != nil {
			b.Fatal(err)
		}
		for _, mode := range []string{"disabled", "enabled", "watcher", "scalar-disabled", "scalar-enabled"} {
			b.Run(q.name+"/"+mode, func(b *testing.B) {
				runCtx := ctx
				if mode == "enabled" || mode == "scalar-enabled" || mode == "watcher" {
					var cancel context.CancelFunc
					runCtx, cancel = context.WithCancel(ctx)
					defer cancel()
				}
				s := db.Session(SessionOptions{})
				defer s.Close()
				if strings.HasPrefix(mode, "scalar-") {
					s.SetMaxCost(math.MaxInt64)
				}
				b.ReportAllocs()
				b.ResetTimer()
				for i := 0; i < b.N; i++ {
					var rows *Rows
					var err error
					var stop func()
					if mode == "watcher" {
						rows, stop, err = benchmarkWatcherQuery(runCtx, s, stmt)
					} else {
						rows, err = s.QueryPrepared(runCtx, stmt)
					}
					if err != nil {
						if stop != nil {
							stop()
						}
						b.Fatal(err)
					}
					for rows.Next() {
						_ = rows.Row()
					}
					if stop != nil {
						stop()
					}
					if err := rows.Err(); err != nil {
						b.Fatal(err)
					}
					rows.Close()
				}
			})
		}
	}
}

// Model the former watcher with corrected lifetime: the caller stops it after draining,
// while the session poll is restored immediately and the cursor retains its atomic flag.
// This has the same no-argument prepared query seam as QueryPrepared, without installing
// a second channel poll around the watcher being measured.
func benchmarkWatcherQuery(ctx context.Context, s *Session, stmt *PreparedStatement) (*Rows, func(), error) {
	if err := ctxErr(ctx); err != nil {
		return nil, nil, err
	}
	var flag atomic.Bool
	done := make(chan struct{})
	go func() {
		select {
		case <-ctx.Done():
			flag.Store(true)
		case <-done:
		}
	}()
	s.engine.session.cancel = flag.Load
	rows, err := s.queryStmt(stmt.ast, nil, &stmt.sc, &stmt.ic)
	s.engine.session.cancel = nil
	if rows != nil {
		rows.ctx = ctx
	}
	return rows, func() { close(done) }, err
}

// Isolate watcher lifecycle and polling from the SQL engine. The watcher covers the entire
// simulated operation (unlike the old cursor bug); teardown matches the old armCancel.
func BenchmarkCancellationMechanism(b *testing.B) {
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	for _, polls := range []int{1, 1000} {
		for _, mode := range []string{"watcher", "channel"} {
			b.Run(fmt.Sprintf("%s/%d", mode, polls), func(b *testing.B) {
				b.ReportAllocs()
				for i := 0; i < b.N; i++ {
					var poll func() bool
					var stop func()
					if mode == "watcher" {
						var flag atomic.Bool
						done := make(chan struct{})
						go func() {
							select {
							case <-ctx.Done():
								flag.Store(true)
							case <-done:
							}
						}()
						poll = flag.Load
						stop = func() { close(done) }
					} else {
						done := ctx.Done()
						poll = func() bool {
							select {
							case <-done:
								return true
							default:
								return false
							}
						}
						stop = func() {}
					}
					for j := 0; j < polls; j++ {
						if poll() {
							b.Fatal("canceled")
						}
					}
					stop()
				}
			})
		}
	}
}

func BenchmarkCancellationCaller(b *testing.B) {
	b.Run("context", func(b *testing.B) {
		b.ReportAllocs()
		for i := 0; i < b.N; i++ {
			ctx, c := context.WithCancel(context.Background())
			_ = ctx.Done()
			c()
		}
	})
	b.Run("deadline", func(b *testing.B) {
		b.ReportAllocs()
		for i := 0; i < b.N; i++ {
			ctx, c := context.WithTimeout(context.Background(), time.Hour)
			_ = ctx.Done()
			c()
		}
	})
}

// Timer-driven response latency: includes scheduler delivery and engine unwinding. Unlike the
// regression barriers this deliberately measures real concurrent cancellation timing.
func TestCancellationLatencyMeasurement(t *testing.T) {
	if os.Getenv("JED_CANCEL_LATENCY") == "" {
		t.Skip("opt-in measurement")
	}
	db := memDB()
	defer db.Close()
	bg := context.Background()
	for _, sql := range []string{"CREATE TABLE t (id i32 PRIMARY KEY, v i32)", "INSERT INTO t SELECT i, i % 100 FROM generate_series(1, 1000000) AS g(i)"} {
		if _, err := db.Exec(bg, sql); err != nil {
			t.Fatal(err)
		}
	}
	for _, sql := range []string{"SELECT sum(v) FROM t", "SELECT v FROM t", "SELECT v FROM t ORDER BY id LIMIT 1000000"} {
		samples := make([]int64, 0, 20)
		for i := 0; i < 20; i++ {
			ctx, cancel := context.WithCancel(bg)
			r, err := db.Query(ctx, sql)
			if err != nil {
				t.Fatal(err)
			}
			fired := make(chan time.Time, 1)
			go func() { time.Sleep(time.Millisecond); start := time.Now(); cancel(); fired <- start }()
			for r.Next() {
			}
			end := time.Now()
			if code := cancelCode(t, r.Err()); code != "57014" {
				t.Fatal(code)
			}
			samples = append(samples, end.Sub(<-fired).Nanoseconds())
			r.Close()
		}
		sort.Slice(samples, func(i, j int) bool { return samples[i] < samples[j] })
		t.Logf("%s: cancellation response median=%dns p95=%dns max=%dns", sql, samples[10], samples[18], samples[19])
	}
}
