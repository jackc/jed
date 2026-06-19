package bench

import (
	"fmt"
	"path/filepath"
	"strings"

	"github.com/BurntSushi/toml"
)

// Datasets mirrors bench/corpus/datasets.toml (spec/design/benchmarks.md §4).
type Datasets struct {
	SchemaVersion    int       `toml:"schema_version"`
	GeneratorVersion int       `toml:"generator_version"`
	Dataset          []Dataset `toml:"dataset"`
}

// Dataset is one named benchmark database.
type Dataset struct {
	Name  string  `toml:"name"`
	Table []Table `toml:"table"`
}

// Table is one generated table.
type Table struct {
	Name   string   `toml:"name"`
	Rows   int      `toml:"rows"`
	Seed   uint64   `toml:"seed"`
	Column []Column `toml:"column"`
	Index  []Index  `toml:"index"`
}

// Column is one generated column.
type Column struct {
	Name       string `toml:"name"`
	Type       string `toml:"type"` // i64 | i32 | i16 | text
	Gen        string `toml:"gen"`  // serial | int_uniform | text
	PrimaryKey bool   `toml:"primary_key"`
	Min        int64  `toml:"min"`
	Max        int64  `toml:"max"`
	MinLen     int64  `toml:"min_len"`
	MaxLen     int64  `toml:"max_len"`
}

// Index is one secondary index, created in every engine unless allowlisted.
type Index struct {
	Name    string   `toml:"name"`
	Columns []string `toml:"columns"`
	Engines []string `toml:"engines"` // empty = all
}

// AppliesTo reports whether the index is created for an engine.
func (ix *Index) AppliesTo(engine string) bool {
	if len(ix.Engines) == 0 {
		return true
	}
	for _, e := range ix.Engines {
		if e == engine {
			return true
		}
	}
	return false
}

// LoadDatasets parses <corpusDir>/datasets.toml.
func LoadDatasets(corpusDir string) (*Datasets, error) {
	var d Datasets
	if _, err := toml.DecodeFile(filepath.Join(corpusDir, "datasets.toml"), &d); err != nil {
		return nil, err
	}
	if d.SchemaVersion != 1 {
		return nil, fmt.Errorf("datasets.toml: unsupported schema_version %d", d.SchemaVersion)
	}
	return &d, nil
}

// Find returns the named dataset.
func (d *Datasets) Find(name string) (*Dataset, error) {
	for i := range d.Dataset {
		if d.Dataset[i].Name == name {
			return &d.Dataset[i], nil
		}
	}
	return nil, fmt.Errorf("datasets.toml: no dataset %q", name)
}

// columnType maps a spec type to an engine's DDL type (the fixed map in
// spec/design/benchmarks.md §4). The SQLite pk maps to INTEGER PRIMARY KEY (the rowid
// alias — SQLite's idiomatic fast path).
func columnType(engine string, c Column) string {
	switch engine {
	case "jed", "postgres":
		switch c.Type {
		case "i64":
			return "bigint"
		case "i32":
			return "integer"
		case "i16":
			return "smallint"
		case "text":
			return "text"
		}
	case "sqlite":
		if c.Type == "text" {
			return "TEXT"
		}
		return "INTEGER"
	}
	panic(fmt.Sprintf("no type map for engine %q type %q", engine, c.Type))
}

// DDL derives the CREATE TABLE + CREATE INDEX statements for one engine — the dataset
// spec is declarative; literal SQL never appears in datasets.toml.
func (t *Table) DDL(engine string) []string {
	cols := make([]string, len(t.Column))
	for i, c := range t.Column {
		def := c.Name + " " + columnType(engine, c)
		if c.PrimaryKey {
			def += " PRIMARY KEY"
		}
		cols[i] = def
	}
	stmts := []string{fmt.Sprintf("CREATE TABLE %s (%s)", t.Name, strings.Join(cols, ", "))}
	for _, ix := range t.Index {
		if ix.AppliesTo(engine) {
			stmts = append(stmts, fmt.Sprintf("CREATE INDEX %s ON %s (%s)", ix.Name, t.Name, strings.Join(ix.Columns, ", ")))
		}
	}
	return stmts
}

// RowStream generates the table's rows in the deterministic order of the contract
// (§4): one splitmix64 stream per table, rows 1..N, non-serial columns drawn in
// declared column order. Values are i64 or string.
type RowStream struct {
	table *Table
	prng  *Prng
	row   int
}

// NewRowStream starts generation for one table.
func NewRowStream(t *Table) *RowStream { return &RowStream{table: t, prng: NewPrng(t.Seed)} }

// Next returns the next row, or nil after the last.
func (r *RowStream) Next() []any {
	if r.row >= r.table.Rows {
		return nil
	}
	r.row++
	vals := make([]any, len(r.table.Column))
	for i, c := range r.table.Column {
		switch c.Gen {
		case "serial":
			vals[i] = int64(r.row)
		case "int_uniform":
			vals[i] = r.prng.IntUniform(c.Min, c.Max)
		case "text":
			vals[i] = r.prng.Text(c.MinLen, c.MaxLen)
		default:
			panic("unknown column gen " + c.Gen)
		}
	}
	return vals
}
