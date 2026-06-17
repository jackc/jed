package jed

import (
	"fmt"
	"math"
	"strconv"
	"strings"
)

// foldInt converts a lexed unsigned magnitude (<= 2^63) and a sign into a signed
// int64, reporting ok=false when the result does not fit (a bare 2^63, or the
// not-negated 2^63). -(2^63) folds to int64's minimum. See spec/design/grammar.md §4.
func foldInt(magnitude uint64, negate bool) (int64, bool) {
	if negate {
		if magnitude <= uint64(math.MaxInt64) {
			return -int64(magnitude), true
		}
		if magnitude == uint64(1)<<63 {
			return math.MinInt64, true
		}
		return 0, false
	}
	if magnitude <= uint64(math.MaxInt64) {
		return int64(magnitude), true
	}
	return 0, false
}

// binaryExpr builds a binary-operator expression node.
func binaryExpr(op BinaryOp, lhs, rhs Expr) Expr {
	return Expr{Kind: ExprBinary, Binary: &BinaryExpr{Op: op, Lhs: lhs, Rhs: rhs}}
}

// Hand-written recursive-descent parser (CLAUDE.md §5, §10).
//
// Statement productions are filled in feature-by-feature (Phases B–E). Until a
// production is implemented it returns a structured 0A000 feature-not-supported
// error rather than panicking, so the harness reports "not yet" cleanly.

// Parser is a token cursor over a single statement.
// maxExprDepth is the maximum expression / subquery / set-operation nesting depth a statement
// may reach (spec/design/cost.md §7; CLAUDE.md §13). The §13 native-stack-safety gate for
// untrusted input: the recursive-descent parser and the resolve/eval walks recurse to a
// statement's nesting depth, so deeply-nested SQL would overflow the call stack BEFORE the cost
// meter runs (54P01 cannot catch it). Counting logical depth against this fixed bound — rather
// than PG's runtime stack-pointer probe — is deterministic and cross-core identical (§8): the
// constant is the SAME in every core (Rust / Go / TS). 256 sits with a >2× margin under the
// weakest core's native ceiling (the TS/Node default stack: ~547 nested subqueries) yet far above
// any realistic query. Exceeding it aborts with 54001 statement_too_complex.
const maxExprDepth = 256

type Parser struct {
	tokens []Token
	pos    int
	// depth is the current expression/query nesting depth (see maxExprDepth). Incremented once
	// per AST level descended (deepen), restored on the way back up; left stale on the error path
	// because a depth error aborts the whole parse.
	depth int
}

// NewParser builds a parser over the given tokens.
func NewParser(tokens []Token) *Parser {
	return &Parser{tokens: tokens}
}

// deepen descends one nesting level, enforcing maxExprDepth (spec/design/cost.md §7). Call at
// every point the AST gains a level — a binary-chain step, a unary, a postfix, a re-entry into a
// fresh sub-expression, a nested subquery, a set-op branch. The caller restores the depth with
// undeepen on the success path (an error short-circuits, leaving it stale, which is harmless: the
// parse is aborting).
func (p *Parser) deepen() error {
	p.depth++
	if p.depth > maxExprDepth {
		return NewError(StatementTooComplex, fmt.Sprintf(
			"statement too complex: nesting depth exceeds the maximum of %d", maxExprDepth,
		))
	}
	return nil
}

// undeepen restores one nesting level taken by deepen (success path only).
func (p *Parser) undeepen() { p.depth-- }

// ParseSQL parses a single complete statement from sql.
func ParseSQL(sql string) (Statement, error) {
	tokens, err := Lex(sql)
	if err != nil {
		return Statement{}, err
	}
	p := NewParser(tokens)
	stmt, err := p.parseStatement()
	if err != nil {
		return Statement{}, err
	}
	if err := p.expectEof(); err != nil {
		return Statement{}, err
	}
	return stmt, nil
}

func (p *Parser) parseStatement() (Statement, error) {
	switch p.peekKeyword() {
	// CREATE / DROP dispatch on the object keyword (TABLE vs [UNIQUE] INDEX — grammar.md
	// §30; UNIQUE needs no lookahead of its own — after CREATE the next word being UNIQUE
	// can only be CREATE UNIQUE INDEX).
	case "create":
		if p.peekKeywordAt(1) == "index" || p.peekKeywordAt(1) == "unique" {
			ci, err := p.parseCreateIndex()
			if err != nil {
				return Statement{}, err
			}
			return Statement{CreateIndex: ci}, nil
		}
		// CREATE TYPE — a 2-token lookahead keeps TYPE non-reserved (the CREATE UNIQUE INDEX
		// precedent — composite.md §1).
		if p.peekKeywordAt(1) == "type" {
			ct, err := p.parseCreateType()
			if err != nil {
				return Statement{}, err
			}
			return Statement{CreateType: ct}, nil
		}
		ct, err := p.parseCreateTable()
		if err != nil {
			return Statement{}, err
		}
		return Statement{CreateTable: ct}, nil
	case "drop":
		if p.peekKeywordAt(1) == "index" {
			di, err := p.parseDropIndex()
			if err != nil {
				return Statement{}, err
			}
			return Statement{DropIndex: di}, nil
		}
		if p.peekKeywordAt(1) == "type" {
			dt, err := p.parseDropType()
			if err != nil {
				return Statement{}, err
			}
			return Statement{DropType: dt}, nil
		}
		dt, err := p.parseDropTable()
		if err != nil {
			return Statement{}, err
		}
		return Statement{DropTable: dt}, nil
	case "insert":
		ins, err := p.parseInsert()
		if err != nil {
			return Statement{}, err
		}
		return Statement{Insert: ins}, nil
	case "select":
		return p.parseQueryExpr()
	case "update":
		upd, err := p.parseUpdate()
		if err != nil {
			return Statement{}, err
		}
		return Statement{Update: upd}, nil
	case "delete":
		del, err := p.parseDelete()
		if err != nil {
			return Statement{}, err
		}
		return Statement{Delete: del}, nil
	case "begin", "start":
		return p.parseBegin()
	case "commit", "end":
		return p.parseCommit()
	case "rollback":
		return p.parseRollback()
	case "":
		return Statement{}, NewError(SyntaxError, "expected a SQL statement")
	default:
		return Statement{}, NewError(SyntaxError, fmt.Sprintf("unexpected keyword '%s'", p.peekKeyword()))
	}
}

// parseBegin parses `BEGIN [TRANSACTION|WORK] [READ ONLY|READ WRITE]` or `START TRANSACTION
// [READ ONLY|READ WRITE]` — open an explicit transaction (spec/design/grammar.md §27). The access
// mode defaults to READ WRITE.
func (p *Parser) parseBegin() (Statement, error) {
	if p.peekKeyword() == "start" {
		p.advance()
		if err := p.expectKeyword("transaction"); err != nil {
			return Statement{}, err
		}
	} else {
		p.advance() // BEGIN
		if kw := p.peekKeyword(); kw == "transaction" || kw == "work" {
			p.advance()
		}
	}
	writable, modeSet, err := p.parseAccessMode()
	if err != nil {
		return Statement{}, err
	}
	return Statement{Begin: &Begin{Writable: writable, ModeSet: modeSet}}, nil
}

// parseAccessMode parses the optional access mode after a transaction opener: `READ ONLY` →
// (false, true), `READ WRITE` → (true, true), absent → (false, false) (unspecified — the
// executor applies the handle's default: READ WRITE, or READ ONLY on a read-only handle;
// transactions.md §4.3, api.md §2.1).
func (p *Parser) parseAccessMode() (writable, modeSet bool, err error) {
	if p.peekKeyword() != "read" {
		return false, false, nil
	}
	p.advance() // READ
	switch p.peekKeyword() {
	case "only":
		p.advance()
		return false, true, nil
	case "write":
		p.advance()
		return true, true, nil
	default:
		return false, false, NewError(SyntaxError, fmt.Sprintf("expected ONLY or WRITE after READ, found '%s'", p.peekKeyword()))
	}
}

// parseCommit parses `COMMIT [TRANSACTION|WORK]` / `END [TRANSACTION|WORK]` (grammar.md §27).
func (p *Parser) parseCommit() (Statement, error) {
	p.advance() // COMMIT or END
	p.consumeTransactionOrWork()
	return Statement{Commit: &Commit{}}, nil
}

// parseRollback parses `ROLLBACK [TRANSACTION|WORK]` (grammar.md §27).
func (p *Parser) parseRollback() (Statement, error) {
	if err := p.expectKeyword("rollback"); err != nil {
		return Statement{}, err
	}
	p.consumeTransactionOrWork()
	return Statement{Rollback: &Rollback{}}, nil
}

// consumeTransactionOrWork consumes the optional trailing TRANSACTION / WORK noise word.
func (p *Parser) consumeTransactionOrWork() {
	if kw := p.peekKeyword(); kw == "transaction" || kw == "work" {
		p.advance()
	}
}

// parseCreateTable parses `CREATE TABLE <name> ( <element> [, <element>]* )`, where
// each <element> is a column definition or the table-level `PRIMARY KEY ( <col> [,
// <col>]* )` constraint (spec/design/grammar.md §28). An element starting with the two
// keywords PRIMARY KEY is the table constraint — nothing is lost, since a column named
// "primary" would need a type named "key", which does not exist. Type names are kept as
// written and resolved during execution (the catalog owns the type lattice); the
// constraint's member names are likewise resolved there (42703/42701/42P16).
func (p *Parser) parseCreateTable() (*CreateTable, error) {
	if err := p.expectKeyword("create"); err != nil {
		return nil, err
	}
	if err := p.expectKeyword("table"); err != nil {
		return nil, err
	}
	name, err := p.expectIdentifier()
	if err != nil {
		return nil, err
	}
	if err := p.expect(TokLParen); err != nil {
		return nil, err
	}

	var columns []ColumnDef
	var tablePKs [][]string
	var checks []CheckDef
	var uniques []UniqueDef
	for {
		if p.peekKeyword() == "primary" && p.peekKeywordAt(1) == "key" {
			p.advance()
			p.advance()
			pkCols, err := p.parsePKColumnList()
			if err != nil {
				return nil, err
			}
			tablePKs = append(tablePKs, pkCols)
		} else if p.atCheckConstraint() {
			check, err := p.parseCheckConstraint()
			if err != nil {
				return nil, err
			}
			checks = append(checks, check)
		} else if p.atUniqueTableConstraint() {
			u, err := p.parseUniqueTableConstraint()
			if err != nil {
				return nil, err
			}
			uniques = append(uniques, u)
		} else {
			col, err := p.parseColumnDef(&checks, &uniques)
			if err != nil {
				return nil, err
			}
			columns = append(columns, col)
		}
		switch p.advance().Kind {
		case TokComma:
			continue
		case TokRParen:
		default:
			return nil, NewError(SyntaxError, "expected ',' or ')'")
		}
		break
	}
	if len(columns) == 0 {
		return nil, NewError(SyntaxError, "a table must have at least one column")
	}
	return &CreateTable{Name: name, Columns: columns, TablePKs: tablePKs, Checks: checks, Uniques: uniques}, nil
}

// atCheckConstraint reports whether the cursor sits on a CHECK constraint: the keyword
// CHECK followed by "(", or CONSTRAINT <ident> CHECK "(" (spec/design/grammar.md §29). The
// keywords stay non-reserved — a column named "check"/"constraint" is followed by a type
// name (an identifier, never "("), so the lookahead loses nothing.
func (p *Parser) atCheckConstraint() bool {
	if p.peekKeyword() == "check" && p.peekKindAt(1) == TokLParen {
		return true
	}
	return p.peekKeyword() == "constraint" &&
		p.peekKeywordAt(2) == "check" && p.peekKindAt(3) == TokLParen
}

