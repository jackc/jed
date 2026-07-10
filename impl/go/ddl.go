package jed

import (
	"bytes"
	"fmt"
	"slices"
	"sort"
	"strconv"
	"strings"
)

// Statement dispatch and DDL execution. This file holds the post-privilege statement traffic cop
// (stmtKind/dispatchStmt/dispatchStmtBody that route a parsed statement to its executor) and the DDL
// executors themselves: CREATE/DROP TABLE (executeCreateTable with CHECK/DEFAULT/serial resolution,
// executeDropTable), CREATE/DROP INDEX, CREATE/DROP TYPE, and CREATE/DROP/ALTER SEQUENCE, plus the
// sequence-definition builders (buildSequenceDef/applySeqAlter/chooseSerialSeqName).

// stmtKind is a short label for a statement kind, for the 25006 read-only-violation message (the
// message text is informational — never matched; spec/design/conformance.md §2).
func stmtKind(stmt statement) string {
	switch {
	case stmt.CreateTable != nil:
		return "CREATE TABLE"
	case stmt.DropTable != nil:
		return "DROP TABLE"
	case stmt.CreateIndex != nil:
		return "CREATE INDEX"
	case stmt.DropIndex != nil:
		return "DROP INDEX"
	case stmt.CreateType != nil:
		return "CREATE TYPE"
	case stmt.DropType != nil:
		return "DROP TYPE"
	case stmt.CreateSequence != nil:
		return "CREATE SEQUENCE"
	case stmt.AlterSequence != nil:
		return "ALTER SEQUENCE"
	case stmt.DropSequence != nil:
		return "DROP SEQUENCE"
	case stmt.Insert != nil:
		return "INSERT"
	case stmt.Update != nil:
		return "UPDATE"
	case stmt.Delete != nil:
		return "DELETE"
	case stmt.Explain != nil:
		return "EXPLAIN"
	default:
		return "statement"
	}
}

// dispatchStmt routes one parsed statement to its executor. The autocommit transaction handling
// (capture / durable commit / rollback-on-error) lives in ExecuteStmtParams.
func (db *engine) dispatchStmt(stmt statement, params []Value) (outcome, error) {
	// Lifetime budget admission (spec/design/session.md §5.4): once the session's cumulative cost has
	// reached lifetime_max_cost, every further statement is rejected 54P02 BEFORE it can accrue —
	// checked ahead of privileges/existence, so an exhausted session runs nothing. A no-op when the
	// budget is unlimited (the default). Transaction control (BEGIN/COMMIT/ROLLBACK) never reaches
	// dispatch (handled earlier), so an exhausted session can still close out an open block.
	if err := db.checkLifetimeAdmission(); err != nil {
		return outcome{}, err
	}
	// Authorization (spec/design/session.md §5.3): enforce the session's privilege envelope before the
	// statement runs — DDL gated by allowDDL, DML by per-table/per-function privileges, all 42501.
	// Skipped on a fully-permissive session (the default), so the common path pays nothing. The
	// physical access-mode gate (25006) is checked earlier in ExecuteStmtParams, so it wins when both
	// apply.
	if err := db.checkPrivileges(stmt); err != nil {
		return outcome{}, err
	}
	out, err := db.dispatchStmtBody(stmt, params)
	// Keep each GiST index's resident R-tree current: after a statement that mutated the main image,
	// rebuild it from the (now-updated) leaf store so the next read descends a fresh tree (gist.md
	// §3/§4.1). A no-op for reads / temp-only writes (mainDirty unset).
	if err == nil {
		if herr := db.rebuildMainGistTreesIfDirty(); herr != nil {
			return outcome{}, herr
		}
	}
	return out, err
}

// rebuildMainGistTreesIfDirty refreshes the main working snapshot's resident GiST trees iff the
// current statement mutated the main image (gist.md §3/§4.1). Gated on mainDirty (set by the
// statement's own working() writes): a read or a temp-only write leaves it unset, so this is a no-op
// and never forces a spurious main-image persist (the temp-no-file-write invariant). GiST on a temp
// table is 0A000 this slice, so only the main working snapshot is refreshed.
func (db *engine) rebuildMainGistTreesIfDirty() error {
	if db.session.tx != nil && db.session.tx.mainDirty {
		return db.session.tx.working.rebuildGistTrees()
	}
	return nil
}

func (db *engine) dispatchStmtBody(stmt statement, params []Value) (outcome, error) {
	switch {
	case stmt.CreateTable != nil:
		if err := rejectParamsForDDL(params); err != nil {
			return outcome{}, err
		}
		return db.executeCreateTable(stmt.CreateTable)
	case stmt.DropTable != nil:
		if err := rejectParamsForDDL(params); err != nil {
			return outcome{}, err
		}
		return db.executeDropTable(stmt.DropTable)
	case stmt.CreateIndex != nil:
		if err := rejectParamsForDDL(params); err != nil {
			return outcome{}, err
		}
		return db.executeCreateIndex(stmt.CreateIndex)
	case stmt.DropIndex != nil:
		if err := rejectParamsForDDL(params); err != nil {
			return outcome{}, err
		}
		return db.executeDropIndex(stmt.DropIndex)
	case stmt.CreateType != nil:
		if err := rejectParamsForDDL(params); err != nil {
			return outcome{}, err
		}
		return db.executeCreateType(stmt.CreateType)
	case stmt.DropType != nil:
		if err := rejectParamsForDDL(params); err != nil {
			return outcome{}, err
		}
		return db.executeDropType(stmt.DropType)
	case stmt.CreateSequence != nil:
		if err := rejectParamsForDDL(params); err != nil {
			return outcome{}, err
		}
		return db.executeCreateSequence(stmt.CreateSequence)
	case stmt.AlterSequence != nil:
		if err := rejectParamsForDDL(params); err != nil {
			return outcome{}, err
		}
		return db.executeAlterSequence(stmt.AlterSequence)
	case stmt.DropSequence != nil:
		if err := rejectParamsForDDL(params); err != nil {
			return outcome{}, err
		}
		return db.executeDropSequence(stmt.DropSequence)
	case stmt.Insert != nil:
		return db.executeInsert(stmt.Insert, params, cteCtx{})
	case stmt.Select != nil:
		return db.executeSelect(stmt.Select, params)
	case stmt.SetOp != nil:
		return db.executeSetOp(stmt.SetOp, params)
	case stmt.With != nil:
		return db.executeWith(stmt.With, params)
	case stmt.Update != nil:
		return db.executeUpdate(stmt.Update, params, cteCtx{})
	case stmt.Delete != nil:
		return db.executeDelete(stmt.Delete, params, cteCtx{})
	case stmt.Explain != nil:
		return db.executeExplain(stmt.Explain, params)
	default:
		return outcome{}, newError(SyntaxError, "empty statement")
	}
}

// rejectParamsForDDL errors (42601) if bind parameters are supplied to a CREATE/DROP TABLE
// (which has no expressions to bind — spec/design/api.md §5).
func rejectParamsForDDL(params []Value) error {
	if len(params) > 0 {
		return newError(SyntaxError, "bind parameters are not allowed in a DDL statement")
	}
	return nil
}

