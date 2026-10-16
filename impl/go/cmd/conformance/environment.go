package main

import (
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"sync"

	jed "github.com/jackc/jed/impl/go"
)

// The harness's Environment (spec/design/environment.md §4): one per corpus file, holding the data
// bundles the file's `# load-collation:` / `# load-timezone:` directives name and the harness's own
// switchable random + clock sources, which implement `# seed:` / `# clock:` / `# clock_advance:` by
// switching to a seeded / manual / advancing source before a record and back to OS entropy / the wall
// clock for a record without one.

// harnessRandom is the harness's random source: OS entropy by default, or a seeded splitmix64 stream
// (reseeded per `# seed:` record).
type harnessRandom struct {
	mu      sync.Mutex
	os      jed.RandomSource
	seeded  *jed.SeededRandom
	useSeed bool
}

func newHarnessRandom() *harnessRandom {
	return &harnessRandom{os: jed.OSRandomSource(), seeded: jed.NewSeededRandom(0)}
}

func (r *harnessRandom) Fill(buf []byte) error {
	r.mu.Lock()
	useSeed := r.useSeed
	r.mu.Unlock()
	if useSeed {
		return r.seeded.Fill(buf)
	}
	return r.os.Fill(buf)
}

// seed switches to the seeded stream, restarted at seed; nil switches back to OS entropy.
func (r *harnessRandom) seed(seed *uint64) {
	r.mu.Lock()
	defer r.mu.Unlock()
	r.useSeed = seed != nil
	if seed != nil {
		r.seeded.Reseed(*seed)
	}
}

// clockMode selects which clock harnessClock reads.
type clockMode int

const (
	clockWall clockMode = iota
	clockManual
	clockAdvancing
)

// harnessClock is the harness's clock source: the wall clock by default, a manual clock set per
// `# clock:` record, or an advancing clock reset per `# clock_advance:` record.
type harnessClock struct {
	mu        sync.Mutex
	mode      clockMode
	wall      jed.ClockSource
	manual    *jed.ManualClock
	advancing *jed.AdvancingClock
}

func newHarnessClock() *harnessClock {
	return &harnessClock{wall: jed.WallClock(), manual: jed.NewManualClock(0), advancing: jed.NewAdvancingClock(0, 0)}
}

func (c *harnessClock) NowMicros() int64 {
	c.mu.Lock()
	mode := c.mode
	c.mu.Unlock()
	switch mode {
	case clockManual:
		return c.manual.NowMicros()
	case clockAdvancing:
		return c.advancing.NowMicros()
	default:
		return c.wall.NowMicros()
	}
}

// set applies a record's clock directives: `# clock_advance:` (an advancing clock) takes precedence
// over `# clock:` (a fixed instant); neither switches back to the wall clock.
func (c *harnessClock) set(fixed *int64, advance *clockAdvance) {
	c.mu.Lock()
	defer c.mu.Unlock()
	switch {
	case advance != nil:
		c.advancing.Reset(advance.start, advance.step)
		c.mode = clockAdvancing
	case fixed != nil:
		c.manual.Set(*fixed)
		c.mode = clockManual
	default:
		c.mode = clockWall
	}
}

// fileEnvironment builds the Environment a corpus file runs under: the harness's switchable sources,
// plus jed's pinned production Unicode (spec/collation/fixtures/unicode.jucd) and/or time-zone
// (spec/tz/fixtures/tzdata.jtz) bundle when the file carries a `# load-collation:` /
// `# load-timezone:` directive. The environment is fixed when the database opens, so the directives
// are found by a scan before it is created; at their own position they only assert availability
// (loadCollation / loadTimezone). A `# fixture:` image may reference a collation, so it loads the
// Unicode bundle too.
func fileEnvironment(lines []string, random *harnessRandom, clock *harnessClock) (*jed.Environment, error) {
	wantUnicode, wantTz := false, false
	for _, raw := range lines {
		line := strings.TrimSpace(raw)
		if _, _, ok := parseLoadCollationDirective(line); ok {
			wantUnicode = true
		}
		if _, ok := parseLoadTimezoneDirective(line); ok {
			wantTz = true
		}
		if _, _, ok := parseFixtureDirective(line); ok {
			wantUnicode = true
		}
	}
	b := jed.NewEnvironmentBuilder()
	b.SetRandomSource(random)
	b.SetClockSource(clock)
	if wantUnicode {
		path := filepath.Join(repoRoot(), "spec", "collation", "fixtures", "unicode.jucd")
		data, err := os.ReadFile(path)
		if err != nil {
			return nil, fmt.Errorf("load-collation: read %s: %w", path, err)
		}
		if err := b.LoadUnicodeData(data); err != nil {
			return nil, fmt.Errorf("load-collation: load unicode.jucd: %w", err)
		}
	}
	if wantTz {
		path := filepath.Join(repoRoot(), "spec", "tz", "fixtures", "tzdata.jtz")
		data, err := os.ReadFile(path)
		if err != nil {
			return nil, fmt.Errorf("load-timezone: read %s: %w", path, err)
		}
		if err := b.LoadTimeZoneData(data); err != nil {
			return nil, fmt.Errorf("load-timezone: load tzdata.jtz: %w", err)
		}
	}
	return b.Build(), nil
}