// parseCheckConstraint parses one `[CONSTRAINT name] CHECK ( expr )` (the cursor is
// verified by atCheckConstraint). The token span between the parentheses is re-rendered as
// the constraint's persisted text (spec/fileformat/format.md "Check-expression text").
func (p *Parser) parseCheckConstraint() (CheckDef, error) {
	name := ""
	if p.peekKeyword() == "constraint" {
		p.advance()
		n, err := p.expectIdentifier()
		if err != nil {
			return CheckDef{}, err
		}
		name = n
	}
	if err := p.expectKeyword("check"); err != nil {
		return CheckDef{}, err
	}
	if err := p.expect(TokLParen); err != nil {
		return CheckDef{}, err
	}
	start := p.pos
	expr, err := p.parseExpr()
	if err != nil {
		return CheckDef{}, err
	}
	text := renderTokens(p.tokens[start:p.pos])
	if err := p.expect(TokRParen); err != nil {
		return CheckDef{}, err
	}
	return CheckDef{Name: name, Expr: expr, Text: text}, nil
}

// atUniqueTableConstraint reports whether the cursor sits on a table-level UNIQUE
// constraint: the keyword UNIQUE followed by "(", or CONSTRAINT <ident> UNIQUE
// (spec/design/grammar.md §31). The keywords stay non-reserved — a column named "unique"
// is followed by a type name (an identifier, never "("), so the lookahead loses nothing.
func (p *Parser) atUniqueTableConstraint() bool {
	if p.peekKeyword() == "unique" && p.peekKindAt(1) == TokLParen {
		return true
	}
	return p.peekKeyword() == "constraint" && p.peekKeywordAt(2) == "unique"
}

// parseUniqueTableConstraint parses one table-level `[CONSTRAINT name] UNIQUE ( col [,
// col]* )` (the cursor is verified by atUniqueTableConstraint). The member list reuses
// the PRIMARY KEY list shape (spec/design/grammar.md §31).
func (p *Parser) parseUniqueTableConstraint() (UniqueDef, error) {
	name := ""
	if p.peekKeyword() == "constraint" {
		p.advance()
		n, err := p.expectIdentifier()
		if err != nil {
			return UniqueDef{}, err
		}
		name = n
	}
	if err := p.expectKeyword("unique"); err != nil {
		return UniqueDef{}, err
	}
	columns, err := p.parsePKColumnList()
	if err != nil {
		return UniqueDef{}, err
	}
	return UniqueDef{Name: name, Columns: columns}, nil
}

// parsePKColumnList parses the parenthesized member list of a table-level PRIMARY KEY
// constraint: `( <col> [, <col>]* )`. Must be non-empty — `PRIMARY KEY ()` is 42601 (the
// first expectIdentifier rejects `)`).
func (p *Parser) parsePKColumnList() ([]string, error) {
	if err := p.expect(TokLParen); err != nil {
		return nil, err
	}
	first, err := p.expectIdentifier()
	if err != nil {
		return nil, err
	}
	cols := []string{first}
	for {
		switch p.advance().Kind {
		case TokComma:
			col, err := p.expectIdentifier()
			if err != nil {
				return nil, err
			}
			cols = append(cols, col)
		case TokRParen:
			return cols, nil
		default:
			return nil, NewError(SyntaxError, "expected ',' or ')'")
		}
	}
}

func (p *Parser) parseColumnDef(checks *[]CheckDef, uniques *[]UniqueDef) (ColumnDef, error) {
	name, err := p.expectIdentifier()
	if err != nil {
		return ColumnDef{}, err
	}
	typeName, err := p.expectIdentifier()
	if err != nil {
		return ColumnDef{}, err
	}
	typeMod, err := p.parseTypeMod()
	if err != nil {
		return ColumnDef{}, err
	}
	isArray, err := p.consumeArrayBrackets()
	if err != nil {
		return ColumnDef{}, err
	}
	if isArray {
		typeName += "[]"
	}
	// Zero or more order-free column constraints: PRIMARY KEY, NOT NULL, DEFAULT <literal>,
	// [CONSTRAINT name] CHECK ( expr ), and [CONSTRAINT name] UNIQUE. A boolean constraint
	// may be repeated harmlessly; a repeated DEFAULT keeps the last; each CHECK is a
	// distinct constraint, collected into the statement-wide list in textual order (a
	// column-level check is semantically identical to a table-level one —
	// spec/design/constraints.md §4). A column-level UNIQUE collects the same way as the
	// one-member form (a repeat folds at execution — spec/design/constraints.md §5).
	primaryKey := false
	notNull := false
	var def *DefaultDef
	for {
		if p.atCheckConstraint() {
			check, err := p.parseCheckConstraint()
			if err != nil {
				return ColumnDef{}, err
			}
			*checks = append(*checks, check)
			continue
		}
		// CONSTRAINT <name> UNIQUE in column position (the named one-member form;
		// CONSTRAINT <name> CHECK ( was caught above).
		if p.peekKeyword() == "constraint" && p.peekKeywordAt(2) == "unique" {
			p.advance()
			cname, err := p.expectIdentifier()
			if err != nil {
				return ColumnDef{}, err
			}
			if err := p.expectKeyword("unique"); err != nil {
				return ColumnDef{}, err
			}
			*uniques = append(*uniques, UniqueDef{Name: cname, Columns: []string{name}})
			continue
		}
		switch p.peekKeyword() {
		case "primary":
			p.advance()
			if err := p.expectKeyword("key"); err != nil {
				return ColumnDef{}, err
			}
			primaryKey = true
		case "not":
			p.advance()
			if err := p.expectKeyword("null"); err != nil {
				return ColumnDef{}, err
			}
			notNull = true
		case "default":
			p.advance()
			// A DEFAULT takes any scalar expression (constraints.md §2). Capture the
			// re-rendered token span as the persisted text (format.md "Check-expression
			// text"), as a CHECK does — the executor classifies a bare literal (constant
			// fast-path) vs an expression (text-persisted).
			start := p.pos
			expr, err := p.parseExpr()
			if err != nil {
				return ColumnDef{}, err
			}
			text := renderTokens(p.tokens[start:p.pos])
			def = &DefaultDef{Expr: expr, Text: text}
		case "unique":
			p.advance()
			*uniques = append(*uniques, UniqueDef{Columns: []string{name}})
		default:
			return ColumnDef{Name: name, TypeName: typeName, TypeMod: typeMod, PrimaryKey: primaryKey, NotNull: notNull, Default: def}, nil
		}
	}
}

// parseTypeMod parses an optional parenthesized type modifier "(" integer ("," integer)? ")"
// after a type name (the first parameterized type, decimal — spec/grammar/grammar.ebnf
// type_name). The shape is accepted for any type name; whether a typmod is meaningful (decimal
// only) and in range is decided at resolve. Empty parens or a non-integer inside is 42601.
// consumeArrayBrackets consumes a trailing array type suffix `[]` (spec/design/array.md §1) after a
// type name (and its optional typmod). Returns whether the type is an array. Multiple `[][]`
// collapse to one array level — multidimensionality is a value property, not array-of-array (§2).
// Only the empty-bracket form `[]` is accepted this slice.
func (p *Parser) consumeArrayBrackets() (bool, error) {
	isArray := false
	for p.peek().Kind == TokLBracket {
		p.advance() // '['
		if err := p.expect(TokRBracket); err != nil {
			return false, err
		}
		isArray = true
	}
	return isArray, nil
}

func (p *Parser) parseTypeMod() (*TypeMod, error) {
	if p.peek().Kind != TokLParen {
		return nil, nil
	}
	p.advance() // '('
	precision, err := p.expectTypmodInt()
	if err != nil {
		return nil, err
	}
	var scale *uint64
	if p.peek().Kind == TokComma {
		p.advance()
		s, err := p.expectTypmodInt()
		if err != nil {
			return nil, err
		}
		scale = &s
	}
	if err := p.expect(TokRParen); err != nil {
		return nil, err
	}
	return &TypeMod{Precision: precision, Scale: scale}, nil
}

func (p *Parser) expectTypmodInt() (uint64, error) {
	t := p.advance()
	if t.Kind != TokInt {
		return 0, NewError(SyntaxError, "expected an integer type modifier")
	}
	return t.Int, nil
}

// parseDropTable parses `DROP TABLE <name>`. A missing table is rejected at execution
// time (42P01), not here. Single table; no IF EXISTS, no CASCADE/RESTRICT this slice
// (spec/design/grammar.md §13).
func (p *Parser) parseDropTable() (*DropTable, error) {
	if err := p.expectKeyword("drop"); err != nil {
		return nil, err
	}
	if err := p.expectKeyword("table"); err != nil {
		return nil, err
	}
	name, err := p.expectIdentifier()
	if err != nil {
		return nil, err
	}
	return &DropTable{Name: name}, nil
}

// parseCreateIndex parses `CREATE INDEX [name] ON <table> ( col [, col]* )`
// (spec/design/grammar.md §30). The optional name needs one disambiguation because no
// word is reserved: the word after INDEX is the index name UNLESS it is `ON` followed by
// a word and then `(` — that exact three-token shape can only be the unnamed form's
// `ON table (`. Key columns are bare identifiers (no expression/ordered/partial keys this
// slice — a `(`/`ASC`/`DESC` after a key is the natural 42601).
func (p *Parser) parseCreateIndex() (*CreateIndex, error) {
	if err := p.expectKeyword("create"); err != nil {
		return nil, err
	}
	unique := p.peekKeyword() == "unique"
	if unique {
		p.advance()
	}
	if err := p.expectKeyword("index"); err != nil {
		return nil, err
	}
	unnamed := p.peekKeyword() == "on" &&
		p.peekKindAt(1) == TokWord && p.peekKindAt(2) == TokLParen
	name := ""
	if !unnamed {
		n, err := p.expectIdentifier()
		if err != nil {
			return nil, err
		}
		name = n
	}
	if err := p.expectKeyword("on"); err != nil {
		return nil, err
	}
	table, err := p.expectIdentifier()
	if err != nil {
		return nil, err
	}
	if err := p.expect(TokLParen); err != nil {
		return nil, err
	}
	var columns []string
	for {
		col, err := p.expectIdentifier()
		if err != nil {
			return nil, err
		}
		columns = append(columns, col)
		tok := p.advance()
		if tok.Kind == TokComma {
			continue
		}
		if tok.Kind == TokRParen {
			break
		}
		return nil, NewError(SyntaxError, fmt.Sprintf("expected ',' or ')', found %v", tok))
	}
	return &CreateIndex{Name: name, Table: table, Columns: columns, Unique: unique}, nil
}

// parseDropIndex parses `DROP INDEX <name>` (spec/design/grammar.md §30). A missing index
// (42704) or a table's name (42809) is rejected at execution time, not here.
func (p *Parser) parseDropIndex() (*DropIndex, error) {
	if err := p.expectKeyword("drop"); err != nil {
		return nil, err
	}
	if err := p.expectKeyword("index"); err != nil {
		return nil, err
	}
	name, err := p.expectIdentifier()
	if err != nil {
		return nil, err
	}
	return &DropIndex{Name: name}, nil
}