// executeCreateTable analyzes and runs a CREATE TABLE: resolve each column's type
// name, enforce a single primary key across both forms (column-level and the
// table-level PRIMARY KEY (a, b, ...) constraint — which is implicitly NOT NULL per
// member), reject duplicate table and column names, then register the table.
// Constraint checks mirror PostgreSQL's order (oracle-probed, constraints.md §3):
// a second primary key traps 42P16 before its members resolve; members resolve
// left to right (unknown 42703, repeated 42701); then the jed narrowings — the
// declaration-order rule and the per-member key-type gate — trap 0A000.
func (db *engine) executeCreateTable(ct *createTable) (outcome, error) {
	// A session-local temporary table (spec/design/temp-tables.md) is built exactly like a persistent
	// one but registered into the session temp snapshot at the end (§2), so it makes zero file writes.
	// FOREIGN KEY on a temp table is deferred this slice (§8) — rejected HERE, before any persistent
	// parent resolves, so the error is a clean 0A000. The other temp narrowings (composite/collated
	// columns, serial/IDENTITY) are checked just before registration, once the columns are built.
	//
	// Resolve the optional database qualifier (attached-databases.md §3, Slice 1b): `main`/`temp` fold
	// into the implicit scope (main = bare persistent, temp = TEMP); a host-attached name routes the new
	// table INTO that attachment's working snapshot (§6). TEMP with an explicit database is
	// contradictory unless the database IS `temp` (42601).
	targetTemp := ct.Temp
	attachName := ""
	if ct.DB != nil {
		switch strings.ToLower(*ct.DB) {
		case "main":
			if ct.Temp {
				return outcome{}, newError(SyntaxError, `cannot create a TEMP table in database "main"`)
			}
		case "temp":
			targetTemp = true
		default:
			if ct.Temp {
				return outcome{}, newError(SyntaxError, "cannot create a TEMP table in an attached database")
			}
			attachName = strings.ToLower(*ct.DB)
			if db.attachReadSnap(attachName) == nil {
				return outcome{}, newError(UndefinedTable, `database "`+*ct.DB+`" is not attached`)
			}
			// A DDL write to a READ-ONLY attachment is 25006 before any work (attached-databases.md §4).
			if err := db.checkAttachmentWritable(ct.DB); err != nil {
				return outcome{}, err
			}
		}
	}
	if targetTemp && len(ct.Excludes) > 0 {
		// An EXCLUDE constraint's backing GiST index would live on the temp snapshot — deferred with
		// the rest of the GiST-on-temp narrowing (spec/design/gist.md §11), a clean 0A000.
		return outcome{}, newError(FeatureNotSupported, "an EXCLUDE constraint on a temporary table is not yet supported")
	}
	if targetTemp && len(ct.ForeignKeys) > 0 {
		return outcome{}, newError(FeatureNotSupported, "FOREIGN KEY on a temporary table is not yet supported")
	}
	// Deferred narrowings on an attached-database table this slice (attached-databases.md §8), each a
	// clean 0A000 before any column work: FOREIGN KEY and EXCLUDE (their probe/backing structures would
	// need cross-scope catalog access this slice does not thread). Serial/IDENTITY and composite/collated
	// columns are checked just before registration, once the columns are built (as for temp).
	if attachName != "" {
		if len(ct.ForeignKeys) > 0 {
			return outcome{}, newError(FeatureNotSupported, "FOREIGN KEY on an attached-database table is not supported yet")
		}
		if len(ct.Excludes) > 0 {
			return outcome{}, newError(FeatureNotSupported, "an EXCLUDE constraint on an attached-database table is not supported yet")
		}
	}
	if err := checkReservedName("table", ct.Name); err != nil {
		return outcome{}, err
	}
	// The relation namespace is shared between tables and indexes (indexes.md §2), so a CREATE TABLE
	// colliding with either kind is the same 42P07 — PG's "relation" word. For a bare/main/temp target
	// relationExists is temp-aware (a temp name collides with temp + persistent alike — temp-tables.md
	// §3); an attachment target checks its OWN snapshot's namespace (each attached database is
	// independent, §3).
	if attachName != "" {
		as := db.attachReadSnap(attachName)
		if _, ok := as.table(ct.Name); ok {
			return outcome{}, newError(DuplicateTable, "relation already exists: "+ct.Name)
		}
		if _, _, ok := as.findIndex(ct.Name); ok {
			return outcome{}, newError(DuplicateTable, "relation already exists: "+ct.Name)
		}
	} else if db.relationExists(ct.Name) {
		return outcome{}, newError(DuplicateTable, "relation already exists: "+ct.Name)
	}

	columns := make([]catColumn, 0, len(ct.Columns))
	// pk is the primary-key member ordinals in KEY order (constraints.md §3): the
	// column-level form is the one-member case; the table-level list below records its
	// own order.
	var pk []int
	pkSeen := false
	// The OWNED sequences a serial column desugars to (spec/design/sequences.md §12), collected
	// during the column walk and staged into the working snapshot only after the whole CREATE TABLE
	// validates — so a later failure (e.g. a bad CHECK) discards them with the statement.
	var pendingSerials []*sequenceDef
	for _, def := range ct.Columns {
		for _, c := range columns {
			if strings.EqualFold(c.Name, def.Name) {
				return outcome{}, newError(DuplicateColumn, "duplicate column name: "+def.Name)
			}
		}
		// Resolve the column type: a built-in scalar, or a user-defined composite referenced by name
		// (spec/design/composite.md §3). An unknown name is 42704. A composite column carries no
		// typmod (the composite's fields carry their own); a type modifier written on a composite
		// column is rejected (0A000). A composite column is storable (S3) but never keyable — the PK
		// gate below rejects it 0A000 (§6).
		// A serial / bigserial / smallserial pseudo-type (spec/design/sequences.md §12): CREATE TABLE
		// sugar for an integer column that is NOT NULL with a DEFAULT nextval(...) backed by a
		// newly-created OWNED sequence. Here we only resolve the underlying integer type; the
		// desugaring (the owned sequence + default + NOT NULL force) happens below. serial[] is NOT a
		// serial column (it falls to the array branch as an unknown element type — §12.1).
		serialKind, isSerial := serialPseudoType(def.TypeName)
		var colType dataType
		var decimal *decimalTypmod
		var varcharLen *uint32
		isComposite := false
		isArray := false
		isRange := false
		if isSerial {
			// A serial column takes no typmod (serial(5) is 42601) and no [] (the array branch).
			if def.TypeMod != nil {
				return outcome{}, newError(SyntaxError,
					"type modifier is not allowed for type "+def.TypeName)
			}
			colType = scalarT(serialKind)
		} else if base, ok := strings.CutSuffix(def.TypeName, "[]"); ok {
			// An array column (spec/design/array.md §3). The element type is a scalar or a
			// previously-defined composite (array-of-composite, §12 AC1 — element_type_code 14 +
			// name); a nested-array element and an array typmod (numeric(p,s)[]) stay deferred (0A000).
			if def.TypeMod != nil {
				return outcome{}, newError(FeatureNotSupported,
					"a type modifier on an array type is not supported yet")
			}
			if elemScalar, scalarOK := scalarTypeFromName(base); scalarOK {
				colType = arrayT(scalarT(elemScalar))
			} else if ctype := db.readSnap().compositeType(base); ctype != nil {
				colType = arrayT(compositeT(ctype.Name))
			} else {
				return outcome{}, newError(UndefinedObject, "type does not exist: "+base)
			}
			isArray = true
		} else if rdesc, ok := rangeByName(def.TypeName); ok {
			// A range column (spec/design/ranges.md §3): structural like array, the element carried
			// inline. A range takes no typmod (numrange(10,2) is not a thing — the element is the
			// unconstrained subtype), so a type modifier is rejected.
			if def.TypeMod != nil {
				return outcome{}, newError(FeatureNotSupported,
					"a type modifier on a range type is not supported")
			}
			colType = rangeT(scalarT(elementScalar(rdesc)))
			isRange = true
		} else if _, ok := scalarTypeFromName(def.TypeName); ok {
			ty, d, vl, err := resolveTypeAndTypmod(def.TypeName, def.TypeMod)
			if err != nil {
				return outcome{}, err
			}
			// jsonpath is literal-only this slice (P1a) — a jsonpath COLUMN is 0A000, like a J0-stage
			// json column (a storable jsonpath is a follow-on).
			if ty == scalarJsonPath {
				return outcome{}, newError(FeatureNotSupported, "a jsonpath column is not supported yet")
			}
			colType = scalarT(ty)
			decimal = d
			varcharLen = vl
		} else if ctype := db.readSnap().compositeType(def.TypeName); ctype != nil {
			if def.TypeMod != nil {
				return outcome{}, newError(FeatureNotSupported,
					"a type modifier is not supported for composite type "+def.TypeName)
			}
			colType = compositeT(ctype.Name)
			isComposite = true
		} else {
			return outcome{}, newError(UndefinedObject, "type does not exist: "+def.TypeName)
		}
		if def.PrimaryKey {
			// The key-encodable scalars may be a PRIMARY KEY. The fixed-width ones — integers,
			// boolean (bool-byte §2.9), uuid (uuid-raw16 §2.7), timestamp/timestamptz (i64
			// int-be-signflip, timestamp.md §6), date (i32, date.md §5), interval (interval-span-i128,
			// the 16-byte span key §2.10) — plus the variable-width text/bytea (…-terminated-escape
			// §2.4/§2.6) and decimal (decimal-order-preserving §2.5), all self-delimiting so they
			// compose in composite keys / index suffixes — plus the range container (range-bounds
			// §2.11, the first container key) and the array container (array-elements-terminated
			// §2.14, the second container key — keyable when its element is a key-encodable scalar,
			// isArrayKeyable, INCLUDING a float element since the §2.8 lift) — plus float itself
			// (float-order-preserving §2.8, the last scalar to become keyable). Still 0A000: only a
			// composite-element array and the recursive composite container.
			if isComposite || (isArray && !isArrayKeyable(colType)) {
				// A composite PRIMARY KEY (composite.md §6) or a non-keyable array PRIMARY KEY (a
				// composite element) is rejected 0A000. colType.CanonicalName() gives the
				// canonical type name (e.g. addr[], even when declared with an alias).
				return outcome{}, newError(FeatureNotSupported,
					"a "+colType.CanonicalName()+" primary key is not supported yet")
			}
			// A range / keyable array is a container key (encoding.md §2.11/§2.14); every other
			// keyable column is a scalar, gated here.
			if !isRange && !isArray {
				if ty := colType.Scalar; !ty.IsInteger() && !ty.IsBool() && !ty.IsText() && !ty.IsBytea() && !ty.IsDecimal() && !ty.IsUuid() && !ty.IsTimestamp() && !ty.IsTimestamptz() && !ty.IsDate() && !ty.IsInterval() && !ty.IsFloat() {
					return outcome{}, newError(FeatureNotSupported,
						"a "+ty.CanonicalName()+" primary key is not supported yet")
				}
			}
			if pkSeen {
				return outcome{}, newError(InvalidTableDefinition,
					"multiple primary keys for table "+ct.Name+" are not allowed")
			}
			pkSeen = true
			pk = append(pk, len(columns)) // this column's ordinal (appended below)
		}
		// Classify the DEFAULT by syntactic form (constraints.md §2). A bad default fails at
		// CREATE TABLE either way; NOT NULL is NOT enforced here (notNull=false), so a DEFAULT
		// NULL on a NOT NULL column is accepted and traps 23502 only when applied.
		//   - a bare literal is pre-evaluated + type-coerced to a constant value (the fast-path:
		//     out of range 22003, cross-family 42804, decimal rounded to typmod);
		//   - any other expression is validated (structural pre-walk, then resolved against an
		//     EMPTY scope — a default may not reference a column — then its result type is
		//     checked assignable to the column, 42804) and stored as text for per-row eval.
		var defaultVal *Value
		var defaultExpr *defaultExprDef
		var identityKind *identityKind
		// A serial pseudo-type OR a GENERATED … AS IDENTITY constraint both desugar to an
		// auto-numbered column: an OWNED sequence + a synthesized DEFAULT nextval(...) + NOT NULL
		// (sequences.md §12/§13). Identity additionally records ALWAYS/BY DEFAULT and gates the
		// column type to i16/i32/i64.
		if isSerial || def.Identity != nil {
			// IDENTITY type gate: the declared column type must be smallint/integer/bigint
			// (sequences.md §13.1). serial's type is the pseudo-type (always integer), so this only
			// bites an identity column written on a non-integer type.
			if def.Identity != nil && !colType.IsInteger() {
				return outcome{}, newError(InvalidParameterValue,
					"identity column type must be smallint, integer, or bigint")
			}
			// Conflicts (42601, sequences.md §13.2). An explicit DEFAULT — or a serial type, itself a
			// synthesized default — alongside IDENTITY is "both default and identity"; a serial column
			// with its own explicit DEFAULT is "multiple default values" (the S3 message, unchanged).
			if def.Identity != nil && (def.Default != nil || isSerial) {
				return outcome{}, newError(SyntaxError, fmt.Sprintf(
					"both default and identity specified for column %s of table %s", def.Name, ct.Name,
				))
			}
			if isSerial && def.Default != nil {
				return outcome{}, newError(SyntaxError, fmt.Sprintf(
					"multiple default values specified for column %s of table %s", def.Name, ct.Name,
				))
			}
			// Create the OWNED sequence — a default ascending i64 for serial, or the IDENTITY column's
			// `( seq_options )` (defaulting the same way) — and synthesize the DEFAULT nextval(...)
			// expression default (format_version 8 mechanism).
			seqName := db.chooseSerialSeqName(ct.Name, def.Name, pendingSerials)
			owner := &seqOwner{Table: ct.Name, Column: uint16(len(columns))} // this column's ordinal
			var opts seqOptions
			if def.Identity != nil {
				opts = def.Identity.Options
			}
			// The owned sequence's data type follows the column (§14): serial → the pseudo-type,
			// identity → the column type. An explicit `AS` inside the identity `( … )` options
			// conflicts with that — 42601 (PG: "conflicting or redundant options"). serial carries no
			// parsed options, so this only fires for identity.
			if opts.DataType != "" {
				return outcome{}, newError(SyntaxError, "conflicting or redundant options")
			}
			seqScalar := serialKind
			if !isSerial {
				seqScalar = colType.ScalarTy()
			}
			seqDtype, ok := seqDataTypeForScalar(seqScalar)
			if !ok {
				// Unreachable: a serial / identity column is i16/i32/i64 (gated above).
				return outcome{}, newError(InvalidParameterValue,
					"serial / identity column is i16/i32/i64")
			}
			opts.DataType = seqDtype.PgName()
			seqDef, err := buildSequenceDef(seqName, opts, owner)
			if err != nil {
				return outcome{}, err
			}
			pendingSerials = append(pendingSerials, seqDef)
			// Render the synthetic default exactly as the parser would the equivalent
			// DEFAULT nextval('<seqName>') (space-joined tokens — the canonical expression-text form),
			// so the in-memory expr matches what reload re-parses. The seqName is a lowercased
			// identifier-derived name, so the quoting is always safe.
			exprText := "nextval ( '" + strings.ReplaceAll(seqName, "'", "''") + "' )"
			expr, err := parseExpression(exprText)
			if err != nil {
				return outcome{}, err
			}
			defaultExpr = &defaultExprDef{ExprText: exprText, Expr: expr}
			if def.Identity != nil {
				k := identityByDefault
				if def.Identity.Always {
					k = identityAlways
				}
				identityKind = &k
			}
		} else if isComposite || isArray || isRange {
			// A DEFAULT on a composite-, array-, or range-typed column is not supported this slice
			// (composite.md §12 / array.md §12 / ranges.md §8).
			if def.Default != nil {
				return outcome{}, newError(FeatureNotSupported,
					"a DEFAULT on a composite-, array-, or range-typed column is not supported yet")
			}
		} else if def.Default != nil {
			ty := colType.Scalar
			// A clock-relative date string DEFAULT ('today'/'now'/…) must NOT fold at CREATE
			// TABLE: it routes to the EXPRESSION path below, re-resolved to the STABLE
			// reDateClock node and evaluated per INSERT — where PostgreSQL folds the literal to
			// the table-creation day, the documented fold-footgun divergence (date.md §6).
			// 'epoch' and every ordinary date string stay foldable constants.
			clockDefault := ty == scalarDate && def.Default.Expr.Kind == exprLiteral &&
				def.Default.Expr.Literal.Kind == literalText &&
				dateClockIsRelative(def.Default.Expr.Literal.Str)
			if def.Default.Expr.Kind == exprLiteral && !clockDefault {
				dv, err := storeValue(literalToValue(*def.Default.Expr.Literal), ty, decimal, varcharLen, false, def.Name)
				if err != nil {
					return outcome{}, err
				}
				defaultVal = &dv
			} else {
				if err := rejectDefaultStructure(def.Default.Expr); err != nil {
					return outcome{}, err
				}
				_, rt, err := resolve(emptyScope(db), def.Default.Expr, &ty, &aggCtx{collecting: false}, &paramTypes{})
				if err != nil {
					return outcome{}, err
				}
				if !assignableTo(rt, ty) {
					return outcome{}, typeError(fmt.Sprintf(
						"column %s is of type %s but default expression is of type %s",
						def.Name, ty.CanonicalName(), rtName(rt),
					))
				}
				defaultExpr = &defaultExprDef{ExprText: def.Default.Text, Expr: def.Default.Expr}
			}
		}
		// The column's effective collation, frozen now (spec/design/collation.md §1). An explicit
		// COLLATE "name" is text-only (42804) and must name a loaded collation or C (42704); a text
		// column without a clause inherits the per-database default. A C effective collation stores
		// as "" (the fast path).
		collation := ""
		if def.Collation != "" {
			if !colType.IsText() {
				return outcome{}, typeError(fmt.Sprintf(
					"collations are not supported by type %s", colType.CanonicalName(),
				))
			}
			if _, err := resolveCollationName(db, def.Collation); err != nil {
				return outcome{}, err
			}
			if def.Collation != "C" {
				collation = def.Collation
			}
		} else if colType.IsText() {
			collation = db.readSnap().defaultCollation
		}
		columns = append(columns, catColumn{
			Name:       def.Name,
			Type:       colType,
			Decimal:    decimal,
			VarcharLen: varcharLen,
			PrimaryKey: def.PrimaryKey,
			// PRIMARY KEY ⇒ NOT NULL; a serial or IDENTITY column is NOT NULL too (sequences.md §12/§13).
			NotNull:     def.PrimaryKey || def.NotNull || isSerial || def.Identity != nil,
			Default:     defaultVal,
			DefaultExpr: defaultExpr,
			Identity:    identityKind,
			Collation:   collation,
		})
	}

	// Table-level PRIMARY KEY (a, b, ...) constraints (constraints.md §3). Check order
	// mirrors PostgreSQL (oracle-probed): a second primary key is 42P16 before its
	// members resolve; members resolve left to right (42703 unknown, 42701 repeated).
	// The LIST order is the KEY order — it may differ from declaration order (the v5
	// catalog persists the ordinal list; the old 0A000 narrowing is lifted). The
	// per-member key-type gate (0A000) remains.
	for _, pkList := range ct.TablePKs {
		if pkSeen {
			return outcome{}, newError(InvalidTableDefinition,
				"multiple primary keys for table "+ct.Name+" are not allowed")
		}
		pkSeen = true
		indices := make([]int, 0, len(pkList))
		for _, name := range pkList {
			idx := -1
			for i := range columns {
				if strings.EqualFold(columns[i].Name, name) {
					idx = i
					break
				}
			}
			if idx < 0 {
				return outcome{}, newError(UndefinedColumn,
					"column "+name+" named in key does not exist")
			}
			if slices.Contains(indices, idx) {
				return outcome{}, newError(DuplicateColumn,
					"column "+name+" appears twice in primary key constraint")
			}
			indices = append(indices, idx)
		}
		for _, i := range indices {
			ty := columns[i].Type
			if !ty.IsInteger() && !ty.IsBool() && !ty.IsText() && !ty.IsBytea() && !ty.IsDecimal() && !ty.IsUuid() && !ty.IsTimestamp() && !ty.IsTimestamptz() && !ty.IsDate() && !ty.IsInterval() && !ty.IsFloat() && !ty.IsRange() && !isArrayKeyable(ty) {
				return outcome{}, newError(FeatureNotSupported,
					"a "+ty.CanonicalName()+" primary key is not supported yet")
			}
			columns[i].PrimaryKey = true
			columns[i].NotNull = true // PRIMARY KEY ⇒ NOT NULL, per member
		}
		pk = indices
	}

	// UNIQUE constraints (constraints.md §5.1): resolve members in textual definition
	// order, AFTER the PRIMARY KEY constraints and BEFORE any CHECK validates (PG's
	// order, oracle-probed — transformIndexConstraint runs first). Each member must exist
	// (42703, PG's "named in key" wording), appear once (42701), and be of a key-encodable
	// type (0A000 — the same narrowing as a PK member / index key column; unlike a PK
	// member it stays nullable). Folding + naming happen LAST (after check naming),
	// mirroring PG's index_create-at-execution timing.
	type resolvedUnique struct {
		name string
		cols []int
	}
	runiques := make([]resolvedUnique, 0, len(ct.Uniques))
	for _, u := range ct.Uniques {
		indices := make([]int, 0, len(u.Columns))
		for _, cname := range u.Columns {
			idx := -1
			for i := range columns {
				if strings.EqualFold(columns[i].Name, cname) {
					idx = i
					break
				}
			}
			if idx < 0 {
				return outcome{}, newError(UndefinedColumn,
					"column "+cname+" named in key does not exist")
			}
			if slices.Contains(indices, idx) {
				return outcome{}, newError(DuplicateColumn,
					"column "+cname+" appears twice in unique constraint")
			}
			indices = append(indices, idx)
		}
		for _, i := range indices {
			ty := columns[i].Type
			if !ty.IsInteger() && !ty.IsBool() && !ty.IsText() && !ty.IsBytea() && !ty.IsDecimal() && !ty.IsUuid() && !ty.IsTimestamp() && !ty.IsTimestamptz() && !ty.IsDate() && !ty.IsInterval() && !ty.IsFloat() && !ty.IsRange() && !isArrayKeyable(ty) {
				return outcome{}, newError(FeatureNotSupported,
					"a "+ty.CanonicalName()+" unique constraint member is not supported yet")
			}
		}
		runiques = append(runiques, resolvedUnique{name: u.Name, cols: indices})
	}

	// CHECK constraints (constraints.md §4). All validation runs first, in textual
	// definition order, AFTER the PRIMARY KEY constraints resolved (PG's order,
	// oracle-probed); naming follows in a second pass, so a 42703 in a later check fires
	// before a 42710 between earlier ones. Resolution needs a catalog *Table, so build it
	// now (checks attach below, before putTable).
	table := &catTable{Name: ct.Name, Columns: columns, PK: pk}
	for i := range ct.Checks {
		def := &ct.Checks[i]
		// Structural rejections first (a single pre-walk — a documented micro-order
		// divergence from PG, which interleaves them with name/type resolution): subquery
		// 0A000, aggregate 42803, bind parameter 42P02 (constraints.md §4.1).
		if err := rejectCheckStructure(def.Expr); err != nil {
			return outcome{}, err
		}
		s := singleScope(db, table)
		_, ty, err := resolve(s, def.Expr, nil, &aggCtx{collecting: false}, &paramTypes{})
		if err != nil {
			return outcome{}, err
		}
		if ty.kind != rtBool && ty.kind != rtNull {
			return outcome{}, typeError("argument of CHECK must be boolean")
		}
	}
	// Naming (constraints.md §4.3): a single pass in textual order. An explicit name is
	// used as written; a derived name is built from the LOWERCASED table/column names —
	// `<table>_<col>_check` when the expression references exactly one distinct column,
	// else `<table>_check` — suffixed with the smallest positive integer that frees it. A
	// collision (case-insensitive, PG folds) is 42710; derived names never yield to a later
	// explicit one (oracle-probed).
	checks := make([]checkConstraint, 0, len(ct.Checks))
	nameTaken := func(name string) bool {
		for _, c := range checks {
			if strings.EqualFold(c.Name, name) {
				return true
			}
		}
		return false
	}
	for i := range ct.Checks {
		def := &ct.Checks[i]
		name := def.Name
		if name != "" {
			if nameTaken(name) {
				return outcome{}, newError(DuplicateObject,
					"constraint "+name+" for relation "+table.Name+" already exists")
			}
		} else {
			cols := checkReferencedColumns(def.Expr, columns)
			var base string
			if len(cols) == 1 {
				base = strings.ToLower(table.Name) + "_" + strings.ToLower(columns[cols[0]].Name) + "_check"
			} else {
				base = strings.ToLower(table.Name) + "_check"
			}
			name = base
			for suffix := 1; nameTaken(name); suffix++ {
				name = base + strconv.Itoa(suffix)
			}
		}
		checks = append(checks, checkConstraint{Name: name, ExprText: def.Text, Expr: def.Expr})
	}
	// Evaluation (and on-disk) order: ascending byte order of the lowercased name
	// (constraints.md §4.4 — PG evaluates checks sorted by name, oracle-probed).
	sort.SliceStable(checks, func(i, j int) bool {
		return strings.ToLower(checks[i].Name) < strings.ToLower(checks[j].Name)
	})
	table.Checks = checks

	// UNIQUE fold + naming (constraints.md §5.2/§5.3, PG-probed). Fold first: a
	// constraint whose member list equals the primary key's (same order) creates nothing;
	// identical lists fold into the first occurrence, the surviving name being the first
	// explicitly-named one's. Then each survivor names its backing index in textual order:
	// an explicit name checks the relation namespace (42P07 — existing relations, the
	// table being created, and the statement's earlier indexes) before the table's
	// constraint names (42710); a derived `<table>_<cols>_key` suffix-walks past BOTH
	// namespaces.
	var survivors []resolvedUnique
	for _, ru := range runiques {
		if slices.Equal(ru.cols, table.PK) {
			continue
		}
		folded := false
		for i := range survivors {
			if slices.Equal(survivors[i].cols, ru.cols) {
				if survivors[i].name == "" {
					survivors[i].name = ru.name
				}
				folded = true
				break
			}
		}
		if !folded {
			survivors = append(survivors, ru)
		}
	}
	relationTaken := func(n string) bool {
		if db.relationExists(n) || strings.EqualFold(table.Name, n) {
			return true
		}
		for _, ix := range table.Indexes {
			if strings.EqualFold(ix.Name, n) {
				return true
			}
		}
		return false
	}
	checkNameTaken := func(n string) bool {
		for _, c := range table.Checks {
			if strings.EqualFold(c.Name, n) {
				return true
			}
		}
		return false
	}
	for _, ru := range survivors {
		name := ru.name
		if name != "" {
			// A named UNIQUE constraint IS its backing index (constraints.md §5), so the
			// user-written name enters the relation namespace — reserved-prefix checked like
			// any relation name (introspection.md §4).
			if err := checkReservedName("constraint", name); err != nil {
				return outcome{}, err
			}
			if relationTaken(name) {
				return outcome{}, newError(DuplicateTable, "relation already exists: "+name)
			}
			if checkNameTaken(name) {
				return outcome{}, newError(DuplicateObject,
					"constraint "+name+" for relation "+table.Name+" already exists")
			}
		} else {
			base := strings.ToLower(table.Name)
			for _, i := range ru.cols {
				base += "_" + strings.ToLower(table.Columns[i].Name)
			}
			base += "_key"
			name = base
			for suffix := 1; relationTaken(name) || checkNameTaken(name); suffix++ {
				name = base + strconv.Itoa(suffix)
			}
		}
		// Insert in catalog (ascending lowercased-name) order — indexes.md §6.
		def := indexDef{Name: name, Keys: columnKeys(ru.cols), Unique: true, Kind: indexBtree}
		nameKey := strings.ToLower(name)
		pos := len(table.Indexes)
		for i, ix := range table.Indexes {
			if strings.ToLower(ix.Name) > nameKey {
				pos = i
				break
			}
		}
		table.Indexes = slices.Insert(table.Indexes, pos, def)
	}

	// FOREIGN KEY constraints (constraints.md §6). Resolved AFTER the PK / UNIQUE / CHECK
	// constraints (PG's order), each in textual definition order: resolve the local columns
	// (42703/42701) against this table; look up the parent (42P01, or the table itself for a
	// self-reference); resolve the referenced columns (default to the parent PK, 42704 if it
	// has none); check the arity (42830); name the constraint (explicit collision 42710, else
	// derive `<table>_<cols>_fkey` with a suffix walk through the constraint namespace); reject
	// the unsupported write-actions (0A000); require the referenced columns to be the parent PK
	// or a UNIQUE set (42830); and require same-type pairing (42804, stricter than PG). An FK
	// owns no B-tree — enforcement probes the parent at every write (§6.4/§6.5).
	resolvedFks := make([]foreignKey, 0, len(ct.ForeignKeys))
	for _, fk := range ct.ForeignKeys {
		// 1. Local (referencing) columns into this table.
		local := make([]int, 0, len(fk.Columns))
		for _, cname := range fk.Columns {
			idx := -1
			for i := range table.Columns {
				if strings.EqualFold(table.Columns[i].Name, cname) {
					idx = i
					break
				}
			}
			if idx < 0 {
				return outcome{}, newError(UndefinedColumn,
					"column "+cname+" named in key does not exist")
			}
			if slices.Contains(local, idx) {
				return outcome{}, newError(DuplicateColumn,
					"column "+cname+" appears twice in foreign key constraint")
			}
			local = append(local, idx)
		}
		// 2. Parent table — a self-reference resolves against the in-progress definition.
		selfRef := strings.EqualFold(fk.RefTable, table.Name)
		var parent *catTable
		if selfRef {
			parent = table
		} else {
			p, ok := db.Table(fk.RefTable)
			if !ok {
				return outcome{}, newError(UndefinedTable, "table does not exist: "+fk.RefTable)
			}
			parent = p
		}
		// 3. Referenced columns into the parent (default to the parent's primary key).
		var refs []int
		if fk.RefColumns == nil {
			if len(parent.PK) == 0 {
				// Omitting the referenced list defaults to the parent's PRIMARY KEY; a parent
				// without one is 42704 (PG's code here — undefined_object — even when the parent
				// has a UNIQUE), distinct from the explicit-no-match 42830.
				return outcome{}, newError(UndefinedObject,
					"there is no primary key for referenced table "+parent.Name)
			}
			refs = append([]int(nil), parent.PK...)
		} else {
			refs = make([]int, 0, len(fk.RefColumns))
			for _, cname := range fk.RefColumns {
				idx := -1
				for i := range parent.Columns {
					if strings.EqualFold(parent.Columns[i].Name, cname) {
						idx = i
						break
					}
				}
				if idx < 0 {
					return outcome{}, newError(UndefinedColumn,
						"column "+cname+" named in key does not exist")
				}
				if slices.Contains(refs, idx) {
					return outcome{}, newError(DuplicateColumn,
						"column "+cname+" appears twice in foreign key constraint")
				}
				refs = append(refs, idx)
			}
		}
		// 4. Referencing/referenced count must agree.
		if len(local) != len(refs) {
			return outcome{}, newError(InvalidForeignKey,
				"number of referencing and referenced columns for foreign key disagree")
		}
		// 5. Name — the per-table constraint namespace, shared with CHECK (§6.2/§6.7).
		var name string
		if fk.Name != "" {
			collide := false
			for _, c := range table.Checks {
				if strings.EqualFold(c.Name, fk.Name) {
					collide = true
					break
				}
			}
			if !collide {
				for _, f := range resolvedFks {
					if strings.EqualFold(f.Name, fk.Name) {
						collide = true
						break
					}
				}
			}
			if collide {
				return outcome{}, newError(DuplicateObject,
					"constraint "+fk.Name+" for relation "+table.Name+" already exists")
			}
			name = fk.Name
		} else {
			base := strings.ToLower(table.Name)
			for _, i := range local {
				base += "_" + strings.ToLower(table.Columns[i].Name)
			}
			base += "_fkey"
			fkNameTaken := func(candidate string) bool {
				for _, c := range table.Checks {
					if strings.EqualFold(c.Name, candidate) {
						return true
					}
				}
				for _, f := range resolvedFks {
					if strings.EqualFold(f.Name, candidate) {
						return true
					}
				}
				return false
			}
			name = base
			for suffix := 1; fkNameTaken(name); suffix++ {
				name = base + strconv.Itoa(suffix)
			}
		}
		// 6. Reject the unsupported write-actions (§6.6).
		onDelete, err := newFkAction(fk.OnDelete, "DELETE")
		if err != nil {
			return outcome{}, err
		}
		onUpdate, err := newFkAction(fk.OnUpdate, "UPDATE")
		if err != nil {
			return outcome{}, err
		}
		// 7. The referenced columns must be the parent's PK or a UNIQUE set (§6.2).
		refSet := sortedUnique(refs)
		matchesUnique := len(parent.PK) > 0 && slices.Equal(sortedUnique(parent.PK), refSet)
		if !matchesUnique {
			for _, ix := range parent.Indexes {
				if cols := ix.columnOrdinals(); ix.Unique && cols != nil && slices.Equal(sortedUnique(cols), refSet) {
					matchesUnique = true
					break
				}
			}
		}
		if !matchesUnique {
			return outcome{}, newError(InvalidForeignKey,
				"there is no unique constraint matching given keys for referenced table "+parent.Name)
		}
		// 8. Same-type pairing (§6.2). Because the referenced columns are a PK/UNIQUE key they
		// are key-encodable, so a same-typed local column is key-encodable too — no separate
		// 0A000 type gate is needed.
		for i := range local {
			lt := table.Columns[local[i]].Type
			rt := parent.Columns[refs[i]].Type
			if !typesEqual(lt, rt) {
				return outcome{}, newError(DatatypeMismatch, fmt.Sprintf(
					"foreign key constraint %s cannot be implemented: key columns %s and %s are of incompatible types: %s and %s",
					name,
					table.Columns[local[i]].Name,
					parent.Columns[refs[i]].Name,
					lt.CanonicalName(),
					rt.CanonicalName(),
				))
			}
		}
		resolvedFks = append(resolvedFks, foreignKey{
			Name:       name,
			Columns:    local,
			RefTable:   parent.Name,
			RefColumns: refs,
			OnDelete:   onDelete,
			OnUpdate:   onUpdate,
		})
	}
	// Held in ascending lowercased-name order (the catalog's on-disk + evaluation order, §6.9).
	sort.SliceStable(resolvedFks, func(i, j int) bool {
		return strings.ToLower(resolvedFks[i].Name) < strings.ToLower(resolvedFks[j].Name)
	})
	table.ForeignKeys = resolvedFks

	// EXCLUDE constraints (spec/design/gist.md §7). Resolved AFTER the PK / UNIQUE / CHECK / FK
	// constraints, each in textual order: resolve the element columns (42703/42701) and the WITH
	// operators against the column types (42704 no-opclass / 0A000 deferred-or-unsupported), name the
	// constraint + its backing GiST index (the constraint IS its index — they share a name;
	// 42P07/42710 across the relation + constraint namespaces), and build the MULTI-COLUMN GiST index
	// that enforces it. The probe + 23P01 live in INSERT/UPDATE.
	for _, exc := range ct.Excludes {
		if exc.Using != "" && !strings.EqualFold(exc.Using, "gist") {
			return outcome{}, newError(UndefinedObject, "access method "+exc.Using+" does not support exclusion constraints")
		}
		indices := make([]int, 0, len(exc.Elements))
		elements := make([]exclusionElement, 0, len(exc.Elements))
		for _, el := range exc.Elements {
			ci := -1
			for i := range table.Columns {
				if strings.EqualFold(table.Columns[i].Name, el.Column) {
					ci = i
					break
				}
			}
			if ci < 0 {
				return outcome{}, newError(UndefinedColumn, "column "+el.Column+" named in key does not exist")
			}
			if slices.Contains(indices, ci) {
				return outcome{}, newError(DuplicateColumn, "column "+el.Column+" appears twice in exclusion constraint")
			}
			ty := table.Columns[ci].Type
			// The WITH operator must pair with the column's GiST opclass (gist.md §7): && over a
			// range column (range_ops), = over a fixed-width keyable scalar (the in-core btree_gist).
			var op exclusionOp
			switch el.Op {
			case "&&":
				if !ty.IsRange() {
					return outcome{}, newError(UndefinedObject,
						"data type "+ty.CanonicalName()+" has no default operator class for access method gist that accepts operator &&")
				}
				op = exclOverlaps
			case "=":
				switch {
				case isGistScalarType(ty):
					op = exclEqual
				case isGistDeferredScalarType(ty):
					return outcome{}, newError(FeatureNotSupported,
						"an exclusion constraint with = over "+ty.CanonicalName()+" is not supported yet")
				default:
					return outcome{}, newError(UndefinedObject,
						"data type "+ty.CanonicalName()+" has no default operator class for access method gist")
				}
			default:
				return outcome{}, newError(FeatureNotSupported, "exclusion constraint operator "+el.Op+" is not supported yet")
			}
			indices = append(indices, ci)
			elements = append(elements, exclusionElement{Column: ci, Op: op})
		}
		// Name the constraint (= its backing index name). An explicit name checks the relation
		// namespace (42P07) then the table's constraint names (42710); a derived `<table>_<cols>_excl`
		// suffix-walks both.
		relTaken := func(n string) bool {
			if db.relationExists(n) || strings.EqualFold(table.Name, n) {
				return true
			}
			for _, ix := range table.Indexes {
				if strings.EqualFold(ix.Name, n) {
					return true
				}
			}
			return false
		}
		conTaken := func(n string) bool {
			for _, c := range table.Checks {
				if strings.EqualFold(c.Name, n) {
					return true
				}
			}
			for _, f := range table.ForeignKeys {
				if strings.EqualFold(f.Name, n) {
					return true
				}
			}
			for _, e := range table.Exclusions {
				if strings.EqualFold(e.Name, n) {
					return true
				}
			}
			return false
		}
		var name string
		if exc.Name != "" {
			// The named EXCLUDE constraint's backing GiST index carries the user-written name
			// into the relation namespace (introspection.md §4).
			if err := checkReservedName("constraint", exc.Name); err != nil {
				return outcome{}, err
			}
			if relTaken(exc.Name) {
				return outcome{}, newError(DuplicateTable, "relation already exists: "+exc.Name)
			}
			if conTaken(exc.Name) {
				return outcome{}, newError(DuplicateObject, "constraint "+exc.Name+" for relation "+table.Name+" already exists")
			}
			name = exc.Name
		} else {
			base := strings.ToLower(table.Name)
			for _, i := range indices {
				base += "_" + strings.ToLower(table.Columns[i].Name)
			}
			base += "_excl"
			name = base
			for suffix := 1; relTaken(name) || conTaken(name); suffix++ {
				name = base + strconv.Itoa(suffix)
			}
		}
		// Insert the backing GiST index in catalog (ascending lowercased-name) order.
		def := indexDef{Name: name, Keys: columnKeys(indices), Unique: false, Kind: indexGist}
		nameKey := strings.ToLower(name)
		pos := len(table.Indexes)
		for i, ix := range table.Indexes {
			if strings.ToLower(ix.Name) > nameKey {
				pos = i
				break
			}
		}
		table.Indexes = slices.Insert(table.Indexes, pos, def)
		table.Exclusions = append(table.Exclusions, exclusionConstraint{Name: name, Index: name, Elements: elements})
	}
	// Held in ascending lowercased-name order (the catalog's on-disk order — gist.md §8).
	sort.SliceStable(table.Exclusions, func(i, j int) bool {
		return strings.ToLower(table.Exclusions[i].Name) < strings.ToLower(table.Exclusions[j].Name)
	})

	if attachName != "" {
		// Deferred narrowings on an attached-database table this slice (attached-databases.md §8), each a
		// clean 0A000: a COMPOSITE-typed column (its type lives in the MAIN catalog — no cross-scope type
		// reference this slice), a serial/IDENTITY column (its OWNED sequence would be a cross-scope
		// sequence), and a collated column (the attachment snapshot carries no collation catalog). Plain
		// scalar / array / range / decimal columns with PK / NOT NULL / DEFAULT / CHECK / UNIQUE and
		// secondary btree indexes are fully supported.
		for _, c := range table.Columns {
			if c.Type.IsComposite() {
				return outcome{}, newError(FeatureNotSupported, "a composite-typed column on an attached-database table is not supported yet")
			}
			if c.Collation != "" {
				return outcome{}, newError(FeatureNotSupported, "COLLATE on an attached-database-table column "+c.Name+" is not yet supported")
			}
		}
		if len(pendingSerials) > 0 {
			return outcome{}, newError(FeatureNotSupported, "a serial / IDENTITY column on an attached-database table is not supported yet")
		}
		// Register into the attachment's working snapshot (attached-databases.md §6) — never the main
		// image; published into roots.attached at commit (N-root commit, §5). attachWriteSnap clones the
		// attachment's committed root on first write and marks it dirty. Its NEW stores bind to the
		// attachment's own paging (the storePaging seam — the same one temp/in-memory main use).
		ws := db.attachWriteSnap(attachName)
		ws.storePaging = db.core.attachments[attachName].storage.paging
		mainTypes := db.readSnap().types
		colTypes := make([]colType, len(table.Columns))
		for i, c := range table.Columns {
			colTypes[i] = resolveColType(c.Type, mainTypes)
		}
		// Build the attachment's new stores at ITS OWN page size (§2) — a file attachment may serialize at
		// a different page size than main, and its records must split to match its physical pages.
		aps := db.attachPageSize(attachName)
		ws.putTableResolved(table, colTypes, aps)
		for _, ix := range table.Indexes {
			ws.putIndexStore(strings.ToLower(ix.Name), newTableStore(pagePayload(aps), nil))
		}
		return outcome{Kind: outcomeStatement, Cost: 0}, nil
	}

	if targetTemp {
		// Deferred narrowing on a temp table this slice (spec/design/temp-tables.md §8), a clean 0A000:
		// a collated column (needs the temp snapshot to carry the collation catalog). Plain
		// scalar/array/range/decimal columns with PK / NOT NULL / DEFAULT / CHECK / UNIQUE,
		// serial/IDENTITY columns (the OWNED sequence is staged into the same temp snapshot below), and
		// COMPOSITE-typed columns (resolved against the MAIN type catalog just below) are fully supported.
		for _, c := range table.Columns {
			if c.Collation != "" {
				return outcome{}, newError(FeatureNotSupported, "COLLATE on temporary-table column "+c.Name+" is not yet supported")
			}
		}
		// Resolve each column's ColType against the MAIN snapshot's composite-type catalog
		// (spec/design/temp-tables.md §8): composites are always persistent (CREATE TYPE is persistent
		// DDL), so the temp snapshot's own types map is empty — resolving there would miss a composite
		// reference. The resulting ColType tree is self-contained, so the temp store needs nothing from
		// the catalog after this (composite.md §4).
		mainTypes := db.readSnap().types
		colTypes := make([]colType, len(table.Columns))
		for i, c := range table.Columns {
			colTypes[i] = resolveColType(c.Type, mainTypes)
		}
		// Register into the session-local temp snapshot — never the main image, so the table makes zero
		// file writes (§2). Flag tempDirty so the commit can skip persisting the main image.
		db.session.tx.tempDirty = true
		ts := db.session.tx.tempWorking
		// The session-local temp snapshot rides a per-domain MemoryBlockStore pager (temp-tables.md §6):
		// lazily create the domain storage on first use and stamp its paging onto this working snapshot, so
		// putTableResolved / putIndexStore attach it to every temp store.
		ts.storePaging = db.tempDomainPaging()
		ts.putTableResolved(table, colTypes, db.pageSize)
		for _, ix := range table.Indexes {
			ts.putIndexStore(strings.ToLower(ix.Name), newTableStore(pagePayload(db.pageSize), nil))
		}
		// Stage each serial/IDENTITY column's OWNED sequence into the SAME temp snapshot
		// (spec/design/sequences.md §12, temp-tables.md §8) — never the main image, so the sequence
		// (like the table) makes zero file writes and is dropped with the table. The names were resolved
		// collision-free during the column walk (relationExists is temp-aware); nextval resolves and
		// advances them via the scope-aware sequence funnel.
		for _, s := range pendingSerials {
			ts.putSequence(s)
		}
		return outcome{Kind: outcomeStatement, Cost: 0}, nil
	}

	db.putTable(table)
	// The table is brand new (no rows), so each backing index store starts empty.
	for _, ix := range table.Indexes {
		db.working().putIndexStore(strings.ToLower(ix.Name), newTableStore(pagePayload(db.pageSize), nil))
	}
	// Stage each serial column's OWNED sequence now that the table validated
	// (spec/design/sequences.md §12). The names were resolved (collision-free) during the column
	// walk; the table is in the catalog, so a DROP TABLE will auto-drop these.
	for _, s := range pendingSerials {
		db.working().putSequence(s)
	}
	// DDL touches no rows and evaluates no expressions: zero cost.
	return outcome{Kind: outcomeStatement, Cost: 0}, nil
}

