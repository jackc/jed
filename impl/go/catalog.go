package jed

import "strings"

// Column is a column definition: name, declared type, nullability, primary-key flag.
type Column struct {
	Name string
	// Type is the column's declared type — a built-in scalar or a user-defined composite
	// (spec/design/composite.md). The open Type wrapper (CLAUDE.md §4): scalar-only call sites
	// read Type.ScalarTy(); the value codec / resolver branch on Type.IsComposite() (S2+).
	Type Type
	// Decimal is the numeric(p,s) typmod for a decimal column, or nil for a non-decimal column
	// OR an unconstrained numeric (spec/design/decimal.md §2). A constrained decimal column
	// coerces stored values to this precision/scale.
	Decimal    *DecimalTypmod
	PrimaryKey bool
	// NotNull is implied true for a PRIMARY KEY column.
	NotNull bool
	// Default is the column's CONSTANT DEFAULT value, pre-evaluated and type-coerced at CREATE
	// TABLE, or nil if it has no default or an EXPRESSION default (DefaultExpr). A non-nil
	// pointer to a ValNull value is an explicit DEFAULT NULL. Applied for an omitted column or a
	// DEFAULT keyword at INSERT (spec/design/constraints.md §2).
	Default *Value
	// DefaultExpr is the column's EXPRESSION DEFAULT (a non-constant default like uuidv7() or
	// 1 + 1), or nil if it has no default or a constant default (Default). Mutually exclusive
	// with Default. Stored as expression text (re-rendered verbatim at every commit, like a
	// CHECK — spec/fileformat/format.md) plus the parsed expression the write paths resolve and
	// evaluate per row (spec/design/constraints.md §2).
	DefaultExpr *DefaultExpr
}

// DefaultExpr is a column's EXPRESSION DEFAULT (spec/design/constraints.md §2): its persisted
// expression text — written back verbatim at every commit so the catalog bytes are stable
// (spec/fileformat/format.md "Check-expression text") — and the parsed expression the write
// paths resolve (against an empty scope, no columns) and evaluate per inserted row. Modeled on
// CheckConstraint.
type DefaultExpr struct {
	ExprText string
	Expr     Expr
}

// Table is a table definition.
type Table struct {
	Name    string
	Columns []Column
	// PK is the primary-key member column ordinals in KEY order (which may differ from
	// declaration order — constraints.md §3; the v5 catalog persists this list). Empty =
	// no primary key (synthetic rowid keys). The per-column PrimaryKey flag is derived
	// membership convenience; this list is the authority for order.
	PK []int
	// Checks is the table's CHECK constraints in EVALUATION ORDER — ascending byte order
	// of the lowercased name (spec/design/constraints.md §4.4); the on-disk catalog stores
	// them in this same order. Empty for an unchecked table.
	Checks []CheckConstraint
	// Indexes is the table's secondary indexes in ascending lowercased-name order (the
	// catalog's on-disk order and the planner's tie-break order — spec/design/indexes.md).
	Indexes []IndexDef
	// ForeignKeys is the table's FOREIGN KEY constraints in ascending lowercased-name order
	// (the catalog's on-disk order and the child-side evaluation order —
	// spec/design/constraints.md §6.9). Empty for a table with none.
	ForeignKeys []ForeignKey
}

// FkAction is the persisted referential action for a foreign key's `ON DELETE` / `ON UPDATE`
// (spec/design/constraints.md §6.6). Only FkNoAction (the default) and FkRestrict are
// supported — they are identical in jed (no deferrable constraints). The write-actions
// (CASCADE / SET NULL / SET DEFAULT) are rejected 0A000 at CREATE TABLE, so never reach here;
// the on-disk encoding reserves codes for them (format.md).
type FkAction int

const (
	// FkNoAction is NO ACTION (the default), on-disk code 0.
	FkNoAction FkAction = iota
	// FkRestrict is RESTRICT, on-disk code 1.
	FkRestrict
)

// ForeignKey is one resolved FOREIGN KEY constraint of a table (spec/design/constraints.md §6):
// its (per-table constraint-namespace) name, the referencing column ordinals into THIS table in
// list order, the referenced (parent) table name, the referenced column ordinals into the PARENT
// in list order (same length as Columns), and the referential actions. An FK owns no B-tree;
// enforcement probes the parent's PK store or a unique index (§6.4). Held in ascending
// lowercased-name order on the table (the catalog's on-disk order and the child-side evaluation
// order — §6.9).
type ForeignKey struct {
	Name       string
	Columns    []int
	RefTable   string
	RefColumns []int
	OnDelete   FkAction
	OnUpdate   FkAction
}

// IndexDef is one secondary index of a table (spec/design/indexes.md): its
// (relation-namespace) name and the indexed column ordinals in index-key order
// (duplicates allowed — PG). The index's B-tree lives in the snapshot's index-store map,
// keyed by the lowercased name. A Unique index enforces uniqueness over its key tuple
// (NULLS DISTINCT — spec/design/indexes.md §8); it is what backs a UNIQUE constraint
// (spec/design/constraints.md §5).
type IndexDef struct {
	Name    string
	Columns []int
	Unique  bool
}