// parseCreateType parses `CREATE TYPE <name> AS ( <field> <type> [NOT NULL] [, …] )` — a
// composite (row) type (spec/design/composite.md, grammar.md). At least one field (an empty list
// is a syntax error); each field's type is a bare type name (built-in or a composite), resolved at
// execution (42704 if unknown).
func (p *Parser) parseCreateType() (*CreateType, error) {
	if err := p.expectKeyword("create"); err != nil {
		return nil, err
	}
	if err := p.expectKeyword("type"); err != nil {
		return nil, err
	}
	name, err := p.expectIdentifier()
	if err != nil {
		return nil, err
	}
	if err := p.expectKeyword("as"); err != nil {
		return nil, err
	}
	if err := p.expect(TokLParen); err != nil {
		return nil, err
	}
	var fields []TypeFieldDef
	for {
		fname, err := p.expectIdentifier()
		if err != nil {
			return nil, err
		}
		typeName, err := p.expectIdentifier()
		if err != nil {
			return nil, err
		}
		typeMod, err := p.parseTypeMod()
		if err != nil {
			return nil, err
		}
		notNull := false
		if p.peekKeyword() == "not" {
			p.advance()
			if err := p.expectKeyword("null"); err != nil {
				return nil, err
			}
			notNull = true
		}
		fields = append(fields, TypeFieldDef{Name: fname, TypeName: typeName, TypeMod: typeMod, NotNull: notNull})
		tok := p.advance()
		if tok.Kind == TokComma {
			continue
		}
		if tok.Kind == TokRParen {
			break
		}
		return nil, NewError(SyntaxError, fmt.Sprintf("expected ',' or ')', found %v", tok))
	}
	return &CreateType{Name: name, Fields: fields}, nil
}

// parseDropType parses `DROP TYPE [IF EXISTS] <name> [RESTRICT | CASCADE]`
// (spec/design/composite.md §7). RESTRICT is the default and the only behavior this slice;
// CASCADE is rejected (0A000) at execution. A missing type (42704) and dependents (2BP01) are
// execution-time.
func (p *Parser) parseDropType() (*DropType, error) {
	if err := p.expectKeyword("drop"); err != nil {
		return nil, err
	}
	if err := p.expectKeyword("type"); err != nil {
		return nil, err
	}
	ifExists := p.peekKeyword() == "if"
	if ifExists {
		p.advance()
		if err := p.expectKeyword("exists"); err != nil {
			return nil, err
		}
	}
	name, err := p.expectIdentifier()
	if err != nil {
		return nil, err
	}
	// Optional trailing RESTRICT / CASCADE (a keyword, consumed here; CASCADE is 0A000 at exec).
	cascade := false
	switch p.peekKeyword() {
	case "restrict":
		p.advance()
	case "cascade":
		p.advance()
		cascade = true
	}
	if cascade {
		return nil, NewError(FeatureNotSupported, "DROP TYPE ... CASCADE is not supported")
	}
	return &DropType{Name: name, IfExists: ifExists}, nil
}

// parseInsert parses `INSERT INTO <table> [( <col> [, <col>]* )] VALUES <row> [, <row>]*`,
// where each <row> is `( <value> [, <value>]* )` and each <value> is a literal or the DEFAULT
// keyword. The optional column list names the target columns; unlisted columns take their
// default. The executor resolves names + type-checks each row and inserts all-or-nothing
// (spec/design/grammar.md §12, constraints.md §2).
func (p *Parser) parseInsert() (*Insert, error) {
	if err := p.expectKeyword("insert"); err != nil {
		return nil, err
	}
	if err := p.expectKeyword("into"); err != nil {
		return nil, err
	}
	table, err := p.expectIdentifier()
	if err != nil {
		return nil, err
	}

	// Optional column list `( col [, col]* )` before VALUES. An empty `()` is rejected (the
	// first expectIdentifier errors 42601 on `)`).
	var columns []string
	if p.peek().Kind == TokLParen {
		p.advance() // '('
		for {
			name, err := p.expectIdentifier()
			if err != nil {
				return nil, err
			}
			columns = append(columns, name)
			switch p.advance().Kind {
			case TokComma:
				continue
			case TokRParen:
			default:
				return nil, NewError(SyntaxError, "expected ',' or ')'")
			}
			break
		}
	}

	// The source is EITHER a SELECT (INSERT ... SELECT — §24) OR a VALUES list. `VALUES` and
	// `SELECT` are disjoint leading keywords, so a peek decides without lookahead.
	if p.peekKeyword() == "select" {
		sel, err := p.parseSelect()
		if err != nil {
			return nil, err
		}
		returning, err := p.parseReturning()
		if err != nil {
			return nil, err
		}
		return &Insert{Table: table, Columns: columns, Select: sel, Returning: returning}, nil
	}

	if err := p.expectKeyword("values"); err != nil {
		return nil, err
	}

	var rows [][]InsertValue
	for {
		row, err := p.parseInsertRow()
		if err != nil {
			return nil, err
		}
		rows = append(rows, row)
		if p.peek().Kind == TokComma {
			p.advance()
			continue
		}
		break
	}
	returning, err := p.parseReturning()
	if err != nil {
		return nil, err
	}
	return &Insert{Table: table, Columns: columns, Rows: rows, Returning: returning}, nil
}

// parseInsertRow parses one parenthesized `( <value> [, <value>]* )` row of an INSERT.
func (p *Parser) parseInsertRow() ([]InsertValue, error) {
	if err := p.expect(TokLParen); err != nil {
		return nil, err
	}
	var values []InsertValue
	for {
		v, err := p.parseInsertValue()
		if err != nil {
			return nil, err
		}
		values = append(values, v)
		switch p.advance().Kind {
		case TokComma:
			continue
		case TokRParen:
		default:
			return nil, NewError(SyntaxError, "expected ',' or ')'")
		}
		break
	}
	if len(values) == 0 {
		return nil, NewError(SyntaxError, "a VALUES row must have at least one value")
	}
	return values, nil
}

// parseInsertValue parses one INSERT value slot: the DEFAULT keyword (not reserved — §3), a
// ROW(...) composite constructor (spec/design/composite.md §1), a bind parameter ($N, bound at
// execute — spec/design/api.md §5), else a literal.
func (p *Parser) parseInsertValue() (InsertValue, error) {
	if p.peekKeyword() == "default" {
		p.advance()
		return InsertValue{IsDefault: true}, nil
	}
	if p.peekKeyword() == "row" && p.peekKindAt(1) == TokLParen {
		// ROW(field, field, …) — recurse on each field (a literal, a $N, or a nested ROW).
		p.advance() // ROW
		if err := p.expect(TokLParen); err != nil {
			return InsertValue{}, err
		}
		var fields []InsertValue
		if p.peek().Kind != TokRParen {
			for {
				f, err := p.parseInsertValue()
				if err != nil {
					return InsertValue{}, err
				}
				fields = append(fields, f)
				tok := p.advance()
				if tok.Kind == TokComma {
					continue
				}
				if tok.Kind == TokRParen {
					break
				}
				return InsertValue{}, NewError(SyntaxError, fmt.Sprintf("expected ',' or ')', found %v", tok))
			}
		} else {
			p.advance() // the empty ROW() — consume ')'
		}
		return InsertValue{IsRow: true, Row: fields}, nil
	}
	if p.peekKeyword() == "array" && p.peekKindAt(1) == TokLBracket {
		// ARRAY[elem, …] — recurse on each element (a literal or a $N).
		p.advance() // ARRAY
		if err := p.expect(TokLBracket); err != nil {
			return InsertValue{}, err
		}
		var elems []InsertValue
		if p.peek().Kind != TokRBracket {
			for {
				e, err := p.parseInsertValue()
				if err != nil {
					return InsertValue{}, err
				}
				elems = append(elems, e)
				tok := p.advance()
				if tok.Kind == TokComma {
					continue
				}
				if tok.Kind == TokRBracket {
					break
				}
				return InsertValue{}, NewError(SyntaxError, fmt.Sprintf("expected ',' or ']', found %v", tok))
			}
		} else {
			p.advance() // the empty ARRAY[] — consume ']'
		}
		return InsertValue{IsArray: true, Array: elems}, nil
	}
	if p.peek().Kind == TokParam {
		n := p.advance().Int
		return InsertValue{IsParam: true, Param: n}, nil
	}
	lit, err := p.parseLiteral()
	if err != nil {
		return InsertValue{}, err
	}
	return InsertValue{Lit: lit}, nil
}

// parseLiteral parses a literal value for INSERT: an integer (with an optional leading
// unary minus, folded here), or one of the keywords NULL / TRUE / FALSE. INSERT takes
// literals only — not general expressions (spec/grammar/grammar.ebnf `literal`).
func (p *Parser) parseLiteral() (Literal, error) {
	negate := false
	if p.peek().Kind == TokMinus {
		p.advance()
		negate = true
	}
	t := p.advance()
	switch {
	case t.Kind == TokInt:
		v, ok := foldInt(t.Int, negate)
		if !ok {
			return Literal{}, NewError(NumericValueOutOfRange,
				"value out of range: integer literal exceeds the maximum signed 64-bit value")
		}
		return Literal{Kind: LiteralInt, Int: v}, nil
	case t.Kind == TokDecimal:
		// A decimal literal carries the unscaled coefficient + scale; the leading unary minus
		// (if any) folds into the sign. Cap checks are at resolve.
		return Literal{Kind: LiteralDecimal, Dec: DecimalFromDigitsScale(negate, t.Word, uint32(t.Int))}, nil
	case !negate && t.Kind == TokStr:
		return Literal{Kind: LiteralText, Str: t.Word}, nil
	case !negate && t.Kind == TokWord && toLowerASCII(t.Word) == "null":
		return Literal{Kind: LiteralNull}, nil
	case !negate && t.Kind == TokWord && toLowerASCII(t.Word) == "true":
		return Literal{Kind: LiteralBool, Bool: true}, nil
	case !negate && t.Kind == TokWord && toLowerASCII(t.Word) == "false":
		return Literal{Kind: LiteralBool, Bool: false}, nil
	default:
		return Literal{}, NewError(SyntaxError, "expected a literal value")
	}
}

// parseSelect parses
// `SELECT <items> FROM <table> [WHERE <predicate>] [ORDER BY <key> [, <key>]*]
// [LIMIT <count>] [OFFSET <count>]`, where <items> is `*` or a comma-separated list of
// column refs / CASTs. LIMIT/OFFSET may appear in either order (§9).
// parseQueryExpr parses a top-level query expression (spec/design/grammar.md §25): one or more
// SELECT cores combined by UNION/INTERSECT/EXCEPT, with an optional trailing ORDER BY/LIMIT/OFFSET
// applying to the whole result. A lone query (no set operator) folds the trailing clauses back onto
// the single Select and is returned as Statement{Select}, leaving the plain-query path untouched;
// otherwise it is Statement{SetOp}.
func (p *Parser) parseQueryExpr() (Statement, error) {
	node, err := p.parseSetExpr()
	if err != nil {
		return Statement{}, err
	}
	// Trailing ORDER BY / LIMIT / OFFSET parse once, onto a scratch Select, then move onto the
	// outermost node (the lone Select, or the outermost SetOp).
	var trailing Select
	if err := p.parseOrderBy(&trailing); err != nil {
		return Statement{}, err
	}
	if err := p.parseLimitOffset(&trailing); err != nil {
		return Statement{}, err
	}
	if node.Select != nil {
		sel := node.Select
		sel.OrderBy = trailing.OrderBy
		sel.Limit = trailing.Limit
		sel.Offset = trailing.Offset
		return Statement{Select: sel}, nil
	}
	so := node.SetOp
	so.OrderBy = trailing.OrderBy
	so.Limit = trailing.Limit
	so.Offset = trailing.Offset
	return Statement{SetOp: so}, nil
}