// resolveChecks resolves a table's CHECK constraints for a write statement: each stored
// expression against a one-relation scope, in the catalog's (evaluation/name) order.
// Cannot fail for a catalog produced by CREATE TABLE or a well-formed file (both
// validated); a hand-corrupted expression surfaces its natural resolve error.
func (db *engine) resolveChecks(table *catTable) ([]namedCheck, error) {
	if len(table.Checks) == 0 {
		return nil, nil
	}
	s := singleScope(db, table)
	out := make([]namedCheck, 0, len(table.Checks))
	for i := range table.Checks {
		node, _, err := resolve(s, table.Checks[i].Expr, nil, &aggCtx{collecting: false}, &paramTypes{})
		if err != nil {
			return nil, err
		}
		out = append(out, namedCheck{name: table.Checks[i].Name, node: node})
	}
	return out, nil
}

// resolveDefaultExprs resolves each column's EXPRESSION default (constraints.md §2) to an
// rExpr, once per INSERT statement — insertRows (and the VALUES DEFAULT-keyword
// materialization) evaluate it per omitted/DEFAULT slot. Returns a slot per column (parallel to
// table.Columns): a non-nil node for an expression default, nil for a column with a constant
// default or no default. The default resolves against an EMPTY scope (no columns; a column
// reference was rejected 0A000 at CREATE TABLE) with the column's type as the operand hint.
func (db *engine) resolveDefaultExprs(table *catTable) ([]*rExpr, error) {
	out := make([]*rExpr, len(table.Columns))
	for i := range table.Columns {
		de := table.Columns[i].DefaultExpr
		if de == nil {
			continue
		}
		colScalar := table.Columns[i].Type.ScalarTy()
		node, _, err := resolve(emptyScope(db), de.Expr, &colScalar, &aggCtx{collecting: false}, &paramTypes{})
		if err != nil {
			return nil, err
		}
		out[i] = node
	}
	return out, nil
}

