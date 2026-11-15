package jed

import (
	"context"
	"errors"
	"math"
	"os"
	"path/filepath"
	"testing"
	"time"
)

func requireInvalidOption(t *testing.T, err error) {
	t.Helper()
	var ee *EngineError
	if !errors.As(err, &ee) || ee.Code() != "22023" {
		t.Fatalf("want invalid_parameter_value (22023), got %v", err)
	}
}

func TestSessionConfigurationValidation(t *testing.T) {
	for _, readOnly := range []bool{false, true} {
		t.Run(map[bool]string{false: "memory", true: "read-only file"}[readOnly], func(t *testing.T) {
			var db *Database
			if readOnly {
				path := filepath.Join(t.TempDir(), "db.jed")
				created, err := CreateDatabase(CreateOptions{Path: path, Locking: LockingNone})
				if err != nil {
					t.Fatal(err)
				}
				if err := created.Close(); err != nil {
					t.Fatal(err)
				}
				db, err = OpenDatabaseWithOptions(path, OpenOptions{ReadOnly: true, Locking: LockingNone})
				if err != nil {
					t.Fatal(err)
				}
			}
			if !readOnly {
				db = memDB()
			}
			defer db.Close()
			for _, opts := range []SessionOptions{
				{TimeZone: "not/a-zone"}, {TimeZone: "America/New_York"}, {TimeZone: "+999:00"}, {LockTimeout: -time.Nanosecond},
			} {
				s, err := db.Session(opts)
				requireInvalidOption(t, err)
				if s != nil {
					s.Close()
					t.Fatal("invalid options returned a session")
				}
				db.core.liveMu.Lock()
				pins := len(db.core.live)
				db.core.liveMu.Unlock()
				if pins != 0 {
					t.Fatalf("failed construction retained %d reader pins", pins)
				}
			}
			for _, zone := range []string{"", "UTC", "+05:30"} {
				s, err := db.Session(SessionOptions{TimeZone: zone})
				if err != nil {
					t.Fatal(err)
				}
				var current string
				err = s.QueryRow(context.Background(), "SELECT (timestamptz '2024-01-15 12:00:00+00')::text").Scan(&current)
				s.Close()
				if err != nil {
					t.Fatal(err)
				}
				want := "2024-01-15 12:00:00+00"
				if zone == "+05:30" {
					want = "2024-01-15 06:30:00-05:30"
				}
				if current != want {
					t.Fatalf("zone %q: got %q", zone, current)
				}
			}
		})
	}
	builder := NewEnvironmentBuilder()
	if err := builder.LoadTimeZoneData(tzBundleBytes(t)); err != nil {
		t.Fatal(err)
	}
	db := memDBIn(builder.Build())
	defer db.Close()
	s, err := db.Session(SessionOptions{TimeZone: "America/New_York"})
	if err != nil {
		t.Fatal(err)
	}
	defer s.Close()
	var zone string
	if err := s.QueryRow(context.Background(), "SELECT (timestamptz '2024-01-15 12:00:00+00')::text").Scan(&zone); err != nil {
		t.Fatal(err)
	}
	if zone != "2024-01-15 07:00:00-05" {
		t.Fatalf("got zone %q", zone)
	}
}

func TestDurationOptionsRejectNegativeBeforeIO(t *testing.T) {
	negative := -time.Nanosecond
	cases := []struct {
		name   string
		create CreateOptions
		open   OpenOptions
	}{
		{"flush", CreateOptions{FlushInterval: &negative}, OpenOptions{FlushInterval: &negative}},
		{"file lock", CreateOptions{FileLockTimeout: &negative}, OpenOptions{FileLockTimeout: &negative}},
		{"idle transaction", CreateOptions{IdleInTransactionTimeout: &negative}, OpenOptions{IdleInTransactionTimeout: &negative}},
		{"idle session", CreateOptions{IdleSessionTimeout: &negative}, OpenOptions{IdleSessionTimeout: &negative}},
		{"yield", CreateOptions{YieldGrace: &negative}, OpenOptions{YieldGrace: &negative}},
		{"shutdown", CreateOptions{ShutdownGrace: &negative}, OpenOptions{ShutdownGrace: &negative}},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			db, err := CreateDatabase(tc.create)
			requireInvalidOption(t, err)
			if db != nil {
				db.Close()
				t.Fatal("invalid options created a memory database")
			}
			path := filepath.Join(t.TempDir(), "new.jed")
			tc.create.Path = path
			db, err = CreateDatabase(tc.create)
			requireInvalidOption(t, err)
			if db != nil {
				db.Close()
				t.Fatal("invalid options created a file database")
			}
			if _, err := os.Stat(path); !errors.Is(err, os.ErrNotExist) {
				t.Fatalf("create touched path: %v", err)
			}
			db, err = OpenDatabaseWithOptions(path, tc.open)
			requireInvalidOption(t, err)
			if db != nil {
				db.Close()
				t.Fatal("invalid options opened a database")
			}
			db, err = OpenRemote([]string{"/nonexistent/jed-test-transport"}, "unused", tc.open)
			requireInvalidOption(t, err)
			if db != nil {
				db.Close()
				t.Fatal("invalid options opened a remote")
			}
		})
	}
	db := memDB()
	defer db.Close()
	requireInvalidOption(t, db.Attach("aux", AttachFileWithOptions("/nonexistent/jed-test-file", AttachFileOptions{FileLockTimeout: &negative}), false))
}