// parseSubquery parses a parenthesized subquery's inner query_expr (grammar.md §26): a full
// set-expression plus an optional trailing ORDER BY / LIMIT / OFFSET folded onto the node. Mirrors
// parseQueryExpr but yields a QueryExpr (the subquery operand) rather than a Statement. The caller
// has consumed the opening "(" and consumes the closing ")".
func (p *Parser) parseSubquery() (QueryExpr, error) {
	// A nested scalar subquery / EXISTS / IN (SELECT …) is one query-nesting level deeper; the
	// guard also protects the parser's own stack against `(SELECT (SELECT … ))`.
	if err := p.deepen(); err != nil {
		return QueryExpr{}, err
	}
	node, err := p.parseSetExpr()
	if err != nil {
		return QueryExpr{}, err
	}
	var trailing Select
	if err := p.parseOrderBy(&trailing); err != nil {
		return QueryExpr{}, err
	}
	if err := p.parseLimitOffset(&trailing); err != nil {
		return QueryExpr{}, err
	}
	if node.Select != nil {
		node.Select.OrderBy = trailing.OrderBy
		node.Select.Limit = trailing.Limit
		node.Select.Offset = trailing.Offset
	} else {
		node.SetOp.OrderBy = trailing.OrderBy
		node.SetOp.Limit = trailing.Limit
		node.SetOp.Offset = trailing.Offset
	}
	p.undeepen()
	return node, nil
}

// parseSetExpr parses the lower-precedence, left-associative UNION/EXCEPT level. INTERSECT binds
// tighter (parsed inside parseIntersectExpr), so `a UNION b INTERSECT c` becomes
// `a UNION (b INTERSECT c)`.
func (p *Parser) parseSetExpr() (QueryExpr, error) {
	base := p.depth
	left, err := p.parseIntersectExpr()
	if err != nil {
		return QueryExpr{}, err
	}
	for {
		var op SetOpKind
		switch p.peekKeyword() {
		case "union":
			op = SetOpUnion
		case "except":
			op = SetOpExcept
		default:
			p.depth = base
			return left, nil
		}
		if err := p.deepen(); err != nil { // each chained UNION/EXCEPT is one more set-op level
			return QueryExpr{}, err
		}
		p.advance() // UNION | EXCEPT
		all := p.parseSetOpQuantifier()
		right, err := p.parseIntersectExpr()
		if err != nil {
			return QueryExpr{}, err
		}
		left = QueryExpr{SetOp: &SetOp{Op: op, All: all, Lhs: left, Rhs: right}}
	}
}

// parseIntersectExpr parses the higher-precedence, left-associative INTERSECT level.
func (p *Parser) parseIntersectExpr() (QueryExpr, error) {
	base := p.depth
	core, err := p.parseSelectCore()
	if err != nil {
		return QueryExpr{}, err
	}
	left := QueryExpr{Select: core}
	for p.peekKeyword() == "intersect" {
		if err := p.deepen(); err != nil { // each chained INTERSECT is one more set-op level
			return QueryExpr{}, err
		}
		p.advance() // INTERSECT
		all := p.parseSetOpQuantifier()
		right, err := p.parseSelectCore()
		if err != nil {
			return QueryExpr{}, err
		}
		left = QueryExpr{SetOp: &SetOp{Op: SetOpIntersect, All: all, Lhs: left, Rhs: QueryExpr{Select: right}}}
	}
	p.depth = base
	return left, nil
}

// parseSetOpQuantifier consumes the optional ALL (multiset) or DISTINCT (explicit default)
// quantifier after a set operator, returning whether ALL was given.
func (p *Parser) parseSetOpQuantifier() bool {
	switch p.peekKeyword() {
	case "all":
		p.advance()
		return true
	case "distinct":
		p.advance()
		return false
	default:
		return false
	}
}

// parseSelect parses a complete SELECT with its own trailing ORDER BY/LIMIT/OFFSET — the form an
// INSERT ... SELECT source takes (spec/design/grammar.md §24). Behaviorally identical to the
// pre-set-operations select: a select_core plus the trailing clauses.
func (p *Parser) parseSelect() (*Select, error) {
	sel, err := p.parseSelectCore()
	if err != nil {
		return nil, err
	}
	if err := p.parseOrderBy(sel); err != nil {
		return nil, err
	}
	if err := p.parseLimitOffset(sel); err != nil {
		return nil, err
	}
	return sel, nil
}

// parseSelectCore parses a SELECT without a trailing ORDER BY/LIMIT/OFFSET — the operand form of a
// set operation (spec/design/grammar.md §25). The returned Select has no OrderBy/Limit/Offset set.
// The FROM clause is optional: with no `from` keyword the SELECT is FROM-less — one virtual
// zero-column row (spec/design/grammar.md §34).
func (p *Parser) parseSelectCore() (*Select, error) {
	if err := p.expectKeyword("select"); err != nil {
		return nil, err
	}

	// DISTINCT is not reserved (a column may be named `distinct`), and it is the only
	// modifier before the select list, so it takes a two-token lookahead: the leading
	// `DISTINCT` is the modifier iff the next token is neither FROM nor end-of-input —
	// otherwise the word is a column named `distinct` (spec/design/grammar.md §11). This
	// rule must be byte-identical across cores.
	distinct := false
	if p.peekKeyword() == "distinct" {
		next := p.tokens[p.pos+1]
		modifier := next.Kind != TokEof && !(next.Kind == TokWord && toLowerASCII(next.Word) == "from")
		if modifier {
			p.advance()
			distinct = true
		}
	}

	items, err := p.parseSelectItems()
	if err != nil {
		return nil, err
	}
	var from *TableRef
	var joins []JoinClause
	if p.peekKeyword() == "from" {
		p.advance() // FROM
		f, j, err := p.parseFromClause()
		if err != nil {
			return nil, err
		}
		from, joins = &f, j
	}

	sel := &Select{Distinct: distinct, Items: items, From: from, Joins: joins}

	filter, err := p.parseOptionalWhere()
	if err != nil {
		return nil, err
	}
	sel.Filter = filter

	if err := p.parseGroupBy(sel); err != nil {
		return nil, err
	}

	if err := p.parseHaving(sel); err != nil {
		return nil, err
	}

	return sel, nil
}

// parseFromClause parses `from_clause ::= table_ref join_clause*` (grammar.md §15): the first
// table reference followed by a left-deep chain of zero or more joins. The join keywords are
// not reserved (§3); the loop recognizes a join only by a leading join keyword, so any other
// trailing word ends the FROM clause.
func (p *Parser) parseFromClause() (TableRef, []JoinClause, error) {
	from, err := p.parseTableRef()
	if err != nil {
		return TableRef{}, nil, err
	}
	var joins []JoinClause
	for {
		j, ok, err := p.parseJoinClause()
		if err != nil {
			return TableRef{}, nil, err
		}
		if !ok {
			break
		}
		joins = append(joins, j)
	}
	return from, joins, nil
}

// parseTableRef parses `table_ref ::= (identifier | table_function) ("AS"? identifier)?` where
// `table_function ::= identifier "(" expr ("," expr)* ")"` — a base table name OR a
// set-returning function call (generate_series(1, 5)) used as a row source, each with an
// optional alias (grammar.md §15/§35). A `(` immediately after the leading identifier marks the
// function form; the resolver owns arity/type errors. The alias logic is identical for both: an
// explicit AS takes the next identifier unconditionally; an implicit alias is taken only when the
// next token is a word that is NOT a clause/join keyword. The stop-keyword set is a §8 surface.
func (p *Parser) parseTableRef() (TableRef, error) {
	name, err := p.expectIdentifier()
	if err != nil {
		return TableRef{}, err
	}
	// A `(` right after the name = a set-returning function call (no `*`/`DISTINCT`).
	var args []*Expr
	isFunc := false
	if p.peek().Kind == TokLParen {
		isFunc = true
		p.advance()
		for {
			arg, err := p.parseExpr()
			if err != nil {
				return TableRef{}, err
			}
			args = append(args, &arg)
			if p.peek().Kind != TokComma {
				break
			}
			p.advance()
		}
		if err := p.expect(TokRParen); err != nil {
			return TableRef{}, err
		}
	}
	var alias *string
	if p.peekKeyword() == "as" {
		p.advance()
		a, err := p.expectIdentifier()
		if err != nil {
			return TableRef{}, err
		}
		alias = &a
	} else if t := p.peek(); t.Kind == TokWord && !isTableRefStopKeyword(toLowerASCII(t.Word)) {
		a := t.Word
		p.advance()
		alias = &a
	}
	// The column-alias-list form `... AS g(n)` is a deferred narrowing (grammar.md §35): a `(`
	// after the alias is unambiguous (a base table never has one there) and rejected.
	if alias != nil && p.peek().Kind == TokLParen {
		return TableRef{}, NewError(FeatureNotSupported,
			"column alias list on a table function is not supported yet")
	}
	return TableRef{Name: name, Alias: alias, IsFunc: isFunc, Args: args}, nil
}

// parseJoinClause parses one join_clause if a join keyword begins here (returns ok=false to end
// the FROM chain). CROSS JOIN has no ON; the INNER/outer kinds require ON <expr> (a missing ON
// is 42601). The outer kinds (LEFT/RIGHT/FULL [OUTER]) parse into the AST but are rejected at
// execution (0A000) — spec/design/grammar.md §15.
func (p *Parser) parseJoinClause() (JoinClause, bool, error) {
	kw := p.peekKeyword()
	var kind JoinKind
	isCross := false
	switch kw {
	case "join": // a bare JOIN is INNER
		p.advance()
		kind = JoinInner
	case "inner":
		p.advance()
		if err := p.expectKeyword("join"); err != nil {
			return JoinClause{}, false, err
		}
		kind = JoinInner
	case "cross":
		p.advance()
		if err := p.expectKeyword("join"); err != nil {
			return JoinClause{}, false, err
		}
		kind = JoinCross
		isCross = true
	case "left", "right", "full":
		p.advance()
		if p.peekKeyword() == "outer" { // optional OUTER
			p.advance()
		}
		if err := p.expectKeyword("join"); err != nil {
			return JoinClause{}, false, err
		}
		switch kw {
		case "left":
			kind = JoinLeft
		case "right":
			kind = JoinRight
		default:
			kind = JoinFull
		}
	default: // not a join keyword: the FROM chain ends here
		return JoinClause{}, false, nil
	}
	table, err := p.parseTableRef()
	if err != nil {
		return JoinClause{}, false, err
	}
	var on *Expr
	if !isCross {
		if err := p.expectKeyword("on"); err != nil {
			return JoinClause{}, false, err
		}
		e, err := p.parseExpr()
		if err != nil {
			return JoinClause{}, false, err
		}
		on = &e
	}
	return JoinClause{Kind: kind, Table: table, On: on}, true, nil
}

// isTableRefStopKeyword reports whether kw (already lower-cased) is a keyword that may legally
// follow a table_ref, and so must NOT be swallowed as an implicit table alias: a trailing
// clause keyword (where/order/limit/offset) or any join-machinery keyword
// (join/inner/cross/left/right/full/outer/on). `as` is handled separately. This set is a
// CLAUDE.md §8 cross-core determinism surface (spec/design/grammar.md §15).
func isTableRefStopKeyword(kw string) bool {
	switch kw {
	case "where", "group", "having", "order", "limit", "offset",
		"join", "inner", "cross", "left", "right", "full", "outer", "on", "as",
		// set operators end a SELECT core — they must not be swallowed as an implicit table
		// alias (`FROM a UNION ...` is a UNION, not a table `a` aliased `union`). §25.
		"union", "intersect", "except",
		// RETURNING ends an INSERT ... SELECT source — it must not be swallowed as the
		// source's implicit table alias (`... SELECT v FROM t RETURNING v` is the INSERT's
		// clause). §32; PostgreSQL fully reserves the word.
		"returning":
		return true
	default:
		return false
	}
}