// resolveIndex resolves an index's key elements for one statement's maintenance
// (spec/design/indexes.md §4), modeled on resolveChecks: a column key keeps its ordinal; an
// expression key resolves against the table's columns to an rExpr + its encoding Type + collation.
// The expression was validated (immutable, indexable result) at CREATE INDEX, so resolution here
// cannot newly fail (an aggregate/window/subquery/param was rejected then, and re-resolving with a
// non-collecting aggCtx is inert). Returns an owned resolvedIndex.
func (db *engine) resolveIndex(table *catTable, def indexDef) (resolvedIndex, error) {
	keys := make([]resolvedKey, 0, len(def.Keys))
	for _, k := range def.Keys {
		if k.Expr == nil {
			keys = append(keys, resolvedKey{Col: k.Col})
			continue
		}
		s := singleScope(db, table)
		node, rtype, err := resolve(s, k.Expr.Expr, nil, &aggCtx{collecting: false}, &paramTypes{})
		if err != nil {
			return resolvedIndex{}, err
		}
		ty, ok := resolvedToKeyType(rtype)
		if !ok {
			panic("index expression result type validated indexable at CREATE INDEX")
		}
		d, err := deriveCollation(s, k.Expr.Expr)
		if err != nil {
			return resolvedIndex{}, err
		}
		coll, err := resolveDeriv(db, d)
		if err != nil {
			return resolvedIndex{}, err
		}
		keys = append(keys, resolvedKey{Expr: node, Ty: ty, Coll: coll})
	}
	// A partial index's predicate (indexes.md §9), re-resolved against the table's columns — it was
	// validated boolean + immutable at CREATE INDEX, so this cannot newly fail.
	var predicate *rExpr
	if def.Predicate != nil {
		s := singleScope(db, table)
		node, err := resolveBooleanFilter(s, &def.Predicate.Expr, &paramTypes{})
		if err != nil {
			return resolvedIndex{}, err
		}
		predicate = node
	}
	return resolvedIndex{Name: def.Name, Unique: def.Unique, Kind: def.Kind, Keys: keys, Predicate: predicate}, nil
}

