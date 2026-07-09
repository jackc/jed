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

// Table is one generated table. An empty Engines allowlist means every engine; a
// non-empty one restricts the table to those engines (the gin dataset's array table is
// jed+postgres only — SQLite has no array type or GIN).
type Table struct {
	Name    string   `toml:"name"`
	Rows    int      `toml:"rows"`
	Seed    uint64   `toml:"seed"`
	Engines []string `toml:"engines"` // empty = all
	Column  []Column `toml:"column"`
	Index   []Index  `toml:"index"`
}

// AppliesTo reports whether the table is generated for an engine.
func (t *Table) AppliesTo(engine string) bool { return appliesTo(t.Engines, engine) }

// Column is one generated column.
type Column struct {
	Name       string `toml:"name"`
	Type       string `toml:"type"` // i64 | i32 | i16 | text | i64[] | i32[] | i16[]
	Gen        string `toml:"gen"`  // serial | int_uniform | text | int_array
	PrimaryKey bool   `toml:"primary_key"`
	Min        int64  `toml:"min"`
	Max        int64  `toml:"max"`
	MinLen     int64  `toml:"min_len"` // text: string length range; int_array: array length range
	MaxLen     int64  `toml:"max_len"`
	ElemMin    int64  `toml:"elem_min"` // int_array: element value range
	ElemMax    int64  `toml:"elem_max"`
}

// Index is one index. Method "" is the default ordered btree; "gin" is an inverted index
// over an array column (CREATE INDEX ... USING gin). Created in every engine unless
// allowlisted.
type Index struct {
	Name    string   `toml:"name"`
	Columns []string `toml:"columns"`
	Method  string   `toml:"method"`  // "" (btree) | "gin"
	Where   string   `toml:"where"`   // "" (full) | a partial-index predicate (indexes.md §9)
	Engines []string `toml:"engines"` // empty = all
}

// AppliesTo reports whether the index is created for an engine.
func (ix *Index) AppliesTo(engine string) bool { return appliesTo(ix.Engines, engine) }

func appliesTo(engines []string, engine string) bool {
	if len(engines) == 0 {
		return true
	}
	for _, e := range engines {
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
		case "i64[]":
			return "bigint[]"
		case "i32[]":
			return "integer[]"
		case "i16[]":
			return "smallint[]"
		}
	case "sqlite":
		// Array columns never reach SQLite — a table carrying one is allowlisted to
		// jed+postgres and skipped here (see bench-setup). This map covers the scalars only.
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
		if !ix.AppliesTo(engine) {
			continue
		}
		using := ""
		if ix.Method != "" {
			using = " USING " + ix.Method
		}
		where := ""
		if ix.Where != "" {
			where = " WHERE " + ix.Where // a partial index (indexes.md §9)
		}
		stmts = append(stmts, fmt.Sprintf("CREATE INDEX %s ON %s%s (%s)%s", ix.Name, t.Name, using, strings.Join(ix.Columns, ", "), where))
	}
	return stmts
}

// RowStream generates the table's rows in the deterministic order of the contract
// (§4): one splitmix64 stream per table, rows 1..N, non-serial columns drawn in
// declared column order. Values are i64, string, or []int64 (int_array).
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
		case "int_array":
			// One length draw, then that many element draws — mirrors text generation.
			n := r.prng.IntUniform(c.MinLen, c.MaxLen)
			arr := make([]int64, n)
			for j := range arr {
				arr[j] = r.prng.IntUniform(c.ElemMin, c.ElemMax)
			}
			vals[i] = arr
		default:
			panic("unknown column gen " + c.Gen)
		}
	}
	return vals
}