// parseOrderBy parses an optional `ORDER BY <key> ("," <key>)*`, where each key is a bare
// column with an optional ASC/DESC and an optional NULLS FIRST|LAST, setting the keys on
// sel. NullsFirst is resolved here: explicit if given, else the direction default (ASC ->
// last, DESC -> first). A bare NULLS not followed by FIRST/LAST is a syntax error (42601).
// Leaves sel.OrderBy nil when there is no ORDER BY (spec/grammar/grammar.ebnf `order_by`).
// parseGroupBy parses `group_by ::= "GROUP" "BY" column_ref ("," column_ref)*` (grammar.md
// §18), after WHERE and before ORDER BY. Each key is a bare/qualified column (never an
// expression/alias/ordinal). `GROUP` is not reserved, so it is a clause only when immediately
// followed by `BY`.
func (p *Parser) parseGroupBy(sel *Select) error {
	if p.peekKeyword() != "group" {
		return nil
	}
	p.advance() // GROUP
	if err := p.expectKeyword("by"); err != nil {
		return err
	}
	for {
		qualifier, col, err := p.parseColumnRef()
		if err != nil {
			return err
		}
		var key Expr
		if qualifier != "" {
			key = Expr{Kind: ExprQualifiedColumn, Qualifier: qualifier, Column: col}
		} else {
			key = Expr{Kind: ExprColumn, Column: col}
		}
		sel.GroupBy = append(sel.GroupBy, key)
		if p.peek().Kind == TokComma {
			p.advance()
			continue
		}
		break
	}
	return nil
}

// parseHaving parses `having_clause ::= "HAVING" expr` (grammar.md §19), after GROUP BY and
// before ORDER BY. `HAVING` is not reserved; the predicate is a general expression (it may
// reference aggregates) checked for boolean at resolve.
func (p *Parser) parseHaving(sel *Select) error {
	if p.peekKeyword() != "having" {
		return nil
	}
	p.advance() // HAVING
	h, err := p.parseExpr()
	if err != nil {
		return err
	}
	sel.Having = &h
	return nil
}

func (p *Parser) parseOrderBy(sel *Select) error {
	if p.peekKeyword() != "order" {
		return nil
	}
	p.advance()
	if err := p.expectKeyword("by"); err != nil {
		return err
	}
	for {
		qualifier, col, err := p.parseColumnRef()
		if err != nil {
			return err
		}
		descending := false
		switch p.peekKeyword() {
		case "asc":
			p.advance()
		case "desc":
			p.advance()
			descending = true
		}
		// Default follows direction (grammar.md §10): NULL is the largest value
		// (PostgreSQL model), so ASC → NULLS LAST, DESC → NULLS FIRST.
		nullsFirst := descending
		if p.peekKeyword() == "nulls" {
			p.advance()
			switch p.peekKeyword() {
			case "first":
				p.advance()
				nullsFirst = true
			case "last":
				p.advance()
				nullsFirst = false
			default:
				return NewError(SyntaxError, "NULLS must be followed by FIRST or LAST")
			}
		}
		sel.OrderBy = append(sel.OrderBy, OrderKey{Qualifier: qualifier, Column: col, Descending: descending, NullsFirst: nullsFirst})
		if p.peek().Kind == TokComma {
			p.advance()
			continue
		}
		break
	}
	return nil
}

// parseLimitOffset parses an optional trailing `LIMIT <count>` and/or `OFFSET <count>`
// in either order, each at most once (a repeat is a syntax error, 42601), setting the
// resolved non-negative counts on sel (spec/grammar/grammar.ebnf `limit_offset`).
func (p *Parser) parseLimitOffset(sel *Select) error {
	for {
		switch p.peekKeyword() {
		case "limit":
			if sel.Limit != nil {
				return NewError(SyntaxError, "duplicate LIMIT clause")
			}
			p.advance()
			n, err := p.parseCount(true)
			if err != nil {
				return err
			}
			sel.Limit = &n
		case "offset":
			if sel.Offset != nil {
				return NewError(SyntaxError, "duplicate OFFSET clause")
			}
			p.advance()
			n, err := p.parseCount(false)
			if err != nil {
				return err
			}
			sel.Offset = &n
		default:
			return nil
		}
	}
}

// parseCount parses a LIMIT/OFFSET count: a non-negative integer literal. The sign is
// folded as in parseLiteral; a negative value is rejected with 2201W (LIMIT) / 2201X
// (OFFSET), and a magnitude over int64's max traps 22003 (the value -0 folds to 0 and is
// accepted). isLimit selects which structured error to raise.
func (p *Parser) parseCount(isLimit bool) (int64, error) {
	negate := false
	if p.peek().Kind == TokMinus {
		p.advance()
		negate = true
	}
	t := p.advance()
	if t.Kind != TokInt {
		return 0, NewError(SyntaxError, "expected an integer count")
	}
	v, ok := foldInt(t.Int, negate)
	if !ok {
		return 0, NewError(NumericValueOutOfRange,
			"value out of range: count exceeds the maximum signed 64-bit value")
	}
	if v < 0 {
		if isLimit {
			return 0, NewError(InvalidRowCountInLimitClause, "LIMIT must not be negative")
		}
		return 0, NewError(InvalidRowCountInOffsetClause, "OFFSET must not be negative")
	}
	return v, nil
}

// parseUpdate parses
// `UPDATE <table> SET <col> = <operand> [, <col> = <operand>]* [WHERE <pred>]`.
func (p *Parser) parseUpdate() (*Update, error) {
	if err := p.expectKeyword("update"); err != nil {
		return nil, err
	}
	table, err := p.expectIdentifier()
	if err != nil {
		return nil, err
	}
	if err := p.expectKeyword("set"); err != nil {
		return nil, err
	}

	var assignments []Assignment
	for {
		column, err := p.expectIdentifier()
		if err != nil {
			return nil, err
		}
		if err := p.expect(TokEq); err != nil {
			return nil, err
		}
		value, err := p.parseExpr()
		if err != nil {
			return nil, err
		}
		assignments = append(assignments, Assignment{Column: column, Value: value})
		if p.peek().Kind == TokComma {
			p.advance()
			continue
		}
		break
	}
	if len(assignments) == 0 {
		return nil, NewError(SyntaxError, "UPDATE must set at least one column")
	}

	filter, err := p.parseOptionalWhere()
	if err != nil {
		return nil, err
	}
	returning, err := p.parseReturning()
	if err != nil {
		return nil, err
	}
	return &Update{Table: table, Assignments: assignments, Filter: filter, Returning: returning}, nil
}

// parseDelete parses `DELETE FROM <table> [WHERE <pred>]`. No WHERE deletes all rows.
func (p *Parser) parseDelete() (*Delete, error) {
	if err := p.expectKeyword("delete"); err != nil {
		return nil, err
	}
	if err := p.expectKeyword("from"); err != nil {
		return nil, err
	}
	table, err := p.expectIdentifier()
	if err != nil {
		return nil, err
	}
	filter, err := p.parseOptionalWhere()
	if err != nil {
		return nil, err
	}
	returning, err := p.parseReturning()
	if err != nil {
		return nil, err
	}
	return &Delete{Table: table, Filter: filter, Returning: returning}, nil
}

// parseOptionalWhere parses an optional trailing `WHERE <expr>` (shared by
// SELECT / UPDATE / DELETE). The expression must resolve to boolean (checked by the
// executor).
func (p *Parser) parseOptionalWhere() (*Expr, error) {
	if p.peekKeyword() != "where" {
		return nil, nil
	}
	p.advance()
	e, err := p.parseExpr()
	if err != nil {
		return nil, err
	}
	return &e, nil
}

// parseReturning parses an optional terminal `RETURNING <select_items>` clause (shared by
// INSERT/UPDATE/DELETE — spec/design/grammar.md §32). RETURNING is not reserved (§3): it is a
// clause only in this trailing position (and it joins the table_ref implicit-alias stop set,
// so an `INSERT ... SELECT` source never swallows it — §15). The item list is the ordinary
// select-items production (`*` or expressions with optional AS labels); an empty list fails
// in parseExpr (42601).
func (p *Parser) parseReturning() (*SelectItems, error) {
	if p.peekKeyword() != "returning" {
		return nil, nil
	}
	p.advance() // RETURNING
	items, err := p.parseSelectItems()
	if err != nil {
		return nil, err
	}
	return &items, nil
}

func (p *Parser) parseSelectItems() (SelectItems, error) {
	if p.peek().Kind == TokStar {
		p.advance()
		return SelectItems{All: true}, nil
	}
	var items []SelectItem
	for {
		e, err := p.parseExpr()
		if err != nil {
			return SelectItems{}, err
		}
		// Optional `AS alias` output label. `AS` is not reserved, so it is taken as an
		// alias marker only here, after a complete expr (spec/grammar/grammar.ebnf
		// `select_item`). The alias never enters resolution (grammar.md §8).
		var alias *string
		if p.peekKeyword() == "as" {
			p.advance()
			name, err := p.expectIdentifier()
			if err != nil {
				return SelectItems{}, err
			}
			alias = &name
		}
		items = append(items, SelectItem{Expr: e, Alias: alias})
		if p.peek().Kind == TokComma {
			p.advance()
			continue
		}
		break
	}
	return SelectItems{Items: items}, nil
}

// --- expression precedence ladder (spec/grammar/grammar.ebnf `expr`) ----------
// Loosest to tightest: OR < AND < NOT < comparison/IS NULL < additive <
// multiplicative < unary minus < primary. One function per level keeps the grammar
// legible (CLAUDE.md §10). The precedence is authored data (spec/functions/catalog.toml);
// this ladder must agree with it.

// parseExpr is the entry point for WHERE, the SELECT list, and UPDATE assignment values.
func (p *Parser) parseExpr() (Expr, error) {
	// A fresh sub-expression is one nesting level deeper (parens, ARRAY/ROW/CASE/function
	// operands, subscript indices all re-enter here). Bounds the recursive descent itself.
	if err := p.deepen(); err != nil {
		return Expr{}, err
	}
	e, err := p.parseOr()
	if err != nil {
		return Expr{}, err
	}
	p.undeepen()
	return e, nil
}

func (p *Parser) parseOr() (Expr, error) {
	base := p.depth
	lhs, err := p.parseAnd()
	if err != nil {
		return Expr{}, err
	}
	for p.peekKeyword() == "or" {
		if err := p.deepen(); err != nil { // each chained OR is one more AST level
			return Expr{}, err
		}
		p.advance()
		rhs, err := p.parseAnd()
		if err != nil {
			return Expr{}, err
		}
		lhs = binaryExpr(OpOr, lhs, rhs)
	}
	p.depth = base
	return lhs, nil
}

func (p *Parser) parseAnd() (Expr, error) {
	base := p.depth
	lhs, err := p.parseNot()
	if err != nil {
		return Expr{}, err
	}
	for p.peekKeyword() == "and" {
		if err := p.deepen(); err != nil { // each chained AND is one more AST level
			return Expr{}, err
		}
		p.advance()
		rhs, err := p.parseNot()
		if err != nil {
			return Expr{}, err
		}
		lhs = binaryExpr(OpAnd, lhs, rhs)
	}
	p.depth = base
	return lhs, nil
}

func (p *Parser) parseNot() (Expr, error) {
	if p.peekKeyword() == "not" {
		p.advance()
		// right-associative: NOT NOT x — each NOT is one more AST level (recursion here, so the
		// depth guard also protects the parser's own stack).
		if err := p.deepen(); err != nil {
			return Expr{}, err
		}
		operand, err := p.parseNot()
		if err != nil {
			return Expr{}, err
		}
		p.undeepen()
		return Expr{Kind: ExprUnary, Unary: &UnaryExpr{Op: OpNot, Operand: operand}}, nil
	}
	return p.parseComparison()
}