// resolveTableIndexes resolves every index of a table once per statement (the maintenance driver —
// INSERT / UPDATE / DELETE build their resolvedIndex list up front, parallel to table.Indexes).
func (db *engine) resolveTableIndexes(table *catTable) ([]resolvedIndex, error) {
	out := make([]resolvedIndex, 0, len(table.Indexes))
	for _, def := range table.Indexes {
		ri, err := db.resolveIndex(table, def)
		if err != nil {
			return nil, err
		}
		out = append(out, ri)
	}
	return out, nil
}

// indexMaintEnv builds the unmetered eval env for phase-1 index-expression evaluation: params/CTEs
// are empty (an index expression cannot reference them) and the fresh statement rng is never read
// (an index expression is immutable). Used by the index-entries / index-prefix / arbiter helpers.
func (db *engine) indexMaintEnv() *evalEnv {
	return &evalEnv{exec: db, rng: newStmtRng()}
}

// indexEntries computes a row's secondary-index entry keys for maintenance (spec/design/indexes.md
// §4), building the unmetered eval env internally. Returns owned bytes, so callers compute all
// entries through this &engine call BEFORE the store-mutating writes.
func (db *engine) indexEntries(columns []catColumn, colls []*Collation, rindex *resolvedIndex, storageKey []byte, row storedRow) ([][]byte, error) {
	return indexEntryKeys(columns, colls, rindex, storageKey, row, db.indexMaintEnv())
}

// indexPrefix computes a row's uniqueness-probe prefix for one index (spec/design/indexes.md §8),
// building the unmetered eval env internally (as indexEntries).
func (db *engine) indexPrefix(columns []catColumn, colls []*Collation, rindex *resolvedIndex, row storedRow) ([]byte, bool, error) {
	return indexPrefixKey(columns, colls, rindex, row, db.indexMaintEnv())
}

// arbiterProbeKey computes a candidate row's arbiter key for ON CONFLICT (spec/design/upsert.md §3),
// building the unmetered eval env internally (an expression-index arbiter evaluates its keys — as
// indexPrefix).
func (db *engine) arbiterProbeKey(arb *arbiter, table *catTable, pk []int, colls []*Collation, rindexes []resolvedIndex, row storedRow) ([]byte, bool, error) {
	return arbiterKey(arb, table, pk, colls, rindexes, row, db.indexMaintEnv())
}

// evalDefault is the value an omitted column or a DEFAULT value slot takes (constraints.md §2):
// the column's pre-evaluated constant (col.Default, or NULL when it has none), OR — for an
// expression default — the resolved rExpr evaluated against an empty row through the
// per-statement seam/clock (rng) and metered (operator_eval per node). Reused by the VALUES
// materialization (a DEFAULT keyword) and insertRows (an omitted column), sharing ONE StmtRng
// so a multi-row DEFAULT uuidv7() stays monotonic. defaultRExpr is nil for a constant/no default.
func (db *engine) evalDefault(col catColumn, defaultRExpr *rExpr, rng *stmtRng, meter *costMeter) (Value, error) {
	if defaultRExpr == nil {
		return defaultOrNull(col), nil
	}
	if err := meter.Guard(); err != nil {
		return Value{}, err
	}
	env := &evalEnv{exec: db, rng: rng}
	return defaultRExpr.eval(nil, env, meter)
}

// namedCheck is one statement-resolved CHECK constraint: its name (for the 23514
// message) and the resolved expression evaluated per candidate row.
type namedCheck struct {
	name string
	node *rExpr
}

// evalChecks evaluates a row's CHECK constraints in name order (constraints.md §4.4):
// TRUE and NULL pass; the first FALSE aborts with 23514 and PG's message. Shared by the
// INSERT and UPDATE write paths.
func evalChecks(checks []namedCheck, relation string, row storedRow, env *evalEnv, meter *costMeter) error {
	for _, c := range checks {
		v, err := c.node.eval(row, env, meter)
		if err != nil {
			return err
		}
		if v.Kind == ValBool && !v.boolVal() {
			return newCheckViolation(relation, c.name)
		}
	}
	return nil
}

// dropScope is the scope a resolved DROP TABLE target lives in (temp-tables.md §3) — it governs
// which working snapshot the removal routes to.
type dropScope int

const (
	dropTemp dropScope = iota
	dropPersistent
)

type dropTarget struct {
	key   string // lowercased catalog key
	scope dropScope
}

// executeDropTable runs a DROP TABLE [IF EXISTS] a [, …] [CASCADE | RESTRICT]: remove each named
// table's definition and row store from the catalog (keyed by lower-cased name). Two-phase /
// all-or-nothing (spec/design/grammar.md §13): every name is resolved and validated first — a
// missing table is 42P01 (unless IF EXISTS skips just that name), a non-table relation is 42809,
// and an external FK dependent is 2BP01 under RESTRICT — and only if the whole list checks out is
// anything removed. A repeated name is deduplicated; a FK between two tables both in the drop set
// never blocks; CASCADE drops the surviving tables' now-dangling FK constraints. Like CREATE TABLE
// it touches no rows and evaluates no expression tree, so it accrues zero cost.
func (db *engine) executeDropTable(dt *dropTable) (outcome, error) {
	// ---- Phase 1: resolve & classify every name into the drop set. Nothing is removed yet. A
	// repeated name is deduplicated (PG collects the targets into a set, so `DROP TABLE a, a` drops
	// `a` once and succeeds); seen is the set of lowercased keys actually being dropped.
	var targets []dropTarget
	seen := map[string]bool{}
	for _, name := range dt.Names {
		key := strings.ToLower(name)
		if seen[key] {
			continue // already resolved this exact target (deduplicated)
		}
		// A built-in catalog relation resolves BEFORE the user catalog (introspection.md §5), and a
		// system relation cannot be dropped: 42809. IF EXISTS does not suppress this (the relation
		// exists — this is a kind rejection, not a missing name).
		if isCatalogRelName(key) {
			return outcome{}, newError(WrongObjectType, `cannot drop system relation "`+key+`"`)
		}
		// Resolution walk: session-local temp → persistent. Preclude-overlaps keeps a name in at most one
		// scope, so this is just "where it lives" (temp-tables.md §3).
		var scope dropScope
		switch {
		case db.isTempTable(name):
			scope = dropTemp
		default:
			if _, ok := db.readSnap().table(name); ok {
				scope = dropPersistent
			} else {
				// Not a table in any scope. An index's name is the wrong object kind (42809 —
				// indexes.md §2); IF EXISTS does NOT suppress this. Otherwise a missing table is
				// 42P01, unless IF EXISTS makes it a no-op for just this name.
				if _, _, ok := db.findIndex(name); ok {
					return outcome{}, newError(WrongObjectType, name+" is not a table")
				}
				if dt.IfExists {
					continue
				}
				return outcome{}, newError(UndefinedTable, "table does not exist: "+name)
			}
		}
		seen[key] = true
		targets = append(targets, dropTarget{key: key, scope: scope})
	}
	// ---- Phase 2: FK dependency check (RESTRICT) / removal collection (CASCADE). Only a persistent
	// table can be an FK parent (a temp table never is, §8), so the scan runs over the persistent
	// snapshot; a dependent whose referencing table is itself in the drop set does not count (the
	// drop-set exclusion is the whole seen set, so `DROP TABLE parent, child` succeeds even under
	// RESTRICT).
	deps := db.readSnap().foreignKeyDependentsExcluding(seen)
	var cascadeRemovals []fkDependent
	if dt.Cascade {
		cascadeRemovals = deps
	} else if len(deps) > 0 {
		// RESTRICT (the default, and the bare form's behavior): an external FK dependent blocks the
		// drop with 2BP01 — the same message the single-table check produced.
		d := deps[0]
		return outcome{}, newError(DependentObjectsStillExist,
			"cannot drop table "+d.droppedName+" because other objects depend on it: constraint "+
				d.fkName+" on table "+d.refTableName)
	}
	// ---- Phase 3: apply. CASCADE first drops each surviving table's now-dangling FK constraint (in
	// place, preserving its rows). A FK only ever lives on a persistent table (temp tables reject FKs
	// at CREATE), so the removal routes to the main working snapshot.
	for _, d := range cascadeRemovals {
		db.working().removeForeignKey(d.refTableKey, d.fkName)
	}
	// Then remove every target from its own scope, auto-dropping the sequences it owns — a
	// serial/IDENTITY column's owned sequence (spec/design/sequences.md §12; an owned sequence is
	// never an FK dependent, so the phase-2 check never blocked on it). A temp drop touches only its
	// temp snapshot, never the main image, so it makes zero file writes.
	for _, tgt := range targets {
		switch tgt.scope {
		case dropTemp:
			db.session.tx.tempDirty = true
			ts := db.tempSnap()
			for _, sk := range ts.sequencesOwnedBy(tgt.key) {
				ts.removeSequence(sk)
			}
			ts.removeTable(tgt.key)
		case dropPersistent:
			ownedSeqs := db.readSnap().sequencesOwnedBy(tgt.key)
			w := db.working()
			for _, sk := range ownedSeqs {
				w.removeSequence(sk)
			}
			w.removeTable(tgt.key)
		}
	}
	return outcome{Kind: outcomeStatement, Cost: 0}, nil
}

// chooseSerialSeqName chooses the auto-generated name for a serial column's OWNED sequence
// (spec/design/sequences.md §12), matching PostgreSQL: lower(table)_lower(column)_seq, with the
// smallest integer suffix 1, 2, … appended until the name is free in the relation namespace — not
// taken by an existing relation, not equal to the table being created, and not already chosen by an
// earlier serial column of the same statement (pending). All-lowercase identifier-derived.
func (db *engine) chooseSerialSeqName(table, column string, pending []*sequenceDef) string {
	base := strings.ToLower(table) + "_" + strings.ToLower(column) + "_seq"
	taken := func(c string) bool {
		if db.relationExists(c) || strings.EqualFold(c, table) {
			return true
		}
		for _, s := range pending {
			if strings.EqualFold(s.Name, c) {
				return true
			}
		}
		return false
	}
	if !taken(base) {
		return base
	}
	for n := 1; ; n++ {
		cand := fmt.Sprintf("%s%d", base, n)
		if !taken(cand) {
			return cand
		}
	}
}

