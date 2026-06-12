package bench

import (
	"fmt"
	"path/filepath"

	"github.com/BurntSushi/toml"
)

// Corpus mirrors bench/corpus/benchmarks.toml (spec/design/benchmarks.md §3).
type Corpus struct {
	SchemaVersion int     `toml:"schema_version"`
	Bench         []Bench `toml:"bench"`
}

// Bench is one benchmark definition.
type Bench struct {
	Name              string              `toml:"name"`
	Description       string              `toml:"description"`
	Dataset           string              `toml:"dataset"`
	Kind              string              `toml:"kind"` // query | write_rollback | write_durable
	SQL               string              `toml:"sql"`
	Warmup            int                 `toml:"warmup"`
	Iterations        int                 `toml:"iterations"`
	Seed              uint64              `toml:"seed"`
	ExpectRowsPerIter int                 `toml:"expect_rows_per_iter"` // 0 = unchecked
	Engines           []string            `toml:"engines"`              // empty = all
	Batch             int                 `toml:"batch"`                // write kinds: statements per iteration
	SetupSQL          []string            `toml:"setup_sql"`            // write kinds: run once before warmup
	SQLOverride       map[string]string   `toml:"sql_override"`
	SetupSQLOverride  map[string][]string `toml:"setup_sql_override"`
	Param             []Param             `toml:"param"`
}

// Param is one $N parameter generator.
type Param struct {
	Gen    string `toml:"gen"` // int_uniform | serial | text
	Min    int64  `toml:"min"`
	Max    int64  `toml:"max"`
	Start  int64  `toml:"start"`
	MinLen int64  `toml:"min_len"`
	MaxLen int64  `toml:"max_len"`
}

// SQLFor returns the bench's SQL for an engine, honoring sql_override.
func (b *Bench) SQLFor(engine string) string {
	if s, ok := b.SQLOverride[engine]; ok {
		return s
	}
	return b.SQL
}

// SetupSQLFor returns the bench's setup statements for an engine, honoring the override.
func (b *Bench) SetupSQLFor(engine string) []string {
	if s, ok := b.SetupSQLOverride[engine]; ok {
		return s
	}
	return b.SetupSQL
}

// RunsOn reports whether the bench applies to an engine (empty allowlist = all).
func (b *Bench) RunsOn(engine string) bool {
	if len(b.Engines) == 0 {
		return true
	}
	for _, e := range b.Engines {
		if e == engine {
			return true
		}
	}
	return false
}

// LoadCorpus parses <corpusDir>/benchmarks.toml.
func LoadCorpus(corpusDir string) (*Corpus, error) {
	var c Corpus
	if _, err := toml.DecodeFile(filepath.Join(corpusDir, "benchmarks.toml"), &c); err != nil {
		return nil, err
	}
	if c.SchemaVersion != 1 {
		return nil, fmt.Errorf("benchmarks.toml: unsupported schema_version %d", c.SchemaVersion)
	}
	for i := range c.Bench {
		b := &c.Bench[i]
		switch b.Kind {
		case "query", "write_rollback", "write_durable":
		default:
			return nil, fmt.Errorf("bench %q: unknown kind %q", b.Name, b.Kind)
		}
		if b.Kind == "write_rollback" && b.Batch <= 0 {
			return nil, fmt.Errorf("bench %q: write_rollback requires batch > 0", b.Name)
		}
	}
	return &c, nil
}

// ParamStream draws the per-iteration argument lists for a bench: one shared PRNG
// consumed continuously across warmup + measured iterations, serial counters advancing
// per statement (spec/design/benchmarks.md §3/§4). Args are int64 or string.
type ParamStream struct {
	params  []Param
	prng    *Prng
	serials []int64
}

// NewParamStream starts the stream for one bench.
func NewParamStream(b *Bench) *ParamStream {
	s := &ParamStream{params: b.Param, prng: NewPrng(b.Seed), serials: make([]int64, len(b.Param))}
	for i, p := range b.Param {
		if p.Gen == "serial" {
			s.serials[i] = p.Start
		}
	}
	return s
}

// Next draws one statement's arguments.
func (s *ParamStream) Next() []any {
	args := make([]any, len(s.params))
	for i, p := range s.params {
		switch p.Gen {
		case "serial":
			args[i] = s.serials[i]
			s.serials[i]++
		case "int_uniform":
			args[i] = s.prng.IntUniform(p.Min, p.Max)
		case "text":
			args[i] = s.prng.Text(p.MinLen, p.MaxLen)
		default:
			panic("unknown param gen " + p.Gen)
		}
	}
	return args
}