// parseComparison parses one comparison, a postfix IS [NOT] NULL, or
// IS [NOT] DISTINCT FROM, all non-associative: `a = b = c` is a syntax error, and
// `a + 1 IS NULL` binds as `(a + 1) IS NULL`. After the shared `IS` `NOT`? it dispatches
// on the NULL vs DISTINCT FROM keyword (spec/grammar/grammar.ebnf `comparison`).
func (p *Parser) parseComparison() (Expr, error) {
	lhs, err := p.parseConcat()
	if err != nil {
		return Expr{}, err
	}
	if p.peekKeyword() == "is" {
		p.advance()
		negated := false
		if p.peekKeyword() == "not" {
			p.advance()
			negated = true
		}
		// IS [NOT] DISTINCT FROM <concat> — NULL-safe equality; else IS [NOT] NULL.
		if p.peekKeyword() == "distinct" {
			p.advance()
			if err := p.expectKeyword("from"); err != nil {
				return Expr{}, err
			}
			rhs, err := p.parseConcat()
			if err != nil {
				return Expr{}, err
			}
			return Expr{Kind: ExprIsDistinct, IsDistinct: &IsDistinctExpr{Lhs: lhs, Rhs: rhs, Negated: negated}}, nil
		}
		if err := p.expectKeyword("null"); err != nil {
			return Expr{}, err
		}
		return Expr{Kind: ExprIsNull, IsNullOf: &IsNullExpr{Operand: lhs, Negated: negated}}, nil
	}
	// `NOT`? (`IN` (...) | `BETWEEN` lo `AND` hi) — a `NOT` here is consumed only when followed
	// by one of these postfix-predicate keywords (two-token lookahead; the prefix `NOT` was
	// already taken by parseNot). Non-associative, at the comparison level (grammar.md §20-§21).
	predNegated := p.peekKeyword() == "not" &&
		(p.peekKeywordAt(1) == "in" || p.peekKeywordAt(1) == "between" || p.peekKeywordAt(1) == "like")
	if predNegated {
		p.advance() // NOT
	}
	if p.peekKeyword() == "in" {
		p.advance()
		if err := p.expect(TokLParen); err != nil {
			return Expr{}, err
		}
		// `IN (SELECT ...)` is the uncorrelated IN-subquery (grammar.md §26), disambiguated by a
		// leading `SELECT`; otherwise a non-empty value list (`IN ()` is a 42601 syntax error).
		if p.peekKeyword() == "select" {
			q, err := p.parseSubquery()
			if err != nil {
				return Expr{}, err
			}
			if err := p.expect(TokRParen); err != nil {
				return Expr{}, err
			}
			return Expr{Kind: ExprInSubquery, InSubquery: &InSubqueryExpr{Lhs: lhs, Query: q, Negated: predNegated}}, nil
		}
		// A non-empty value list (`IN ()` — parseConcat on `)` is a 42601 syntax error).
		first, err := p.parseConcat()
		if err != nil {
			return Expr{}, err
		}
		list := []Expr{first}
		for p.peek().Kind == TokComma {
			p.advance()
			elem, err := p.parseConcat()
			if err != nil {
				return Expr{}, err
			}
			list = append(list, elem)
		}
		if err := p.expect(TokRParen); err != nil {
			return Expr{}, err
		}
		return Expr{Kind: ExprIn, In: &InExpr{Lhs: lhs, List: list, Negated: predNegated}}, nil
	}
	if p.peekKeyword() == "between" {
		p.advance()
		// Both bounds parse at the CONCAT level (one tighter than comparison), which never
		// consumes `AND` (a looser level owned by parseAnd). So the BETWEEN's structural `AND` is
		// matched here and `x BETWEEN a AND b AND c` parses as `(x BETWEEN a AND b) AND c`
		// (grammar.md §21); a `||` bound still works.
		lo, err := p.parseConcat()
		if err != nil {
			return Expr{}, err
		}
		if err := p.expectKeyword("and"); err != nil {
			return Expr{}, err
		}
		hi, err := p.parseConcat()
		if err != nil {
			return Expr{}, err
		}
		return Expr{Kind: ExprBetween, Between: &BetweenExpr{Lhs: lhs, Lo: lo, Hi: hi, Negated: predNegated}}, nil
	}
	if p.peekKeyword() == "like" {
		p.advance()
		rhs, err := p.parseConcat()
		if err != nil {
			return Expr{}, err
		}
		return Expr{Kind: ExprLike, Like: &LikeExpr{Lhs: lhs, Rhs: rhs, Negated: predNegated}}, nil
	}
	var op BinaryOp
	switch p.peek().Kind {
	case TokEq:
		op = OpEq
	case TokLt:
		op = OpLt
	case TokGt:
		op = OpGt
	case TokLe:
		op = OpLe
	case TokGe:
		op = OpGe
	default:
		return lhs, nil
	}
	p.advance()
	// `op ANY/SOME/ALL ( array )` — a quantified array comparison (grammar.md §41): a quantifier
	// may stand in for the ordinary right operand. SOME folds to ANY.
	if kw := p.peekKeyword(); kw == "all" || kw == "any" || kw == "some" {
		all := kw == "all"
		p.advance() // ANY / SOME / ALL
		if err := p.expect(TokLParen); err != nil {
			return Expr{}, err
		}
		// The subquery quantifier form `op ANY(SELECT …)` is a separate deferred Phase-4 item
		// (array-functions.md §11); only the array operand is supported.
		if p.peekKeyword() == "select" {
			return Expr{}, NewError(FeatureNotSupported,
				"the subquery form of ANY/ALL is not supported; use an array operand")
		}
		array, err := p.parseExpr() // a full expression resolving to an array
		if err != nil {
			return Expr{}, err
		}
		if err := p.expect(TokRParen); err != nil {
			return Expr{}, err
		}
		return Expr{Kind: ExprQuantified, Quantified: &QuantifiedExpr{Op: op, All: all, Lhs: lhs, Array: array}}, nil
	}
	rhs, err := p.parseConcat()
	if err != nil {
		return Expr{}, err
	}
	return binaryExpr(op, lhs, rhs), nil
}

// parseConcat parses the "any other operator" level (grammar.md §39/§40, array-functions.md §8/§10):
// one rung tighter than the comparisons, looser than additive, left-associative. It hosts `||` array
// concatenation plus the `@>`/`<@`/`&&` array containment/overlap operators — all the same precedence
// in PostgreSQL. Each operand is an additive expression, so `a + b || c` is `(a + b) || c`; chaining
// mixes freely (`a || b @> c` is `(a || b) @> c`).
func (p *Parser) parseConcat() (Expr, error) {
	base := p.depth
	lhs, err := p.parseAdditive()
	if err != nil {
		return Expr{}, err
	}
	for {
		var op BinaryOp
		switch p.peek().Kind {
		case TokConcat:
			op = OpConcat
		case TokContains:
			op = OpContains
		case TokContainedBy:
			op = OpContainedBy
		case TokOverlaps:
			op = OpOverlaps
		default:
			p.depth = base
			return lhs, nil
		}
		if err := p.deepen(); err != nil { // each chained operator is one more AST level
			return Expr{}, err
		}
		p.advance()
		rhs, err := p.parseAdditive()
		if err != nil {
			return Expr{}, err
		}
		lhs = binaryExpr(op, lhs, rhs)
	}
}

func (p *Parser) parseAdditive() (Expr, error) {
	base := p.depth
	lhs, err := p.parseMultiplicative()
	if err != nil {
		return Expr{}, err
	}
	for {
		var op BinaryOp
		switch p.peek().Kind {
		case TokPlus:
			op = OpAdd
		case TokMinus:
			op = OpSub
		default:
			p.depth = base
			return lhs, nil
		}
		if err := p.deepen(); err != nil { // each chained +/- is one more AST level (`1+1+…`)
			return Expr{}, err
		}
		p.advance()
		rhs, err := p.parseMultiplicative()
		if err != nil {
			return Expr{}, err
		}
		lhs = binaryExpr(op, lhs, rhs)
	}
}

func (p *Parser) parseMultiplicative() (Expr, error) {
	base := p.depth
	lhs, err := p.parseUnary()
	if err != nil {
		return Expr{}, err
	}
	for {
		var op BinaryOp
		switch p.peek().Kind {
		case TokStar:
			op = OpMul
		case TokSlash:
			op = OpDiv
		case TokPercent:
			op = OpMod
		default:
			p.depth = base
			return lhs, nil
		}
		if err := p.deepen(); err != nil { // each chained * / % is one more AST level
			return Expr{}, err
		}
		p.advance()
		rhs, err := p.parseUnary()
		if err != nil {
			return Expr{}, err
		}
		lhs = binaryExpr(op, lhs, rhs)
	}
}

func (p *Parser) parseUnary() (Expr, error) {
	if p.peek().Kind == TokMinus {
		p.advance()
		// Fold unary-minus-of-an-integer-literal into one negative literal, so int64's
		// minimum is representable and the literal range-checks against context. SUPPRESSED
		// when a `::` immediately follows: `::` binds tighter than unary minus (PostgreSQL),
		// so `-N::T` is `-(N::T)` — the cast applies to the unsigned magnitude first
		// (grammar.md §37). A one-token lookahead on the token AFTER the literal.
		if p.peek().Kind == TokInt && p.peekKindAt(1) != TokDoubleColon {
			v, ok := foldInt(p.advance().Int, true)
			if !ok {
				return Expr{}, NewError(NumericValueOutOfRange,
					"value out of range: integer literal exceeds the maximum signed 64-bit value")
			}
			return Expr{Kind: ExprLiteral, Literal: &Literal{Kind: LiteralInt, Int: v}}, nil
		}
		// Fold unary-minus of a decimal literal into one negative decimal literal (decimal
		// negation never overflows). Same `::` suppression.
		if p.peek().Kind == TokDecimal && p.peekKindAt(1) != TokDoubleColon {
			t := p.advance()
			return Expr{Kind: ExprLiteral, Literal: &Literal{
				Kind: LiteralDecimal, Dec: DecimalFromDigitsScale(true, t.Word, uint32(t.Int)),
			}}, nil
		}
		// each chained unary `-` is one more AST level (recursion here, so the depth guard also
		// protects the parser's own stack against `- - - … x`).
		if err := p.deepen(); err != nil {
			return Expr{}, err
		}
		operand, err := p.parseUnary()
		if err != nil {
			return Expr{}, err
		}
		p.undeepen()
		return Expr{Kind: ExprUnary, Unary: &UnaryExpr{Op: OpNeg, Operand: operand}}, nil
	}
	return p.parsePostfix()
}