// buildSequenceDef resolves a parsed SeqOptions set into a validated SequenceDef
// (spec/design/sequences.md §1/§14), shared by CREATE SEQUENCE and an IDENTITY column's
// `( seq_options )` (§13). The AS type (or the serial/identity-supplied default) sets the default +
// validated bounds; then validates INCREMENT (≠ 0), CACHE (≥ 1), explicit MIN/MAX within the type
// range, MINVALUE ≤ MAXVALUE, and START in [min, max] (each 22023); a fresh sequence starts with
// LastValue = Start, IsCalled = false. ownedBy carries the IDENTITY / serial owner link (nil for a
// plain CREATE SEQUENCE).
func buildSequenceDef(name string, options seqOptions, ownedBy *seqOwner) (*sequenceDef, error) {
	// The value type (§14): `AS <type>` → the named type (22023 if not an integer type), else bigint.
	dtype := seqBigInt
	if options.DataType != "" {
		dt, ok := seqDataTypeFromName(options.DataType)
		if !ok {
			return nil, newError(InvalidParameterValue,
				"sequence type must be smallint, integer, or bigint")
		}
		dtype = dt
	}
	typeMin, typeMax := dtype.Range()
	increment := int64(1)
	if options.Increment != nil {
		increment = *options.Increment
	}
	if increment == 0 {
		return nil, newError(InvalidParameterValue, "INCREMENT must not be zero")
	}
	cache := int64(1)
	if options.Cache != nil {
		cache = *options.Cache
	}
	if cache < 1 {
		return nil, newError(InvalidParameterValue,
			fmt.Sprintf("CACHE (%d) must be greater than zero", cache))
	}
	defMin, defMax := dtype.DefaultBounds(increment)
	// An explicit MAXVALUE/MINVALUE outside the type range is 22023 — checked (MAX first, PG order)
	// BEFORE the MIN > MAX consistency check (§14.2).
	if options.MaxValue != nil && !options.MaxValue.NoValue && options.MaxValue.Value > typeMax {
		return nil, newError(InvalidParameterValue, fmt.Sprintf(
			"MAXVALUE (%d) is out of range for sequence data type %s", options.MaxValue.Value, dtype.PgName(),
		))
	}
	if options.MinValue != nil && !options.MinValue.NoValue && options.MinValue.Value < typeMin {
		return nil, newError(InvalidParameterValue, fmt.Sprintf(
			"MINVALUE (%d) is out of range for sequence data type %s", options.MinValue.Value, dtype.PgName(),
		))
	}
	// A non-nil SeqBound with NoValue selects the default; with a value sets the explicit bound; a
	// nil SeqBound means the option was unset → the default (the Rust Some(Some)/Some(None)/None).
	minValue := defMin
	if options.MinValue != nil && !options.MinValue.NoValue {
		minValue = options.MinValue.Value
	}
	maxValue := defMax
	if options.MaxValue != nil && !options.MaxValue.NoValue {
		maxValue = options.MaxValue.Value
	}
	// PG requires MINVALUE strictly less than MAXVALUE (a one-value sequence is rejected); jed
	// previously allowed `==` — corrected here so CREATE and ALTER (sequences.md §15.2) agree with PG.
	if minValue >= maxValue {
		return nil, newError(InvalidParameterValue,
			fmt.Sprintf("MINVALUE (%d) must be less than MAXVALUE (%d)", minValue, maxValue))
	}
	// START defaults to MINVALUE (ascending) / MAXVALUE (descending) and must lie in [min, max].
	start := minValue
	if increment < 0 {
		start = maxValue
	}
	if options.Start != nil {
		start = *options.Start
	}
	if err := seqBoundCheckStart(start, minValue, maxValue); err != nil {
		return nil, err
	}
	cycle := false
	if options.Cycle != nil {
		cycle = *options.Cycle
	}
	return &sequenceDef{
		Name:      name,
		Increment: increment,
		MinValue:  minValue,
		MaxValue:  maxValue,
		Start:     start,
		Cache:     cache,
		Cycle:     cycle,
		LastValue: start,
		IsCalled:  false,
		OwnedBy:   ownedBy,
	}, nil
}

// seqBoundCheckStart is PG's START-in-bounds cross-check (init_params): start ∈ [min, max], else
// 22023 with PG's wording. Shared by CREATE (buildSequenceDef) and ALTER (applySeqAlter).
func seqBoundCheckStart(start, minValue, maxValue int64) error {
	if start < minValue {
		return newError(InvalidParameterValue,
			fmt.Sprintf("START value (%d) cannot be less than MINVALUE (%d)", start, minValue))
	}
	if start > maxValue {
		return newError(InvalidParameterValue,
			fmt.Sprintf("START value (%d) cannot be greater than MAXVALUE (%d)", start, maxValue))
	}
	return nil
}

// seqBoundCheckLast is PG's last_value (RESTART) cross-check (init_params): the post-edit last_value ∈
// [min, max], else 22023. PG uses the "RESTART value …" wording even with no RESTART written (§15.2).
func seqBoundCheckLast(lastValue, minValue, maxValue int64) error {
	if lastValue < minValue {
		return newError(InvalidParameterValue,
			fmt.Sprintf("RESTART value (%d) cannot be less than MINVALUE (%d)", lastValue, minValue))
	}
	if lastValue > maxValue {
		return newError(InvalidParameterValue,
			fmt.Sprintf("RESTART value (%d) cannot be greater than MAXVALUE (%d)", lastValue, maxValue))
	}
	return nil
}

// applySeqAlter re-edits an existing SequenceDef per ALTER SEQUENCE s <options>
// (spec/design/sequences.md §15.2) — PG init_params with isInit=false. Only the WRITTEN options
// change; LastValue/IsCalled are preserved unless restart is given. The value type is not persisted
// (§14.4), so NO MINVALUE/NO MAXVALUE reset the open direction to the bigint bound and an explicit
// bound is i64-checked only. options.DataType must be "" (the caller rejects AS as 0A000 first).
func applySeqAlter(existing *sequenceDef, options seqOptions, restart *seqRestart) (*sequenceDef, error) {
	def := *existing
	if options.Increment != nil {
		if *options.Increment == 0 {
			return nil, newError(InvalidParameterValue, "INCREMENT must not be zero")
		}
		def.Increment = *options.Increment
	}
	if options.Cache != nil {
		if *options.Cache < 1 {
			return nil, newError(InvalidParameterValue,
				fmt.Sprintf("CACHE (%d) must be greater than zero", *options.Cache))
		}
		def.Cache = *options.Cache
	}
	// NO MINVALUE/NO MAXVALUE recompute the default for the (possibly new) INCREMENT sign — against
	// the bigint range (the value type is not persisted, §14.4). An explicit bound is taken as
	// written; an unwritten bound is preserved (PG keeps it even when the sign flips).
	defMin, defMax := seqBigInt.DefaultBounds(def.Increment)
	if options.MinValue != nil {
		if options.MinValue.NoValue {
			def.MinValue = defMin
		} else {
			def.MinValue = options.MinValue.Value
		}
	}
	if options.MaxValue != nil {
		if options.MaxValue.NoValue {
			def.MaxValue = defMax
		} else {
			def.MaxValue = options.MaxValue.Value
		}
	}
	if def.MinValue >= def.MaxValue {
		return nil, newError(InvalidParameterValue,
			fmt.Sprintf("MINVALUE (%d) must be less than MAXVALUE (%d)", def.MinValue, def.MaxValue))
	}
	if options.Start != nil {
		def.Start = *options.Start
	}
	// Cross-check 1: START ∈ [min, max].
	if err := seqBoundCheckStart(def.Start, def.MinValue, def.MaxValue); err != nil {
		return nil, err
	}
	// RESTART (applied last, before the last_value cross-check).
	if restart != nil {
		if restart.ToStart {
			def.LastValue = def.Start
		} else {
			def.LastValue = restart.Value
		}
		def.IsCalled = false
	}
	// Cross-check 2: the preserved/restarted last_value ∈ [min, max].
	if err := seqBoundCheckLast(def.LastValue, def.MinValue, def.MaxValue); err != nil {
		return nil, err
	}
	if options.Cycle != nil {
		def.Cycle = *options.Cycle
	}
	return &def, nil
}

// serialPseudoType maps a serial pseudo-type name to its underlying integer scalar
// (spec/design/sequences.md §12) — serial/serial4 → Int32, bigserial/serial8 → Int64,
// smallserial/serial2 → Int16. The bool is false for any other name. Recognized only in a
// CREATE TABLE column-type position; the match is case-insensitive.
func serialPseudoType(name string) (scalarType, bool) {
	switch strings.ToLower(name) {
	case "serial", "serial4":
		return scalarInt32, true
	case "bigserial", "serial8":
		return scalarInt64, true
	case "smallserial", "serial2":
		return scalarInt16, true
	default:
		return 0, false
	}
}

// findIndex finds the table owning the named index in the visible snapshot
// (case-insensitive).
func (db *engine) findIndex(name string) (string, indexDef, bool) {
	return db.readSnap().findIndex(name)
}

// checkReservedName rejects a USER-written catalog object name beginning jed_ — the prefix is
// reserved for the engine's own catalog relations (spec/design/introspection.md §4). Case-insensitive
// (resolution folds case and there is no quoted-identifier escape — grammar.md §3). Engine-GENERATED
// names (a serial's <table>_<col>_seq, an index auto-name — both legal for a table named jed) never
// pass through here; the check sits with each site's namespace-collision check so established
// validation orders (42P01/42703 before name checks) are preserved. kind is the object word in the
// message: table / index / sequence / type.
func checkReservedName(kind, name string) error {
	if len(name) >= 4 && strings.EqualFold(name[:4], "jed_") {
		return newError(ReservedName, kind+" name "+name+" is reserved (the jed_ prefix is reserved for system objects)")
	}
	return nil
}

// relationExists reports whether name is taken in the shared relation namespace (a table
// OR an index — spec/design/indexes.md §2), case-insensitively.
func (db *engine) relationExists(name string) bool {
	// Session-local temp tables + their (UNIQUE) index names join the namespace too, so a name colliding
	// with any temp relation is also 42P07 (preclude-overlaps — spec/design/temp-tables.md §3). db.Table
	// is persistent-only, so the temp snapshot is checked explicitly.
	if _, ok := db.Table(name); ok {
		return true
	}
	if _, ok := db.tempSnap().table(name); ok {
		return true
	}
	if _, _, ok := db.findIndex(name); ok {
		return true
	}
	if _, _, ok := db.tempSnap().findIndex(name); ok {
		return true
	}
	// The sequence funnel walks session-local → persistent, so an owned TEMP sequence's name joins the
	// namespace (temp-tables.md §8) — a collision with it is 42P07 too.
	return db.sequence(name) != nil
}