func TestLockTimeoutDurations(t *testing.T) {
	db := memDB()
	defer db.Close()
	for _, tc := range []struct{ input, want time.Duration }{
		{0, 0},
		{time.Nanosecond, time.Millisecond},
		{time.Millisecond + time.Nanosecond, 2 * time.Millisecond},
		{2 * time.Second, 2 * time.Second},
		{time.Duration(math.MaxInt64), time.Duration(math.MaxInt64)},
	} {
		s, err := db.Session(SessionOptions{LockTimeout: tc.input})
		if err != nil {
			t.Fatal(err)
		}
		if got := s.LockTimeout(); got != tc.want {
			t.Fatalf("%v: got %v, want %v", tc.input, got, tc.want)
		}
		requireInvalidOption(t, s.SetLockTimeout(-time.Nanosecond))
		if got := s.LockTimeout(); got != tc.want {
			t.Fatalf("invalid setter changed timeout to %v", got)
		}
		if err := s.SetLockTimeout(time.Nanosecond); err != nil {
			t.Fatal(err)
		}
		if got := s.LockTimeout(); got != time.Millisecond {
			t.Fatalf("setter rounded to %v", got)
		}
		s.Close()
	}
	// Rounding the maximum duration up to milliseconds must not wrap timers negative.
	maxMillis := durationMillis(time.Duration(math.MaxInt64))
	if got := millisDuration(maxMillis); got != time.Duration(math.MaxInt64) {
		t.Fatalf("maximum duration wrapped: %v", got)
	}
}

func TestDurationOptionDefaultsAndZero(t *testing.T) {
	zero := time.Duration(0)
	custom := 1500 * time.Microsecond
	for _, tc := range []struct {
		name  string
		value *time.Duration
		want  uint64
	}{
		{"default", nil, 1000}, {"disabled", &zero, 0}, {"rounded", &custom, 2},
	} {
		t.Run(tc.name, func(t *testing.T) {
			path := filepath.Join(t.TempDir(), "db.jed")
			db, err := CreateDatabase(CreateOptions{Path: path, Locking: LockingNone, FlushInterval: tc.value})
			if err != nil {
				t.Fatal(err)
			}
			if got := db.core.storage.deadline.intervalMs(); got != tc.want {
				t.Fatalf("create flush interval %d, want %d", got, tc.want)
			}
			if err := db.Close(); err != nil {
				t.Fatal(err)
			}
			db, err = OpenDatabaseWithOptions(path, OpenOptions{Locking: LockingNone, FlushInterval: tc.value})
			if err != nil {
				t.Fatal(err)
			}
			defer db.Close()
			if got := db.core.storage.deadline.intervalMs(); got != tc.want {
				t.Fatalf("open flush interval %d, want %d", got, tc.want)
			}
		})
	}
	defaults := ownerLimitsFrom(OpenOptions{})
	if defaults.idleInTransaction != time.Minute || defaults.idleSession != 0 || defaults.yieldGrace != 5*time.Second || defaults.shutdownGrace != 30*time.Second {
		t.Fatalf("unexpected defaults: %+v", defaults)
	}
	disabled := ownerLimitsFrom(OpenOptions{IdleInTransactionTimeout: &zero, IdleSessionTimeout: &zero, YieldGrace: &zero, ShutdownGrace: &zero})
	if disabled != (ownerLimits{}) {
		t.Fatalf("explicit zero lost: %+v", disabled)
	}
	precise := ownerLimitsFrom(OpenOptions{IdleInTransactionTimeout: &custom, IdleSessionTimeout: &custom, YieldGrace: &custom, ShutdownGrace: &custom})
	if precise.idleInTransaction != custom || precise.idleSession != custom || precise.yieldGrace != custom || precise.shutdownGrace != custom {
		t.Fatalf("duration precision lost: %+v", precise)
	}
	for _, tc := range []struct {
		value *time.Duration
		want  uint64
	}{{nil, 5000}, {&zero, 0}, {&custom, 2}} {
		h := newServiceHandoff(nil, "unused", OpenOptions{FileLockTimeout: tc.value})
		if h.timeoutMs != tc.want {
			t.Fatalf("file lock timeout %d, want %d", h.timeoutMs, tc.want)
		}
	}
}