// parsePostfix parses a primary optionally followed by one or more postfix operators, applied
// left-to-right in token order: a `::type` PostgreSQL typecast (grammar.md §37) or a `.field` /
// `.*` composite field selection (spec/design/composite.md §S4). `expr :: type` desugars to
// CAST(expr AS type) here at parse time — one resolver / evaluator / cost path for both spellings
// — and casts chain left-associatively (`x::int8::int2` = `(x::int8)::int2`). A typmod rides on
// the type name exactly as in CAST (`x::numeric(10,2)`).
//
// Field selection follows PostgreSQL's **parens-required** rule: `.field` / `.*` applies ONLY to a
// **parenthesized** base — `(home).zip`, `(t.home).zip`, `(ROW(1,2)).f1` — and chains on a prior
// field access (`(c).a.b`). A bare `home.zip` / `t.home.zip` is a (multi-part) column reference,
// never field access (PG raises `42P01` for the unparenthesized form). So `.field` fires only when
// the primary started with `(` or after a previous `.field`; otherwise the `.` is left for the
// caller (a trailing `.field` on a bare name is then a syntax error, like PG). NB: a bare `a.b` is
// consumed as a single ExprQualifiedColumn by parseColumnRef inside parsePrimary.
func (p *Parser) parsePostfix() (Expr, error) {
	// Only a PARENTHESIZED primary is field-accessible (PG requires `(expr).field`). A subsequent
	// `.field` keeps the chain field-accessible (`(c).a.b`); a `::` cast does not.
	base0 := p.depth
	fieldAccessible := p.peek().Kind == TokLParen
	expr, err := p.parsePrimary()
	if err != nil {
		return Expr{}, err
	}
	for {
		// each postfix `::`/`[…]`/`.field` wraps the base in one more AST level; deepen only when
		// a postfix actually follows (not on the terminating non-postfix token).
		isPostfix := p.peek().Kind == TokDoubleColon || p.peek().Kind == TokLBracket ||
			(p.peek().Kind == TokDot && fieldAccessible)
		if !isPostfix {
			p.depth = base0
			return expr, nil
		}
		if err := p.deepen(); err != nil {
			return Expr{}, err
		}
		switch {
		case p.peek().Kind == TokDoubleColon:
			p.advance()
			typeName, err := p.expectIdentifier()
			if err != nil {
				return Expr{}, err
			}
			typeMod, err := p.parseTypeMod()
			if err != nil {
				return Expr{}, err
			}
			isArray, err := p.consumeArrayBrackets()
			if err != nil {
				return Expr{}, err
			}
			if isArray {
				typeName += "[]"
			}
			expr = Expr{Kind: ExprCast, Cast: &CastExpr{Inner: expr, TypeName: typeName, TypeMod: typeMod}}
			fieldAccessible = false
		// `base[..][..]` — array subscript (spec/design/array.md §6). Applies to ANY base (no parens
		// rule, unlike `.field`). Consecutive `[…]` brackets collect into ONE access (so `a[1][2]` is
		// a single multidim element read, not nested). Each spec is an index `[i]` or a slice `[m:n]`
		// (bounds optionally omitted). After a subscript a `.field` still needs parens (PG).
		case p.peek().Kind == TokLBracket:
			base := expr
			var subs []SubscriptSpec
			for p.peek().Kind == TokLBracket {
				p.advance() // [
				// The lower bound / index is absent only before a `:` or `]` (`[:n]`, `[]`).
				var lower *Expr
				if p.peek().Kind != TokColon && p.peek().Kind != TokRBracket {
					e, err := p.parseExpr()
					if err != nil {
						return Expr{}, err
					}
					lower = &e
				}
				if p.peek().Kind == TokColon {
					p.advance() // :
					var upper *Expr
					if p.peek().Kind != TokRBracket {
						e, err := p.parseExpr()
						if err != nil {
							return Expr{}, err
						}
						upper = &e
					}
					if err := p.expect(TokRBracket); err != nil {
						return Expr{}, err
					}
					subs = append(subs, SubscriptSpec{IsSlice: true, Lower: lower, Upper: upper})
				} else {
					// Index form: a bare `[]` (no index, no colon) is a syntax error.
					if lower == nil {
						return Expr{}, NewError(SyntaxError, "array subscript requires an index")
					}
					if err := p.expect(TokRBracket); err != nil {
						return Expr{}, err
					}
					subs = append(subs, SubscriptSpec{Index: lower})
				}
			}
			expr = Expr{Kind: ExprSubscript, Base: &base, Subscripts: subs}
			fieldAccessible = false
		// `.field` / `.*` — composite field selection (spec/design/composite.md §S4),
		// parens-required: only on a parenthesized / chained-field base.
		case p.peek().Kind == TokDot && fieldAccessible:
			p.advance()
			base := expr
			if p.peek().Kind == TokStar {
				p.advance()
				expr = Expr{Kind: ExprFieldStar, Base: &base}
				fieldAccessible = false // `.*` is terminal
			} else {
				field, err := p.expectIdentifier()
				if err != nil {
					return Expr{}, err
				}
				expr = Expr{Kind: ExprFieldAccess, Base: &base, Field: field}
				// a field value may itself be composite → `(c).a.b` chains
			}
		default:
			// unreachable: the isPostfix precheck above already returned on a non-postfix token.
			p.depth = base0
			return expr, nil
		}
	}
}

// parsePrimary parses a parenthesized expression, CAST(...), a literal (integer,
// TRUE/FALSE, NULL), or a column reference.
func (p *Parser) parsePrimary() (Expr, error) {
	if p.peek().Kind == TokLParen {
		p.advance()
		// `(SELECT ...)` is a scalar subquery (grammar.md §26), disambiguated by a leading
		// `SELECT` after the `(`; otherwise this is a parenthesized expression.
		if p.peekKeyword() == "select" {
			q, err := p.parseSubquery()
			if err != nil {
				return Expr{}, err
			}
			if err := p.expect(TokRParen); err != nil {
				return Expr{}, err
			}
			return Expr{Kind: ExprScalarSubquery, Subquery: &q}, nil
		}
		e, err := p.parseExpr()
		if err != nil {
			return Expr{}, err
		}
		if err := p.expect(TokRParen); err != nil {
			return Expr{}, err
		}
		return e, nil
	}
	// `EXISTS ( SELECT ... )` — the existence predicate (grammar.md §26). Recognized only when an
	// open-paren + `SELECT` follows, so `exists` stays usable as a column / function name.
	if p.peekKeyword() == "exists" && p.peekKindAt(1) == TokLParen && p.peekKeywordAt(2) == "select" {
		p.advance() // EXISTS
		if err := p.expect(TokLParen); err != nil {
			return Expr{}, err
		}
		q, err := p.parseSubquery()
		if err != nil {
			return Expr{}, err
		}
		if err := p.expect(TokRParen); err != nil {
			return Expr{}, err
		}
		return Expr{Kind: ExprExists, Subquery: &q}, nil
	}
	// `ROW(e1, e2, …)` composite constructor (spec/design/composite.md §1). Recognized when ROW is
	// immediately followed by `(`, so `row` stays usable as a column / function name otherwise. The
	// bare `(a, b)` form is deferred (0A000); only the keyword form parses.
	if p.peekKeyword() == "row" && p.peekKindAt(1) == TokLParen {
		p.advance() // ROW
		if err := p.expect(TokLParen); err != nil {
			return Expr{}, err
		}
		var items []Expr
		if p.peek().Kind != TokRParen {
			for {
				e, err := p.parseExpr()
				if err != nil {
					return Expr{}, err
				}
				items = append(items, e)
				tok := p.advance()
				if tok.Kind == TokComma {
					continue
				}
				if tok.Kind == TokRParen {
					break
				}
				return Expr{}, NewError(SyntaxError, fmt.Sprintf("expected ',' or ')', found %v", tok))
			}
		} else {
			p.advance() // the empty ROW() — consume ')'
		}
		return Expr{Kind: ExprRow, RowItems: items}, nil
	}
	// `ARRAY[e1, e2, …]` array constructor (spec/design/array.md §1). Recognized when ARRAY is
	// immediately followed by `[`, so `array` stays usable as an identifier otherwise.
	if p.peekKeyword() == "array" && p.peekKindAt(1) == TokLBracket {
		p.advance() // ARRAY
		if err := p.expect(TokLBracket); err != nil {
			return Expr{}, err
		}
		var items []Expr
		if p.peek().Kind != TokRBracket {
			for {
				e, err := p.parseExpr()
				if err != nil {
					return Expr{}, err
				}
				items = append(items, e)
				tok := p.advance()
				if tok.Kind == TokComma {
					continue
				}
				if tok.Kind == TokRBracket {
					break
				}
				return Expr{}, NewError(SyntaxError, fmt.Sprintf("expected ',' or ']', found %v", tok))
			}
		} else {
			p.advance() // the empty ARRAY[] — consume ']'
		}
		return Expr{Kind: ExprArray, RowItems: items}, nil
	}
	if p.peekKeyword() == "cast" {
		p.advance()
		if err := p.expect(TokLParen); err != nil {
			return Expr{}, err
		}
		inner, err := p.parseExpr()
		if err != nil {
			return Expr{}, err
		}
		if err := p.expectKeyword("as"); err != nil {
			return Expr{}, err
		}
		typeName, err := p.expectIdentifier()
		if err != nil {
			return Expr{}, err
		}
		typeMod, err := p.parseTypeMod()
		if err != nil {
			return Expr{}, err
		}
		isArray, err := p.consumeArrayBrackets()
		if err != nil {
			return Expr{}, err
		}
		if isArray {
			typeName += "[]"
		}
		if err := p.expect(TokRParen); err != nil {
			return Expr{}, err
		}
		return Expr{Kind: ExprCast, Cast: &CastExpr{Inner: inner, TypeName: typeName, TypeMod: typeMod}}, nil
	}
	// A typed string literal `type '...'` (grammar.md §36) — PostgreSQL's `type 'string'`, equal to
	// CAST('string' AS type) over a string-literal operand: ANY type-naming word immediately followed
	// by a string (`INTERVAL '1 day'`, `TIMESTAMP '...'`, `INTEGER '42'`, `BYTEA '\xDE'`, …).
	// Recognized only when the next token is a string — a one-token lookahead — so the word stays
	// usable as a column / function name otherwise. true/false/null are excluded (their own value
	// literals). The type name is resolved (and the string coerced to it) at resolve; unknown → 42704.
	if kw := p.peekKeyword(); kw != "" && kw != "null" && kw != "true" && kw != "false" && p.peekKindAt(1) == TokStr {
		name := p.advance().Word // the named type (original case; ScalarFromName lowercases)
		t := p.advance()
		return Expr{Kind: ExprTypedLiteral, TypeLitName: name, TypeLitText: t.Word}, nil
	}
	if p.peekKeyword() == "case" {
		p.advance()
		// Simple form has an operand between CASE and the first WHEN; the searched form starts
		// directly with WHEN (grammar.md §23).
		var operand *Expr
		if p.peekKeyword() != "when" {
			op, err := p.parseExpr()
			if err != nil {
				return Expr{}, err
			}
			operand = &op
		}
		var whens []CaseWhen
		for p.peekKeyword() == "when" {
			p.advance()
			cond, err := p.parseExpr()
			if err != nil {
				return Expr{}, err
			}
			if err := p.expectKeyword("then"); err != nil {
				return Expr{}, err
			}
			res, err := p.parseExpr()
			if err != nil {
				return Expr{}, err
			}
			whens = append(whens, CaseWhen{Cond: cond, Result: res})
		}
		if len(whens) == 0 {
			return Expr{}, NewError(SyntaxError, "CASE requires at least one WHEN clause")
		}
		var els *Expr
		if p.peekKeyword() == "else" {
			p.advance()
			e, err := p.parseExpr()
			if err != nil {
				return Expr{}, err
			}
			els = &e
		}
		if err := p.expectKeyword("end"); err != nil {
			return Expr{}, err
		}
		return Expr{Kind: ExprCase, Case: &CaseExpr{Operand: operand, Whens: whens, Els: els}}, nil
	}
	t := p.peek()
	switch {
	case t.Kind == TokParam:
		return Expr{Kind: ExprParam, Param: p.advance().Int}, nil
	case t.Kind == TokInt:
		v, ok := foldInt(p.advance().Int, false)
		if !ok {
			// The only magnitude > MaxInt64 the lexer admits is 2^63, which fits no
			// signed integer type unless negated (handled by the unary-minus fold).
			return Expr{}, NewError(NumericValueOutOfRange,
				"value out of range: integer literal exceeds the maximum signed 64-bit value")
		}
		return Expr{Kind: ExprLiteral, Literal: &Literal{Kind: LiteralInt, Int: v}}, nil
	case t.Kind == TokDecimal:
		p.advance()
		return Expr{Kind: ExprLiteral, Literal: &Literal{
			Kind: LiteralDecimal, Dec: DecimalFromDigitsScale(false, t.Word, uint32(t.Int)),
		}}, nil
	case t.Kind == TokStr:
		p.advance()
		return Expr{Kind: ExprLiteral, Literal: &Literal{Kind: LiteralText, Str: t.Word}}, nil
	case t.Kind == TokWord && toLowerASCII(t.Word) == "null":
		p.advance()
		return Expr{Kind: ExprLiteral, Literal: &Literal{Kind: LiteralNull}}, nil
	case t.Kind == TokWord && toLowerASCII(t.Word) == "true":
		p.advance()
		return Expr{Kind: ExprLiteral, Literal: &Literal{Kind: LiteralBool, Bool: true}}, nil
	case t.Kind == TokWord && toLowerASCII(t.Word) == "false":
		p.advance()
		return Expr{Kind: ExprLiteral, Literal: &Literal{Kind: LiteralBool, Bool: false}}, nil
	case t.Kind == TokWord && toLowerASCII(t.Word) == "current_timestamp" &&
		!(p.pos+1 < len(p.tokens) && p.tokens[p.pos+1].Kind == TokLParen):
		// `current_timestamp` — the SQL-standard bare keyword (no parens), reserved like the value
		// literals above. Pure sugar: desugar to a `now()` call so resolution / execution / cost /
		// volatility are entirely shared (spec/design/functions.md §12). Not fired when followed by
		// `(` (a precision typmod, deferred) so that form resolves normally (42883).
		p.advance()
		return Expr{Kind: ExprFuncCall, FuncCall: &FuncCallExpr{Name: "now"}}, nil
	case t.Kind == TokWord:
		// Function call: a BARE identifier IMMEDIATELY followed by "(" is a call (the engine's
		// first call syntax — grammar.md §17). The one-token lookahead keeps function names
		// non-reserved (a column may be named `count`); a qualified name is never a call. Only
		// aggregates resolve (42883 otherwise).
		if p.pos+1 < len(p.tokens) && p.tokens[p.pos+1].Kind == TokLParen {
			return p.parseFunctionCall()
		}
		qualifier, name, err := p.parseColumnRef()
		if err != nil {
			return Expr{}, err
		}
		if qualifier != "" {
			return Expr{Kind: ExprQualifiedColumn, Qualifier: qualifier, Column: name}, nil
		}
		return Expr{Kind: ExprColumn, Column: name}, nil
	default:
		return Expr{}, NewError(SyntaxError, "expected an expression")
	}
}