// executeCreateIndex analyzes and runs a CREATE INDEX (spec/design/indexes.md §2).
// Validation mirrors PostgreSQL's order (oracle-probed): the table must exist (42P01);
// each key column, in list order, must exist (42703) and be of a key-encodable type
// (0A000 — the same narrowing as a PRIMARY KEY member); then an explicit name is checked
// against the shared relation namespace (42P07), or an omitted name derives PG's choice —
// the lowercased <table>_<col>..._idx with the smallest free suffix. The index is then
// built by scanning the table once: page_read per node + storage_row_read per row (the
// metered build scan — cost.md §3); maintenance thereafter is unmetered.
func (db *engine) executeCreateIndex(ci *createIndex) (outcome, error) {
	// A standalone CREATE INDEX targets whichever scope owns the table — session-local temp,
	// persistent, or a host-attached database (spec/design/temp-tables.md §8, attached-databases.md §3).
	// The build below is scope-agnostic (the scoped lkpTable/lkpStore/writeIndexStore funnels route by
	// the qualifier + resolution walk; the cost meter, UNIQUE validation, naming/namespace collision,
	// and the storage budget are all generic); only the catalog putIndex write must target the owning
	// snapshot, so the routing happens there.
	// A built-in catalog relation cannot be indexed (introspection.md §5): 42809, checked by NAME
	// before qualifier validation, like the DML targets.
	if err := checkCatalogRelWrite(ci.Table); err != nil {
		return outcome{}, err
	}
	// A DDL write to a READ-ONLY host attachment is 25006 before any work — checked BEFORE the qualifier
	// existence gate so a read-only attachment refuses the write deterministically (attached-databases.md §4).
	if err := db.checkAttachmentWritable(ci.DB); err != nil {
		return outcome{}, err
	}
	if err := db.checkTableQualifier(ci.DB, ci.Table); err != nil { // attached-databases.md §3
		return outcome{}, err
	}
	attachName := ""
	if isAttachmentScope(ci.DB) {
		attachName = strings.ToLower(*ci.DB)
	}
	table, ok := db.lkpTableScoped(ci.DB, ci.Table)
	if !ok {
		return outcome{}, newError(UndefinedTable, "table does not exist: "+ci.Table)
	}
	tableKey := strings.ToLower(table.Name)
	columns := table.Columns
	// Refuse building a collated index on a version-skewed table (slice 2d, collation.md §12, XX002):
	// the new B-tree would be pinned inconsistently with the file's other structures.
	if err := db.ensureCollationsWritable(columns); err != nil {
		return outcome{}, err
	}
	// Per-column frozen collations for the collated text key form (§2.12); nil everywhere for a
	// C-only / non-text table (the fast path).
	colls := db.columnCollations(columns)
	// Resolve the access method (spec/design/gin.md §3): the default / "btree" is the ordered
	// B-tree, "gin" a GIN inverted index; an unknown method is 42704. Resolved here (not in the
	// parser) so the error is the resolve-time undefined_object, after the table-exists check.
	var kind indexKind
	switch strings.ToLower(ci.Using) {
	case "", "btree":
		kind = indexBtree
	case "gin":
		kind = indexGin
	case "gist":
		kind = indexGist
	default:
		return outcome{}, newError(UndefinedObject, "access method does not exist: "+ci.Using)
	}
	ciKeys := make([]indexKey, 0, len(ci.Keys))
	for _, elem := range ci.Keys {
		// An EXPRESSION key element (spec/design/indexes.md §1/§2): resolve it against the table's
		// columns, validate it is immutable + indexable-typed, and store its canonical text
		// (persisted, format_version 26). Expression keys are B-tree only this slice — GIN/GiST take
		// a single plain column.
		if elem.isExpr() {
			if kind != indexBtree {
				return outcome{}, newError(FeatureNotSupported,
					"an expression key on a "+ci.Using+" index is not supported yet")
			}
			// A subquery is not a deterministic function of the row — 0A000 (the resolver admits an
			// uncorrelated one, so it is rejected here, before resolution).
			if indexExprHasSubquery(*elem.Expr) {
				return outcome{}, newError(FeatureNotSupported, "cannot use subquery in index expression")
			}
			// Resolve against the table (an aggregate 42803 / window 42P20 / bind parameter 42P02
			// fall out of the resolver, as for a CHECK).
			s := singleScope(db, table)
			pt := &paramTypes{}
			_, rtype, rerr := resolve(s, *elem.Expr, nil, &aggCtx{collecting: false}, pt)
			if rerr != nil {
				return outcome{}, rerr
			}
			// Immutability (§2): a non-immutable seam/sequence/current_setting call, a session-
			// timezone-dependent expression (one that reads or produces a timestamptz — conservatively
			// fail-closed), or a resolved STABLE node (the runtime text→date cast, flagged at its
			// birth — resolve.go paramTypes.nonimmutable), is 42P17.
			tzHazard := rtype.kind == rtTimestamptz
			if !tzHazard {
				for _, ref := range checkReferencedColumns(*elem.Expr, columns) {
					if s, ok := columns[ref].Type.AsScalar(); ok && s == scalarTimestamptz {
						tzHazard = true
						break
					}
				}
			}
			if indexExprNonimmutableCall(*elem.Expr) || tzHazard || pt.nonimmutable {
				return outcome{}, newError(InvalidObjectDefinition,
					"functions in index expression must be marked IMMUTABLE")
			}
			// The result type must be key-encodable (a composite result is 0A000).
			if _, ok := resolvedToKeyType(rtype); !ok {
				return outcome{}, newError(FeatureNotSupported,
					"an index on an expression of this result type is not supported yet")
			}
			ciKeys = append(ciKeys, indexKey{Expr: &indexKeyExpr{ExprText: elem.Text, Expr: *elem.Expr}})
			continue
		}
		name := elem.Column
		idx := table.ColumnIndex(name)
		if idx < 0 {
			return outcome{}, newError(UndefinedColumn, "column does not exist: "+name)
		}
		ty := columns[idx].Type
		switch kind {
		case indexBtree:
			if !ty.IsInteger() && !ty.IsBool() && !ty.IsText() && !ty.IsBytea() && !ty.IsDecimal() && !ty.IsUuid() && !ty.IsTimestamp() && !ty.IsTimestamptz() && !ty.IsDate() && !ty.IsInterval() && !ty.IsFloat() && !ty.IsRange() && !isArrayKeyable(ty) {
				return outcome{}, newError(FeatureNotSupported,
					"a "+ty.CanonicalName()+" index column is not supported yet")
			}
		case indexGin:
			// GIN needs an operator class for the column type: only an array has one (else 42704),
			// and only a FIXED-WIDTH KEY-ENCODABLE element type (else 0A000) — the GIN term IS that
			// element's key encoding (gin.md §3/§4), so the admitted set is the integers, boolean,
			// uuid, date, timestamp, timestamptz (interval's GIN-element support is a separate
			// follow-on — its key landed but the GIN slice has not; gin.md §3/§10).
			if ty.Array == nil {
				return outcome{}, newError(UndefinedObject,
					"data type "+ty.CanonicalName()+" has no default operator class for access method gin")
			}
			if elem, ok := ty.Array.AsScalar(); !ok || !isGinElementType(elem) {
				return outcome{}, newError(FeatureNotSupported,
					"a gin index on "+ty.CanonicalName()+" is not supported yet")
			}
		case indexGist:
			// GiST opclasses (gist.md §5/§6): range_ops over a range column, or the in-core
			// btree_gist-equivalent scalar `=` opclass over a FIXED-WIDTH keyable scalar (integers /
			// boolean / uuid / date / timestamp / timestamptz — its bound is [min,max] over that type's
			// order-preserving key encoding, all pure byte comparison). A keyable-but-deferred scalar
			// (text / bytea / decimal / interval) is 0A000 — we will support it (the GIN element-staging
			// precedent, §11); any other type (float / json / array / composite / jsonpath) has no GiST
			// opclass at all — 42704 (PG's wording).
			if !ty.IsRange() {
				switch {
				case isGistScalarType(ty):
					// supported scalar `=` opclass — ok
				case isGistDeferredScalarType(ty):
					return outcome{}, newError(FeatureNotSupported,
						"a gist index on "+ty.CanonicalName()+" is not supported yet")
				default:
					return outcome{}, newError(UndefinedObject,
						"data type "+ty.CanonicalName()+" has no default operator class for access method gist")
				}
			}
		}
		// A duplicate column in the list is ALLOWED (PostgreSQL allows it — indexes.md §1).
		ciKeys = append(ciKeys, indexKey{Col: idx})
	}
	// GIN narrowings this slice (spec/design/gin.md §3): no uniqueness (undefined for an inverted
	// index) and a single column only — both deferred 0A000.
	if kind == indexGin {
		if ci.Unique {
			return outcome{}, newError(FeatureNotSupported, "access method gin does not support unique indexes")
		}
		if len(ciKeys) != 1 {
			return outcome{}, newError(FeatureNotSupported, "a multi-column gin index is not supported yet")
		}
	}
	// GiST narrowings (gist.md §1/§5/§11): no uniqueness (express it as EXCLUDE … WITH =, GX3) and a
	// single column only (multi-column GiST is GX2/GX3). A GiST index on a TEMP table is 0A000 (its
	// resident R-tree would live on the temp snapshot — deferred, gist.md §11). File persistence
	// landed in GX1b, so a file-backed GiST index is supported.
	if kind == indexGist {
		if ci.Unique {
			return outcome{}, newError(FeatureNotSupported, "access method gist does not support unique indexes")
		}
		if len(ciKeys) != 1 {
			return outcome{}, newError(FeatureNotSupported, "a multi-column gist index is not supported yet")
		}
		if db.isTempTable(ci.Table) {
			return outcome{}, newError(FeatureNotSupported, "a gist index on a temporary table is not supported yet")
		}
	}
	// A non-btree (GIN / GiST) index on an attached-database table is a deferred narrowing this slice
	// (attached-databases.md §8) — the attachment stores only btree PK / UNIQUE / secondary indexes.
	if attachName != "" && kind != indexBtree {
		return outcome{}, newError(FeatureNotSupported, "a "+ci.Using+" index on an attached-database table is not supported yet")
	}
	// The optional `WHERE predicate` making the index PARTIAL (spec/design/indexes.md §9): a boolean
	// expression over the table's own columns, validated with PG-agreeing codes. B-tree only this
	// slice. Validated after the key elements and stored as canonical text (format_version 27).
	var predicate *indexKeyExpr
	if ci.Predicate != nil {
		if kind != indexBtree {
			return outcome{}, newError(FeatureNotSupported,
				"a partial (WHERE) "+ci.Using+" index is not supported yet")
		}
		// Structural pre-walk: a subquery is 0A000 and a bind parameter 42P02 (both admitted by the
		// resolver). The aggregate 42803 / window 42P20 / non-boolean 42804 rejections then fall out of
		// the Forbidden-context boolean resolve below.
		if err := rejectIndexPredicateStructure(ci.Predicate.Expr); err != nil {
			return outcome{}, err
		}
		s := singleScope(db, table)
		pt := &paramTypes{}
		if _, err := resolveBooleanFilter(s, &ci.Predicate.Expr, pt); err != nil {
			return outcome{}, err
		}
		// Immutability (§9), the same rule an expression key carries: a non-immutable seam/clock/
		// sequence call, a timestamptz-dependent subexpression (references a timestamptz column —
		// conservatively fail-closed), or a resolved STABLE node (the runtime text→date cast,
		// paramTypes.nonimmutable), is 42P17.
		tzHazard := false
		for _, ref := range checkReferencedColumns(ci.Predicate.Expr, columns) {
			if sc, ok := columns[ref].Type.AsScalar(); ok && sc == scalarTimestamptz {
				tzHazard = true
				break
			}
		}
		if indexExprNonimmutableCall(ci.Predicate.Expr) || tzHazard || pt.nonimmutable {
			return outcome{}, newError(InvalidObjectDefinition,
				"functions in index predicate must be marked IMMUTABLE")
		}
		predicate = &indexKeyExpr{ExprText: ci.Predicate.Text, Expr: ci.Predicate.Expr}
	}
	// relationExistsScoped checks the namespace of the target scope: an attachment's OWN snapshot for an
	// attached table (each attached database is an independent namespace, §3), else the temp-aware
	// implicit namespace.
	relationTaken := func(n string) bool {
		if attachName != "" {
			as := db.attachReadSnap(attachName)
			if _, ok := as.table(n); ok {
				return true
			}
			_, _, ok := as.findIndex(n)
			return ok
		}
		return db.relationExists(n)
	}
	name := ci.Name
	if name != "" {
		if err := checkReservedName("index", name); err != nil {
			return outcome{}, err
		}
		if relationTaken(name) {
			return outcome{}, newError(DuplicateTable, "relation already exists: "+name)
		}
	} else {
		// PG's ChooseIndexName / ChooseIndexColumnNames (probed): lowercased table + one name part
		// per key element (list order, duplicates included) + "idx", then the smallest free suffix.
		// A column key's part is the column name; a bare-function-call expression's is the function
		// name (lower(email) → lower); any other expression's is the literal expr (indexes.md §2).
		base := tableKey
		for _, elem := range ci.Keys {
			base += "_" + indexNamePart(elem)
		}
		base += "_idx"
		name = base
		for suffix := 1; relationTaken(name); suffix++ {
			name = base + strconv.Itoa(suffix)
		}
	}

	def := indexDef{Name: name, Keys: ciKeys, Unique: ci.Unique, Kind: kind, Predicate: predicate}
	// The build scan (cost.md §3): page_read per table-tree node + storage_row_read per row. The
	// touched set is the columns the key elements read — an index column for a column key, or every
	// column an expression key references (which may be variable-width, so a spilled value adds its
	// value_decompress slabs — indexes.md §5). An empty table charges 0. Entries are computed here
	// against the pre-index store; the writes below are unmetered. An expression key evaluating with
	// an error aborts the build (nothing is registered — indexes.md §4), preserving all-or-nothing.
	meter := db.session.newMeter()
	mask := make([]bool, len(columns))
	for _, k := range def.Keys {
		if k.Expr == nil {
			mask[k.Col] = true
			continue
		}
		for _, c := range checkReferencedColumns(k.Expr.Expr, columns) {
			mask[c] = true
		}
	}
	// A partial index's predicate is evaluated per row during the build (indexes.md §9), so the
	// columns it references join the touched set — keeping the build cost deterministic + cross-core.
	if def.Predicate != nil {
		for _, c := range checkReferencedColumns(def.Predicate.Expr, columns) {
			mask[c] = true
		}
	}
	// Resolve the index once (column ordinals + resolved expression keys); the maintenance helpers
	// build the unmetered eval env for any expression key (index expressions are immutable, so the
	// rng is never read).
	rindex, err := db.resolveIndex(table, def)
	if err != nil {
		return outcome{}, err
	}
	store := db.lkpStoreScoped(ci.DB, ci.Table)
	stored, nodes, slabs, err := store.ScanWithUnits(mask)
	if err != nil {
		return outcome{}, err
	}
	meter.Charge(costs.PageRead*int64(nodes) + costs.ValueDecompress*int64(slabs))
	entries := make([][]byte, 0, len(stored))
	// A UNIQUE build verifies the existing rows before the index is registered
	// (indexes.md §8): two rows sharing a fully-non-NULL key tuple — i.e. an exempt-free
	// prefix — trap 23505 and create nothing. Unmetered validation (cost.md §3).
	seenPrefixes := make(map[string]bool)
	for _, e := range stored {
		if err := meter.Guard(); err != nil { // enforce the cost ceiling per scanned row
			return outcome{}, err
		}
		meter.Charge(costs.StorageRowRead)
		// The build reads the referenced columns; resolve a faulted row's touched columns before
		// encoding (an expression key may read a spilled value; the evaluator's Unfetched backstop
		// also handles it).
		row, err := store.resolveInlineColumns(e.Row)
		if err != nil {
			return outcome{}, err
		}
		if def.Unique {
			prefix, ok, err := db.indexPrefix(columns, colls, &rindex, row)
			if err != nil {
				return outcome{}, err
			}
			if ok {
				if seenPrefixes[string(prefix)] {
					return outcome{}, newUniqueViolation(ci.Table, def.Name)
				}
				seenPrefixes[string(prefix)] = true
			}
		}
		eks, err := db.indexEntries(columns, colls, &rindex, e.Key, row)
		if err != nil {
			return outcome{}, err
		}
		entries = append(entries, eks...)
	}
	if err := meter.Guard(); err != nil {
		return outcome{}, err
	}

	nameKey := strings.ToLower(def.Name)
	// Register the index catalog entry + its (empty) store in the snapshot that owns the table (the
	// resolution walk — temp-tables.md §2/§4/§8): a session-local temp table's index lives in the
	// session temp snapshot, so the index makes ZERO file writes (the dirty bit lets the commit skip the
	// main image). The entry writes below then route through writeIndexStore, which finds the new store
	// in that same temp snapshot.
	switch {
	case attachName != "":
		// The attachment's index catalog entry + (empty) store live in its working snapshot, published
		// into roots.attached at commit (attached-databases.md §5/§6). attachWriteSnap marks it dirty.
		ws := db.attachWriteSnap(attachName)
		ws.storePaging = db.core.attachments[attachName].storage.paging
		ws.putIndex(tableKey, def, db.attachPageSize(attachName)) // the attachment's own page space (§2)
	case db.isTempTable(ci.Table):
		db.session.tx.tempDirty = true
		db.session.tx.tempWorking.putIndex(tableKey, def, db.pageSize)
	default:
		db.working().putIndex(tableKey, def, db.pageSize)
	}
	istore := db.writeIndexStoreScoped(ci.DB, nameKey)
	// Insert sorted by entry key (indexes.md §1): every insert is then a right-edge append,
	// so the built tree packs ~full instead of splintering under the storage-key order the
	// scan produced (random in entry-key space). Part of the byte contract — the sort fixes
	// the built tree's shape across cores.
	slices.SortFunc(entries, bytes.Compare)
	for _, ek := range entries {
		inserted, err := istore.Insert(ek, nil)
		if err != nil {
			return outcome{}, err
		}
		if !inserted {
			panic("index entry keys are unique (storage-key suffix)")
		}
	}
	return outcome{Kind: outcomeStatement, Cost: meter.Accrued}, nil
}