// CompositeType is a user-defined composite (row) type (spec/design/composite.md): a named,
// ordered list of typed fields, living in the database's type catalog (a database-level object,
// not per-table). Created by CREATE TYPE name AS (field type, …), referenced by name from a
// column's Type. Recursive — a field's Type may itself be a composite (a nested composite,
// persisted by name; spec/fileformat/format.md *Composite-type entry*).
type CompositeType struct {
	// Name is the type name (original case — round-trips what the user typed); looked up
	// case-insensitively.
	Name string
	// Fields are the fields in declaration order (≥ 1).
	Fields []CompositeField
}

// CompositeField is one field of a composite type: its name, type, and declared nullability.
type CompositeField struct {
	Name string
	Type Type
	// Decimal is the decimal numeric(p,s) typmod when Type is decimal, else nil (mirrors Column).
	Decimal *DecimalTypmod
	// NotNull is whether the field was declared NOT NULL.
	NotNull bool
}

// ColType is a fully-resolved storage/codec column type (spec/design/composite.md §4): a scalar,
// or a composite resolved to the codec/coercion tree of its fields. Built ONCE from a catalog Type
// against the snapshot's composite-type definitions (ResolveColType) and held by the TableStore, so
// the value codec and store-coercion never re-walk the type catalog on every row. Recursive — a
// composite field may itself be composite. Go has no sum types, so the composite arm is a non-nil
// Fields slice discriminant: Composite == false means scalar (read Scalar); Composite == true means
// a resolved composite (read Name/Fields). The codec reads only the scalar / field structure; the
// field Typmod / NotNull are consulted by store-coercion (executor).
type ColType struct {
	// Composite discriminates: false ⇒ a scalar (Scalar is meaningful); true ⇒ a composite
	// (Name/Fields are meaningful).
	Composite bool
	// Scalar is the inner scalar type when Composite == false && Elem == nil.
	Scalar ScalarType
	// Name is the (original-case) composite type name when Composite == true, used in
	// store-coercion error messages.
	Name string
	// Fields is the composite type's resolved fields in declaration order when Composite == true.
	Fields []ColField
	// Elem is the resolved element type when this is an array ColType (spec/design/array.md §3),
	// else nil. Structural — the element type is carried inline, recursively.
	Elem *ColType
}

// ColField is one resolved field of a composite ColType — its name, recursively-resolved type, the
// decimal typmod (when the field is decimal), and declared nullability (mirrors CompositeField, but
// with the type fully resolved for the codec/coercion path).
type ColField struct {
	Name    string
	Type    ColType
	Typmod  *DecimalTypmod
	NotNull bool
}

// ScalarColType wraps a scalar type as a (scalar) ColType.
func ScalarColType(s ScalarType) ColType { return ColType{Scalar: s} }

// ResolveColType resolves a catalog Type into a self-contained ColType against the database's
// composite definitions (keyed by lowercased name, the Snapshot.types map). A composite reference is
// looked up case-insensitively and recursively resolved; the lookup is guaranteed to succeed because
// validateCompositeTypes (the two-pass load / CREATE TYPE gate) proved every reference exists and the
// graph is acyclic before any store is built (spec/design/composite.md §3).
func ResolveColType(ty Type, types map[string]*CompositeType) ColType {
	if ty.Array != nil {
		elem := ResolveColType(*ty.Array, types)
		return ColType{Elem: &elem}
	}
	if ty.Comp == nil {
		return ColType{Scalar: ty.Scalar}
	}
	def := types[strings.ToLower(ty.Comp.Name)]
	fields := make([]ColField, len(def.Fields))
	for i, f := range def.Fields {
		fields[i] = ColField{
			Name:    f.Name,
			Type:    ResolveColType(f.Type, types),
			Typmod:  f.Decimal,
			NotNull: f.NotNull,
		}
	}
	return ColType{Composite: true, Name: def.Name, Fields: fields}
}

// CheckConstraint is one CHECK constraint: its (resolved, unique-per-table) name, its
// persisted expression text — written back verbatim at every commit so the catalog bytes
// are stable (spec/fileformat/format.md "Check-expression text") — and the parsed
// expression the write paths resolve and evaluate per candidate row (constraints.md §4).
type CheckConstraint struct {
	Name     string
	ExprText string
	Expr     Expr
}

// ColumnIndex returns the index of the named column (case-insensitive), or -1.
func (t *Table) ColumnIndex(name string) int {
	for i, c := range t.Columns {
		if strings.EqualFold(c.Name, name) {
			return i
		}
	}
	return -1
}

// PKIndices returns the primary-key member columns' indices in KEY order (the explicit
// PK list — the v5 catalog persists key order independent of declaration order). Empty =
// the table has no primary key (synthetic rowid keys).
func (t *Table) PKIndices() []int {
	return t.PK
}

// PrimaryKeyIndex returns the primary-key column's index iff the key is SINGLE-column,
// else -1. The PK pushdown (point lookup / range bound) recognizes single-column keys
// only — a composite-PK table full-scans this slice (spec/design/constraints.md §3) — so
// every pushdown site routes through this accessor and stays sound by construction.
func (t *Table) PrimaryKeyIndex() int {
	idxs := t.PKIndices()
	if len(idxs) == 1 {
		return idxs[0]
	}
	return -1
}