// parseFunctionCall parses
// `function_call ::= identifier "(" ( "*" | function_arg ("," function_arg)* )? ")"` and
// `function_arg ::= ( identifier "=>" )? expr` — the shared aggregate/scalar call syntax
// (grammar.md §17). COUNT(*) is the star form; the argument list may be empty (a function whose
// parameters all DEFAULT, e.g. make_interval()); otherwise it is a comma-separated list of
// positional and/or NAMED (name => value) arguments. A positional argument may not follow a named
// one (42601). ArgNames stays nil when every argument is positional. DISTINCT inside the parens is
// deferred (rejected 42601). Resolution checks per-function arity and fills defaults.
func (p *Parser) parseFunctionCall() (Expr, error) {
	name, err := p.expectIdentifier()
	if err != nil {
		return Expr{}, err
	}
	if err := p.expect(TokLParen); err != nil {
		return Expr{}, err
	}
	// DISTINCT inside a function call (COUNT(DISTINCT x)) is deferred — reject at parse.
	if p.peekKeyword() == "distinct" {
		return Expr{}, NewError(SyntaxError, "DISTINCT inside an aggregate is not supported yet")
	}
	fc := &FuncCallExpr{Name: name}
	anyNamed := false
	switch {
	case p.peek().Kind == TokStar:
		p.advance()
		fc.Star = true
	case p.peek().Kind == TokRParen:
		// Empty argument list (make_interval()) — leave Args/ArgNames empty.
	default:
		var names []*string
		for {
			// The final argument may be `VARIADIC expr` (grammar.md §17, array-functions.md §12):
			// the array is passed directly to a variadic parameter. VARIADIC is a plain keyword
			// (not reserved) recognized only at the start of an argument; once seen, no further
			// argument may follow (42601) and it does not combine with a name.
			if p.peekKeyword() == "variadic" {
				p.advance()
				fc.Variadic = true
				arg, err := p.parseExpr()
				if err != nil {
					return Expr{}, err
				}
				fc.Args = append(fc.Args, &arg)
				names = append(names, nil)
				// A VARIADIC argument must be the last (PostgreSQL, 42601).
				if p.peek().Kind == TokComma {
					return Expr{}, NewError(SyntaxError, "VARIADIC argument must be the last argument")
				}
				break
			}
			// A named argument is `identifier "=>" expr` (grammar.md §17); a two-token lookahead
			// (word then "=>") distinguishes it from a bare expr that starts with an identifier.
			var argName *string
			if p.peek().Kind == TokWord && p.peekKindAt(1) == TokFatArrow {
				nm, err := p.expectIdentifier()
				if err != nil {
					return Expr{}, err
				}
				if err := p.expect(TokFatArrow); err != nil {
					return Expr{}, err
				}
				anyNamed = true
				argName = &nm
			} else if anyNamed {
				// A positional argument may not follow a named one (PostgreSQL, 42601).
				return Expr{}, NewError(SyntaxError, "positional argument cannot follow named argument")
			}
			arg, err := p.parseExpr()
			if err != nil {
				return Expr{}, err
			}
			fc.Args = append(fc.Args, &arg)
			names = append(names, argName)
			if p.peek().Kind != TokComma {
				break
			}
			p.advance()
		}
		// Keep ArgNames nil unless a name appeared (the all-positional sentinel — §8).
		if anyNamed {
			fc.ArgNames = names
		}
	}
	if err := p.expect(TokRParen); err != nil {
		return Expr{}, err
	}
	return Expr{Kind: ExprFuncCall, FuncCall: fc}, nil
}

// parseColumnRef parses `column_ref ::= identifier ("." identifier)?` — a bare column name, or
// a qualified `rel.col` (the "." is TokDot). Returns (qualifier, name); qualifier is "" for a
// bare column (spec/grammar/grammar.ebnf `column_ref`, grammar.md §15).
func (p *Parser) parseColumnRef() (string, string, error) {
	first, err := p.expectIdentifier()
	if err != nil {
		return "", "", err
	}
	if p.peek().Kind == TokDot {
		p.advance()
		second, err := p.expectIdentifier()
		if err != nil {
			return "", "", err
		}
		return first, second, nil
	}
	return "", first, nil
}

// peek returns the current token without consuming it.
func (p *Parser) peek() Token { return p.tokens[p.pos] }

// peekKeyword returns the current token lowercased if it is a word, else "".
func (p *Parser) peekKeyword() string {
	t := p.peek()
	if t.Kind == TokWord {
		return toLowerASCII(t.Word)
	}
	return ""
}

// peekKeywordAt returns the keyword (lowercased) offset tokens ahead of the cursor if that
// token is a word, else "". Used for the two-token NOT IN/BETWEEN/LIKE lookahead (a
// CLAUDE.md §8 determinism surface — byte-identical across the three parsers).
func (p *Parser) peekKeywordAt(offset int) string {
	if p.pos+offset < len(p.tokens) {
		if t := p.tokens[p.pos+offset]; t.Kind == TokWord {
			return toLowerASCII(t.Word)
		}
	}
	return ""
}

// peekKindAt returns the token kind offset tokens ahead of the cursor, or TokEof past the end.
// Used by the EXISTS / scalar-subquery lookahead (grammar.md §26).
func (p *Parser) peekKindAt(offset int) TokenKind {
	if p.pos+offset < len(p.tokens) {
		return p.tokens[p.pos+offset].Kind
	}
	return TokEof
}

// advance consumes and returns the current token.
func (p *Parser) advance() Token {
	t := p.tokens[p.pos]
	if p.pos+1 < len(p.tokens) {
		p.pos++
	}
	return t
}

// expect consumes the current token, requiring its kind to equal want.
func (p *Parser) expect(want TokenKind) error {
	if got := p.advance(); got.Kind != want {
		return NewError(SyntaxError, "unexpected token")
	}
	return nil
}

// expectKeyword consumes the current token, requiring it to be the given keyword
// (case-insensitive).
func (p *Parser) expectKeyword(kw string) error {
	t := p.advance()
	if t.Kind == TokWord && toLowerASCII(t.Word) == kw {
		return nil
	}
	return NewError(SyntaxError, fmt.Sprintf("expected keyword '%s'", kw))
}

// expectIdentifier consumes the current token, requiring it to be a bare word.
func (p *Parser) expectIdentifier() (string, error) {
	t := p.advance()
	if t.Kind != TokWord {
		return "", NewError(SyntaxError, "expected an identifier")
	}
	return t.Word, nil
}

// expectEof requires that all input has been consumed.
func (p *Parser) expectEof() error {
	if p.peek().Kind != TokEof {
		return NewError(SyntaxError, "unexpected trailing input")
	}
	return nil
}

func toLowerASCII(s string) string {
	b := []byte(s)
	for i, c := range b {
		if c >= 'A' && c <= 'Z' {
			b[i] = c + 32
		}
	}
	return string(b)
}

// ParseExpression parses a bare expression — the catalog-load path for a persisted CHECK
// expression (spec/design/constraints.md §4.5). The text was written by renderTokens, so
// it re-lexes to a value-identical token sequence; the caller maps a failure to XX001
// (the file claimed to be well-formed).
func ParseExpression(text string) (Expr, error) {
	tokens, err := Lex(text)
	if err != nil {
		return Expr{}, err
	}
	p := &Parser{tokens: tokens}
	expr, err := p.parseExpr()
	if err != nil {
		return Expr{}, err
	}
	if err := p.expectEof(); err != nil {
		return Expr{}, err
	}
	return expr, nil
}

// renderTokens re-renders a token slice as the persisted check-expression text: each token
// rendered by the closed table in spec/fileformat/format.md "Check-expression text", joined
// with single spaces. A byte contract — identical across every core (CLAUDE.md §8).
func renderTokens(tokens []Token) string {
	parts := make([]string, len(tokens))
	for i, t := range tokens {
		parts[i] = renderToken(t)
	}
	return strings.Join(parts, " ")
}

func renderToken(t Token) string {
	switch t.Kind {
	case TokWord:
		return t.Word
	case TokInt:
		return strconv.FormatUint(t.Int, 10)
	case TokDecimal:
		// The digit string with '.' inserted `scale` digits from the right. The lexer
		// guarantees scale <= len(coeff) (every fractional digit is in the coefficient), so
		// the insertion point is in range; scale == len renders a leading-dot form (".5")
		// and scale == 0 a trailing-dot form ("1."), both of which re-lex as the same
		// decimal value (spec/fileformat/format.md "Check-expression text").
		split := len(t.Word) - int(t.Int)
		return t.Word[:split] + "." + t.Word[split:]
	case TokStr:
		return "'" + strings.ReplaceAll(t.Word, "'", "''") + "'"
	case TokParam:
		return "$" + strconv.FormatUint(t.Int, 10)
	case TokComma:
		return ","
	case TokDot:
		return "."
	case TokLParen:
		return "("
	case TokRParen:
		return ")"
	case TokLBracket:
		return "["
	case TokRBracket:
		return "]"
	case TokStar:
		return "*"
	case TokPlus:
		return "+"
	case TokMinus:
		return "-"
	case TokSlash:
		return "/"
	case TokPercent:
		return "%"
	case TokEq:
		return "="
	case TokLt:
		return "<"
	case TokGt:
		return ">"
	case TokLe:
		return "<="
	case TokGe:
		return ">="
	case TokFatArrow:
		return "=>"
	case TokColon:
		return ":"
	case TokConcat:
		return "||"
	case TokContains:
		return "@>"
	case TokContainedBy:
		return "<@"
	case TokOverlaps:
		return "&&"
	default: // TokEof — never inside the parentheses
		return ""
	}
}