// executeDropIndex runs a DROP INDEX (spec/design/indexes.md §2): a table's name is
// 42809, a missing one 42704. A pure catalog edit — zero cost, like DROP TABLE. The index is
// resolved along the resolution walk (session-local → persistent — temp-tables.md §8) and removed
// from the snapshot that owns it, so dropping a temp table's index makes zero file writes.
func (db *engine) executeDropIndex(di *dropIndex) (outcome, error) {
	// lkpTable covers both scopes, so DROP INDEX naming a table is 42809 regardless of kind.
	if _, ok := db.lkpTable(di.Name); ok {
		return outcome{}, newError(WrongObjectType, di.Name+" is not an index")
	}
	nameKey := strings.ToLower(di.Name)
	switch {
	case db.isTempIndex(di.Name):
		tableKey, _, _ := db.tempSnap().findIndex(di.Name)
		db.session.tx.tempDirty = true
		db.session.tx.tempWorking.removeIndex(tableKey, nameKey)
	default:
		tableKey, _, ok := db.findIndex(di.Name)
		if !ok {
			return outcome{}, newError(UndefinedObject, "index does not exist: "+di.Name)
		}
		// An index that backs an EXCLUDE constraint cannot be dropped directly — the constraint owns
		// it (the UNIQUE-backing precedent; jed has no ALTER TABLE … DROP CONSTRAINT yet). 2BP01,
		// matching PG's "cannot drop index … because constraint … requires it" (gist.md §7).
		if t, tok := db.lkpTable(tableKey); tok {
			for _, e := range t.Exclusions {
				if strings.EqualFold(e.Index, di.Name) {
					return outcome{}, newError(DependentObjectsStillExist,
						"cannot drop index "+di.Name+" because constraint "+di.Name+" on table "+t.Name+" requires it")
				}
			}
		}
		db.working().removeIndex(tableKey, nameKey)
	}
	return outcome{Kind: outcomeStatement, Cost: 0}, nil
}

// executeCreateType analyzes and runs a CREATE TYPE (spec/design/composite.md): reject a duplicate
// type name (42710), resolve each field's type (a built-in scalar, or a previously-defined
// composite — 42704 if unknown; no self- or forward-reference), reject a duplicate field name
// (42701), then register the composite type in the catalog. Named composites only.
func (db *engine) executeCreateType(ct *createType) (outcome, error) {
	if err := checkReservedName("type", ct.Name); err != nil {
		return outcome{}, err
	}
	if db.readSnap().compositeType(ct.Name) != nil {
		return outcome{}, newError(DuplicateObject, "type "+ct.Name+" already exists")
	}
	fields := make([]compositeField, 0, len(ct.Fields))
	for _, f := range ct.Fields {
		for _, g := range fields {
			if strings.EqualFold(g.Name, f.Name) {
				return outcome{}, newError(DuplicateColumn, "attribute "+f.Name+" specified more than once")
			}
		}
		var fty dataType
		var fdecimal *decimalTypmod
		var fvarchar *uint32
		if base, ok := strings.CutSuffix(f.TypeName, "[]"); ok {
			// An array-typed field (spec/design/array.md §12 — the mirror of an array-of-composite
			// element). The element is a scalar or a previously-defined composite (element_type_code
			// 14 + name on disk); a nested-array element and an array typmod stay deferred (0A000),
			// exactly as for an array column.
			if f.TypeMod != nil {
				return outcome{}, newError(FeatureNotSupported,
					"a type modifier on an array type is not supported yet")
			}
			if elemScalar, scalarOK := scalarTypeFromName(base); scalarOK {
				fty = arrayT(scalarT(elemScalar))
			} else if ctype := db.readSnap().compositeType(base); ctype != nil {
				fty = arrayT(compositeT(ctype.Name))
			} else {
				return outcome{}, newError(UndefinedObject, "type does not exist: "+base)
			}
		} else if _, ok := scalarTypeFromName(f.TypeName); ok {
			s, d, vl, err := resolveTypeAndTypmod(f.TypeName, f.TypeMod)
			if err != nil {
				return outcome{}, err
			}
			fty, fdecimal, fvarchar = scalarT(s), d, vl
		} else if _, ok := rangeByName(f.TypeName); ok {
			// A range-typed composite field (a range inside CREATE TYPE) is deferred this slice (only
			// range *columns* are storable — spec/design/ranges.md §3); the type name IS known, so this
			// is 0A000, not the 42704 below.
			return outcome{}, newError(FeatureNotSupported,
				"a range-typed composite field ("+f.TypeName+") is not supported yet")
		} else if db.readSnap().compositeType(f.TypeName) != nil {
			if f.TypeMod != nil {
				return outcome{}, newError(FeatureNotSupported,
					"a type modifier is not supported for composite type "+f.TypeName)
			}
			fty = compositeT(f.TypeName)
		} else {
			return outcome{}, newError(UndefinedObject, "type does not exist: "+f.TypeName)
		}
		fields = append(fields, compositeField{Name: f.Name, Type: fty, Decimal: fdecimal, VarcharLen: fvarchar, NotNull: f.NotNull})
	}
	// Bound composite-type nesting depth (CLAUDE.md §13; cost.md §7b). A chain of CREATE TYPEs each
	// nesting the previous (`a`, `b AS (x a)`, …) builds unbounded depth across many cheap statements —
	// invisible to the per-statement input-size cap and the parser nesting counter — and every derived
	// recursive walk (codec, comparator, record_out/in, ResolveColType) recurses to this depth. Reject
	// at the producer so no over-deep type enters the catalog and every downstream walk stays
	// stack-safe. Fields reference only existing types (each already ≤ maxCompositeDepth), so this
	// depth computation's recursion is itself bounded.
	cache := make(map[string]int)
	maxField := 0
	for _, f := range fields {
		if d := db.readSnap().compositeTypeDepth(f.Type, cache); d > maxField {
			maxField = d
		}
	}
	if depth := 1 + maxField; depth > maxCompositeDepth {
		return outcome{}, newError(StatementTooComplex,
			fmt.Sprintf("composite type %s nesting depth %d exceeds the maximum of %d", ct.Name, depth, maxCompositeDepth))
	}
	db.working().putType(&compositeType{Name: ct.Name, Fields: fields})
	return outcome{Kind: outcomeStatement, Cost: 0}, nil
}

// executeDropType analyzes and runs a DROP TYPE (spec/design/composite.md §7). RESTRICT (the only
// behavior this slice): a missing type is 42704 unless IF EXISTS; if any table column or composite
// field still references the type, 2BP01; otherwise remove it from the catalog.
func (db *engine) executeDropType(dt *dropType) (outcome, error) {
	if db.readSnap().compositeType(dt.Name) == nil {
		if dt.IfExists {
			return outcome{Kind: outcomeStatement, Cost: 0}, nil
		}
		return outcome{}, newError(UndefinedObject, "type does not exist: "+dt.Name)
	}
	if dep, ok := db.compositeDependentAny(dt.Name); ok {
		return outcome{}, newError(DependentObjectsStillExist,
			"cannot drop type "+dt.Name+" because other objects depend on it: "+dep)
	}
	db.working().removeType(strings.ToLower(dt.Name))
	return outcome{Kind: outcomeStatement, Cost: 0}, nil
}

// executeCreateSequence analyzes and runs a CREATE SEQUENCE (spec/design/sequences.md). Resolve
// the option overrides against the INCREMENT sign's type defaults, validate the set (22023),
// reject a relation-namespace collision (42P07 unless IF NOT EXISTS), and register the sequence.
func (db *engine) executeCreateSequence(cs *createSequence) (outcome, error) {
	// The reservation is not a collision, so IF NOT EXISTS does not suppress it
	// (spec/design/introspection.md §4).
	if err := checkReservedName("sequence", cs.Name); err != nil {
		return outcome{}, err
	}
	if db.relationExists(cs.Name) {
		if cs.IfNotExists {
			return outcome{Kind: outcomeStatement, Cost: 0}, nil
		}
		return outcome{}, newError(DuplicateTable, "relation already exists: "+cs.Name)
	}
	def, err := buildSequenceDef(cs.Name, cs.Options, nil)
	if err != nil {
		return outcome{}, err
	}
	db.working().putSequence(def)
	return outcome{Kind: outcomeStatement, Cost: 0}, nil
}

// executeDropSequence analyzes and runs a DROP SEQUENCE (spec/design/sequences.md §1).
// RESTRICT-only: a missing sequence is 42P01 unless IF EXISTS. No dependency tracking this slice
// (a plain DEFAULT nextval('s') creates none — PG). Multiple names are dropped left to right.
func (db *engine) executeDropSequence(ds *dropSequence) (outcome, error) {
	for _, name := range ds.Names {
		// Missing → 42P01 (unless IF EXISTS). An OWNED (serial) sequence has a dependent — its
		// column's default — so RESTRICT (the only mode this slice; CASCADE 0A000) is 2BP01
		// (spec/design/sequences.md §12).
		seq := db.sequence(name)
		if seq == nil {
			if ds.IfExists {
				continue
			}
			return outcome{}, newError(UndefinedTable, "sequence does not exist: "+name)
		}
		if seq.OwnedBy != nil {
			// The owning table is always present (its own DROP TABLE would auto-drop this sequence
			// first), so the column name for the detail resolves. The scope-aware lkpTable finds an
			// owned TEMP sequence's temp owner (temp-tables.md §8).
			colName, tableName := "", seq.OwnedBy.Table
			if t, ok := db.lkpTable(seq.OwnedBy.Table); ok {
				tableName = t.Name
				if int(seq.OwnedBy.Column) < len(t.Columns) {
					colName = t.Columns[seq.OwnedBy.Column].Name
				}
			}
			return outcome{}, newError(DependentObjectsStillExist, fmt.Sprintf(
				"cannot drop sequence %s because other objects depend on it: default value for column %s of table %s depends on sequence %s",
				seq.Name, colName, tableName, seq.Name,
			))
		}
		// Not owned: remove from whichever scope owns it (a temp sequence is always owned, so this
		// routed path is reached only for a plain persistent sequence — temp-tables.md §8).
		db.removeSequenceRouted(name)
	}
	return outcome{Kind: outcomeStatement, Cost: 0}, nil
}

// executeAlterSequence analyzes and runs an ALTER SEQUENCE [IF EXISTS] s <action>
// (spec/design/sequences.md §4/§15). A missing sequence is 42P01 unless IF EXISTS (then a no-op).
// The option form re-edits the definition (PG init_params, isInit=false — only written options
// change, the counter preserved unless RESTART); RENAME TO moves the catalog key. Touches no session
// state (currval/lastval unchanged). A catalog write (the write path, transactional, §5).
func (db *engine) executeAlterSequence(as *alterSequence) (outcome, error) {
	snapDef := db.sequence(as.Name)
	if snapDef == nil {
		if as.IfExists {
			return outcome{Kind: outcomeStatement, Cost: 0}, nil
		}
		return outcome{}, newError(UndefinedTable, "relation does not exist: "+as.Name)
	}
	existing := *snapDef
	if as.RenameTo != "" {
		if err := db.alterSequenceRename(&existing, as.RenameTo); err != nil {
			return outcome{}, err
		}
	} else {
		// AS type on ALTER is 0A000 — the value type is not persisted (sequences.md §14.4), so the
		// original type for re-deriving a default bound is gone.
		if as.Options.DataType != "" {
			return outcome{}, newError(FeatureNotSupported, "ALTER SEQUENCE ... AS type is not supported")
		}
		newDef, err := applySeqAlter(&existing, as.Options, as.Restart)
		if err != nil {
			return outcome{}, err
		}
		db.putSequenceRouted(newDef)
	}
	return outcome{Kind: outcomeStatement, Cost: 0}, nil
}

// alterSequenceRename implements ALTER SEQUENCE s RENAME TO s2 (spec/design/sequences.md §15.3): a
// collision with any relation — including s itself — is 42P07; otherwise move the entry to the new
// key. For an OWNED sequence, the owning column's DEFAULT nextval('s') text is rewritten in place to
// nextval('s2') (the rows survive — not via putTable) so a later INSERT still advances the renamed
// sequence (jed resolves the sequence by name, unlike PG's OID reference).
func (db *engine) alterSequenceRename(existing *sequenceDef, newName string) error {
	if err := checkReservedName("sequence", newName); err != nil {
		return err
	}
	if db.relationExists(newName) {
		return newError(DuplicateTable, "relation already exists: "+newName)
	}
	if existing.OwnedBy != nil {
		exprText := "nextval ( '" + strings.ReplaceAll(strings.ToLower(newName), "'", "''") + "' )"
		expr, err := parseExpression(exprText)
		if err != nil {
			return err
		}
		// Route to the owner's scope so a renamed owned TEMP sequence rewrites its column default in
		// the temp snapshot (temp-tables.md §8).
		db.setColumnDefaultExprRouted(strings.ToLower(existing.OwnedBy.Table),
			int(existing.OwnedBy.Column), &defaultExprDef{ExprText: exprText, Expr: expr})
	}
	// Capture the owning scope BEFORE the remove: after dropping the old key the new name is in no
	// scope, so a post-remove route would wrongly default to the main image (temp-tables.md §8).
	isTemp := db.isTempSequence(existing.Name)
	def := *existing
	def.Name = newName
	var w *snapshot
	if isTemp {
		db.session.tx.tempDirty = true
		w = db.session.tx.tempWorking
	} else {
		w = db.working()
	}
	w.removeSequence(strings.ToLower(existing.Name))
	w.putSequence(&def)
	return nil
}
