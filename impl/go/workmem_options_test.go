package jed

// OpenOptions.WorkMem == 0 means "the default budget" (256 MiB), NOT "unlimited" — the zero value must
// stay a safe finite budget so a bare OpenOptions{} does not silently disable spill-to-disk. Unbounded
// / never-spill is reachable only at runtime via SetWorkMem(0). This pins the options→session boundary
// that once diverged across cores (Go remapped 0→default; Rust/TS passed 0 through as unlimited). Host-
// API config surface + a deliberate cross-core alignment the corpus cannot express → a per-core unit
// test (CLAUDE.md §10). Mirrors impl/rust/tests/work_mem_options.rs and
// impl/ts/tests/work_mem_options.test.ts.

import (
	"path/filepath"
	"testing"
)

func TestOpenOptionsWorkMemZeroIsDefaultBudget(t *testing.T) {
	path := filepath.Join(t.TempDir(), "wm.jed")
	seed, err := create(path, databaseOptions{PageSize: DefaultPageSize, noSync: true})
	if err != nil {
		t.Fatal(err)
	}
	if err := seed.Close(); err != nil {
		t.Fatal(err)
	}

	cases := []struct {
		name string
		opts OpenOptions
		want int
	}{
		{"unset", OpenOptions{}, defaultWorkMem},
		{"explicit-zero", OpenOptions{WorkMem: 0}, defaultWorkMem}, // the regression guard: 0 ≠ unlimited
		{"explicit-budget", OpenOptions{WorkMem: 1 << 20}, 1 << 20},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			db, err := openWithOptions(path, tc.opts)
			if err != nil {
				t.Fatal(err)
			}
			defer db.Close()
			if got := db.session.workMem; got != tc.want {
				t.Fatalf("OpenOptions %+v: session workMem = %d, want %d", tc.opts, got, tc.want)
			}
		})
	}

	// The unbounded/never-spill budget is still reachable — just at runtime, via the setter (0 ⇒
	// unlimited, the runtime convention its MaxCost/TempBuffers siblings share), never as a bare-options
	// zero value. This is the boundary the remap draws: options 0 ⇒ default, runtime 0 ⇒ unlimited.
	db, err := openWithOptions(path, OpenOptions{SkipFsync: true})
	if err != nil {
		t.Fatal(err)
	}
	defer db.Close()
	(&Session{engine: db}).SetWorkMem(0)
	if got := db.session.workMem; got != 0 {
		t.Fatalf("SetWorkMem(0): runtime workMem = %d, want 0 (unlimited)", got)
	}
}
