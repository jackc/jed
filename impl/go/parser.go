package jed

import (
	"fmt"
	"math"
	"strconv"
	"strings"
)

// foldInt converts a lexed unsigned magnitude (<= 2^63) and a sign into a signed
// i64, reporting ok=false when the result does not fit (a bare 2^63, or the
// not-negated 2^63). -(2^63) folds to i64's minimum. See spec/design/grammar.md §4.
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
func newBinaryExpr(op binaryOp, lhs, rhs exprNode) exprNode {
	return exprNode{Kind: exprBinary, Binary: &binaryExpr{Op: op, Lhs: lhs, Rhs: rhs}}
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

// maxIdentifierLength is the maximum length, in bytes, of a single identifier — table / column /
// type / alias / function name (spec/design/cost.md §7; CLAUDE.md §13). The §13 identifier
// hardening gate for untrusted input: an unbounded identifier would otherwise consume O(input)
// memory and land verbatim in the on-disk catalog and keys. Checked in the lexer when an
// identifier token is built (the producer, so every identifier on every parse path is bounded),
// aborting with 42622 name_too_long. Identifiers are ASCII-only (spec/design/grammar.md §3), so
// the byte length is the character count. 63 matches PostgreSQL's NAMEDATALEN − 1 boundary — but
// jed errors where PG silently truncates (a documented PG divergence: jed has no notices, and a
// silent truncation could collide two distinct names — CLAUDE.md §1). A fixed constant, so it is
// deterministic and cross-core identical (§8): the SAME in every core (Rust / Go / TS).
const maxIdentifierLength = 63

type parser struct {
	tokens []token
	pos    int
	// depth is the current expression/query nesting depth (see maxExprDepth). Incremented once
	// per AST level descended (deepen), restored on the way back up; left stale on the error path
	// because a depth error aborts the whole parse.
	depth int
}

// NewParser builds a parser over the given tokens.
func newParser(tokens []token) *parser {
	return &parser{tokens: tokens}
}

// deepen descends one nesting level, enforcing maxExprDepth (spec/design/cost.md §7). Call at
// every point the AST gains a level — a binary-chain step, a unary, a postfix, a re-entry into a
// fresh sub-expression, a nested subquery, a set-op branch. The caller restores the depth with
// undeepen on the success path (an error short-circuits, leaving it stale, which is harmless: the
// parse is aborting).
func (p *parser) deepen() error {
	p.depth++
	if p.depth > maxExprDepth {
		return newError(StatementTooComplex, fmt.Sprintf(
			"statement too complex: nesting depth exceeds the maximum of %d", maxExprDepth,
		))
	}
	return nil
}

// undeepen restores one nesting level taken by deepen (success path only).
func (p *parser) undeepen() { p.depth-- }

// ParseSQL parses a single complete statement from sql.
func parseSQL(sql string) (statement, error) {
	tokens, err := lex(sql)
	if err != nil {
		return statement{}, err
	}
	p := newParser(tokens)
	stmt, err := p.parseStatement()
	if err != nil {
		return statement{}, err
	}
	if err := p.expectEof(); err != nil {
		return statement{}, err
	}
	return stmt, nil
}

func (p *parser) parseStatement() (statement, error) {
	switch p.peekKeyword() {
	case "analyze":
		analyze, err := p.parseAnalyze()
		if err != nil {
			return statement{}, err
		}
		return statement{Analyze: analyze}, nil
	// CREATE / DROP dispatch on the object keyword (TABLE vs [UNIQUE] INDEX — grammar.md
	// §30; UNIQUE needs no lookahead of its own — after CREATE the next word being UNIQUE
	// can only be CREATE UNIQUE INDEX).
	case "create":
		if p.peekKeywordAt(1) == "index" || p.peekKeywordAt(1) == "unique" {
			ci, err := p.parseCreateIndex()
			if err != nil {
				return statement{}, err
			}
			return statement{CreateIndex: ci}, nil
		}
		// CREATE TYPE — a 2-token lookahead keeps TYPE non-reserved (the CREATE UNIQUE INDEX
		// precedent — composite.md §1).
		if p.peekKeywordAt(1) == "type" {
			ct, err := p.parseCreateType()
			if err != nil {
				return statement{}, err
			}
			return statement{CreateType: ct}, nil
		}
		// CREATE SEQUENCE — a 2-token lookahead keeps SEQUENCE non-reserved (sequences.md).
		if p.peekKeywordAt(1) == "sequence" {
			cs, err := p.parseCreateSequence()
			if err != nil {
				return statement{}, err
			}
			return statement{CreateSequence: cs}, nil
		}
		ct, err := p.parseCreateTable()
		if err != nil {
			return statement{}, err
		}
		return statement{CreateTable: ct}, nil
	case "drop":
		if p.peekKeywordAt(1) == "index" {
			di, err := p.parseDropIndex()
			if err != nil {
				return statement{}, err
			}
			return statement{DropIndex: di}, nil
		}
		if p.peekKeywordAt(1) == "type" {
			dt, err := p.parseDropType()
			if err != nil {
				return statement{}, err
			}
			return statement{DropType: dt}, nil
		}
		if p.peekKeywordAt(1) == "sequence" {
			ds, err := p.parseDropSequence()
			if err != nil {
				return statement{}, err
			}
			return statement{DropSequence: ds}, nil
		}
		dt, err := p.parseDropTable()
		if err != nil {
			return statement{}, err
		}
		return statement{DropTable: dt}, nil
	case "alter":
		// ALTER SEQUENCE — the only ALTER statement this slice (sequences.md §4). A 2-token
		// lookahead recognizes it; any other `ALTER …` (TABLE, SYSTEM, …) is not a statement
		// keyword jed knows and falls through to the generic unknown-keyword 42601 below
		// (the no-escape-hatch surface — resource/no_escape_hatch.test).
		if p.peekKeywordAt(1) == "sequence" {
			as, err := p.parseAlterSequence()
			if err != nil {
				return statement{}, err
			}
			return statement{AlterSequence: as}, nil
		}
		if p.peekKeywordAt(1) == "table" {
			at, err := p.parseAlterTable()
			if err != nil {
				return statement{}, err
			}
			return statement{AlterTable: at}, nil
		}
		return statement{}, newError(SyntaxError, "unexpected keyword 'alter'")
	case "insert":
		ins, err := p.parseInsert()
		if err != nil {
			return statement{}, err
		}
		return statement{Insert: ins}, nil
	case "select":
		return p.parseQueryExpr()
	// `WITH …` at statement start can only begin a query with common table expressions
	// (spec/design/cte.md). `with` is non-reserved but unambiguous here.
	case "with":
		return p.parseWithStatement()
	case "update":
		upd, err := p.parseUpdate()
		if err != nil {
			return statement{}, err
		}
		return statement{Update: upd}, nil
	case "delete":
		del, err := p.parseDelete()
		if err != nil {
			return statement{}, err
		}
		return statement{Delete: del}, nil
	case "explain":
		return p.parseExplain()
	case "begin", "start":
		return p.parseBegin()
	case "commit", "end":
		return p.parseCommit()
	case "rollback":
		return p.parseRollback()
	case "":
		return statement{}, newError(SyntaxError, "expected a SQL statement")
	default:
		return statement{}, newError(SyntaxError, fmt.Sprintf("unexpected keyword '%s'", p.peekKeyword()))
	}
}

// parseAnalyze parses `ANALYZE qualified_table [(identifier [, identifier ...])]`.
func (p *parser) parseAnalyze() (*analyzeStmt, error) {
	if err := p.expectKeyword("analyze"); err != nil {
		return nil, err
	}
	db, name, err := p.parseQualifiedTableName()
	if err != nil {
		return nil, err
	}
	columns := []string{}
	if p.peek().Kind == tokLParen {
		p.advance()
		for {
			column, err := p.expectIdentifier()
			if err != nil {
				return nil, err
			}
			columns = append(columns, column)
			if p.peek().Kind != tokComma {
				break
			}
			p.advance()
		}
		if err := p.expect(tokRParen); err != nil {
			return nil, err
		}
	}
	return &analyzeStmt{Name: name, DB: db, Columns: columns}, nil
}

// parseBegin parses `BEGIN [TRANSACTION|WORK] [READ ONLY|READ WRITE]` or `START TRANSACTION
// [READ ONLY|READ WRITE]` — open an explicit transaction (spec/design/grammar.md §27). The access
// mode defaults to READ WRITE.
func (p *parser) parseBegin() (statement, error) {
	if p.peekKeyword() == "start" {
		p.advance()
		if err := p.expectKeyword("transaction"); err != nil {
			return statement{}, err
		}
	} else {
		p.advance() // BEGIN
		if kw := p.peekKeyword(); kw == "transaction" || kw == "work" {
			p.advance()
		}
	}
	writable, modeSet, err := p.parseAccessMode()
	if err != nil {
		return statement{}, err
	}
	return statement{Begin: &begin{Writable: writable, ModeSet: modeSet}}, nil
}

// parseAccessMode parses the optional access mode after a transaction opener: `READ ONLY` →
// (false, true), `READ WRITE` → (true, true), absent → (false, false) (unspecified — the
// executor applies the handle's default: READ WRITE, or READ ONLY on a read-only handle;
// transactions.md §4.3, api.md §2.1).
func (p *parser) parseAccessMode() (writable, modeSet bool, err error) {
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
		return false, false, newError(SyntaxError, fmt.Sprintf("expected ONLY or WRITE after READ, found '%s'", p.peekKeyword()))
	}
}

// parseCommit parses `COMMIT [TRANSACTION|WORK]` / `END [TRANSACTION|WORK]` (grammar.md §27).
func (p *parser) parseCommit() (statement, error) {
	p.advance() // COMMIT or END
	p.consumeTransactionOrWork()
	return statement{Commit: &commit{}}, nil
}

// parseRollback parses `ROLLBACK [TRANSACTION|WORK]` (grammar.md §27).
func (p *parser) parseRollback() (statement, error) {
	if err := p.expectKeyword("rollback"); err != nil {
		return statement{}, err
	}
	p.consumeTransactionOrWork()
	return statement{Rollback: &rollback{}}, nil
}

// consumeTransactionOrWork consumes the optional trailing TRANSACTION / WORK noise word.
func (p *parser) consumeTransactionOrWork() {
	if kw := p.peekKeyword(); kw == "transaction" || kw == "work" {
		p.advance()
	}
}

// parseExplain parses `EXPLAIN [ANALYZE] <statement>` (spec/design/explain.md). EXPLAIN is a
// positional leading keyword — non-reserved, no lookahead — followed by an optional ANALYZE modifier
// and then a restricted inner statement (a query or DML). ANALYZE is consumed positionally: no inner
// statement begins with the word ANALYZE, so there is no ambiguity.
func (p *parser) parseExplain() (statement, error) {
	p.advance() // EXPLAIN
	analyze, verbose, costs, lane := false, false, true, false
	seen := make(map[string]bool)
	optionList := false
	if p.peek().Kind == tokLParen {
		optionList = true
		p.advance()
		for {
			name := p.peekKeyword()
			switch name {
			case "analyze", "verbose", "costs", "lane":
				p.advance()
			default:
				if name == "" {
					return statement{}, newError(SyntaxError, "expected an EXPLAIN option")
				}
				return statement{}, newError(SyntaxError, "unrecognized EXPLAIN option: "+name)
			}
			if seen[name] {
				return statement{}, newError(SyntaxError, "EXPLAIN option specified more than once: "+name)
			}
			seen[name] = true
			value := true
			if kw := p.peekKeyword(); kw == "true" || kw == "false" || kw == "on" || kw == "off" {
				p.advance()
				value = kw == "true" || kw == "on"
			}
			switch name {
			case "analyze":
				analyze = value
			case "verbose":
				verbose = value
			case "costs":
				costs = value
			case "lane":
				lane = value
			}
			if p.peek().Kind == tokRParen {
				p.advance()
				break
			}
			if p.peek().Kind != tokComma {
				return statement{}, newError(SyntaxError, "expected ',' or ')' in EXPLAIN option list")
			}
			p.advance()
		}
	}
	if p.peekKeyword() == "analyze" {
		if optionList {
			return statement{}, newError(SyntaxError, "cannot mix EXPLAIN option list with positional ANALYZE")
		}
		p.advance()
		analyze = true
	}
	inner, err := p.parseExplainInner()
	if err != nil {
		return statement{}, err
	}
	return statement{Explain: &explain{Analyze: analyze, Verbose: verbose, Costs: costs, Lane: lane, Inner: &inner}}, nil
}

// parseExplainInner parses the statement EXPLAIN wraps — restricted to a query (SELECT / WITH) or a
// DML statement (INSERT / UPDATE / DELETE). DDL, transaction control, and a nested EXPLAIN have no
// query plan to render and are rejected 42601.
func (p *parser) parseExplainInner() (statement, error) {
	switch p.peekKeyword() {
	case "select":
		return p.parseQueryExpr()
	case "with":
		return p.parseWithStatement()
	case "insert":
		ins, err := p.parseInsert()
		if err != nil {
			return statement{}, err
		}
		return statement{Insert: ins}, nil
	case "update":
		upd, err := p.parseUpdate()
		if err != nil {
			return statement{}, err
		}
		return statement{Update: upd}, nil
	case "delete":
		del, err := p.parseDelete()
		if err != nil {
			return statement{}, err
		}
		return statement{Delete: del}, nil
	case "":
		return statement{}, newError(SyntaxError, "expected a statement after EXPLAIN")
	default:
		return statement{}, newError(SyntaxError, fmt.Sprintf("EXPLAIN does not support '%s'", p.peekKeyword()))
	}
}

// parseCreateTable parses `CREATE TABLE <name> ( <element> [, <element>]* )`, where
// each <element> is a column definition or the table-level `PRIMARY KEY ( <col> [,
// <col>]* )` constraint (spec/design/grammar.md §28). An element starting with the two
// keywords PRIMARY KEY is the table constraint — nothing is lost, since a column named
// "primary" would need a type named "key", which does not exist. Type names are kept as
// written and resolved during execution (the catalog owns the type lattice); the
// constraint's member names are likewise resolved there (42703/42701/42P16).
func (p *parser) parseCreateTable() (*createTable, error) {
	if err := p.expectKeyword("create"); err != nil {
		return nil, err
	}
	// An optional table_scope between CREATE and TABLE makes the table TEMPORARY
	// (spec/design/temp-tables.md, grammar.ebnf `table_scope`). TEMP / TEMPORARY are NOT reserved (§3):
	// recognized positionally here — the word after TABLE is always the table name, so
	// `CREATE TABLE temp (...)` is an ordinary persistent table named "temp".
	temp := p.peekKeyword() == "temp" || p.peekKeyword() == "temporary"
	if temp {
		p.advance()
	}
	if err := p.expectKeyword("table"); err != nil {
		return nil, err
	}
	// An optional database qualifier `db.table` (attached-databases.md §3, Slice 1b): create the table
	// INTO the named database (`main` / `temp` / a host attachment). A bare name uses the implicit
	// scope. The `.` after the first identifier makes it the qualifier and the next the table name.
	dbQualifier, name, err := p.parseQualifiedTableName()
	if err != nil {
		return nil, err
	}
	if err := p.expect(tokLParen); err != nil {
		return nil, err
	}

	var columns []columnDef
	var tablePKs [][]string
	var checks []checkDef
	var uniques []uniqueDef
	var foreignKeys []foreignKeyDef
	var excludes []excludeDef
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
		} else if p.atForeignKeyTableConstraint() {
			fk, err := p.parseForeignKeyTableConstraint()
			if err != nil {
				return nil, err
			}
			foreignKeys = append(foreignKeys, fk)
		} else if p.atExclusionTableConstraint() {
			ex, err := p.parseExclusionTableConstraint()
			if err != nil {
				return nil, err
			}
			excludes = append(excludes, ex)
		} else {
			col, err := p.parseColumnDef(name, &checks, &uniques, &foreignKeys)
			if err != nil {
				return nil, err
			}
			columns = append(columns, col)
		}
		switch p.advance().Kind {
		case tokComma:
			continue
		case tokRParen:
		default:
			return nil, newError(SyntaxError, "expected ',' or ')'")
		}
		break
	}
	if len(columns) == 0 {
		return nil, newError(SyntaxError, "a table must have at least one column")
	}
	return &createTable{Name: name, Temp: temp, DB: dbQualifier, Columns: columns, TablePKs: tablePKs, Checks: checks, Uniques: uniques, ForeignKeys: foreignKeys, Excludes: excludes}, nil
}

// atExclusionTableConstraint reports whether the cursor sits on a table-level EXCLUDE constraint:
// the keyword EXCLUDE (followed by USING or `(`), or CONSTRAINT <ident> EXCLUDE
// (spec/design/gist.md §7). The keyword stays non-reserved — a column named "exclude" is followed
// by a type name (an identifier), never USING or `(`, so the lookahead loses nothing.
func (p *parser) atExclusionTableConstraint() bool {
	if p.peekKeyword() == "exclude" && (p.peekKeywordAt(1) == "using" || p.peekKindAt(1) == tokLParen) {
		return true
	}
	return p.peekKeyword() == "constraint" && p.peekKeywordAt(2) == "exclude"
}

// parseExclusionTableConstraint parses one `[CONSTRAINT name] EXCLUDE [USING method] ( col WITH op
// [, col2 WITH op2 ...] )` (the cursor is verified by atExclusionTableConstraint). Each operand is a
// bare column name; the WITH operator is captured as its source text (= / &&) and mapped to a
// strategy at execution (spec/design/gist.md §7). The USING method (only gist) is captured verbatim.
func (p *parser) parseExclusionTableConstraint() (excludeDef, error) {
	var name string
	if p.peekKeyword() == "constraint" {
		p.advance()
		n, err := p.expectIdentifier()
		if err != nil {
			return excludeDef{}, err
		}
		name = n
	}
	if err := p.expectKeyword("exclude"); err != nil {
		return excludeDef{}, err
	}
	var using string
	if p.peekKeyword() == "using" {
		p.advance()
		u, err := p.expectIdentifier()
		if err != nil {
			return excludeDef{}, err
		}
		using = u
	}
	if err := p.expect(tokLParen); err != nil {
		return excludeDef{}, err
	}
	var elements []excludeElementDef
	for {
		col, err := p.expectIdentifier()
		if err != nil {
			return excludeDef{}, err
		}
		if err := p.expectKeyword("with"); err != nil {
			return excludeDef{}, err
		}
		// The operator is a single token (= / &&); render it to source text for execution.
		start := p.pos
		p.advance()
		op := renderTokens(p.tokens[start:p.pos])
		elements = append(elements, excludeElementDef{Column: col, Op: op})
		switch p.advance().Kind {
		case tokComma:
			continue
		case tokRParen:
		default:
			return excludeDef{}, newError(SyntaxError, "expected ',' or ')'")
		}
		break
	}
	return excludeDef{Name: name, Using: using, Elements: elements}, nil
}

// atForeignKeyTableConstraint reports whether the cursor sits on a table-level FOREIGN KEY
// constraint: the two keywords FOREIGN KEY, or CONSTRAINT <ident> FOREIGN KEY
// (spec/design/grammar.md §43). The keywords stay non-reserved — a column named "foreign"
// would need a type named "key" (none exists), so the lookahead loses nothing (the PRIMARY
// KEY precedent).
func (p *parser) atForeignKeyTableConstraint() bool {
	if p.peekKeyword() == "foreign" && p.peekKeywordAt(1) == "key" {
		return true
	}
	return p.peekKeyword() == "constraint" &&
		p.peekKeywordAt(2) == "foreign" && p.peekKeywordAt(3) == "key"
}

// parseForeignKeyTableConstraint parses one table-level `[CONSTRAINT name] FOREIGN KEY ( col
// [, col]* ) references_clause` (the cursor is verified by atForeignKeyTableConstraint). The
// local-column list reuses the PRIMARY KEY list shape (spec/design/grammar.md §43).
func (p *parser) parseForeignKeyTableConstraint() (foreignKeyDef, error) {
	name := ""
	if p.peekKeyword() == "constraint" {
		p.advance()
		n, err := p.expectIdentifier()
		if err != nil {
			return foreignKeyDef{}, err
		}
		name = n
	}
	if err := p.expectKeyword("foreign"); err != nil {
		return foreignKeyDef{}, err
	}
	if err := p.expectKeyword("key"); err != nil {
		return foreignKeyDef{}, err
	}
	columns, err := p.parsePKColumnList()
	if err != nil {
		return foreignKeyDef{}, err
	}
	refTable, refColumns, onDelete, onUpdate, err := p.parseReferencesClause()
	if err != nil {
		return foreignKeyDef{}, err
	}
	return foreignKeyDef{
		Name:       name,
		Columns:    columns,
		RefTable:   refTable,
		RefColumns: refColumns,
		OnDelete:   onDelete,
		OnUpdate:   onUpdate,
	}, nil
}

// parseReferencesClause parses a references_clause from the REFERENCES keyword onward (shared
// by the column-level and table-level forms — spec/design/grammar.md §43): the referenced
// table, an optional referenced-column list (nil defaults to the parent's primary key), and
// the ON DELETE / ON UPDATE actions (each at most once, either order; a repeat is 42601).
func (p *parser) parseReferencesClause() (string, []string, refAction, refAction, error) {
	if err := p.expectKeyword("references"); err != nil {
		return "", nil, 0, 0, err
	}
	refTable, err := p.expectIdentifier()
	if err != nil {
		return "", nil, 0, 0, err
	}
	var refColumns []string
	if p.peek().Kind == tokLParen {
		refColumns, err = p.parsePKColumnList()
		if err != nil {
			return "", nil, 0, 0, err
		}
	}
	onDelete := refNoAction
	onUpdate := refNoAction
	seenDelete := false
	seenUpdate := false
	for p.peekKeyword() == "on" {
		p.advance()
		switch p.peekKeyword() {
		case "delete":
			p.advance()
			if seenDelete {
				return "", nil, 0, 0, newError(SyntaxError, "ON DELETE specified more than once")
			}
			seenDelete = true
			onDelete, err = p.parseReferentialAction()
			if err != nil {
				return "", nil, 0, 0, err
			}
		case "update":
			p.advance()
			if seenUpdate {
				return "", nil, 0, 0, newError(SyntaxError, "ON UPDATE specified more than once")
			}
			seenUpdate = true
			onUpdate, err = p.parseReferentialAction()
			if err != nil {
				return "", nil, 0, 0, err
			}
		default:
			return "", nil, 0, 0, newError(SyntaxError, "expected DELETE or UPDATE after ON")
		}
	}
	return refTable, refColumns, onDelete, onUpdate, nil
}

// parseReferentialAction parses one referential_action (spec/design/grammar.md §43). All five
// PG actions parse; CASCADE / SET NULL / SET DEFAULT are rejected later at CREATE TABLE (0A000).
func (p *parser) parseReferentialAction() (refAction, error) {
	switch p.peekKeyword() {
	case "no":
		p.advance()
		if err := p.expectKeyword("action"); err != nil {
			return 0, err
		}
		return refNoAction, nil
	case "restrict":
		p.advance()
		return refRestrict, nil
	case "cascade":
		p.advance()
		return refCascade, nil
	case "set":
		p.advance()
		switch p.peekKeyword() {
		case "null":
			p.advance()
			return refSetNull, nil
		case "default":
			p.advance()
			return refSetDefault, nil
		default:
			return 0, newError(SyntaxError, "expected NULL or DEFAULT after SET")
		}
	default:
		return 0, newError(SyntaxError,
			"expected a referential action: NO ACTION / RESTRICT / CASCADE / SET NULL / SET DEFAULT")
	}
}

// atCheckConstraint reports whether the cursor sits on a CHECK constraint: the keyword
// CHECK followed by "(", or CONSTRAINT <ident> CHECK "(" (spec/design/grammar.md §29). The
// keywords stay non-reserved — a column named "check"/"constraint" is followed by a type
// name (an identifier, never "("), so the lookahead loses nothing.
func (p *parser) atCheckConstraint() bool {
	if p.peekKeyword() == "check" && p.peekKindAt(1) == tokLParen {
		return true
	}
	return p.peekKeyword() == "constraint" &&
		p.peekKeywordAt(2) == "check" && p.peekKindAt(3) == tokLParen
}

// parseCheckConstraint parses one `[CONSTRAINT name] CHECK ( expr )` (the cursor is
// verified by atCheckConstraint). The token span between the parentheses is re-rendered as
// the constraint's persisted text (spec/fileformat/format.md "Check-expression text").
func (p *parser) parseCheckConstraint() (checkDef, error) {
	name := ""
	if p.peekKeyword() == "constraint" {
		p.advance()
		n, err := p.expectIdentifier()
		if err != nil {
			return checkDef{}, err
		}
		name = n
	}
	if err := p.expectKeyword("check"); err != nil {
		return checkDef{}, err
	}
	if err := p.expect(tokLParen); err != nil {
		return checkDef{}, err
	}
	start := p.pos
	expr, err := p.parseExpr()
	if err != nil {
		return checkDef{}, err
	}
	text := renderTokens(p.tokens[start:p.pos])
	if err := p.expect(tokRParen); err != nil {
		return checkDef{}, err
	}
	return checkDef{Name: name, Expr: expr, Text: text}, nil
}

// atUniqueTableConstraint reports whether the cursor sits on a table-level UNIQUE
// constraint: the keyword UNIQUE followed by "(", or CONSTRAINT <ident> UNIQUE
// (spec/design/grammar.md §31). The keywords stay non-reserved — a column named "unique"
// is followed by a type name (an identifier, never "("), so the lookahead loses nothing.
func (p *parser) atUniqueTableConstraint() bool {
	if p.peekKeyword() == "unique" && p.peekKindAt(1) == tokLParen {
		return true
	}
	return p.peekKeyword() == "constraint" && p.peekKeywordAt(2) == "unique"
}

// atPrimaryKeyTableConstraint reports whether the cursor sits on a table-level PRIMARY KEY
// constraint, including the named form. ALTER TABLE uses this to keep the authoritative-but-
// deferred ADD PRIMARY KEY grammar on its 0A000 path instead of parsing `primary key` as a column.
func (p *parser) atPrimaryKeyTableConstraint() bool {
	if p.peekKeyword() == "primary" && p.peekKeywordAt(1) == "key" {
		return true
	}
	return p.peekKeyword() == "constraint" &&
		p.peekKeywordAt(2) == "primary" && p.peekKeywordAt(3) == "key"
}

// parseUniqueTableConstraint parses one table-level `[CONSTRAINT name] UNIQUE ( col [,
// col]* )` (the cursor is verified by atUniqueTableConstraint). The member list reuses
// the PRIMARY KEY list shape (spec/design/grammar.md §31).
func (p *parser) parseUniqueTableConstraint() (uniqueDef, error) {
	name := ""
	if p.peekKeyword() == "constraint" {
		p.advance()
		n, err := p.expectIdentifier()
		if err != nil {
			return uniqueDef{}, err
		}
		name = n
	}
	if err := p.expectKeyword("unique"); err != nil {
		return uniqueDef{}, err
	}
	columns, err := p.parsePKColumnList()
	if err != nil {
		return uniqueDef{}, err
	}
	return uniqueDef{Name: name, Columns: columns}, nil
}

// parsePKColumnList parses the parenthesized member list of a table-level PRIMARY KEY
// constraint: `( <col> [, <col>]* )`. Must be non-empty — `PRIMARY KEY ()` is 42601 (the
// first expectIdentifier rejects `)`).
func (p *parser) parsePKColumnList() ([]string, error) {
	if err := p.expect(tokLParen); err != nil {
		return nil, err
	}
	first, err := p.expectIdentifier()
	if err != nil {
		return nil, err
	}
	cols := []string{first}
	for {
		switch p.advance().Kind {
		case tokComma:
			col, err := p.expectIdentifier()
			if err != nil {
				return nil, err
			}
			cols = append(cols, col)
		case tokRParen:
			return cols, nil
		default:
			return nil, newError(SyntaxError, "expected ',' or ')'")
		}
	}
}

func (p *parser) parseColumnDef(tableName string, checks *[]checkDef, uniques *[]uniqueDef, foreignKeys *[]foreignKeyDef) (columnDef, error) {
	name, err := p.expectIdentifier()
	if err != nil {
		return columnDef{}, err
	}
	typeName, err := p.expectIdentifier()
	if err != nil {
		return columnDef{}, err
	}
	typeMod, err := p.parseTypeMod()
	if err != nil {
		return columnDef{}, err
	}
	isArray, err := p.consumeArrayBrackets()
	if err != nil {
		return columnDef{}, err
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
	var def *defaultDef
	var identity *identitySpec
	collation := ""
	for {
		if p.atCheckConstraint() {
			check, err := p.parseCheckConstraint()
			if err != nil {
				return columnDef{}, err
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
				return columnDef{}, err
			}
			if err := p.expectKeyword("unique"); err != nil {
				return columnDef{}, err
			}
			*uniques = append(*uniques, uniqueDef{Name: cname, Columns: []string{name}})
			continue
		}
		// CONSTRAINT <name> REFERENCES … in column position (the named one-member FK).
		if p.peekKeyword() == "constraint" && p.peekKeywordAt(2) == "references" {
			p.advance()
			cname, err := p.expectIdentifier()
			if err != nil {
				return columnDef{}, err
			}
			refTable, refColumns, onDelete, onUpdate, err := p.parseReferencesClause()
			if err != nil {
				return columnDef{}, err
			}
			*foreignKeys = append(*foreignKeys, foreignKeyDef{
				Name:       cname,
				Columns:    []string{name},
				RefTable:   refTable,
				RefColumns: refColumns,
				OnDelete:   onDelete,
				OnUpdate:   onUpdate,
			})
			continue
		}
		switch p.peekKeyword() {
		case "primary":
			p.advance()
			if err := p.expectKeyword("key"); err != nil {
				return columnDef{}, err
			}
			primaryKey = true
		case "not":
			p.advance()
			if err := p.expectKeyword("null"); err != nil {
				return columnDef{}, err
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
				return columnDef{}, err
			}
			text := renderTokens(p.tokens[start:p.pos])
			def = &defaultDef{Expr: expr, Text: text}
		case "generated":
			// `GENERATED { ALWAYS | BY DEFAULT } AS IDENTITY [( seq_options )]`
			// (spec/design/sequences.md §13). Two identity specs on one column is 42601
			// ("multiple identity specifications"). The desugaring (owned sequence + nextval default
			// + NOT NULL + the type gate) is at execution.
			p.advance()
			var always bool
			switch p.peekKeyword() {
			case "always":
				p.advance()
				always = true
			case "by":
				p.advance()
				if err := p.expectKeyword("default"); err != nil {
					return columnDef{}, err
				}
				always = false
			default:
				return columnDef{}, newError(SyntaxError,
					fmt.Sprintf("expected ALWAYS or BY DEFAULT after GENERATED, found %q", p.peekKeyword()))
			}
			if err := p.expectKeyword("as"); err != nil {
				return columnDef{}, err
			}
			if err := p.expectKeyword("identity"); err != nil {
				return columnDef{}, err
			}
			var options seqOptions
			if p.peek().Kind == tokLParen {
				options, err = p.parseSequenceOptions(true)
				if err != nil {
					return columnDef{}, err
				}
			}
			if identity != nil {
				return columnDef{}, newError(SyntaxError, fmt.Sprintf(
					"multiple identity specifications for column %s of table %s", name, tableName,
				))
			}
			identity = &identitySpec{Always: always, Options: options}
		case "collate":
			// COLLATE "name" in column position (spec/design/collation.md §1) — a quoted,
			// case-sensitive collation name. Validity (text-only 42804, loaded name 42704) is
			// checked at execution. A repeat keeps the last (like DEFAULT).
			p.advance()
			collation, err = p.expectCollationName()
			if err != nil {
				return columnDef{}, err
			}
		case "unique":
			p.advance()
			*uniques = append(*uniques, uniqueDef{Columns: []string{name}})
		case "references":
			// The column-level one-member FK: `REFERENCES parent [(col)] [actions]`.
			// parseReferencesClause consumes the REFERENCES keyword itself.
			refTable, refColumns, onDelete, onUpdate, err := p.parseReferencesClause()
			if err != nil {
				return columnDef{}, err
			}
			*foreignKeys = append(*foreignKeys, foreignKeyDef{
				Name:       "",
				Columns:    []string{name},
				RefTable:   refTable,
				RefColumns: refColumns,
				OnDelete:   onDelete,
				OnUpdate:   onUpdate,
			})
		default:
			return columnDef{Name: name, TypeName: typeName, TypeMod: typeMod, PrimaryKey: primaryKey, NotNull: notNull, Default: def, Identity: identity, Collation: collation}, nil
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
func (p *parser) consumeArrayBrackets() (bool, error) {
	isArray := false
	for p.peek().Kind == tokLBracket {
		p.advance() // '['
		if err := p.expect(tokRBracket); err != nil {
			return false, err
		}
		isArray = true
	}
	return isArray, nil
}

func (p *parser) parseTypeMod() (*typeMod, error) {
	if p.peek().Kind != tokLParen {
		return nil, nil
	}
	p.advance() // '('
	precision, err := p.expectTypmodInt()
	if err != nil {
		return nil, err
	}
	var scale *uint64
	if p.peek().Kind == tokComma {
		p.advance()
		s, err := p.expectTypmodInt()
		if err != nil {
			return nil, err
		}
		scale = &s
	}
	if err := p.expect(tokRParen); err != nil {
		return nil, err
	}
	return &typeMod{Precision: precision, Scale: scale}, nil
}

func (p *parser) expectTypmodInt() (uint64, error) {
	t := p.advance()
	if t.Kind != tokInt {
		return 0, newError(SyntaxError, "expected an integer type modifier")
	}
	return t.Int, nil
}

// parseDropTable parses `DROP TABLE [IF EXISTS] <name> [, …] [CASCADE | RESTRICT]`.
// Existence/dependency are resolved at execution time (42P01 — or a no-op when IF EXISTS is
// present — and 2BP01), not here. A comma list collects several names; the trailing
// CASCADE/RESTRICT keyword sets the FK-dependency mode (RESTRICT is the default)
// (spec/design/grammar.md §13). IF EXISTS is recognized only when the next two keywords are
// exactly IF EXISTS (the two-token lookahead the statement dispatch uses) — a lone `if` is an
// ordinary non-reserved identifier, so `DROP TABLE if` drops a table named `if` (PG-faithful, §1).
func (p *parser) parseDropTable() (*dropTable, error) {
	if err := p.expectKeyword("drop"); err != nil {
		return nil, err
	}
	if err := p.expectKeyword("table"); err != nil {
		return nil, err
	}
	ifExists := p.peekKeyword() == "if" && p.peekKeywordAt(1) == "exists"
	if ifExists {
		p.advance() // IF
		p.advance() // EXISTS
	}
	name, err := p.expectIdentifier()
	if err != nil {
		return nil, err
	}
	names := []string{name}
	for p.peek().Kind == tokComma {
		p.advance()
		n, err := p.expectIdentifier()
		if err != nil {
			return nil, err
		}
		names = append(names, n)
	}
	// The trailing dependency mode is optional; RESTRICT is the default (and the only mode the
	// bare form ever had). Anything else after the name list is trailing input (the dispatch's
	// end-of-statement check raises 42601).
	cascade := false
	switch p.peekKeyword() {
	case "cascade":
		p.advance()
		cascade = true
	case "restrict":
		p.advance()
	}
	return &dropTable{Names: names, IfExists: ifExists, Cascade: cascade}, nil
}

// parseCreateIndex parses `CREATE INDEX [name] ON <table> ( col [, col]* )`
// (spec/design/grammar.md §30). The optional name needs one disambiguation because no
// word is reserved: the word after INDEX is the index name UNLESS it is `ON` followed by
// a word and then `(` — that exact three-token shape can only be the unnamed form's
// `ON table (`. Key columns are bare identifiers (no expression/ordered/partial keys this
// slice — a `(`/`ASC`/`DESC` after a key is the natural 42601).
func (p *parser) parseCreateIndex() (*createIndex, error) {
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
	// The unnamed form is `INDEX ON <table> [USING <method>] (` — the word after INDEX is the
	// index name unless it is `ON` followed by a word and then `(` OR `USING` (the three-token
	// lookahead, extended for the optional USING clause — grammar.md §30, gin.md §3).
	unnamed := p.peekKeyword() == "on" &&
		p.peekKindAt(1) == tokWord &&
		(p.peekKindAt(2) == tokLParen || p.peekKeywordAt(2) == "using")
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
	// An optional database qualifier `db.table` on the target table (attached-databases.md §3, Slice
	// 1b): build the index ON a table in the named database (`main` / `temp` / a host attachment).
	dbQualifier, table, err := p.parseQualifiedTableName()
	if err != nil {
		return nil, err
	}
	// Optional `USING <method>` between the table name and the column list (PG order — gin.md §3,
	// grammar.md §30). Not reserved (positional); the method is resolved at execution (42704 if
	// unknown), not here.
	using := ""
	if p.peekKeyword() == "using" {
		p.advance()
		m, err := p.expectIdentifier()
		if err != nil {
			return nil, err
		}
		using = m
	}
	if err := p.expect(tokLParen); err != nil {
		return nil, err
	}
	var keys []indexKeyElem
	for {
		key, err := p.parseIndexElement()
		if err != nil {
			return nil, err
		}
		keys = append(keys, key)
		tok := p.advance()
		if tok.Kind == tokComma {
			continue
		}
		if tok.Kind == tokRParen {
			break
		}
		return nil, newError(SyntaxError, fmt.Sprintf("expected ',' or ')', found %v", tok))
	}
	// An optional trailing `WHERE predicate` makes the index PARTIAL (indexes.md §9). `where` is
	// recognized positionally after the closing `)` (non-reserved); its text is captured for the
	// canonical persisted form (like CHECK/DEFAULT).
	var predicate *indexPredicate
	if p.peekKeyword() == "where" {
		p.advance()
		start := p.pos
		expr, err := p.parseExpr()
		if err != nil {
			return nil, err
		}
		predicate = &indexPredicate{Text: renderTokens(p.tokens[start:p.pos]), Expr: expr}
	}
	return &createIndex{Name: name, Table: table, DB: dbQualifier, Keys: keys, Unique: unique, Using: using, Predicate: predicate}, nil
}

// parseIndexElement parses one index_element (grammar.md §30, indexes.md §1): a bare column, a
// bare function call (`lower(email)`), or a parenthesized expression (`(a + b)`). PostgreSQL's
// index_elem: a general operator expression must be parenthesized (a bare `a + b` errors —
// parsePrimary stops before the operator, so the element loop then sees an unexpected token); a
// parenthesized bare column `(a)` normalizes to a column key.
func (p *parser) parseIndexElement() (indexKeyElem, error) {
	switch {
	case p.peek().Kind == tokLParen:
		// `( expr )` — any parenthesized expression.
		p.advance()
		start := p.pos
		expr, err := p.parseExpr()
		if err != nil {
			return indexKeyElem{}, err
		}
		end := p.pos
		if err := p.expect(tokRParen); err != nil {
			return indexKeyElem{}, err
		}
		return p.indexKeyFromExpr(expr, start, end), nil
	case p.peek().Kind == tokWord && p.peekKindAt(1) == tokLParen:
		// A bare function call `f(args)` — parse ONLY the primary, so a trailing operator
		// (`lower(x) + 1`) leaves `+` for the element loop to reject (PG requires parens).
		start := p.pos
		expr, err := p.parsePrimary()
		if err != nil {
			return indexKeyElem{}, err
		}
		end := p.pos
		return p.indexKeyFromExpr(expr, start, end), nil
	default:
		// A bare column name.
		col, err := p.expectIdentifier()
		if err != nil {
			return indexKeyElem{}, err
		}
		return indexKeyElem{Column: col}, nil
	}
}

// indexKeyFromExpr classifies a parsed index-element expression: a bare column reference (`a`,
// `(a)`, `((a))`) becomes a column key (PG-matched), anything else an expression key carrying its
// canonical text (rendered from the captured token span, like CHECK/DEFAULT).
func (p *parser) indexKeyFromExpr(expr exprNode, start, end int) indexKeyElem {
	if expr.Kind == exprColumn {
		return indexKeyElem{Column: expr.Column}
	}
	e := expr
	return indexKeyElem{Expr: &e, Text: renderTokens(p.tokens[start:end])}
}

// parseDropIndex parses `DROP INDEX <name>` (spec/design/grammar.md §30). A missing index
// (42704) or a table's name (42809) is rejected at execution time, not here.
func (p *parser) parseDropIndex() (*dropIndex, error) {
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
	return &dropIndex{Name: name}, nil
}

// parseCreateType parses `CREATE TYPE <name> AS ( <field> <type> [NOT NULL] [, …] )` — a
// composite (row) type (spec/design/composite.md, grammar.md). At least one field (an empty list
// is a syntax error); each field's type is a bare type name (built-in or a composite), resolved at
// execution (42704 if unknown).
func (p *parser) parseCreateType() (*createType, error) {
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
	if err := p.expect(tokLParen); err != nil {
		return nil, err
	}
	fields, err := p.parseFieldDefList()
	if err != nil {
		return nil, err
	}
	return &createType{Name: name, Fields: fields}, nil
}

// skipFormatJSON skips an optional `FORMAT JSON [ENCODING …]` clause after a SQL/JSON context item.
func (p *parser) skipFormatJSON() {
	if p.peekKeyword() == "format" && p.peekKeywordAt(1) == "json" {
		p.advance() // FORMAT
		p.advance() // JSON
	}
}

// parseJSONReturning parses an optional `RETURNING <type> [FORMAT JSON]` clause → the type name
// (resolved later). nil when absent.
func (p *parser) parseJSONReturning() (*string, error) {
	if p.peekKeyword() != "returning" {
		return nil, nil
	}
	p.advance() // RETURNING
	ty, err := p.expectIdentifier()
	if err != nil {
		return nil, err
	}
	p.skipFormatJSON()
	return &ty, nil
}

// parseJSONBehavior parses one constant SQL/JSON behavior word (`ERROR` / `NULL` / `TRUE` / `FALSE` /
// `UNKNOWN` / `EMPTY [ARRAY|OBJECT]`). `DEFAULT expr` is the deferred S3 follow-on (0A000).
func (p *parser) parseJSONBehavior() (jsonOnBehavior, error) {
	switch p.peekKeyword() {
	case "error":
		p.advance()
		return jOBError, nil
	case "null":
		p.advance()
		return jOBNull, nil
	case "true":
		p.advance()
		return jOBTrue, nil
	case "false":
		p.advance()
		return jOBFalse, nil
	case "unknown":
		p.advance()
		return jOBUnknown, nil
	case "empty":
		p.advance()
		switch p.peekKeyword() {
		case "object":
			p.advance()
			return jOBEmptyObject, nil
		case "array":
			p.advance()
			return jOBEmptyArray, nil
		default:
			// bare `EMPTY` defaults to `EMPTY ARRAY` (PostgreSQL).
			return jOBEmptyArray, nil
		}
	case "default":
		return 0, newError(FeatureNotSupported, "ON ERROR / ON EMPTY DEFAULT expr is not supported yet")
	default:
		return 0, newError(SyntaxError, "expected a SQL/JSON ON ERROR/EMPTY behavior")
	}
}

// parseJSONOnErrorOnly parses JSON_EXISTS's single optional `<behavior> ON ERROR` clause.
func (p *parser) parseJSONOnErrorOnly() (*jsonOnBehavior, error) {
	if p.isJSONBehaviorStart() && p.peekOnClauseIs("error") {
		b, err := p.parseJSONBehavior()
		if err != nil {
			return nil, err
		}
		p.advance() // ON
		p.advance() // ERROR
		return &b, nil
	}
	return nil, nil
}

// parseJSONOnClauses parses the optional `<behavior> ON EMPTY` then `<behavior> ON ERROR` clauses (in
// that order).
func (p *parser) parseJSONOnClauses() (onEmpty, onError *jsonOnBehavior, err error) {
	if p.isJSONBehaviorStart() && p.peekOnClauseIs("empty") {
		b, e := p.parseJSONBehavior()
		if e != nil {
			return nil, nil, e
		}
		p.advance() // ON
		p.advance() // EMPTY
		onEmpty = &b
	}
	if p.isJSONBehaviorStart() && p.peekOnClauseIs("error") {
		b, e := p.parseJSONBehavior()
		if e != nil {
			return nil, nil, e
		}
		p.advance() // ON
		p.advance() // ERROR
		onError = &b
	}
	return onEmpty, onError, nil
}

// parseJSONWrapperQuotes parses JSON_QUERY's optional `[WITH [COND|UNCOND] [ARRAY] WRAPPER | WITHOUT
// [ARRAY] WRAPPER]` and `[KEEP|OMIT QUOTES [ON SCALAR STRING]]` clauses. Returns the wrapper mode and
// the keep-quotes flag (true = KEEP, the default).
func (p *parser) parseJSONWrapperQuotes() (jsonWrapper, bool, error) {
	wrapper := jWWithout
	switch p.peekKeyword() {
	case "with":
		p.advance() // WITH
		switch p.peekKeyword() {
		case "conditional":
			p.advance()
			wrapper = jWConditional
		case "unconditional":
			p.advance()
			wrapper = jWUnconditional
		default:
			wrapper = jWUnconditional
		}
		if p.peekKeyword() == "array" {
			p.advance()
		}
		if err := p.expectKeyword("wrapper"); err != nil {
			return 0, false, err
		}
	case "without":
		p.advance() // WITHOUT
		if p.peekKeyword() == "array" {
			p.advance()
		}
		if err := p.expectKeyword("wrapper"); err != nil {
			return 0, false, err
		}
	}
	keepQuotes := true
	switch p.peekKeyword() {
	case "keep":
		p.advance()
		if err := p.expectKeyword("quotes"); err != nil {
			return 0, false, err
		}
		p.skipOnScalarString()
	case "omit":
		p.advance()
		if err := p.expectKeyword("quotes"); err != nil {
			return 0, false, err
		}
		p.skipOnScalarString()
		keepQuotes = false
	}
	return wrapper, keepQuotes, nil
}

// skipOnScalarString skips an optional `ON SCALAR STRING` after a QUOTES clause.
func (p *parser) skipOnScalarString() {
	if p.peekKeyword() == "on" && p.peekKeywordAt(1) == "scalar" {
		p.advance() // ON
		p.advance() // SCALAR
		if p.peekKeyword() == "string" {
			p.advance()
		}
	}
}

// isJSONBehaviorStart reports whether the cursor is at a SQL/JSON behavior word
// (ERROR/NULL/TRUE/FALSE/UNKNOWN/EMPTY/DEFAULT).
func (p *parser) isJSONBehaviorStart() bool {
	switch p.peekKeyword() {
	case "error", "null", "true", "false", "unknown", "empty", "default":
		return true
	}
	return false
}

// peekOnClauseIs reports whether the upcoming clause is `… ON <which>` (a one-or-two-token lookahead
// past the behavior — EMPTY may be `EMPTY ARRAY`/`EMPTY OBJECT`, so scan to the `ON`).
func (p *parser) peekOnClauseIs(which string) bool {
	for _, skip := range []int{1, 2} {
		if p.peekKeywordAt(skip) == "on" && p.peekKeywordAt(skip+1) == which {
			return true
		}
	}
	return false
}

// parseFieldDefList parses a `( field type [numeric(p,s)] [[]] [NOT NULL] [, …] )` field-definition
// list — the body shared by `CREATE TYPE … AS (…)` (composite.md) and a FROM-clause **column-
// definition list** `AS t(col type, …)` (C0, json-table.md §1). The caller has consumed the opening
// `(`; this consumes through the matching `)`.
func (p *parser) parseFieldDefList() ([]typeFieldDef, error) {
	var fields []typeFieldDef
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
		// An array-typed field (`xs i32[]`) — the same `[]` suffix a column type takes
		// (spec/design/array.md §12); the canonical spelling carries the brackets.
		isArray, err := p.consumeArrayBrackets()
		if err != nil {
			return nil, err
		}
		if isArray {
			typeName += "[]"
		}
		notNull := false
		if p.peekKeyword() == "not" {
			p.advance()
			if err := p.expectKeyword("null"); err != nil {
				return nil, err
			}
			notNull = true
		}
		fields = append(fields, typeFieldDef{Name: fname, TypeName: typeName, TypeMod: typeMod, NotNull: notNull})
		tok := p.advance()
		if tok.Kind == tokComma {
			continue
		}
		if tok.Kind == tokRParen {
			break
		}
		return nil, newError(SyntaxError, fmt.Sprintf("expected ',' or ')', found %v", tok))
	}
	return fields, nil
}

// parseDropType parses `DROP TYPE [IF EXISTS] <name> [RESTRICT | CASCADE]`
// (spec/design/composite.md §7). RESTRICT is the default and the only behavior this slice;
// CASCADE is rejected (0A000) at execution. A missing type (42704) and dependents (2BP01) are
// execution-time.
func (p *parser) parseDropType() (*dropType, error) {
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
		return nil, newError(FeatureNotSupported, "DROP TYPE ... CASCADE is not supported")
	}
	return &dropType{Name: name, IfExists: ifExists}, nil
}

// parseCreateSequence parses `CREATE SEQUENCE [IF NOT EXISTS] <name> [options]`
// (spec/design/sequences.md). The options are order-free and each at most once (a repeat is
// 42601); option values are signed integer literals. Validation of the resolved option set
// (22023) and the namespace collision (42P07) are execution-time.
func (p *parser) parseCreateSequence() (*createSequence, error) {
	if err := p.expectKeyword("create"); err != nil {
		return nil, err
	}
	if err := p.expectKeyword("sequence"); err != nil {
		return nil, err
	}
	ifNotExists, err := p.parseIfNotExists()
	if err != nil {
		return nil, err
	}
	name, err := p.expectIdentifier()
	if err != nil {
		return nil, err
	}
	options, err := p.parseSequenceOptions(false)
	if err != nil {
		return nil, err
	}
	return &createSequence{Name: name, IfNotExists: ifNotExists, Options: options}, nil
}

// parseSequenceOptions parses the order-free sequence-option set (INCREMENT [BY] n,
// MINVALUE/MAXVALUE and their NO forms, START [WITH] n, CACHE c, [NO] CYCLE) shared by CREATE
// SEQUENCE and an IDENTITY column's `( seq_options )` (spec/design/sequences.md §13). When
// parenthesized, the options are wrapped in `( … )` and the loop stops at `)`; each option appears
// at most once (a repeat is 42601 via dupCheck). Validation of the resolved set (22023) is
// execution-time.
func (p *parser) parseSequenceOptions(parenthesized bool) (seqOptions, error) {
	seq, _, err := p.parseSeqOptionsInner(parenthesized, false)
	return seq, err
}

// parseSeqOptionsInner is the shared option loop. When allowRestart (only on ALTER SEQUENCE, never
// parenthesized), `RESTART [[WITH] n]` is also accepted as an interleavable pseudo-option and
// returned separately (nil = absent; &{ToStart:true} = bare RESTART; &{Value:n} = RESTART WITH n);
// RESTART is invalid in CREATE/identity, where it ends the loop like any unrecognized keyword.
func (p *parser) parseSeqOptionsInner(parenthesized, allowRestart bool) (seqOptions, *seqRestart, error) {
	if parenthesized {
		if err := p.expect(tokLParen); err != nil {
			return seqOptions{}, nil, err
		}
	}
	var seq seqOptions
	var restart *seqRestart
	// Order-free option loop: dispatch on the leading keyword, each option at most once.
loop:
	for {
		switch p.peekKeyword() {
		case "restart":
			// Only on ALTER; resets the counter (sequences.md §15). Elsewhere end the loop.
			if !allowRestart {
				break loop
			}
			if err := p.dupCheck(restart != nil, "RESTART"); err != nil {
				return seqOptions{}, nil, err
			}
			p.advance()
			r := &seqRestart{ToStart: true}
			if p.peek().Kind == tokInt || p.peek().Kind == tokMinus || p.peekKeyword() == "with" {
				p.consumeKeyword("with")
				v, err := p.parseSignedIntLiteral()
				if err != nil {
					return seqOptions{}, nil, err
				}
				r = &seqRestart{Value: v}
			}
			restart = r
		case "as":
			// `AS <type>` — the sequence value type (order-free, S5 — sequences.md §14). The raw
			// type name is stored; it is resolved (and a non-integer type rejected 22023) at
			// execution. Inside an IDENTITY column's `( … )` a set DataType is 42601.
			if err := p.dupCheck(seq.DataType != "", "AS"); err != nil {
				return seqOptions{}, nil, err
			}
			p.advance()
			name, err := p.expectIdentifier()
			if err != nil {
				return seqOptions{}, nil, err
			}
			seq.DataType = name
		case "increment":
			if err := p.dupCheck(seq.Increment != nil, "INCREMENT"); err != nil {
				return seqOptions{}, nil, err
			}
			p.advance()
			p.consumeKeyword("by")
			v, err := p.parseSignedIntLiteral()
			if err != nil {
				return seqOptions{}, nil, err
			}
			seq.Increment = &v
		case "minvalue":
			if err := p.dupCheck(seq.MinValue != nil, "MINVALUE"); err != nil {
				return seqOptions{}, nil, err
			}
			p.advance()
			v, err := p.parseSignedIntLiteral()
			if err != nil {
				return seqOptions{}, nil, err
			}
			seq.MinValue = &seqBound{Value: v}
		case "maxvalue":
			if err := p.dupCheck(seq.MaxValue != nil, "MAXVALUE"); err != nil {
				return seqOptions{}, nil, err
			}
			p.advance()
			v, err := p.parseSignedIntLiteral()
			if err != nil {
				return seqOptions{}, nil, err
			}
			seq.MaxValue = &seqBound{Value: v}
		case "start":
			if err := p.dupCheck(seq.Start != nil, "START"); err != nil {
				return seqOptions{}, nil, err
			}
			p.advance()
			p.consumeKeyword("with")
			v, err := p.parseSignedIntLiteral()
			if err != nil {
				return seqOptions{}, nil, err
			}
			seq.Start = &v
		case "cache":
			if err := p.dupCheck(seq.Cache != nil, "CACHE"); err != nil {
				return seqOptions{}, nil, err
			}
			p.advance()
			v, err := p.parseSignedIntLiteral()
			if err != nil {
				return seqOptions{}, nil, err
			}
			seq.Cache = &v
		case "cycle":
			if err := p.dupCheck(seq.Cycle != nil, "CYCLE"); err != nil {
				return seqOptions{}, nil, err
			}
			p.advance()
			t := true
			seq.Cycle = &t
		case "no":
			// `NO MINVALUE` / `NO MAXVALUE` / `NO CYCLE`.
			p.advance()
			switch p.peekKeyword() {
			case "minvalue":
				if err := p.dupCheck(seq.MinValue != nil, "MINVALUE"); err != nil {
					return seqOptions{}, nil, err
				}
				p.advance()
				seq.MinValue = &seqBound{NoValue: true}
			case "maxvalue":
				if err := p.dupCheck(seq.MaxValue != nil, "MAXVALUE"); err != nil {
					return seqOptions{}, nil, err
				}
				p.advance()
				seq.MaxValue = &seqBound{NoValue: true}
			case "cycle":
				if err := p.dupCheck(seq.Cycle != nil, "CYCLE"); err != nil {
					return seqOptions{}, nil, err
				}
				p.advance()
				f := false
				seq.Cycle = &f
			default:
				return seqOptions{}, nil, newError(SyntaxError,
					fmt.Sprintf("expected MINVALUE, MAXVALUE, or CYCLE after NO, found %q", p.peekKeyword()))
			}
		default:
			break loop
		}
	}
	if parenthesized {
		if err := p.expect(tokRParen); err != nil {
			return seqOptions{}, nil, err
		}
	}
	return seq, restart, nil
}

// parseDropSequence parses `DROP SEQUENCE [IF EXISTS] <name> [, …] [RESTRICT | CASCADE]`
// (sequences.md §1). CASCADE is 0A000 at execution; a missing sequence (42P01) is
// execution-time.
func (p *parser) parseDropSequence() (*dropSequence, error) {
	if err := p.expectKeyword("drop"); err != nil {
		return nil, err
	}
	if err := p.expectKeyword("sequence"); err != nil {
		return nil, err
	}
	ifExists := p.peekKeyword() == "if"
	if ifExists {
		p.advance()
		if err := p.expectKeyword("exists"); err != nil {
			return nil, err
		}
	}
	first, err := p.expectIdentifier()
	if err != nil {
		return nil, err
	}
	names := []string{first}
	for p.peek().Kind == tokComma {
		p.advance()
		n, err := p.expectIdentifier()
		if err != nil {
			return nil, err
		}
		names = append(names, n)
	}
	cascade := false
	switch p.peekKeyword() {
	case "restrict":
		p.advance()
	case "cascade":
		p.advance()
		cascade = true
	}
	if cascade {
		return nil, newError(FeatureNotSupported, "DROP SEQUENCE ... CASCADE is not supported")
	}
	return &dropSequence{Names: names, IfExists: ifExists}, nil
}

// parseAlterTable parses ALTER TABLE's authoritative grammar frame (alter.md §1). Slices 1-5
// execute the complete planned surface other than identity management.
func (p *parser) parseAlterTable() (*alterTable, error) {
	if err := p.expectKeyword("alter"); err != nil {
		return nil, err
	}
	if err := p.expectKeyword("table"); err != nil {
		return nil, err
	}
	ifExists := false
	if p.peekKeyword() == "if" {
		p.advance()
		if err := p.expectKeyword("exists"); err != nil {
			return nil, err
		}
		ifExists = true
	}
	db, name, err := p.parseQualifiedTableName()
	if err != nil {
		return nil, err
	}
	at := &alterTable{Name: name, DB: db, IfExists: ifExists}
	if p.peekKeyword() == "rename" {
		p.advance()
		switch p.peekKeyword() {
		case "to":
			p.advance()
			at.RenameTable, err = p.expectIdentifier()
		case "constraint":
			p.advance()
			old, e := p.expectIdentifier()
			if e != nil {
				return nil, e
			}
			if e = p.expectKeyword("to"); e != nil {
				return nil, e
			}
			next, e := p.expectIdentifier()
			if e != nil {
				return nil, e
			}
			at.RenameConstraint = &renamePair{Old: old, New: next}
		default:
			if p.peekKeyword() == "column" {
				p.advance()
			}
			old, e := p.expectIdentifier()
			if e != nil {
				return nil, e
			}
			if e = p.expectKeyword("to"); e != nil {
				return nil, e
			}
			next, e := p.expectIdentifier()
			if e != nil {
				return nil, e
			}
			at.RenameColumn = &renamePair{Old: old, New: next}
		}
		if err != nil {
			return nil, err
		}
		return at, nil
	}
	for {
		switch p.peekKeyword() {
		case "add":
			p.advance()
			columnNoise := p.peekKeyword() == "column"
			if columnNoise {
				p.advance()
			}
			ifNotExists := false
			if p.peekKeyword() == "if" {
				p.advance()
				if err := p.expectKeyword("not"); err != nil {
					return nil, err
				}
				if err := p.expectKeyword("exists"); err != nil {
					return nil, err
				}
				ifNotExists = true
			}
			if columnNoise || ifNotExists || !(p.atCheckConstraint() || p.atUniqueTableConstraint() || p.atForeignKeyTableConstraint() || p.atExclusionTableConstraint() || p.atPrimaryKeyTableConstraint()) {
				var checks []checkDef
				var uniques []uniqueDef
				var fks []foreignKeyDef
				col, e := p.parseColumnDef(name, &checks, &uniques, &fks)
				if e != nil {
					return nil, e
				}
				at.Actions = append(at.Actions, alterTableEdit{AddColumn: &alterAddColumn{Column: col, Checks: checks, Uniques: uniques, ForeignKeys: fks, IfNotExists: ifNotExists}})
				if p.peek().Kind != tokComma {
					return at, nil
				}
				p.advance()
				continue
			}
			if p.atPrimaryKeyTableConstraint() {
				if p.peekKeyword() == "constraint" {
					p.advance()
					if _, e := p.expectIdentifier(); e != nil {
						return nil, e
					}
				}
				if e := p.expectKeyword("primary"); e != nil {
					return nil, e
				}
				if e := p.expectKeyword("key"); e != nil {
					return nil, e
				}
				cols, e := p.parsePKColumnList()
				if e != nil {
					return nil, e
				}
				at.Actions = append(at.Actions, alterTableEdit{AddPrimaryKey: cols})
				break
			}
			var add alterConstraintDef
			switch {
			case p.atCheckConstraint():
				v, e := p.parseCheckConstraint()
				if e != nil {
					return nil, e
				}
				add.Check = &v
			case p.atUniqueTableConstraint():
				v, e := p.parseUniqueTableConstraint()
				if e != nil {
					return nil, e
				}
				add.Unique = &v
			case p.atForeignKeyTableConstraint():
				v, e := p.parseForeignKeyTableConstraint()
				if e != nil {
					return nil, e
				}
				add.Foreign = &v
			case p.atExclusionTableConstraint():
				v, e := p.parseExclusionTableConstraint()
				if e != nil {
					return nil, e
				}
				add.Exclude = &v
			default:
				panic("constraint lookahead")
			}
			at.Actions = append(at.Actions, alterTableEdit{Add: &add})
		case "drop":
			p.advance()
			if p.peekKeyword() == "primary" {
				p.advance()
				if err := p.expectKeyword("key"); err != nil {
					return nil, err
				}
				cascade := false
				if p.peekKeyword() == "cascade" {
					p.advance()
					cascade = true
				} else if p.peekKeyword() == "restrict" {
					p.advance()
				}
				at.Actions = append(at.Actions, alterTableEdit{DropPrimaryKey: &alterDropPrimaryKey{Cascade: cascade}})
				break
			}
			constraint := p.peekKeyword() == "constraint"
			if constraint || p.peekKeyword() == "column" {
				p.advance()
			}
			ifExists := false
			if p.peekKeyword() == "if" {
				p.advance()
				if err := p.expectKeyword("exists"); err != nil {
					return nil, err
				}
				ifExists = true
			}
			name, err := p.expectIdentifier()
			if err != nil {
				return nil, err
			}
			cascade := false
			if p.peekKeyword() == "cascade" {
				p.advance()
				cascade = true
			} else if p.peekKeyword() == "restrict" {
				p.advance()
			}
			if constraint {
				at.Actions = append(at.Actions, alterTableEdit{Drop: &dropConstraintDef{Name: name, IfExists: ifExists, Cascade: cascade}})
			} else {
				at.Actions = append(at.Actions, alterTableEdit{DropColumn: &alterDropColumn{Name: name, IfExists: ifExists, Cascade: cascade}})
			}
		default:
			if err := p.expectKeyword("alter"); err != nil {
				return nil, err
			}
			if p.peekKeyword() == "column" {
				p.advance()
			}
			column, err := p.expectIdentifier()
			if err != nil {
				return nil, err
			}
			a := alterColumnAction{Column: column}
			switch p.peekKeyword() {
			case "set":
				p.advance()
				if p.peekKeyword() == "default" {
					p.advance()
					start := p.pos
					e, er := p.parseExpr()
					if er != nil {
						return nil, er
					}
					a.Kind = alterSetDefault
					a.Default = &defaultDef{Expr: e, Text: renderTokens(p.tokens[start:p.pos])}
				} else if p.peekKeyword() == "data" {
					p.advance()
					if err := p.expectKeyword("type"); err != nil {
						return nil, err
					}
					if err := p.parseAlterColumnType(&a); err != nil {
						return nil, err
					}
				} else {
					if err := p.expectKeyword("not"); err != nil {
						return nil, err
					}
					if err := p.expectKeyword("null"); err != nil {
						return nil, err
					}
					a.Kind = alterSetNotNull
				}
			case "drop":
				p.advance()
				if p.peekKeyword() == "default" {
					p.advance()
					a.Kind = alterDropDefault
				} else {
					if err := p.expectKeyword("not"); err != nil {
						return nil, err
					}
					if err := p.expectKeyword("null"); err != nil {
						return nil, err
					}
					a.Kind = alterDropNotNull
				}
			case "type":
				p.advance()
				if err := p.parseAlterColumnType(&a); err != nil {
					return nil, err
				}
			default:
				return nil, newError(SyntaxError, "ALTER COLUMN requires SET or DROP")
			}
			at.Actions = append(at.Actions, alterTableEdit{Column: &a})
		}
		if p.peek().Kind != tokComma {
			break
		}
		p.advance()
	}
	return at, nil
}

func (p *parser) parseAlterColumnType(a *alterColumnAction) error {
	base, err := p.expectIdentifier()
	if err != nil {
		return err
	}
	tm, err := p.parseTypeMod()
	if err != nil {
		return err
	}
	isArray, err := p.consumeArrayBrackets()
	if err != nil {
		return err
	}
	if isArray {
		base += "[]"
	}
	a.Kind, a.TypeName, a.TypeMod = alterSetType, base, tm
	if p.peekKeyword() == "using" {
		p.advance()
		e, err := p.parseExpr()
		if err != nil {
			return err
		}
		a.Using = &e
	}
	return nil
}

// parseAlterSequence parses `ALTER SEQUENCE [IF EXISTS] <name> <action>` (spec/design/sequences.md
// §15). After the name the next keyword dispatches: RENAME → the rename form; OWNED/OWNER/SET →
// 0A000; otherwise the order-free option loop (the CREATE options plus an interleavable RESTART),
// requiring ≥ 1 option (a bare ALTER SEQUENCE s is 42601). AS is parsed into the option set and
// rejected as 0A000 at execution.
func (p *parser) parseAlterSequence() (*alterSequence, error) {
	if err := p.expectKeyword("alter"); err != nil {
		return nil, err
	}
	if err := p.expectKeyword("sequence"); err != nil {
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
	switch p.peekKeyword() {
	case "rename":
		p.advance()
		if err := p.expectKeyword("to"); err != nil {
			return nil, err
		}
		newName, err := p.expectIdentifier()
		if err != nil {
			return nil, err
		}
		return &alterSequence{Name: name, IfExists: ifExists, RenameTo: newName}, nil
	case "owned", "owner", "set":
		// The remaining unsupported ALTER actions are 0A000 (not syntax errors).
		return nil, newError(FeatureNotSupported, "this ALTER SEQUENCE action is not supported")
	default:
		options, restart, err := p.parseSeqOptionsInner(false, true)
		if err != nil {
			return nil, err
		}
		// ≥ 1 action required: a bare ALTER SEQUENCE s (no option, no RESTART) is 42601.
		if (options == seqOptions{}) && restart == nil {
			return nil, newError(SyntaxError, "ALTER SEQUENCE requires at least one action")
		}
		return &alterSequence{Name: name, IfExists: ifExists, Options: options, Restart: restart}, nil
	}
}

// parseIfNotExists consumes an optional `IF NOT EXISTS` prefix, reporting whether it was present.
func (p *parser) parseIfNotExists() (bool, error) {
	if p.peekKeyword() == "if" {
		p.advance()
		if err := p.expectKeyword("not"); err != nil {
			return false, err
		}
		if err := p.expectKeyword("exists"); err != nil {
			return false, err
		}
		return true, nil
	}
	return false, nil
}

// consumeKeyword consumes an optional noise keyword (e.g. the BY in INCREMENT BY, the WITH in
// START WITH) when present.
func (p *parser) consumeKeyword(kw string) {
	if p.peekKeyword() == kw {
		p.advance()
	}
}

// dupCheck reports 42601 when an option appeared twice.
func (p *parser) dupCheck(already bool, opt string) error {
	if already {
		return newError(SyntaxError, fmt.Sprintf("%s specified more than once", opt))
	}
	return nil
}

// parseSignedIntLiteral parses a signed integer literal (`-? INT`) as an i64 — the
// sequence-option value form. The lexer caps an Int magnitude at 2^63, so the only out-of-range
// case is a bare positive 2^63 (22003 — numeric_value_out_of_range); a negated 2^63 is the
// i64 minimum (valid).
func (p *parser) parseSignedIntLiteral() (int64, error) {
	negate := false
	if p.peek().Kind == tokMinus {
		p.advance()
		negate = true
	}
	t := p.advance()
	if t.Kind != tokInt {
		return 0, newError(SyntaxError, fmt.Sprintf("expected an integer, found %v", t))
	}
	v, ok := foldInt(t.Int, negate)
	if !ok {
		return 0, newError(NumericValueOutOfRange, "sequence parameter out of i64 range")
	}
	return v, nil
}

// parseInsert parses `INSERT INTO <table> [( <col> [, <col>]* )] VALUES <row> [, <row>]*`,
// where each <row> is `( <value> [, <value>]* )` and each <value> is a literal or the DEFAULT
// keyword. The optional column list names the target columns; unlisted columns take their
// default. The executor resolves names + type-checks each row and inserts all-or-nothing
// (spec/design/grammar.md §12, constraints.md §2).
func (p *parser) parseInsert() (*insert, error) {
	if err := p.expectKeyword("insert"); err != nil {
		return nil, err
	}
	if err := p.expectKeyword("into"); err != nil {
		return nil, err
	}
	dbQualifier, table, err := p.parseQualifiedTableName()
	if err != nil {
		return nil, err
	}

	// Optional column list `( col [, col]* )` before VALUES. An empty `()` is rejected (the
	// first expectIdentifier errors 42601 on `)`).
	var columns []string
	if p.peek().Kind == tokLParen {
		p.advance() // '('
		for {
			name, err := p.expectIdentifier()
			if err != nil {
				return nil, err
			}
			columns = append(columns, name)
			switch p.advance().Kind {
			case tokComma:
				continue
			case tokRParen:
			default:
				return nil, newError(SyntaxError, "expected ',' or ')'")
			}
			break
		}
	}

	// Optional `OVERRIDING { SYSTEM | USER } VALUE` clause (spec/design/sequences.md §13), after
	// the column list and before the source. OVERRIDING / SYSTEM / USER / VALUE are non-reserved;
	// the clause is unambiguous against a VALUES/SELECT source.
	var overriding *overridingKind
	if p.peekKeyword() == "overriding" {
		p.advance()
		var mode overridingKind
		switch p.peekKeyword() {
		case "system":
			mode = overridingSystem
		case "user":
			mode = overridingUser
		default:
			return nil, newError(SyntaxError,
				fmt.Sprintf("expected SYSTEM or USER after OVERRIDING, found %q", p.peekKeyword()))
		}
		p.advance()
		if err := p.expectKeyword("value"); err != nil {
			return nil, err
		}
		overriding = &mode
	}

	// The source is DEFAULT VALUES, a SELECT (§24), or a VALUES list. DEFAULT VALUES is the
	// all-columns-omitted form, represented through the existing column-list machinery as one empty
	// row mapped by an empty synthetic list. PostgreSQL does not combine it with a column list or
	// OVERRIDING.
	if p.peekKeyword() == "default" {
		if columns != nil || overriding != nil {
			return nil, newError(SyntaxError, "DEFAULT VALUES cannot be combined with a column list or OVERRIDING")
		}
		p.advance()
		if err := p.expectKeyword("values"); err != nil {
			return nil, err
		}
		onConflict, err := p.parseOnConflict()
		if err != nil {
			return nil, err
		}
		returning, err := p.parseReturning()
		if err != nil {
			return nil, err
		}
		return &insert{Table: table, DB: dbQualifier, Columns: []string{}, Rows: [][]insertValue{{}}, OnConflict: onConflict, Returning: returning}, nil
	}
	if p.peekKeyword() == "select" {
		sel, err := p.parseSelect()
		if err != nil {
			return nil, err
		}
		onConflict, err := p.parseOnConflict()
		if err != nil {
			return nil, err
		}
		returning, err := p.parseReturning()
		if err != nil {
			return nil, err
		}
		return &insert{Table: table, DB: dbQualifier, Columns: columns, Overriding: overriding, Select: sel, OnConflict: onConflict, Returning: returning}, nil
	}

	if err := p.expectKeyword("values"); err != nil {
		return nil, err
	}

	var rows [][]insertValue
	for {
		row, err := p.parseInsertRow()
		if err != nil {
			return nil, err
		}
		rows = append(rows, row)
		if p.peek().Kind == tokComma {
			p.advance()
			continue
		}
		break
	}
	onConflict, err := p.parseOnConflict()
	if err != nil {
		return nil, err
	}
	returning, err := p.parseReturning()
	if err != nil {
		return nil, err
	}
	return &insert{Table: table, DB: dbQualifier, Columns: columns, Overriding: overriding, Rows: rows, OnConflict: onConflict, Returning: returning}, nil
}

// parseOnConflict parses the optional `ON CONFLICT [target] action` clause (UPSERT —
// spec/design/upsert.md), after the source and before RETURNING. ON / CONFLICT / DO / NOTHING /
// CONSTRAINT are not reserved (§3); the clause is recognized by the `ON CONFLICT` two-keyword lead.
func (p *parser) parseOnConflict() (*onConflict, error) {
	if p.peekKeyword() != "on" || p.peekKeywordAt(1) != "conflict" {
		return nil, nil
	}
	p.advance() // ON
	p.advance() // CONFLICT

	// Optional conflict target: a `( col, … )` column list or `ON CONSTRAINT name`.
	var target *conflictTarget
	if p.peek().Kind == tokLParen {
		p.advance() // '('
		var cols []string
		for {
			name, err := p.expectIdentifier()
			if err != nil {
				return nil, err
			}
			cols = append(cols, name)
			switch p.advance().Kind {
			case tokComma:
				continue
			case tokRParen:
			default:
				return nil, newError(SyntaxError, "expected ',' or ')'")
			}
			break
		}
		target = &conflictTarget{Columns: cols}
	} else if p.peekKeyword() == "on" {
		p.advance() // ON
		if err := p.expectKeyword("constraint"); err != nil {
			return nil, err
		}
		name, err := p.expectIdentifier()
		if err != nil {
			return nil, err
		}
		target = &conflictTarget{IsConstraint: true, Constraint: name}
	}

	// The action: `DO NOTHING` or `DO UPDATE SET assignment [, …] [WHERE …]`.
	if err := p.expectKeyword("do"); err != nil {
		return nil, err
	}
	switch p.peekKeyword() {
	case "nothing":
		p.advance()
		return &onConflict{Target: target, DoUpdate: false}, nil
	case "update":
		p.advance()
		if err := p.expectKeyword("set"); err != nil {
			return nil, err
		}
		var assignments []assignment
		for {
			column, err := p.expectIdentifier()
			if err != nil {
				return nil, err
			}
			if err := p.expect(tokEq); err != nil {
				return nil, err
			}
			value, err := p.parseExpr()
			if err != nil {
				return nil, err
			}
			assignments = append(assignments, assignment{Column: column, Value: value})
			if p.peek().Kind == tokComma {
				p.advance()
				continue
			}
			break
		}
		filter, err := p.parseOptionalWhere()
		if err != nil {
			return nil, err
		}
		return &onConflict{Target: target, DoUpdate: true, Assignments: assignments, Filter: filter}, nil
	default:
		return nil, newError(SyntaxError,
			fmt.Sprintf("expected NOTHING or UPDATE after ON CONFLICT DO, found %q", p.peekKeyword()))
	}
}

// parseInsertRow parses one parenthesized `( <value> [, <value>]* )` row of an INSERT.
func (p *parser) parseInsertRow() ([]insertValue, error) {
	if err := p.expect(tokLParen); err != nil {
		return nil, err
	}
	var values []insertValue
	for {
		v, err := p.parseInsertValue()
		if err != nil {
			return nil, err
		}
		values = append(values, v)
		switch p.advance().Kind {
		case tokComma:
			continue
		case tokRParen:
		default:
			return nil, newError(SyntaxError, "expected ',' or ')'")
		}
		break
	}
	if len(values) == 0 {
		return nil, newError(SyntaxError, "a VALUES row must have at least one value")
	}
	return values, nil
}

// parseInsertValue parses one INSERT value slot: the DEFAULT keyword (not reserved — §3), a
// ROW(...) composite constructor (spec/design/composite.md §1), a bind parameter ($N, bound at
// execute — spec/design/api.md §5), else a literal.
func (p *parser) parseInsertValue() (insertValue, error) {
	if p.peekKeyword() == "default" {
		p.advance()
		return insertValue{IsDefault: true}, nil
	}
	if p.peekKeyword() == "row" && p.peekKindAt(1) == tokLParen {
		// ROW(field, field, …) — recurse on each field (a literal, a $N, or a nested ROW).
		p.advance() // ROW
		if err := p.expect(tokLParen); err != nil {
			return insertValue{}, err
		}
		var fields []insertValue
		if p.peek().Kind != tokRParen {
			for {
				f, err := p.parseInsertValue()
				if err != nil {
					return insertValue{}, err
				}
				fields = append(fields, f)
				tok := p.advance()
				if tok.Kind == tokComma {
					continue
				}
				if tok.Kind == tokRParen {
					break
				}
				return insertValue{}, newError(SyntaxError, fmt.Sprintf("expected ',' or ')', found %v", tok))
			}
		} else {
			p.advance() // the empty ROW() — consume ')'
		}
		return insertValue{IsRow: true, Row: fields}, nil
	}
	if p.peekKeyword() == "array" && p.peekKindAt(1) == tokLBracket {
		// ARRAY[elem, …] — recurse on each element (a literal or a $N).
		p.advance() // ARRAY
		if err := p.expect(tokLBracket); err != nil {
			return insertValue{}, err
		}
		var elems []insertValue
		if p.peek().Kind != tokRBracket {
			for {
				e, err := p.parseInsertValue()
				if err != nil {
					return insertValue{}, err
				}
				elems = append(elems, e)
				tok := p.advance()
				if tok.Kind == tokComma {
					continue
				}
				if tok.Kind == tokRBracket {
					break
				}
				return insertValue{}, newError(SyntaxError, fmt.Sprintf("expected ',' or ']', found %v", tok))
			}
		} else {
			p.advance() // the empty ARRAY[] — consume ']'
		}
		return insertValue{IsArray: true, Array: elems}, nil
	}
	if p.peek().Kind == tokParam {
		n := p.advance().Int
		return insertValue{IsParam: true, Param: n}, nil
	}
	lit, err := p.parseLiteral()
	if err != nil {
		return insertValue{}, err
	}
	return insertValue{Lit: lit}, nil
}

// parseLiteral parses a literal value for INSERT: an integer (with an optional leading
// unary minus, folded here), or one of the keywords NULL / TRUE / FALSE. INSERT takes
// literals only — not general expressions (spec/grammar/grammar.ebnf `literal`).
func (p *parser) parseLiteral() (literal, error) {
	negate := false
	if p.peek().Kind == tokMinus {
		p.advance()
		negate = true
	}
	t := p.advance()
	switch {
	case t.Kind == tokInt:
		v, ok := foldInt(t.Int, negate)
		if !ok {
			return literal{}, newError(NumericValueOutOfRange,
				"value out of range: integer literal exceeds the maximum signed 64-bit value")
		}
		return literal{Kind: literalInt, Int: v}, nil
	case t.Kind == tokDecimal:
		// A decimal literal carries the unscaled coefficient + scale; the leading unary minus
		// (if any) folds into the sign. Cap checks are at resolve.
		return literal{Kind: literalDecimal, Dec: decimalFromDigitsScale(negate, t.Word, uint32(t.Int))}, nil
	case !negate && t.Kind == tokStr:
		return literal{Kind: literalText, Str: t.Word}, nil
	case !negate && t.Kind == tokWord && toLowerASCII(t.Word) == "null":
		return literal{Kind: literalNull}, nil
	case !negate && t.Kind == tokWord && toLowerASCII(t.Word) == "true":
		return literal{Kind: literalBool, Bool: true}, nil
	case !negate && t.Kind == tokWord && toLowerASCII(t.Word) == "false":
		return literal{Kind: literalBool, Bool: false}, nil
	default:
		return literal{}, newError(SyntaxError, "expected a literal value")
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
func (p *parser) parseQueryExpr() (statement, error) {
	node, err := p.parseQueryExprNode()
	if err != nil {
		return statement{}, err
	}
	if node.Select != nil {
		return statement{Select: node.Select}, nil
	}
	return statement{SetOp: node.SetOp}, nil
}

// parseQueryExprNode parses a top-level query_expr as a QueryExpr node — a set expression plus an
// optional trailing ORDER BY / LIMIT / OFFSET folded onto it. The shared core of parseQueryExpr
// (which wraps it in a Statement) and a WITH clause's main body. Unlike parseSubquery it opens no
// new nesting level — the body is at the statement top level.
func (p *parser) parseQueryExprNode() (queryExpr, error) {
	node, err := p.parseSetExpr()
	if err != nil {
		return queryExpr{}, err
	}
	// Trailing ORDER BY / LIMIT / OFFSET parse once, onto a scratch Select, then move onto the
	// outermost node (the lone Select, or the outermost SetOp).
	var trailing selectStmt
	if err := p.parseOrderBy(&trailing); err != nil {
		return queryExpr{}, err
	}
	if err := p.parseLimitOffset(&trailing); err != nil {
		return queryExpr{}, err
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
	return node, nil
}

// parseWithStatement parses `query_statement ::= with_clause? query_expr` — a top-level query
// prefixed by a WITH clause defining common table expressions (spec/design/cte.md). WITH RECURSIVE
// (spec/design/recursive-cte.md) sets the Recursive flag and lets a CTE reference itself; the CTE
// bodies and the main body are WITH-less query_exprs (the top-level-only narrowing — a nested WITH
// surfaces as 42601 because a body must begin with SELECT).
func (p *parser) parseWithStatement() (statement, error) {
	if err := p.expectKeyword("with"); err != nil {
		return statement{}, err
	}
	// `WITH RECURSIVE …` enables self-reference (recursive-cte.md). RECURSIVE in this position is
	// the keyword (PG reserves it), so a CTE may not be named `recursive` — a documented narrowing.
	// The flag governs the whole list; whether a given CTE is *actually* recursive is decided at
	// planning by whether its body references its own name.
	recursive := false
	if p.peekKeyword() == "recursive" {
		p.advance()
		recursive = true
	}
	var ctes []cte
	for {
		cte, err := p.parseCte()
		if err != nil {
			return statement{}, err
		}
		ctes = append(ctes, cte)
		if p.peek().Kind == tokComma {
			p.advance()
		} else {
			break
		}
	}
	// The primary may be a data-modifying statement (spec/design/writable-cte.md): a leading
	// INSERT/UPDATE/DELETE keyword selects it, otherwise a WITH-less query_expr.
	body, err := p.parseCteBody(false)
	if err != nil {
		return statement{}, err
	}
	return statement{With: &withQuery{Ctes: ctes, Body: body, Recursive: recursive}}, nil
}

// parseCteBody parses a cte_body (spec/design/writable-cte.md): a data-modifying
// INSERT/UPDATE/DELETE when one leads, otherwise a query. parenthesized is true for a CTE body
// inside ( … ) (the closing ) is the caller's), false for the WITH primary (it runs to end of
// statement). A query body parsed here is the WITH-less query_expr (the top-level-only nested-WITH
// narrowing — a nested WITH surfaces as a leftover 42601).
func (p *parser) parseCteBody(parenthesized bool) (cteBody, error) {
	switch p.peekKeyword() {
	case "insert", "update", "delete":
		// A parenthesized data-modifying body counts one nesting level, like parseSubquery does for a
		// parenthesized query body (grammar.md §48); the primary (parenthesized = false) runs at the
		// statement top level and does not.
		if parenthesized {
			if err := p.deepen(); err != nil {
				return cteBody{}, err
			}
		}
		var body cteBody
		var err error
		switch p.peekKeyword() {
		case "insert":
			body.Insert, err = p.parseInsert()
		case "update":
			body.Update, err = p.parseUpdate()
		default:
			body.Delete, err = p.parseDelete()
		}
		if err != nil {
			return cteBody{}, err
		}
		if parenthesized {
			p.undeepen()
		}
		return body, nil
	default:
		if parenthesized {
			q, err := p.parseSubquery()
			if err != nil {
				return cteBody{}, err
			}
			return cteBody{Query: &q}, nil
		}
		q, err := p.parseQueryExprNode()
		if err != nil {
			return cteBody{}, err
		}
		return cteBody{Query: &q}, nil
	}
}

// parseCte parses one common table expression
// `cte ::= identifier ("(" ident ("," ident)* ")")? "AS" ("NOT"? "MATERIALIZED")? "(" query_expr
// ")"` (spec/design/cte.md). The optional column list renames the body's output columns; [NOT]
// MATERIALIZED is the explicit evaluation hint. The body reuses parseSubquery (one nesting level,
// trailing clauses allowed) between its parens.
func (p *parser) parseCte() (cte, error) {
	name, err := p.expectIdentifier()
	if err != nil {
		return cte{}, err
	}
	var columns []string
	if p.peek().Kind == tokLParen {
		p.advance()
		col, err := p.expectIdentifier()
		if err != nil {
			return cte{}, err
		}
		columns = []string{col}
		for p.peek().Kind == tokComma {
			p.advance()
			col, err := p.expectIdentifier()
			if err != nil {
				return cte{}, err
			}
			columns = append(columns, col)
		}
		if err := p.expect(tokRParen); err != nil {
			return cte{}, err
		}
	}
	if err := p.expectKeyword("as"); err != nil {
		return cte{}, err
	}
	var materialized *bool
	switch p.peekKeyword() {
	case "materialized":
		p.advance()
		t := true
		materialized = &t
	case "not":
		if p.peekKeywordAt(1) == "materialized" {
			p.advance()
			p.advance()
			f := false
			materialized = &f
		}
	}
	if err := p.expect(tokLParen); err != nil {
		return cte{}, err
	}
	body, err := p.parseCteBody(true)
	if err != nil {
		return cte{}, err
	}
	if err := p.expect(tokRParen); err != nil {
		return cte{}, err
	}
	return cte{Name: name, Columns: columns, Materialized: materialized, Body: body}, nil
}

// parseSubquery parses a parenthesized subquery's inner query_expr (grammar.md §26): a full
// set-expression plus an optional trailing ORDER BY / LIMIT / OFFSET folded onto the node. Mirrors
// parseQueryExpr but yields a QueryExpr (the subquery operand) rather than a Statement. The caller
// has consumed the opening "(" and consumes the closing ")".
func (p *parser) parseSubquery() (queryExpr, error) {
	// A nested scalar subquery / EXISTS / IN (SELECT …) is one query-nesting level deeper; the
	// guard also protects the parser's own stack against `(SELECT (SELECT … ))`.
	if err := p.deepen(); err != nil {
		return queryExpr{}, err
	}
	var node queryExpr
	var err error
	if p.atWithClause() {
		// A leading WITH begins a nested common-table-expression query (spec/design/cte.md §7).
		node, err = p.parseWithQueryExpr()
	} else {
		node, err = p.parseSubqueryInner()
	}
	if err != nil {
		return queryExpr{}, err
	}
	p.undeepen()
	return node, nil
}

// parseSubqueryInner parses the non-WITH body of a subquery: a set-expression plus an optional
// trailing ORDER BY / LIMIT / OFFSET folded onto the node. Split out so a nested WITH's main query
// (parseWithQueryExpr) reuses it.
func (p *parser) parseSubqueryInner() (queryExpr, error) {
	node, err := p.parseSetExpr()
	if err != nil {
		return queryExpr{}, err
	}
	var trailing selectStmt
	if err := p.parseOrderBy(&trailing); err != nil {
		return queryExpr{}, err
	}
	if err := p.parseLimitOffset(&trailing); err != nil {
		return queryExpr{}, err
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
	return node, nil
}

// parseWithQueryExpr parses a nested `WITH [RECURSIVE] cte (, cte)* query_expr` into a
// QueryExpr{With} (spec/design/cte.md §7). The CTE bodies reuse parseCte (so a CTE body may itself
// nest a WITH); the main query is a WITH-less query_expr. A data-modifying CTE body parses here but
// is rejected at planning (0A000, top-level-only — matching PostgreSQL).
func (p *parser) parseWithQueryExpr() (queryExpr, error) {
	if err := p.expectKeyword("with"); err != nil {
		return queryExpr{}, err
	}
	recursive := false
	if p.peekKeyword() == "recursive" {
		p.advance()
		recursive = true
	}
	var ctes []cte
	for {
		cte, err := p.parseCte()
		if err != nil {
			return queryExpr{}, err
		}
		ctes = append(ctes, cte)
		if p.peek().Kind == tokComma {
			p.advance()
			continue
		}
		break
	}
	body, err := p.parseSubqueryInner()
	if err != nil {
		return queryExpr{}, err
	}
	return queryExpr{With: &withExpr{Ctes: ctes, Recursive: recursive, Body: &body}}, nil
}

// parseSetExpr parses the lower-precedence, left-associative UNION/EXCEPT level. INTERSECT binds
// tighter (parsed inside parseIntersectExpr), so `a UNION b INTERSECT c` becomes
// `a UNION (b INTERSECT c)`.
func (p *parser) parseSetExpr() (queryExpr, error) {
	base := p.depth
	left, err := p.parseIntersectExpr()
	if err != nil {
		return queryExpr{}, err
	}
	for {
		var op setOpKind
		switch p.peekKeyword() {
		case "union":
			op = setOpUnion
		case "except":
			op = setOpExcept
		default:
			p.depth = base
			return left, nil
		}
		if err := p.deepen(); err != nil { // each chained UNION/EXCEPT is one more set-op level
			return queryExpr{}, err
		}
		p.advance() // UNION | EXCEPT
		all := p.parseSetOpQuantifier()
		right, err := p.parseIntersectExpr()
		if err != nil {
			return queryExpr{}, err
		}
		left = queryExpr{SetOp: &setOp{Op: op, All: all, Lhs: left, Rhs: right}}
	}
}

// parseIntersectExpr parses the higher-precedence, left-associative INTERSECT level.
func (p *parser) parseIntersectExpr() (queryExpr, error) {
	base := p.depth
	core, err := p.parseSelectCore()
	if err != nil {
		return queryExpr{}, err
	}
	left := queryExpr{Select: core}
	for p.peekKeyword() == "intersect" {
		if err := p.deepen(); err != nil { // each chained INTERSECT is one more set-op level
			return queryExpr{}, err
		}
		p.advance() // INTERSECT
		all := p.parseSetOpQuantifier()
		right, err := p.parseSelectCore()
		if err != nil {
			return queryExpr{}, err
		}
		left = queryExpr{SetOp: &setOp{Op: setOpIntersect, All: all, Lhs: left, Rhs: queryExpr{Select: right}}}
	}
	p.depth = base
	return left, nil
}

// parseSetOpQuantifier consumes the optional ALL (multiset) or DISTINCT (explicit default)
// quantifier after a set operator, returning whether ALL was given.
func (p *parser) parseSetOpQuantifier() bool {
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
func (p *parser) parseSelect() (*selectStmt, error) {
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
func (p *parser) parseSelectCore() (*selectStmt, error) {
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
		modifier := next.Kind != tokEof && !(next.Kind == tokWord && toLowerASCII(next.Word) == "from")
		if modifier {
			p.advance()
			distinct = true
		}
	}

	items, err := p.parseSelectItems()
	if err != nil {
		return nil, err
	}
	var from *tableRef
	var joins []joinClause
	if p.peekKeyword() == "from" {
		p.advance() // FROM
		f, j, err := p.parseFromClause()
		if err != nil {
			return nil, err
		}
		from, joins = &f, j
	}

	sel := &selectStmt{Distinct: distinct, Items: items, From: from, Joins: joins}

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

	// WINDOW name AS ( definition ) (, …) — named windows referenced by OVER name (window.md §5).
	if err := p.parseWindowClause(sel); err != nil {
		return nil, err
	}

	return sel, nil
}

// parseWindowClause parses `window_clause ::= "WINDOW" identifier "AS" "(" window_definition ")"
// ("," …)*` (window.md §5) into sel.Windows. Each entry is a full window definition (which may
// extend an earlier entry via a leading base-window name — §5). Empty when no WINDOW keyword is
// present. WINDOW is non-reserved. Each definition reuses parseWindowDefinition with the inline OVER.
func (p *parser) parseWindowClause(sel *selectStmt) error {
	if p.peekKeyword() != "window" {
		return nil
	}
	p.advance()
	for {
		name, err := p.expectIdentifier()
		if err != nil {
			return err
		}
		if err := p.expectKeyword("as"); err != nil {
			return err
		}
		if err := p.expect(tokLParen); err != nil {
			return err
		}
		def, err := p.parseWindowDefinition()
		if err != nil {
			return err
		}
		if err := p.expect(tokRParen); err != nil {
			return err
		}
		sel.Windows = append(sel.Windows, namedWindow{Name: name, Def: def})
		if p.peek().Kind != tokComma {
			break
		}
		p.advance()
	}
	return nil
}

// parseWindowDefinition parses a window definition body `[base] [PARTITION BY …] [ORDER BY …]
// [frame]` between the already-consumed `(` and the closing `)` (spec/design/window.md §3, §5).
// The optional leading base-window name (a bareword that is not a clause-introducing keyword) marks
// a definition that extends a named window — the resolver merges it in (§5). Used by both the
// inline `OVER ( … )` and the `WINDOW name AS ( … )` clause so the two spellings parse identically.
func (p *parser) parseWindowDefinition() (windowDef, error) {
	base := p.parseOptBaseWindowName()
	var partition []exprNode
	if p.peekKeyword() == "partition" {
		p.advance()
		if err := p.expectKeyword("by"); err != nil {
			return windowDef{}, err
		}
		// A PARTITION BY key is a general expression (`PARTITION BY a + b`), not just a column
		// (spec/design/window.md §5.1). A bare column resolves to its slot directly; a compound
		// expression is materialized into a synthetic window-key column before the window stage.
		for {
			expr, err := p.parseExpr()
			if err != nil {
				return windowDef{}, err
			}
			partition = append(partition, expr)
			if p.peek().Kind != tokComma {
				break
			}
			p.advance()
		}
	}
	order, err := p.parseWindowOrderBy()
	if err != nil {
		return windowDef{}, err
	}
	frame, err := p.parseWindowFrame()
	if err != nil {
		return windowDef{}, err
	}
	return windowDef{Base: base, Partition: partition, Order: order, Frame: frame}, nil
}

// parseOptBaseWindowName returns the optional leading base-window name of a window definition
// (spec/design/window.md §5). Present when the next token is a bareword that is not a
// clause-introducing keyword (PARTITION/ORDER/ROWS/RANGE/GROUPS) — those start the definition's own
// clauses, so an unquoted occurrence is the keyword, never a base name (matching PostgreSQL; a
// window named like a keyword would need quoting, which jed's window names do not support).
func (p *parser) parseOptBaseWindowName() string {
	t := p.peek()
	if t.Kind != tokWord {
		return ""
	}
	switch toLowerASCII(t.Word) {
	case "partition", "order", "rows", "range", "groups":
		return ""
	}
	p.advance()
	return t.Word
}

// parseFromClause parses `from_clause ::= table_ref join_clause*` (grammar.md §15): the first
// table reference followed by a left-deep chain of zero or more joins. The join keywords are
// not reserved (§3); the loop recognizes a join only by a leading join keyword, so any other
// trailing word ends the FROM clause.
func (p *parser) parseFromClause() (tableRef, []joinClause, error) {
	from, err := p.parseTableRef()
	if err != nil {
		return tableRef{}, nil, err
	}
	var joins []joinClause
	for {
		for {
			j, ok, err := p.parseJoinClause()
			if err != nil {
				return tableRef{}, nil, err
			}
			if !ok {
				break
			}
			joins = append(joins, j)
		}
		// Comma-FROM (grammar.md §15): `FROM a, b` is an implicit CROSS JOIN. The comma separates
		// top-level FROM items, each its own join sub-chain; it binds LOOSER than JOIN, so the new
		// item begins a fresh ON-resolution segment (recorded by Comma: true). The inner loop then
		// picks up any joins of the new item (`a, b JOIN c ON …`) before the next comma.
		if p.peek().Kind == tokComma {
			p.advance()
			table, err := p.parseTableRef()
			if err != nil {
				return tableRef{}, nil, err
			}
			joins = append(joins, joinClause{Kind: joinCross, Table: table, Comma: true})
			continue
		}
		break
	}
	return from, joins, nil
}

// parseTableRef parses `table_ref ::= derived_table derived_alias? | (identifier | table_function)
// ("AS"? identifier)?` (grammar.md §15/§35/§42). A `(` at the START of a table_ref, when a SELECT
// follows, begins a DERIVED TABLE — a parenthesized subquery used as a relation (§42); any other
// leading `(` is a 42601 this slice (no parenthesized-join FROM). Otherwise it is a base table name
// OR a set-returning function call, a `(` immediately after the leading identifier marking the
// function form; the resolver owns arity/type errors. The alias logic is shared: an explicit AS
// takes the next identifier unconditionally; an implicit alias is taken only when the next token is
// a word that is NOT a clause/join keyword. The stop-keyword set is a §8 surface.
func (p *parser) parseTableRef() (tableRef, error) {
	// An optional leading LATERAL (grammar.md §44) marks a derived table / table function as
	// correlated to the EARLIER FROM relations. LATERAL is non-reserved (§3), so it is the keyword
	// only when a derived table `(` or a function call `name(` follows (a two-token lookahead) —
	// otherwise it is an ordinary identifier (e.g. a table named `lateral`). A table function is
	// implicitly lateral regardless, so the keyword is redundant (but accepted) there.
	lateral := p.peekKeyword() == "lateral" &&
		(p.peekKindAt(1) == tokLParen ||
			(p.peekKindAt(1) == tokWord && p.peekKindAt(2) == tokLParen))
	if lateral {
		p.advance()
	}
	if p.peek().Kind == tokLParen {
		tr, err := p.parseDerivedTable()
		if err != nil {
			return tableRef{}, err
		}
		tr.Lateral = lateral
		return tr, nil
	}
	// `JSON_TABLE(ctx, path [AS n] COLUMNS (…))` — a table source (json-table.md §3, T1), recognized
	// by the keyword followed by `(`.
	if p.peekKeyword() == "json_table" && p.peekKindAt(1) == tokLParen {
		return p.parseJsonTable()
	}
	name, err := p.expectIdentifier()
	if err != nil {
		return tableRef{}, err
	}
	// An optional DATABASE qualifier `db "." table` (attached-databases.md §3): a `.` after the first
	// identifier makes it the database qualifier and the next identifier the table name. A qualified
	// name is a BASE TABLE only — never a set-returning function (no cross-database SRF) — so the
	// function `(` branch below is guarded off when a qualifier is present.
	var dbQualifier *string
	if p.peek().Kind == tokDot {
		p.advance() // .
		tbl, err := p.expectIdentifier()
		if err != nil {
			return tableRef{}, err
		}
		q := name
		dbQualifier = &q
		name = tbl
	}
	// A `(` right after the name = a set-returning function call (no `*`/`DISTINCT`).
	var args []*exprNode
	isFunc := false
	if p.peek().Kind == tokLParen {
		if dbQualifier != nil {
			return tableRef{}, newError(SyntaxError, "a database-qualified name cannot be a function call")
		}
		isFunc = true
		p.advance()
		for {
			arg, err := p.parseExpr()
			if err != nil {
				return tableRef{}, err
			}
			args = append(args, &arg)
			if p.peek().Kind != tokComma {
				break
			}
			p.advance()
		}
		if err := p.expect(tokRParen); err != nil {
			return tableRef{}, err
		}
	}
	var alias *string
	if p.peekKeyword() == "as" {
		p.advance()
		a, err := p.expectIdentifier()
		if err != nil {
			return tableRef{}, err
		}
		alias = &a
	} else if t := p.peek(); t.Kind == tokWord && !isTableRefStopKeyword(toLowerASCII(t.Word)) {
		a := t.Word
		p.advance()
		alias = &a
	}
	// A `(` after the alias is a FROM-clause list on a table function (a base table never has one
	// there). The TYPED column-definition list `AS t(col type, …)` (C0, json-table.md §1) — for the
	// record-returning functions — is parsed here; the rename-only form `AS g(col)` (no type) stays a
	// deferred narrowing (grammar.md §35).
	var columnDefs []typeFieldDef
	if alias != nil && p.peek().Kind == tokLParen {
		// Disambiguate: a col-def list has `name type`; a rename list has `name ,`/`name )`. With the
		// cursor still on `(`, the first column name is at offset 1, so a `Word` at offset 2 means a
		// type follows (col-def list).
		if p.peekKindAt(2) != tokWord {
			return tableRef{}, newError(FeatureNotSupported,
				"column alias list on a table function is not supported yet")
		}
		p.advance() // (
		defs, err := p.parseFieldDefList()
		if err != nil {
			return tableRef{}, err
		}
		columnDefs = defs
	}
	// An SRF is implicitly lateral; Lateral records only whether the keyword was written.
	return tableRef{Name: name, DB: dbQualifier, Alias: alias, IsFunc: isFunc, Args: args, ColumnDefs: columnDefs, Lateral: lateral}, nil
}

// parseJsonTable parses `JSON_TABLE(ctx, path [AS n] COLUMNS (col, …)) [AS alias]` (json-table.md §3,
// T1). The caller has verified the `JSON_TABLE` keyword + `(`. An explicit PLAN clause and a PASSING
// clause are the deferred T2 (0A000).
func (p *parser) parseJsonTable() (tableRef, error) {
	p.advance() // JSON_TABLE
	p.advance() // (
	ctx, err := p.parseExpr()
	if err != nil {
		return tableRef{}, err
	}
	p.skipFormatJSON()
	if err := p.expect(tokComma); err != nil {
		return tableRef{}, err
	}
	path, err := p.parseExpr()
	if err != nil {
		return tableRef{}, err
	}
	// An optional `AS name` for the root path (the path-name) is accepted and ignored (it only matters
	// with an explicit PLAN clause, the deferred T2).
	if p.peekKeyword() == "as" {
		p.advance()
		if _, err := p.expectIdentifier(); err != nil {
			return tableRef{}, err
		}
	}
	if p.peekKeyword() == "passing" {
		return tableRef{}, newError(FeatureNotSupported, "JSON_TABLE PASSING clause is not supported yet")
	}
	if err := p.expectKeyword("columns"); err != nil {
		return tableRef{}, err
	}
	columns, err := p.parseJtColumns()
	if err != nil {
		return tableRef{}, err
	}
	// An explicit PLAN clause is the deferred T2 slice.
	if p.peekKeyword() == "plan" {
		return tableRef{}, newError(FeatureNotSupported, "JSON_TABLE explicit PLAN clause is not supported yet")
	}
	if err := p.expect(tokRParen); err != nil {
		return tableRef{}, err
	}
	var alias *string
	if p.peekKeyword() == "as" {
		p.advance()
		a, err := p.expectIdentifier()
		if err != nil {
			return tableRef{}, err
		}
		alias = &a
	} else if t := p.peek(); t.Kind == tokWord && !isTableRefStopKeyword(toLowerASCII(t.Word)) {
		a := t.Word
		p.advance()
		alias = &a
	}
	name := "json_table"
	if alias != nil {
		name = *alias
	}
	return tableRef{
		Name:      name,
		Alias:     alias,
		JsonTable: &jsonTable{Ctx: &ctx, Path: &path, Columns: columns},
	}, nil
}

// parseJtColumns parses a parenthesized `JSON_TABLE` `COLUMNS` list — `"(" jt_column ("," jt_column)*
// ")"`.
func (p *parser) parseJtColumns() ([]jtColumn, error) {
	if err := p.expect(tokLParen); err != nil {
		return nil, err
	}
	first, err := p.parseJtColumn()
	if err != nil {
		return nil, err
	}
	cols := []jtColumn{first}
	for p.peek().Kind == tokComma {
		p.advance()
		c, err := p.parseJtColumn()
		if err != nil {
			return nil, err
		}
		cols = append(cols, c)
	}
	if err := p.expect(tokRParen); err != nil {
		return nil, err
	}
	return cols, nil
}

// parseJtColumn parses one `JSON_TABLE` column: `NESTED [PATH] p [AS n] COLUMNS (…)`, `name FOR
// ORDINALITY`, `name type EXISTS [PATH p] [ON ERROR]`, or a regular `name type [PATH p] [wrapper]
// [quotes] [ON …]` column (json-table.md §3.3).
func (p *parser) parseJtColumn() (jtColumn, error) {
	if p.peekKeyword() == "nested" {
		p.advance() // NESTED
		if p.peekKeyword() == "path" {
			p.advance()
		}
		tok := p.advance()
		if tok.Kind != tokStr {
			return nil, newError(SyntaxError, "expected a string path after NESTED PATH")
		}
		path := tok.Word
		if p.peekKeyword() == "as" {
			p.advance()
			if _, err := p.expectIdentifier(); err != nil {
				return nil, err
			}
		}
		if err := p.expectKeyword("columns"); err != nil {
			return nil, err
		}
		columns, err := p.parseJtColumns()
		if err != nil {
			return nil, err
		}
		return &jtColumnNested{Path: path, Columns: columns}, nil
	}
	name, err := p.expectIdentifier()
	if err != nil {
		return nil, err
	}
	// `name FOR ORDINALITY`.
	if p.peekKeyword() == "for" {
		p.advance()
		if err := p.expectKeyword("ordinality"); err != nil {
			return nil, err
		}
		return &jtColumnOrdinality{Name: name}, nil
	}
	// `name type …` — parse the type name + optional `[]`.
	typeName, err := p.expectIdentifier()
	if err != nil {
		return nil, err
	}
	array := false
	if p.peek().Kind == tokLBracket {
		p.advance()
		if err := p.expect(tokRBracket); err != nil {
			return nil, err
		}
		array = true
	}
	// `EXISTS` column.
	if p.peekKeyword() == "exists" {
		p.advance()
		path, err := p.parseJtPathClause()
		if err != nil {
			return nil, err
		}
		onError, err := p.parseJSONOnErrorOnly()
		if err != nil {
			return nil, err
		}
		return &jtColumnExists{Name: name, TypeName: typeName, Path: path, OnError: onError}, nil
	}
	// A regular column.
	p.skipFormatJSON()
	path, err := p.parseJtPathClause()
	if err != nil {
		return nil, err
	}
	wrapper, keepQuotes, err := p.parseJSONWrapperQuotes()
	if err != nil {
		return nil, err
	}
	onEmpty, onError, err := p.parseJSONOnClauses()
	if err != nil {
		return nil, err
	}
	return &jtColumnRegular{
		Name:       name,
		TypeName:   typeName,
		Array:      array,
		Path:       path,
		Wrapper:    wrapper,
		KeepQuotes: keepQuotes,
		OnEmpty:    onEmpty,
		OnError:    onError,
	}, nil
}

// parseJtPathClause parses an optional `PATH '<string>'` clause on a JSON_TABLE column → the literal
// path string, or nil when absent (the column then defaults to `$.<name>`).
func (p *parser) parseJtPathClause() (*string, error) {
	if p.peekKeyword() != "path" {
		return nil, nil
	}
	p.advance()
	tok := p.advance()
	if tok.Kind != tokStr {
		return nil, newError(SyntaxError, "expected a string after PATH")
	}
	s := tok.Word
	return &s, nil
}

// parseDerivedTable parses a DERIVED TABLE — `"(" query_expr ")" derived_alias?` (grammar.md §42).
// The caller has verified the next token is `(`. A derived table is recognized only when a SELECT
// follows the `(` (the §26 leading-SELECT lookahead, a §8 cross-core surface); any other leading `(`
// is a 42601 (no parenthesized-join FROM this slice). The alias is OPTIONAL (PostgreSQL 18 relaxed
// the old mandatory-alias rule): present, it is the label and may carry a column-rename list; absent,
// the relation has no qualifier (its bare columns still resolve). Name/Alias carry the alias (empty
// when none).
func (p *parser) parseDerivedTable() (tableRef, error) {
	// Consume the opening `(`. The body is EITHER a query_expr (a leading SELECT) OR a VALUES list
	// (a leading VALUES) — FROM (VALUES (e…),(e…)), a computed relation of literal rows
	// (spec/design/grammar.md §42); any other leading `(` is rejected (a parenthesized-join FROM
	// `(a JOIN b ON …)` is a deferred narrowing).
	p.advance()
	var body *queryExpr
	var values [][]*exprNode
	switch {
	case p.peekKeyword() == "values":
		v, err := p.parseValuesBody()
		if err != nil {
			return tableRef{}, err
		}
		values = v
	case p.atSubqueryStart():
		// A leading SELECT, or a nested WITH (cte.md §7), is a query_expr body.
		b, err := p.parseSubquery()
		if err != nil {
			return tableRef{}, err
		}
		body = &b
	default:
		return tableRef{}, newError(SyntaxError,
			"subquery in FROM must begin with SELECT or VALUES (a parenthesized join is not supported)")
	}
	if err := p.expect(tokRParen); err != nil {
		return tableRef{}, err
	}
	// The alias is optional, parsed exactly like a base table's.
	var alias *string
	if p.peekKeyword() == "as" {
		p.advance()
		a, err := p.expectIdentifier()
		if err != nil {
			return tableRef{}, err
		}
		alias = &a
	} else if t := p.peek(); t.Kind == tokWord && !isTableRefStopKeyword(toLowerASCII(t.Word)) {
		a := t.Word
		p.advance()
		alias = &a
	}
	// Optional column-rename list `(c1, c2, …)` — only when a table alias was given (PG: a column
	// list with no preceding alias name is a syntax error; the bare `(` falls through and a later
	// token check rejects it).
	var columnAliases []string
	if alias != nil && p.peek().Kind == tokLParen {
		p.advance()
		for {
			c, err := p.expectIdentifier()
			if err != nil {
				return tableRef{}, err
			}
			columnAliases = append(columnAliases, c)
			if p.peek().Kind != tokComma {
				break
			}
			p.advance()
		}
		if err := p.expect(tokRParen); err != nil {
			return tableRef{}, err
		}
	}
	name := ""
	if alias != nil {
		name = *alias
	}
	return tableRef{Name: name, Alias: alias, Subquery: body, Values: values, ColumnAliases: columnAliases}, nil
}

// parseValuesBody parses a VALUES-body's rows — VALUES "(" expr ("," expr)* ")" ("," …)*
// (spec/design/grammar.md §42), the body of a FROM (VALUES …) derived table. The caller has
// verified the next keyword is VALUES (here consumed). Each row is a parenthesized list of GENERAL
// expressions (unlike the INSERT … VALUES slot, which is a literal/$N/DEFAULT); arity equality
// across rows and per-column type unification are resolve-time concerns (the executor's planValues).
// At least one row, each with at least one value. NO trailing ORDER BY / LIMIT is consumed — the
// caller's `)` follows the last row.
func (p *parser) parseValuesBody() ([][]*exprNode, error) {
	if err := p.expectKeyword("values"); err != nil {
		return nil, err
	}
	var rows [][]*exprNode
	for {
		if err := p.expect(tokLParen); err != nil {
			return nil, err
		}
		var row []*exprNode
		for {
			e, err := p.parseExpr()
			if err != nil {
				return nil, err
			}
			row = append(row, &e)
			if p.peek().Kind != tokComma {
				break
			}
			p.advance()
		}
		if err := p.expect(tokRParen); err != nil {
			return nil, err
		}
		rows = append(rows, row)
		if p.peek().Kind != tokComma {
			break
		}
		p.advance()
	}
	return rows, nil
}

// parseJoinClause parses one join_clause if a join keyword begins here (returns ok=false to end
// the FROM chain). CROSS JOIN has no ON; the INNER/outer kinds require ON <expr> (a missing ON
// is 42601). The outer kinds (LEFT/RIGHT/FULL [OUTER]) parse into the AST but are rejected at
// execution (0A000) — spec/design/grammar.md §15.
func (p *parser) parseJoinClause() (joinClause, bool, error) {
	// An optional leading NATURAL (grammar.md §15) makes the join derive its USING list from the
	// common column names. It is non-reserved (in the table-ref stop set so it is not swallowed as
	// the prior relation's alias); once consumed it MUST be followed by a join (a NATURAL CROSS JOIN
	// / bare NATURAL <non-join> is 42601), and takes no ON/USING.
	natural := p.peekKeyword() == "natural"
	if natural {
		p.advance()
	}
	kw := p.peekKeyword()
	var kind joinKind
	isCross := false
	switch kw {
	case "join": // a bare JOIN is INNER
		p.advance()
		kind = joinInner
	case "inner":
		p.advance()
		if err := p.expectKeyword("join"); err != nil {
			return joinClause{}, false, err
		}
		kind = joinInner
	case "cross":
		if natural {
			return joinClause{}, false, newError(SyntaxError, "NATURAL CROSS JOIN is not allowed")
		}
		p.advance()
		if err := p.expectKeyword("join"); err != nil {
			return joinClause{}, false, err
		}
		kind = joinCross
		isCross = true
	case "left", "right", "full":
		p.advance()
		if p.peekKeyword() == "outer" { // optional OUTER
			p.advance()
		}
		if err := p.expectKeyword("join"); err != nil {
			return joinClause{}, false, err
		}
		switch kw {
		case "left":
			kind = joinLeft
		case "right":
			kind = joinRight
		default:
			kind = joinFull
		}
	default:
		// After NATURAL a join keyword is required; otherwise the FROM chain just ends here.
		if natural {
			return joinClause{}, false, newError(SyntaxError, "NATURAL must be followed by a join")
		}
		return joinClause{}, false, nil
	}
	table, err := p.parseTableRef()
	if err != nil {
		return joinClause{}, false, err
	}
	// A non-CROSS, non-NATURAL join takes either `ON <expr>` or `USING (col, …)` (grammar.md §15).
	// A NATURAL join derives its condition (no ON/USING), and CROSS takes none. USING is not
	// reserved (§3): it is the join condition only as the keyword immediately following the right
	// table_ref. The column list has one or more names; an empty list is a 42601.
	var on *exprNode
	var using []string
	switch {
	case isCross || natural:
		// no condition (NATURAL derives it; CROSS has none)
	case p.peekKeyword() == "using":
		p.advance()
		if err := p.expect(tokLParen); err != nil {
			return joinClause{}, false, err
		}
		name, err := p.expectIdentifier()
		if err != nil {
			return joinClause{}, false, err
		}
		using = []string{name}
		for p.peek().Kind == tokComma {
			p.advance()
			name, err := p.expectIdentifier()
			if err != nil {
				return joinClause{}, false, err
			}
			using = append(using, name)
		}
		if err := p.expect(tokRParen); err != nil {
			return joinClause{}, false, err
		}
	default:
		if err := p.expectKeyword("on"); err != nil {
			return joinClause{}, false, err
		}
		e, err := p.parseExpr()
		if err != nil {
			return joinClause{}, false, err
		}
		on = &e
	}
	return joinClause{Kind: kind, Table: table, On: on, Using: using, Natural: natural}, true, nil
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
		// USING introduces a join condition after the right table_ref (`JOIN b USING (k)`), so it
		// must not be swallowed as `b`'s implicit alias (grammar.md §15).
		"using",
		// NATURAL prefixes a join (`a NATURAL JOIN b`), so it must not be swallowed as the prior
		// relation's alias (grammar.md §15).
		"natural",
		// set operators end a SELECT core — they must not be swallowed as an implicit table
		// alias (`FROM a UNION ...` is a UNION, not a table `a` aliased `union`). §25.
		"union", "intersect", "except",
		// RETURNING ends an INSERT ... SELECT source — it must not be swallowed as the
		// source's implicit table alias (`... SELECT v FROM t RETURNING v` is the INSERT's
		// clause). §32; PostgreSQL fully reserves the word.
		"returning",
		// WINDOW ends a SELECT core's FROM — it introduces the named-window clause and must
		// not be swallowed as an implicit table alias (`FROM t WINDOW w AS …`). window.md §5.
		"window":
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
// parseGroupBy parses `group_by ::= "GROUP" "BY" group_item ("," group_item)*` (grammar.md §18),
// after WHERE and before ORDER BY. Each term is an ordinary column, a parenthesized column group, or
// ROLLUP/CUBE/GROUPING SETS (spec/design/aggregates.md §12); every grouping column is a
// bare/qualified column (the same narrowing ORDER BY makes). `GROUP` is not reserved, so it is a
// clause only when immediately followed by `BY`.
func (p *parser) parseGroupBy(sel *selectStmt) error {
	if p.peekKeyword() != "group" {
		return nil
	}
	p.advance() // GROUP
	if err := p.expectKeyword("by"); err != nil {
		return err
	}
	for {
		item, err := p.parseGroupItem()
		if err != nil {
			return err
		}
		sel.GroupBy = append(sel.GroupBy, item)
		if p.peek().Kind == tokComma {
			p.advance()
			continue
		}
		break
	}
	return nil
}

// parseGroupItem parses one GROUP BY grouping term — a ROLLUP/CUBE/GROUPING SETS construct, or an
// ordinary column group (a bare column, a parenthesized `(a, b)`, or the empty set `()`). Also used
// for the elements of a GROUPING SETS list (which may nest these forms). ROLLUP/CUBE/GROUPING/SETS
// are unreserved, recognized by lookahead only.
func (p *parser) parseGroupItem() (groupItem, error) {
	switch p.peekKeyword() {
	case "rollup":
		p.advance()
		groups, err := p.parseGroupSetList()
		return groupItem{Kind: groupRollup, Groups: groups}, err
	case "cube":
		p.advance()
		groups, err := p.parseGroupSetList()
		return groupItem{Kind: groupCube, Groups: groups}, err
	case "grouping":
		if p.peekKeywordAt(1) == "sets" {
			p.advance() // GROUPING
			p.advance() // SETS
			if err := p.expect(tokLParen); err != nil {
				return groupItem{}, err
			}
			var elems []groupItem
			for {
				elem, err := p.parseGroupItem()
				if err != nil {
					return groupItem{}, err
				}
				elems = append(elems, elem)
				if p.peek().Kind == tokComma {
					p.advance()
					continue
				}
				break
			}
			if err := p.expect(tokRParen); err != nil {
				return groupItem{}, err
			}
			return groupItem{Kind: groupGroupingSets, Elems: elems}, nil
		}
	}
	cols, err := p.parseGroupSet()
	return groupItem{Kind: groupSet, Cols: cols}, err
}

// parseGroupSetList parses the parenthesized `( group_set ("," group_set)* )` argument list of
// ROLLUP / CUBE, where each element is a grouping expression group (spec/design/aggregates.md §12/§15).
func (p *parser) parseGroupSetList() ([][]exprNode, error) {
	if err := p.expect(tokLParen); err != nil {
		return nil, err
	}
	var sets [][]exprNode
	for {
		set, err := p.parseGroupSet()
		if err != nil {
			return nil, err
		}
		sets = append(sets, set)
		if p.peek().Kind == tokComma {
			p.advance()
			continue
		}
		break
	}
	if err := p.expect(tokRParen); err != nil {
		return nil, err
	}
	return sets, nil
}

// parseGroupSet parses a single grouping "expression group": a parenthesized `( e, ... )` / empty
// `()`, or a bare grouping term. Each member is a general expression — a bare/qualified column, a
// select-list ordinal (a bare integer literal), an output alias, or any expression (aggregates.md
// §15). A parenthesized list of two-or-more is a column group `(a, b)`; a single parenthesized
// expression `(a + b)` is one term — both fall out of parsing a comma-list of expressions.
func (p *parser) parseGroupSet() ([]exprNode, error) {
	if p.peek().Kind == tokLParen {
		p.advance()
		cols := []exprNode{}
		if p.peek().Kind != tokRParen {
			for {
				e, err := p.parseExpr()
				if err != nil {
					return nil, err
				}
				cols = append(cols, e)
				if p.peek().Kind == tokComma {
					p.advance()
					continue
				}
				break
			}
		}
		if err := p.expect(tokRParen); err != nil {
			return nil, err
		}
		return cols, nil
	}
	e, err := p.parseExpr()
	if err != nil {
		return nil, err
	}
	return []exprNode{e}, nil
}

// columnRefExpr builds a bare or qualified column-reference Expr from a parsed column_ref (the GROUP
// BY grouping terms are columns only — spec/design/aggregates.md §12).
func columnRefExpr(qualifier, col string) exprNode {
	if qualifier != "" {
		return exprNode{Kind: exprQualifiedColumn, Qualifier: qualifier, Column: col}
	}
	return exprNode{Kind: exprColumn, Column: col}
}

// parseHaving parses `having_clause ::= "HAVING" expr` (grammar.md §19), after GROUP BY and
// before ORDER BY. `HAVING` is not reserved; the predicate is a general expression (it may
// reference aggregates) checked for boolean at resolve.
func (p *parser) parseHaving(sel *selectStmt) error {
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

// parseOrderBy parses the query / set-operation `ORDER BY`. Each key is parsed as a general
// expression and classified into one of the three OrderKey modes (grammar.md §10): a bare integer
// literal (the unary-minus fold makes `-1` one negative Int) is an ordinal; a bare (optionally
// COLLATE-wrapped) column reference is a column key (kept on the fast path so PK-scan elision + the
// column's collation still apply); anything else is a general expression key. (WITHIN GROUP inlines
// its own column-only loop, so an integer there is a 42601 — matching PostgreSQL where a WITHIN GROUP
// integer is a constant, not an ordinal.)
func (p *parser) parseOrderBy(sel *selectStmt) error {
	if p.peekKeyword() != "order" {
		return nil
	}
	p.advance()
	if err := p.expectKeyword("by"); err != nil {
		return err
	}
	for {
		expr, err := p.parseExpr()
		if err != nil {
			return err
		}
		collation, descending, nullsFirst, err := p.parseSortSuffix()
		if err != nil {
			return err
		}
		sel.OrderBy = append(sel.OrderBy, classifyOrderKey(expr, collation, descending, nullsFirst, true))
		if p.peek().Kind == tokComma {
			p.advance()
			continue
		}
		break
	}
	return nil
}

// classifyOrderKey classifies a parsed ORDER BY key expression into one of the three OrderKey modes
// (grammar.md §10). allowOrdinal matches PostgreSQL's rule that only a bare integer constant is an
// ordinal — and only in a query/set-operation ORDER BY: when set, an integer literal (positive, or
// negative via the parser's unary-minus-on-literal fold) is an ordinal; when clear (WITHIN GROUP), the
// same bare integer falls through to a constant expression key. A bare column reference — directly, or
// wrapped in a COLLATE that parseExpr absorbed (`ORDER BY name COLLATE "x"`) — is a column key carrying
// that collation, so it stays on the fast path (PK-scan elision, per-column collation); every other
// shape is a general expression key.
func classifyOrderKey(expr exprNode, collation string, descending, nullsFirst, allowOrdinal bool) orderKey {
	switch expr.Kind {
	case exprLiteral:
		if allowOrdinal && expr.Literal != nil && expr.Literal.Kind == literalInt {
			ord := expr.Literal.Int
			return orderKey{Ordinal: &ord, Collation: collation, Descending: descending, NullsFirst: nullsFirst}
		}
	case exprColumn:
		return orderKey{Column: expr.Column, Collation: collation, Descending: descending, NullsFirst: nullsFirst}
	case exprQualifiedColumn:
		return orderKey{Qualifier: expr.Qualifier, Column: expr.Column, Collation: collation, Descending: descending, NullsFirst: nullsFirst}
	case exprCollate:
		// parseExpr folds a trailing `COLLATE "x"` into the key (collation.md §1). When it wraps a bare
		// column, unwrap back to a column key carrying that explicit collation — exactly the column-only
		// OrderKey the old parser built, so the column fast path is byte-identical.
		if inner := expr.Collate.Inner; inner.Kind == exprColumn {
			return orderKey{Column: inner.Column, Collation: expr.Collate.Collation, Descending: descending, NullsFirst: nullsFirst}
		} else if inner.Kind == exprQualifiedColumn {
			return orderKey{Qualifier: inner.Qualifier, Column: inner.Column, Collation: expr.Collate.Collation, Descending: descending, NullsFirst: nullsFirst}
		}
	}
	return orderKey{Expr: &expr, Collation: collation, Descending: descending, NullsFirst: nullsFirst}
}

// parseSortSuffix parses the trailing modifiers shared by every sort key: an optional `COLLATE
// "name"`, an optional `ASC`/`DESC` direction, and an optional `NULLS FIRST|LAST`. It returns
// (collation, descending, nullsFirst); nullsFirst is resolved here — explicit if given, else the
// direction default (ASC → NULLS LAST, DESC → NULLS FIRST: NULL is the largest value, the PostgreSQL
// model, grammar.md §10). A bare `NULLS` not followed by FIRST/LAST is 42601. Used by both the query
// ORDER BY (after a column ref) and the window ORDER BY (after a general expression).
func (p *parser) parseSortSuffix() (string, bool, bool, error) {
	collation := ""
	if p.peekKeyword() == "collate" {
		p.advance()
		c, err := p.expectCollationName()
		if err != nil {
			return "", false, false, err
		}
		collation = c
	}
	descending := false
	switch p.peekKeyword() {
	case "asc":
		p.advance()
	case "desc":
		p.advance()
		descending = true
	}
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
			return "", false, false, newError(SyntaxError, "NULLS must be followed by FIRST or LAST")
		}
	}
	return collation, descending, nullsFirst, nil
}

// parseLimitOffset parses an optional trailing `LIMIT <count>` and/or `OFFSET <count>`
// in either order, each at most once (a repeat is a syntax error, 42601), setting the
// resolved non-negative counts on sel (spec/grammar/grammar.ebnf `limit_offset`).
func (p *parser) parseLimitOffset(sel *selectStmt) error {
	for {
		switch p.peekKeyword() {
		case "limit":
			if sel.Limit != nil {
				return newError(SyntaxError, "duplicate LIMIT clause")
			}
			p.advance()
			n, err := p.parseCount(true)
			if err != nil {
				return err
			}
			sel.Limit = &n
		case "offset":
			if sel.Offset != nil {
				return newError(SyntaxError, "duplicate OFFSET clause")
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
// (OFFSET), and a magnitude over i64's max traps 22003 (the value -0 folds to 0 and is
// accepted). isLimit selects which structured error to raise.
func (p *parser) parseCount(isLimit bool) (int64, error) {
	negate := false
	if p.peek().Kind == tokMinus {
		p.advance()
		negate = true
	}
	t := p.advance()
	if t.Kind != tokInt {
		return 0, newError(SyntaxError, "expected an integer count")
	}
	v, ok := foldInt(t.Int, negate)
	if !ok {
		return 0, newError(NumericValueOutOfRange,
			"value out of range: count exceeds the maximum signed 64-bit value")
	}
	if v < 0 {
		if isLimit {
			return 0, newError(InvalidRowCountInLimitClause, "LIMIT must not be negative")
		}
		return 0, newError(InvalidRowCountInOffsetClause, "OFFSET must not be negative")
	}
	return v, nil
}

// parseUpdate parses
// `UPDATE <table> SET <col> = <operand> [, <col> = <operand>]* [WHERE <pred>]`.
func (p *parser) parseUpdate() (*update, error) {
	if err := p.expectKeyword("update"); err != nil {
		return nil, err
	}
	dbQualifier, table, err := p.parseQualifiedTableName()
	if err != nil {
		return nil, err
	}
	if err := p.expectKeyword("set"); err != nil {
		return nil, err
	}

	var assignments []assignment
	for {
		column, err := p.expectIdentifier()
		if err != nil {
			return nil, err
		}
		if err := p.expect(tokEq); err != nil {
			return nil, err
		}
		// DEFAULT is the assignment special form only when it is the complete RHS. Because
		// keywords are legal identifiers, `SET x = default + 1` must keep parsing `default` as
		// a column reference (grammar.md §16).
		nextKeyword := p.peekKeywordAt(1)
		nextKind := p.peekKindAt(1)
		isDefault := p.peekKeyword() == "default" &&
			(nextKind == tokComma || nextKind == tokEof || nextKeyword == "where" || nextKeyword == "returning")
		var value exprNode
		if isDefault {
			p.advance()
			value = exprNode{Kind: exprLiteral, Literal: &literal{Kind: literalNull}}
		} else {
			value, err = p.parseExpr()
			if err != nil {
				return nil, err
			}
		}
		assignments = append(assignments, assignment{Column: column, IsDefault: isDefault, Value: value})
		if p.peek().Kind == tokComma {
			p.advance()
			continue
		}
		break
	}
	if len(assignments) == 0 {
		return nil, newError(SyntaxError, "UPDATE must set at least one column")
	}

	filter, err := p.parseOptionalWhere()
	if err != nil {
		return nil, err
	}
	returning, err := p.parseReturning()
	if err != nil {
		return nil, err
	}
	return &update{Table: table, DB: dbQualifier, Assignments: assignments, Filter: filter, Returning: returning}, nil
}

// parseDelete parses `DELETE FROM <table> [WHERE <pred>]`. No WHERE deletes all rows.
func (p *parser) parseDelete() (*deleteStmt, error) {
	if err := p.expectKeyword("delete"); err != nil {
		return nil, err
	}
	if err := p.expectKeyword("from"); err != nil {
		return nil, err
	}
	dbQualifier, table, err := p.parseQualifiedTableName()
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
	return &deleteStmt{Table: table, DB: dbQualifier, Filter: filter, Returning: returning}, nil
}

// parseOptionalWhere parses an optional trailing `WHERE <expr>` (shared by
// SELECT / UPDATE / DELETE). The expression must resolve to boolean (checked by the
// executor).
func (p *parser) parseOptionalWhere() (*exprNode, error) {
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
func (p *parser) parseReturning() (*selectItems, error) {
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

func (p *parser) parseSelectItems() (selectItems, error) {
	if p.peek().Kind == tokStar {
		p.advance()
		return selectItems{All: true}, nil
	}
	var items []selectItem
	for {
		// `t.*` — a qualified star (all columns of the relation labeled `t`), a select-list /
		// RETURNING item MIXABLE with other items (grammar.md §15). Recognized by the three-token
		// shape `identifier "." "*"` before the general expr parser, so `t.col` (Dot then a word)
		// and `a * b` (no Dot) are untouched, and a bare `*` was already handled above. No `AS` alias.
		if p.peek().Kind == tokWord && p.peekKindAt(1) == tokDot && p.peekKindAt(2) == tokStar {
			qualifier, err := p.expectIdentifier()
			if err != nil {
				return selectItems{}, err
			}
			p.advance() // .
			p.advance() // *
			items = append(items, selectItem{Expr: exprNode{Kind: exprQualifiedStar, Qualifier: qualifier}})
			if p.peek().Kind == tokComma {
				p.advance()
				continue
			}
			break
		}
		e, err := p.parseExpr()
		if err != nil {
			return selectItems{}, err
		}
		// Optional `AS alias` output label. `AS` is not reserved, so it is taken as an
		// alias marker only here, after a complete expr (spec/grammar/grammar.ebnf
		// `select_item`). The alias never enters resolution (grammar.md §8).
		var alias *string
		if p.peekKeyword() == "as" {
			p.advance()
			name, err := p.expectIdentifier()
			if err != nil {
				return selectItems{}, err
			}
			alias = &name
		}
		items = append(items, selectItem{Expr: e, Alias: alias})
		if p.peek().Kind == tokComma {
			p.advance()
			continue
		}
		break
	}
	return selectItems{Items: items}, nil
}

// --- expression precedence ladder (spec/grammar/grammar.ebnf `expr`) ----------
// Loosest to tightest: OR < AND < NOT < comparison/IS NULL < additive <
// multiplicative < unary minus < primary. One function per level keeps the grammar
// legible (CLAUDE.md §10). The precedence is authored data (spec/functions/catalog.toml);
// this ladder must agree with it.

// parseExpr is the entry point for WHERE, the SELECT list, and UPDATE assignment values.
func (p *parser) parseExpr() (exprNode, error) {
	// A fresh sub-expression is one nesting level deeper (parens, ARRAY/ROW/CASE/function
	// operands, subscript indices all re-enter here). Bounds the recursive descent itself.
	if err := p.deepen(); err != nil {
		return exprNode{}, err
	}
	e, err := p.parseOr()
	if err != nil {
		return exprNode{}, err
	}
	p.undeepen()
	return e, nil
}

func (p *parser) parseOr() (exprNode, error) {
	base := p.depth
	lhs, err := p.parseAnd()
	if err != nil {
		return exprNode{}, err
	}
	for p.peekKeyword() == "or" {
		if err := p.deepen(); err != nil { // each chained OR is one more AST level
			return exprNode{}, err
		}
		p.advance()
		rhs, err := p.parseAnd()
		if err != nil {
			return exprNode{}, err
		}
		lhs = newBinaryExpr(opOr, lhs, rhs)
	}
	p.depth = base
	return lhs, nil
}

func (p *parser) parseAnd() (exprNode, error) {
	base := p.depth
	lhs, err := p.parseNot()
	if err != nil {
		return exprNode{}, err
	}
	for p.peekKeyword() == "and" {
		if err := p.deepen(); err != nil { // each chained AND is one more AST level
			return exprNode{}, err
		}
		p.advance()
		rhs, err := p.parseNot()
		if err != nil {
			return exprNode{}, err
		}
		lhs = newBinaryExpr(opAnd, lhs, rhs)
	}
	p.depth = base
	return lhs, nil
}

func (p *parser) parseNot() (exprNode, error) {
	if p.peekKeyword() == "not" {
		p.advance()
		// right-associative: NOT NOT x — each NOT is one more AST level (recursion here, so the
		// depth guard also protects the parser's own stack).
		if err := p.deepen(); err != nil {
			return exprNode{}, err
		}
		operand, err := p.parseNot()
		if err != nil {
			return exprNode{}, err
		}
		p.undeepen()
		return exprNode{Kind: exprUnary, Unary: &unaryExpr{Op: opNot, Operand: operand}}, nil
	}
	return p.parseComparison()
}

// parseComparison parses one comparison, a postfix IS [NOT] NULL, or
// IS [NOT] DISTINCT FROM, all non-associative: `a = b = c` is a syntax error, and
// `a + 1 IS NULL` binds as `(a + 1) IS NULL`. After the shared `IS` `NOT`? it dispatches
// on the NULL vs DISTINCT FROM keyword (spec/grammar/grammar.ebnf `comparison`).
func (p *parser) parseComparison() (exprNode, error) {
	lhs, err := p.parseConcat()
	if err != nil {
		return exprNode{}, err
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
				return exprNode{}, err
			}
			rhs, err := p.parseConcat()
			if err != nil {
				return exprNode{}, err
			}
			return exprNode{Kind: exprIsDistinct, IsDistinct: &isDistinctExpr{Lhs: lhs, Rhs: rhs, Negated: negated}}, nil
		}
		// IS [NOT] JSON [VALUE|SCALAR|ARRAY|OBJECT] [(WITH|WITHOUT) UNIQUE [KEYS]] — the SQL/JSON
		// well-formedness predicate (json-sql-functions.md §5).
		if p.peekKeyword() == "json" {
			p.advance()
			kind := jPKValue
			switch p.peekKeyword() {
			case "value":
				p.advance()
				kind = jPKValue
			case "scalar":
				p.advance()
				kind = jPKScalar
			case "array":
				p.advance()
				kind = jPKArray
			case "object":
				p.advance()
				kind = jPKObject
			}
			// The unique-keys clause: `(WITH|WITHOUT) UNIQUE [KEYS]`. Consume `WITH`/`WITHOUT` only
			// when `UNIQUE` follows (a two-token lookahead — `WITH` otherwise starts no
			// expression-level clause here). `KEYS` is optional.
			uniqueKeys := false
			if w := p.peekKeyword(); (w == "with" || w == "without") && p.peekKeywordAt(1) == "unique" {
				p.advance() // WITH / WITHOUT
				p.advance() // UNIQUE
				if p.peekKeyword() == "keys" {
					p.advance()
				}
				uniqueKeys = w == "with"
			}
			return exprNode{Kind: exprIsJson, IsJsonOf: &isJsonExpr{Operand: lhs, Negated: negated, Kind: kind, UniqueKeys: uniqueKeys}}, nil
		}
		if err := p.expectKeyword("null"); err != nil {
			return exprNode{}, err
		}
		return exprNode{Kind: exprIsNull, IsNullOf: &isNullExpr{Operand: lhs, Negated: negated}}, nil
	}
	// `NOT`? (`IN` (...) | `BETWEEN` lo `AND` hi) — a `NOT` here is consumed only when followed
	// by one of these postfix-predicate keywords (two-token lookahead; the prefix `NOT` was
	// already taken by parseNot). Non-associative, at the comparison level (grammar.md §20-§21).
	predNegated := p.peekKeyword() == "not" &&
		(p.peekKeywordAt(1) == "in" || p.peekKeywordAt(1) == "between" ||
			p.peekKeywordAt(1) == "like" || p.peekKeywordAt(1) == "ilike")
	if predNegated {
		p.advance() // NOT
	}
	if p.peekKeyword() == "in" {
		p.advance()
		if err := p.expect(tokLParen); err != nil {
			return exprNode{}, err
		}
		// `IN (SELECT ...)` is the uncorrelated IN-subquery (grammar.md §26), disambiguated by a
		// leading `SELECT` (or a nested `WITH` — cte.md §7); otherwise a non-empty value list
		// (`IN ()` is a 42601 syntax error).
		if p.atSubqueryStart() {
			q, err := p.parseSubquery()
			if err != nil {
				return exprNode{}, err
			}
			if err := p.expect(tokRParen); err != nil {
				return exprNode{}, err
			}
			return exprNode{Kind: exprInSubquery, InSubquery: &inSubqueryExpr{Lhs: lhs, Query: q, Negated: predNegated}}, nil
		}
		// A non-empty value list (`IN ()` — parseConcat on `)` is a 42601 syntax error).
		first, err := p.parseConcat()
		if err != nil {
			return exprNode{}, err
		}
		list := []exprNode{first}
		for p.peek().Kind == tokComma {
			p.advance()
			elem, err := p.parseConcat()
			if err != nil {
				return exprNode{}, err
			}
			list = append(list, elem)
		}
		if err := p.expect(tokRParen); err != nil {
			return exprNode{}, err
		}
		return exprNode{Kind: exprIn, In: &inExpr{Lhs: lhs, List: list, Negated: predNegated}}, nil
	}
	if p.peekKeyword() == "between" {
		p.advance()
		// Both bounds parse at the CONCAT level (one tighter than comparison), which never
		// consumes `AND` (a looser level owned by parseAnd). So the BETWEEN's structural `AND` is
		// matched here and `x BETWEEN a AND b AND c` parses as `(x BETWEEN a AND b) AND c`
		// (grammar.md §21); a `||` bound still works.
		lo, err := p.parseConcat()
		if err != nil {
			return exprNode{}, err
		}
		if err := p.expectKeyword("and"); err != nil {
			return exprNode{}, err
		}
		hi, err := p.parseConcat()
		if err != nil {
			return exprNode{}, err
		}
		return exprNode{Kind: exprBetween, Between: &betweenExpr{Lhs: lhs, Lo: lo, Hi: hi, Negated: predNegated}}, nil
	}
	// LIKE / ILIKE (case-insensitive) — grammar.md §22. `ilike` is just another peeked keyword.
	if p.peekKeyword() == "like" || p.peekKeyword() == "ilike" {
		insensitive := p.peekKeyword() == "ilike"
		p.advance()
		rhs, err := p.parseConcat()
		if err != nil {
			return exprNode{}, err
		}
		return exprNode{Kind: exprLike, Like: &likeExpr{Lhs: lhs, Rhs: rhs, Negated: predNegated, Insensitive: insensitive}}, nil
	}
	// `~` / `~*` / `!~` / `!~*` — regex match (grammar.md §22b, regex.md). Punctuation operators, so
	// `negated`/`insensitive` come from the token itself; there is no `NOT ~` keyword form (`NOT x ~ p`
	// is the prefix-NOT over the whole match, taken a level up). The pattern is one CONCAT expression.
	var rxNegated, rxInsensitive bool
	rxMatch := true
	switch p.peek().Kind {
	case tokTilde:
	case tokTildeStar:
		rxInsensitive = true
	case tokBangTilde:
		rxNegated = true
	case tokBangTildeStar:
		rxNegated, rxInsensitive = true, true
	default:
		rxMatch = false
	}
	if rxMatch {
		p.advance()
		rhs, err := p.parseConcat()
		if err != nil {
			return exprNode{}, err
		}
		return exprNode{Kind: exprRegex, Regex: &regexExpr{Lhs: lhs, Rhs: rhs, Negated: rxNegated, Insensitive: rxInsensitive}}, nil
	}
	var op binaryOp
	switch p.peek().Kind {
	case tokEq:
		op = opEq
	case tokNe:
		op = opNe
	case tokLt:
		op = opLt
	case tokGt:
		op = opGt
	case tokLe:
		op = opLe
	case tokGe:
		op = opGe
	default:
		return lhs, nil
	}
	p.advance()
	// `op ANY/SOME/ALL ( array )` — a quantified array comparison (grammar.md §41): a quantifier
	// may stand in for the ordinary right operand. SOME folds to ANY.
	if kw := p.peekKeyword(); kw == "all" || kw == "any" || kw == "some" {
		all := kw == "all"
		p.advance() // ANY / SOME / ALL
		if err := p.expect(tokLParen); err != nil {
			return exprNode{}, err
		}
		// A leading `SELECT` is the SUBQUERY form `op ANY/ALL(SELECT …)` — the subquery spelling of
		// IN (array-functions.md §11.6), the §26 leading-`SELECT` lookahead (or a nested `WITH` —
		// cte.md §7); anything else is the array operand (§11.1).
		if p.atSubqueryStart() {
			query, err := p.parseSubquery()
			if err != nil {
				return exprNode{}, err
			}
			if err := p.expect(tokRParen); err != nil {
				return exprNode{}, err
			}
			return exprNode{Kind: exprQuantifiedSubquery, QuantifiedSubquery: &quantifiedSubqueryExpr{Op: op, All: all, Lhs: lhs, Query: query}}, nil
		}
		array, err := p.parseExpr() // a full expression resolving to an array
		if err != nil {
			return exprNode{}, err
		}
		if err := p.expect(tokRParen); err != nil {
			return exprNode{}, err
		}
		return exprNode{Kind: exprQuantified, Quantified: &quantifiedExpr{Op: op, All: all, Lhs: lhs, Array: array}}, nil
	}
	rhs, err := p.parseConcat()
	if err != nil {
		return exprNode{}, err
	}
	return newBinaryExpr(op, lhs, rhs), nil
}

// parseConcat parses the "any other operator" level (grammar.md §39/§40, array-functions.md §8/§10):
// one rung tighter than the comparisons, looser than additive, left-associative. It hosts `||` array
// concatenation plus the `@>`/`<@`/`&&` array containment/overlap operators — all the same precedence
// in PostgreSQL. Each operand is an additive expression, so `a + b || c` is `(a + b) || c`; chaining
// mixes freely (`a || b @> c` is `(a || b) @> c`).
func (p *parser) parseConcat() (exprNode, error) {
	base := p.depth
	lhs, err := p.parseAdditive()
	if err != nil {
		return exprNode{}, err
	}
	for {
		var op binaryOp
		switch p.peek().Kind {
		case tokConcat:
			op = opConcat
		case tokContains:
			op = opContains
		case tokJsonPathExists:
			op = opJsonPathExists
		case tokJsonPathMatch:
			op = opJsonPathMatch
		case tokContainedBy:
			op = opContainedBy
		case tokOverlaps:
			op = opOverlaps
		case tokStrictlyLeft:
			op = opStrictlyLeft
		case tokStrictlyRight:
			op = opStrictlyRight
		case tokNotExtendRight:
			op = opNotExtendRight
		case tokNotExtendLeft:
			op = opNotExtendLeft
		case tokAdjacent:
			op = opAdjacent
		// The jsonb accessor operators (json-sql-functions.md §1) — "any other operator" precedence,
		// same level as `@>`/`||`, left-associative (`doc -> 'a' -> 'b'`).
		case tokArrow:
			op = opJsonGet
		case tokArrowText:
			op = opJsonGetText
		case tokHashArrow:
			op = opJsonGetPath
		case tokHashArrowText:
			op = opJsonGetPathText
		case tokQuestion:
			op = opJsonHasKey
		case tokQuestionPipe:
			op = opJsonHasAnyKey
		case tokQuestionAmp:
			op = opJsonHasAllKeys
		case tokHashMinus:
			op = opJsonDeletePath
		default:
			p.depth = base
			return lhs, nil
		}
		if err := p.deepen(); err != nil { // each chained operator is one more AST level
			return exprNode{}, err
		}
		p.advance()
		rhs, err := p.parseAdditive()
		if err != nil {
			return exprNode{}, err
		}
		lhs = newBinaryExpr(op, lhs, rhs)
	}
}

func (p *parser) parseAdditive() (exprNode, error) {
	base := p.depth
	lhs, err := p.parseMultiplicative()
	if err != nil {
		return exprNode{}, err
	}
	for {
		var op binaryOp
		switch p.peek().Kind {
		case tokPlus:
			op = opAdd
		case tokMinus:
			op = opSub
		default:
			p.depth = base
			return lhs, nil
		}
		if err := p.deepen(); err != nil { // each chained +/- is one more AST level (`1+1+…`)
			return exprNode{}, err
		}
		p.advance()
		rhs, err := p.parseMultiplicative()
		if err != nil {
			return exprNode{}, err
		}
		lhs = newBinaryExpr(op, lhs, rhs)
	}
}

func (p *parser) parseMultiplicative() (exprNode, error) {
	base := p.depth
	lhs, err := p.parseAtTimeZone()
	if err != nil {
		return exprNode{}, err
	}
	for {
		var op binaryOp
		switch p.peek().Kind {
		case tokStar:
			op = opMul
		case tokSlash:
			op = opDiv
		case tokPercent:
			op = opMod
		default:
			p.depth = base
			return lhs, nil
		}
		if err := p.deepen(); err != nil { // each chained * / % is one more AST level
			return exprNode{}, err
		}
		p.advance()
		rhs, err := p.parseAtTimeZone()
		if err != nil {
			return exprNode{}, err
		}
		lhs = newBinaryExpr(op, lhs, rhs)
	}
}

// parseAtTimeZone parses the `AT TIME ZONE` rung (grammar.md §49, timezones.md §6): a
// left-associative infix operator binding tighter than `* / %`, additive, and the comparisons, looser
// than COLLATE / `::` / unary minus (PostgreSQL's %left AT). `value AT TIME ZONE zone` desugars to the
// function call `timezone(zone, value)` — PostgreSQL's own implementation — so the resolver/evaluator/
// cost have one path for the operator and the bare call. AT/TIME/ZONE are non-reserved (matched as a
// three-token sequence), so a bare column named at/time/zone is unaffected.
func (p *parser) parseAtTimeZone() (exprNode, error) {
	base := p.depth
	lhs, err := p.parseUnary()
	if err != nil {
		return exprNode{}, err
	}
	for p.peekKeyword() == "at" && p.peekKeywordAt(1) == "time" && p.peekKeywordAt(2) == "zone" {
		if err := p.deepen(); err != nil { // each chained AT TIME ZONE is one more AST level
			return exprNode{}, err
		}
		p.advance() // AT
		p.advance() // TIME
		p.advance() // ZONE
		zone, err := p.parseUnary()
		if err != nil {
			return exprNode{}, err
		}
		prev := lhs // capture before reassigning, so the &-of-value stays stable
		lhs = exprNode{Kind: exprFuncCall, FuncCall: &funcCallExpr{
			Name: "timezone",
			Args: []*exprNode{&zone, &prev},
		}}
	}
	p.depth = base
	return lhs, nil
}

func (p *parser) parseUnary() (exprNode, error) {
	if p.peek().Kind == tokMinus {
		p.advance()
		// Fold unary-minus-of-an-integer-literal into one negative literal, so i64's
		// minimum is representable and the literal range-checks against context. SUPPRESSED
		// when a `::` immediately follows: `::` binds tighter than unary minus (PostgreSQL),
		// so `-N::T` is `-(N::T)` — the cast applies to the unsigned magnitude first
		// (grammar.md §37). A one-token lookahead on the token AFTER the literal.
		if p.peek().Kind == tokInt && p.peekKindAt(1) != tokDoubleColon {
			v, ok := foldInt(p.advance().Int, true)
			if !ok {
				return exprNode{}, newError(NumericValueOutOfRange,
					"value out of range: integer literal exceeds the maximum signed 64-bit value")
			}
			return exprNode{Kind: exprLiteral, Literal: &literal{Kind: literalInt, Int: v}}, nil
		}
		// Fold unary-minus of a decimal literal into one negative decimal literal (decimal
		// negation never overflows). Same `::` suppression.
		if p.peek().Kind == tokDecimal && p.peekKindAt(1) != tokDoubleColon {
			t := p.advance()
			return exprNode{Kind: exprLiteral, Literal: &literal{
				Kind: literalDecimal, Dec: decimalFromDigitsScale(true, t.Word, uint32(t.Int)),
			}}, nil
		}
		// each chained unary `-` is one more AST level (recursion here, so the depth guard also
		// protects the parser's own stack against `- - - … x`).
		if err := p.deepen(); err != nil {
			return exprNode{}, err
		}
		operand, err := p.parseUnary()
		if err != nil {
			return exprNode{}, err
		}
		p.undeepen()
		return exprNode{Kind: exprUnary, Unary: &unaryExpr{Op: opNeg, Operand: operand}}, nil
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
func (p *parser) parsePostfix() (exprNode, error) {
	// Only a PARENTHESIZED primary is field-accessible (PG requires `(expr).field`). A subsequent
	// `.field` keeps the chain field-accessible (`(c).a.b`); a `::` cast does not.
	base0 := p.depth
	fieldAccessible := p.peek().Kind == tokLParen
	expr, err := p.parsePrimary()
	if err != nil {
		return exprNode{}, err
	}
	for {
		// each postfix `::`/`[…]`/`.field`/COLLATE wraps the base in one more AST level; deepen only
		// when a postfix actually follows (not on the terminating non-postfix token). COLLATE shares
		// this rung so it binds tighter than `||` and the comparisons (PG precedence).
		isCollate := p.peek().Kind == tokWord && p.peekKeyword() == "collate"
		isPostfix := p.peek().Kind == tokDoubleColon || p.peek().Kind == tokLBracket ||
			(p.peek().Kind == tokDot && fieldAccessible) || isCollate
		if !isPostfix {
			p.depth = base0
			return expr, nil
		}
		if err := p.deepen(); err != nil {
			return exprNode{}, err
		}
		switch {
		case isCollate:
			p.advance() // COLLATE
			name, err := p.expectCollationName()
			if err != nil {
				return exprNode{}, err
			}
			expr = exprNode{Kind: exprCollate, Collate: &collateExpr{Inner: expr, Collation: name}}
			fieldAccessible = false
		case p.peek().Kind == tokDoubleColon:
			p.advance()
			typeName, err := p.expectIdentifier()
			if err != nil {
				return exprNode{}, err
			}
			typeMod, err := p.parseTypeMod()
			if err != nil {
				return exprNode{}, err
			}
			isArray, err := p.consumeArrayBrackets()
			if err != nil {
				return exprNode{}, err
			}
			if isArray {
				typeName += "[]"
			}
			expr = exprNode{Kind: exprCast, Cast: &castExpr{Inner: expr, TypeName: typeName, TypeMod: typeMod}}
			fieldAccessible = false
		// `base[..][..]` — array subscript (spec/design/array.md §6). Applies to ANY base (no parens
		// rule, unlike `.field`). Consecutive `[…]` brackets collect into ONE access (so `a[1][2]` is
		// a single multidim element read, not nested). Each spec is an index `[i]` or a slice `[m:n]`
		// (bounds optionally omitted). After a subscript a `.field` still needs parens (PG).
		case p.peek().Kind == tokLBracket:
			base := expr
			var subs []subscriptSpec
			for p.peek().Kind == tokLBracket {
				p.advance() // [
				// The lower bound / index is absent only before a `:` or `]` (`[:n]`, `[]`).
				var lower *exprNode
				if p.peek().Kind != tokColon && p.peek().Kind != tokRBracket {
					e, err := p.parseExpr()
					if err != nil {
						return exprNode{}, err
					}
					lower = &e
				}
				if p.peek().Kind == tokColon {
					p.advance() // :
					var upper *exprNode
					if p.peek().Kind != tokRBracket {
						e, err := p.parseExpr()
						if err != nil {
							return exprNode{}, err
						}
						upper = &e
					}
					if err := p.expect(tokRBracket); err != nil {
						return exprNode{}, err
					}
					subs = append(subs, subscriptSpec{IsSlice: true, Lower: lower, Upper: upper})
				} else {
					// Index form: a bare `[]` (no index, no colon) is a syntax error.
					if lower == nil {
						return exprNode{}, newError(SyntaxError, "array subscript requires an index")
					}
					if err := p.expect(tokRBracket); err != nil {
						return exprNode{}, err
					}
					subs = append(subs, subscriptSpec{Index: lower})
				}
			}
			expr = exprNode{Kind: exprSubscript, Base: &base, Subscripts: subs}
			fieldAccessible = false
		// `.field` / `.*` — composite field selection (spec/design/composite.md §S4),
		// parens-required: only on a parenthesized / chained-field base.
		case p.peek().Kind == tokDot && fieldAccessible:
			p.advance()
			base := expr
			if p.peek().Kind == tokStar {
				p.advance()
				expr = exprNode{Kind: exprFieldStar, Base: &base}
				fieldAccessible = false // `.*` is terminal
			} else {
				field, err := p.expectIdentifier()
				if err != nil {
					return exprNode{}, err
				}
				expr = exprNode{Kind: exprFieldAccess, Base: &base, Field: field}
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
func (p *parser) parsePrimary() (exprNode, error) {
	if p.peek().Kind == tokLParen {
		p.advance()
		// `(SELECT ...)` is a scalar subquery (grammar.md §26), disambiguated by a leading
		// `SELECT` (or a nested `WITH` — cte.md §7) after the `(`; otherwise a parenthesized expr.
		if p.atSubqueryStart() {
			q, err := p.parseSubquery()
			if err != nil {
				return exprNode{}, err
			}
			if err := p.expect(tokRParen); err != nil {
				return exprNode{}, err
			}
			return exprNode{Kind: exprScalarSubquery, Subquery: &q}, nil
		}
		e, err := p.parseExpr()
		if err != nil {
			return exprNode{}, err
		}
		if err := p.expect(tokRParen); err != nil {
			return exprNode{}, err
		}
		return e, nil
	}
	// `EXISTS ( SELECT ... )` — the existence predicate (grammar.md §26). Recognized only when an
	// open-paren + a query start (`SELECT`, or a nested `WITH` — cte.md §7) follows, so `exists`
	// stays usable as a column / function name.
	if p.peekKeyword() == "exists" && p.peekKindAt(1) == tokLParen && p.isQueryStartAtOffset(2) {
		p.advance() // EXISTS
		if err := p.expect(tokLParen); err != nil {
			return exprNode{}, err
		}
		q, err := p.parseSubquery()
		if err != nil {
			return exprNode{}, err
		}
		if err := p.expect(tokRParen); err != nil {
			return exprNode{}, err
		}
		return exprNode{Kind: exprExists, Subquery: &q}, nil
	}
	// `ROW(e1, e2, …)` composite constructor (spec/design/composite.md §1). Recognized when ROW is
	// immediately followed by `(`, so `row` stays usable as a column / function name otherwise. The
	// bare `(a, b)` form is deferred (0A000); only the keyword form parses.
	if p.peekKeyword() == "row" && p.peekKindAt(1) == tokLParen {
		p.advance() // ROW
		if err := p.expect(tokLParen); err != nil {
			return exprNode{}, err
		}
		var items []exprNode
		if p.peek().Kind != tokRParen {
			for {
				e, err := p.parseExpr()
				if err != nil {
					return exprNode{}, err
				}
				items = append(items, e)
				tok := p.advance()
				if tok.Kind == tokComma {
					continue
				}
				if tok.Kind == tokRParen {
					break
				}
				return exprNode{}, newError(SyntaxError, fmt.Sprintf("expected ',' or ')', found %v", tok))
			}
		} else {
			p.advance() // the empty ROW() — consume ')'
		}
		return exprNode{Kind: exprRow, RowItems: items}, nil
	}
	// `ARRAY[e1, e2, …]` array constructor (spec/design/array.md §1). Recognized when ARRAY is
	// immediately followed by `[`, so `array` stays usable as an identifier otherwise.
	if p.peekKeyword() == "array" && p.peekKindAt(1) == tokLBracket {
		p.advance() // ARRAY
		if err := p.expect(tokLBracket); err != nil {
			return exprNode{}, err
		}
		var items []exprNode
		if p.peek().Kind != tokRBracket {
			for {
				e, err := p.parseExpr()
				if err != nil {
					return exprNode{}, err
				}
				items = append(items, e)
				tok := p.advance()
				if tok.Kind == tokComma {
					continue
				}
				if tok.Kind == tokRBracket {
					break
				}
				return exprNode{}, newError(SyntaxError, fmt.Sprintf("expected ',' or ']', found %v", tok))
			}
		} else {
			p.advance() // the empty ARRAY[] — consume ']'
		}
		return exprNode{Kind: exprArray, RowItems: items}, nil
	}
	if p.peekKeyword() == "cast" {
		p.advance()
		if err := p.expect(tokLParen); err != nil {
			return exprNode{}, err
		}
		inner, err := p.parseExpr()
		if err != nil {
			return exprNode{}, err
		}
		if err := p.expectKeyword("as"); err != nil {
			return exprNode{}, err
		}
		typeName, err := p.expectIdentifier()
		if err != nil {
			return exprNode{}, err
		}
		typeMod, err := p.parseTypeMod()
		if err != nil {
			return exprNode{}, err
		}
		isArray, err := p.consumeArrayBrackets()
		if err != nil {
			return exprNode{}, err
		}
		if isArray {
			typeName += "[]"
		}
		if err := p.expect(tokRParen); err != nil {
			return exprNode{}, err
		}
		return exprNode{Kind: exprCast, Cast: &castExpr{Inner: inner, TypeName: typeName, TypeMod: typeMod}}, nil
	}
	// EXTRACT(field FROM source) (grammar.md §50, timezones.md §9.2). Recognized only when `extract`
	// is immediately followed by `(`, so `extract` stays usable as a column / function name otherwise
	// (the one-token lookahead, §8). The field is an identifier or a string literal (lowercased).
	if p.peekKeyword() == "extract" && p.peekKindAt(1) == tokLParen {
		p.advance() // EXTRACT
		if err := p.expect(tokLParen); err != nil {
			return exprNode{}, err
		}
		var field string
		if p.peekKindAt(0) == tokStr {
			field = p.advance().Word
		} else {
			id, err := p.expectIdentifier()
			if err != nil {
				return exprNode{}, err
			}
			field = id
		}
		if err := p.expectKeyword("from"); err != nil {
			return exprNode{}, err
		}
		source, err := p.parseExpr()
		if err != nil {
			return exprNode{}, err
		}
		if err := p.expect(tokRParen); err != nil {
			return exprNode{}, err
		}
		return exprNode{Kind: exprExtract, Extract: &extractExpr{Field: strings.ToLower(field), Source: source}}, nil
	}
	// A typed string literal `type '...'` (grammar.md §36) — PostgreSQL's `type 'string'`, equal to
	// CAST('string' AS type) over a string-literal operand: ANY type-naming word immediately followed
	// by a string (`INTERVAL '1 day'`, `TIMESTAMP '...'`, `INTEGER '42'`, `BYTEA '\xDE'`, …).
	// Recognized only when the next token is a string — a one-token lookahead — so the word stays
	// usable as a column / function name otherwise. true/false/null are excluded (their own value
	// literals). The type name is resolved (and the string coerced to it) at resolve; unknown → 42704.
	if kw := p.peekKeyword(); kw != "" && kw != "null" && kw != "true" && kw != "false" && p.peekKindAt(1) == tokStr {
		name := p.advance().Word // the named type (original case; ScalarFromName lowercases)
		t := p.advance()
		return exprNode{Kind: exprTypedLiteral, TypeLitName: name, TypeLitText: t.Word}, nil
	}
	// The SQL/JSON query functions `JSON_EXISTS` / `JSON_VALUE` / `JSON_QUERY` (json-sql-functions.md
	// §5, S2) — keyword-led primaries with sub-clauses. Recognized by the function keyword immediately
	// followed by `(`.
	if kw := p.peekKeyword(); (kw == "json_exists" || kw == "json_value" || kw == "json_query") && p.peekKindAt(1) == tokLParen {
		p.advance() // the function keyword
		p.advance() // (
		ctx, err := p.parseExpr()
		if err != nil {
			return exprNode{}, err
		}
		// `FORMAT JSON` after the context item is accepted (and ignored — a text/json/jsonb context is
		// coerced to jsonb regardless).
		p.skipFormatJSON()
		if err := p.expect(tokComma); err != nil {
			return exprNode{}, err
		}
		path, err := p.parseExpr()
		if err != nil {
			return exprNode{}, err
		}
		// `PASSING arg AS name, …` (the path-variable surface) is the deferred S2 follow-on.
		if p.peekKeyword() == "passing" {
			return exprNode{}, newError(FeatureNotSupported, "JSON query function PASSING clause is not supported yet")
		}
		var expr exprNode
		switch kw {
		case "json_exists":
			onError, err := p.parseJSONOnErrorOnly()
			if err != nil {
				return exprNode{}, err
			}
			expr = exprNode{Kind: exprJsonExists, JsonExists: &jsonExistsExpr{Ctx: ctx, Path: path, OnError: onError}}
		case "json_value":
			returning, err := p.parseJSONReturning()
			if err != nil {
				return exprNode{}, err
			}
			onEmpty, onError, err := p.parseJSONOnClauses()
			if err != nil {
				return exprNode{}, err
			}
			expr = exprNode{Kind: exprJsonValue, JsonValue: &jsonValueExpr{Ctx: ctx, Path: path, Returning: returning, OnEmpty: onEmpty, OnError: onError}}
		default: // json_query
			returning, err := p.parseJSONReturning()
			if err != nil {
				return exprNode{}, err
			}
			wrapper, keepQuotes, err := p.parseJSONWrapperQuotes()
			if err != nil {
				return exprNode{}, err
			}
			onEmpty, onError, err := p.parseJSONOnClauses()
			if err != nil {
				return exprNode{}, err
			}
			expr = exprNode{Kind: exprJsonQuery, JsonQuery: &jsonQueryExpr{Ctx: ctx, Path: path, Returning: returning, Wrapper: wrapper, KeepQuotes: keepQuotes, OnEmpty: onEmpty, OnError: onError}}
		}
		if err := p.expect(tokRParen); err != nil {
			return exprNode{}, err
		}
		return expr, nil
	}
	// `JSON(expr [(WITH|WITHOUT) UNIQUE [KEYS]])` — the SQL/JSON JSON() constructor
	// (json-sql-functions.md §5). Distinguished from the `json '...'` typed literal (handled above, a
	// string follows) and a generic call by being the JSON keyword immediately followed by `(`.
	if p.peekKeyword() == "json" && p.peekKindAt(1) == tokLParen {
		p.advance() // JSON
		p.advance() // (
		operand, err := p.parseExpr()
		if err != nil {
			return exprNode{}, err
		}
		uniqueKeys := false
		if kw := p.peekKeyword(); (kw == "with" || kw == "without") && p.peekKeywordAt(1) == "unique" {
			p.advance() // WITH / WITHOUT
			p.advance() // UNIQUE
			if p.peekKeyword() == "keys" {
				p.advance() // KEYS (optional)
			}
			uniqueKeys = kw == "with"
		}
		if err := p.expect(tokRParen); err != nil {
			return exprNode{}, err
		}
		return exprNode{Kind: exprJsonCtor, JsonCtorOf: &jsonCtorExpr{Operand: operand, UniqueKeys: uniqueKeys}}, nil
	}
	// `COALESCE(a, b, …)` — the first-non-NULL conditional (grammar.md §51). Recognized only when
	// COALESCE is immediately followed by `(` (the JSON(/EXTRACT( one-token lookahead), so the
	// word stays usable as a column name. At least one argument (an empty list is 42601 —
	// PostgreSQL's grammar has no empty form).
	if p.peekKeyword() == "coalesce" && p.peekKindAt(1) == tokLParen {
		p.advance() // COALESCE
		p.advance() // (
		if p.peek().Kind == tokRParen {
			return exprNode{}, newError(SyntaxError, "COALESCE requires at least one argument")
		}
		var args []exprNode
		for {
			arg, err := p.parseExpr()
			if err != nil {
				return exprNode{}, err
			}
			args = append(args, arg)
			if p.peek().Kind != tokComma {
				break
			}
			p.advance() // ,
		}
		if err := p.expect(tokRParen); err != nil {
			return exprNode{}, err
		}
		return exprNode{Kind: exprCoalesce, Coalesce: args}, nil
	}
	// `GREATEST(a, b, …)` / `LEAST(a, b, …)` — the variadic max/min (grammar.md §52). Recognized
	// only when the keyword is immediately followed by `(` (the same one-token lookahead), so the
	// words stay usable as column names. At least one argument (an empty list is 42601 —
	// PostgreSQL's grammar has no empty form).
	if kw := p.peekKeyword(); (kw == "greatest" || kw == "least") && p.peekKindAt(1) == tokLParen {
		greatest := kw == "greatest"
		p.advance() // GREATEST / LEAST
		p.advance() // (
		if p.peek().Kind == tokRParen {
			if greatest {
				return exprNode{}, newError(SyntaxError, "GREATEST requires at least one argument")
			}
			return exprNode{}, newError(SyntaxError, "LEAST requires at least one argument")
		}
		var args []exprNode
		for {
			arg, err := p.parseExpr()
			if err != nil {
				return exprNode{}, err
			}
			args = append(args, arg)
			if p.peek().Kind != tokComma {
				break
			}
			p.advance() // ,
		}
		if err := p.expect(tokRParen); err != nil {
			return exprNode{}, err
		}
		return exprNode{Kind: exprGreatestLeast, GreatestLeast: args, Greatest: greatest}, nil
	}
	if p.peekKeyword() == "case" {
		p.advance()
		// Simple form has an operand between CASE and the first WHEN; the searched form starts
		// directly with WHEN (grammar.md §23).
		var operand *exprNode
		if p.peekKeyword() != "when" {
			op, err := p.parseExpr()
			if err != nil {
				return exprNode{}, err
			}
			operand = &op
		}
		var whens []caseWhen
		for p.peekKeyword() == "when" {
			p.advance()
			cond, err := p.parseExpr()
			if err != nil {
				return exprNode{}, err
			}
			if err := p.expectKeyword("then"); err != nil {
				return exprNode{}, err
			}
			res, err := p.parseExpr()
			if err != nil {
				return exprNode{}, err
			}
			whens = append(whens, caseWhen{Cond: cond, Result: res})
		}
		if len(whens) == 0 {
			return exprNode{}, newError(SyntaxError, "CASE requires at least one WHEN clause")
		}
		var els *exprNode
		if p.peekKeyword() == "else" {
			p.advance()
			e, err := p.parseExpr()
			if err != nil {
				return exprNode{}, err
			}
			els = &e
		}
		if err := p.expectKeyword("end"); err != nil {
			return exprNode{}, err
		}
		return exprNode{Kind: exprCase, Case: &caseExpr{Operand: operand, Whens: whens, Els: els}}, nil
	}
	t := p.peek()
	switch {
	case t.Kind == tokParam:
		return exprNode{Kind: exprParam, Param: p.advance().Int}, nil
	case t.Kind == tokInt:
		v, ok := foldInt(p.advance().Int, false)
		if !ok {
			// The only magnitude > MaxInt64 the lexer admits is 2^63, which fits no
			// signed integer type unless negated (handled by the unary-minus fold).
			return exprNode{}, newError(NumericValueOutOfRange,
				"value out of range: integer literal exceeds the maximum signed 64-bit value")
		}
		return exprNode{Kind: exprLiteral, Literal: &literal{Kind: literalInt, Int: v}}, nil
	case t.Kind == tokDecimal:
		p.advance()
		return exprNode{Kind: exprLiteral, Literal: &literal{
			Kind: literalDecimal, Dec: decimalFromDigitsScale(false, t.Word, uint32(t.Int)),
		}}, nil
	case t.Kind == tokStr:
		p.advance()
		return exprNode{Kind: exprLiteral, Literal: &literal{Kind: literalText, Str: t.Word}}, nil
	case t.Kind == tokWord && toLowerASCII(t.Word) == "null":
		p.advance()
		return exprNode{Kind: exprLiteral, Literal: &literal{Kind: literalNull}}, nil
	case t.Kind == tokWord && toLowerASCII(t.Word) == "true":
		p.advance()
		return exprNode{Kind: exprLiteral, Literal: &literal{Kind: literalBool, Bool: true}}, nil
	case t.Kind == tokWord && toLowerASCII(t.Word) == "false":
		p.advance()
		return exprNode{Kind: exprLiteral, Literal: &literal{Kind: literalBool, Bool: false}}, nil
	case t.Kind == tokWord && toLowerASCII(t.Word) == "current_timestamp" &&
		!(p.pos+1 < len(p.tokens) && p.tokens[p.pos+1].Kind == tokLParen):
		// `current_timestamp` — the SQL-standard bare keyword (no parens), reserved like the value
		// literals above. Pure sugar: desugar to a `now()` call so resolution / execution / cost /
		// volatility are entirely shared (spec/design/functions.md §12). Not fired when followed by
		// `(` (a precision typmod, deferred) so that form resolves normally (42883).
		p.advance()
		return exprNode{Kind: exprFuncCall, FuncCall: &funcCallExpr{Name: "now"}}, nil
	case t.Kind == tokWord && toLowerASCII(t.Word) == "current_date" &&
		!(p.pos+1 < len(p.tokens) && p.tokens[p.pos+1].Kind == tokLParen):
		// `current_date` — the SQL-standard bare keyword, desugared to the current_date() catalog
		// function (functions.md §12, date.md §6). Unlike current_timestamp there is no typmod
		// form; a following `(` is the explicit call spelling, which jed also resolves (PG rejects
		// it as a syntax error — a documented jed-lenient divergence, catalog.toml).
		p.advance()
		return exprNode{Kind: exprFuncCall, FuncCall: &funcCallExpr{Name: "current_date"}}, nil
	case t.Kind == tokWord:
		// Function call: a BARE identifier IMMEDIATELY followed by "(" is a call (the engine's
		// first call syntax — grammar.md §17). The one-token lookahead keeps function names
		// non-reserved (a column may be named `count`); a qualified name is never a call. Only
		// aggregates resolve (42883 otherwise).
		if p.pos+1 < len(p.tokens) && p.tokens[p.pos+1].Kind == tokLParen {
			return p.parseFunctionCall()
		}
		qualifier, name, err := p.parseColumnRef()
		if err != nil {
			return exprNode{}, err
		}
		if qualifier != "" {
			return exprNode{Kind: exprQualifiedColumn, Qualifier: qualifier, Column: name}, nil
		}
		return exprNode{Kind: exprColumn, Column: name}, nil
	default:
		return exprNode{}, newError(SyntaxError, "expected an expression")
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
func (p *parser) parseFunctionCall() (exprNode, error) {
	name, err := p.expectIdentifier()
	if err != nil {
		return exprNode{}, err
	}
	if err := p.expect(tokLParen); err != nil {
		return exprNode{}, err
	}
	fc := &funcCallExpr{Name: name}
	// A leading DISTINCT (`COUNT(DISTINCT x)`, aggregates.md §5) folds only the distinct argument
	// values. It is not reserved, but here — right after `(` — it is always the modifier.
	// `DISTINCT *` and `DISTINCT )` (no argument) are both 42601 syntax errors (PG); the resolver
	// rejects DISTINCT on a non-aggregate (42809) or a window function (0A000).
	if p.peekKeyword() == "distinct" {
		p.advance()
		if p.peek().Kind == tokStar {
			return exprNode{}, newError(SyntaxError, "DISTINCT cannot be used with *")
		}
		if p.peek().Kind == tokRParen {
			return exprNode{}, newError(SyntaxError, "DISTINCT requires an aggregate argument")
		}
		fc.Distinct = true
	}
	anyNamed := false
	switch {
	case p.peek().Kind == tokStar:
		p.advance()
		fc.Star = true
	case p.peek().Kind == tokRParen:
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
					return exprNode{}, err
				}
				fc.Args = append(fc.Args, &arg)
				names = append(names, nil)
				// A VARIADIC argument must be the last (PostgreSQL, 42601).
				if p.peek().Kind == tokComma {
					return exprNode{}, newError(SyntaxError, "VARIADIC argument must be the last argument")
				}
				break
			}
			// A named argument is `identifier "=>" expr` (grammar.md §17); a two-token lookahead
			// (word then "=>") distinguishes it from a bare expr that starts with an identifier.
			var argName *string
			if p.peek().Kind == tokWord && p.peekKindAt(1) == tokFatArrow {
				nm, err := p.expectIdentifier()
				if err != nil {
					return exprNode{}, err
				}
				if err := p.expect(tokFatArrow); err != nil {
					return exprNode{}, err
				}
				anyNamed = true
				argName = &nm
			} else if anyNamed {
				// A positional argument may not follow a named one (PostgreSQL, 42601).
				return exprNode{}, newError(SyntaxError, "positional argument cannot follow named argument")
			}
			arg, err := p.parseExpr()
			if err != nil {
				return exprNode{}, err
			}
			fc.Args = append(fc.Args, &arg)
			names = append(names, argName)
			if p.peek().Kind != tokComma {
				break
			}
			p.advance()
		}
		// Keep ArgNames nil unless a name appeared (the all-positional sentinel — §8).
		if anyNamed {
			fc.ArgNames = names
		}
	}
	if err := p.expect(tokRParen); err != nil {
		return exprNode{}, err
	}
	// A trailing WITHIN GROUP (ORDER BY <key>) marks an ordered-set aggregate (mode /
	// percentile_cont / percentile_disc — aggregates.md §13). It comes between the argument list and
	// any FILTER / OVER (PG order). WITHIN/GROUP are not reserved; right after the call's `)` they are
	// always the clause. The order key is a general expression (`ORDER BY a + b`) classified with
	// allowOrdinal OFF, so a bare integer is a constant (not an ordinal), matching PostgreSQL; the
	// resolver enforces exactly one key (42883) and the per-name rules.
	if p.peekKeyword() == "within" {
		p.advance()
		if err := p.expectKeyword("group"); err != nil {
			return exprNode{}, err
		}
		if err := p.expect(tokLParen); err != nil {
			return exprNode{}, err
		}
		if p.peekKeyword() != "order" {
			return exprNode{}, newError(SyntaxError, "WITHIN GROUP requires an ORDER BY clause")
		}
		p.advance()
		if err := p.expectKeyword("by"); err != nil {
			return exprNode{}, err
		}
		keys := []orderKey{}
		for {
			expr, err := p.parseExpr()
			if err != nil {
				return exprNode{}, err
			}
			collation, descending, nullsFirst, err := p.parseSortSuffix()
			if err != nil {
				return exprNode{}, err
			}
			keys = append(keys, classifyOrderKey(expr, collation, descending, nullsFirst, false))
			if p.peek().Kind == tokComma {
				p.advance()
				continue
			}
			break
		}
		if err := p.expect(tokRParen); err != nil {
			return exprNode{}, err
		}
		fc.WithinGroup = keys
	}
	// A trailing FILTER (WHERE cond) restricts which input rows feed THIS aggregate
	// (aggregates.md §11). PG syntax: `agg(args) FILTER (WHERE cond) [OVER (...)]` — FILTER binds to
	// the aggregate and precedes any OVER. FILTER is not reserved, but right after the call's `)` it
	// is always the modifier (PG: `count(*) filter` with no `(` is a syntax error, not an alias). The
	// resolver rejects FILTER on a non-aggregate (42809) or a window function (0A000), an aggregate
	// inside cond (42803), and a non-boolean cond (42804).
	if p.peekKeyword() == "filter" {
		p.advance()
		if err := p.expect(tokLParen); err != nil {
			return exprNode{}, err
		}
		if p.peekKeyword() != "where" {
			return exprNode{}, newError(SyntaxError, "FILTER requires a WHERE clause")
		}
		p.advance()
		cond, err := p.parseExpr()
		if err != nil {
			return exprNode{}, err
		}
		if err := p.expect(tokRParen); err != nil {
			return exprNode{}, err
		}
		fc.Filter = &cond
	}
	// A trailing OVER (...) turns the call into a window-function call (spec/design/window.md,
	// grammar.ebnf `over_clause`). The inline OVER ( [PARTITION BY cols] [ORDER BY ...] ) form is
	// parsed here; a named window `OVER name` (the WINDOW clause — window.md §5) sets OverName and
	// returns early, desugared to its definition (into Over) before resolution.
	if p.peekKeyword() == "over" {
		p.advance()
		// `OVER name` references a named window (the WINDOW clause — window.md §5); `OVER (...)`
		// is an inline definition. A named reference is desugared to its definition at resolve.
		if p.peek().Kind != tokLParen {
			oname, err := p.expectIdentifier()
			if err != nil {
				return exprNode{}, err
			}
			fc.OverName = oname
			return exprNode{Kind: exprFuncCall, FuncCall: fc}, nil
		}
		if err := p.expect(tokLParen); err != nil {
			return exprNode{}, err
		}
		// `[base] [PARTITION BY cols] [ORDER BY …] [frame]` — the shared definition body. A leading
		// base-window name (window.md §5) extends a named window; merged at resolve.
		def, err := p.parseWindowDefinition()
		if err != nil {
			return exprNode{}, err
		}
		if err := p.expect(tokRParen); err != nil {
			return exprNode{}, err
		}
		fc.Over = &def
	}
	return exprNode{Kind: exprFuncCall, FuncCall: fc}, nil
}

// parseWindowFrame parses an optional window frame clause `{ROWS|RANGE|GROUPS} frame_extent
// [EXCLUDE …]` (spec/design/window.md §6, grammar.ebnf `frame_clause`). A single bound is the
// START (END = CURRENT ROW). EXCLUDE is rejected 0A000 in S4. Returns nil when no frame keyword
// is present (the default frame).
func (p *parser) parseWindowFrame() (*windowFrame, error) {
	var mode frameMode
	switch p.peekKeyword() {
	case "rows":
		mode = frameRows
	case "range":
		mode = frameRange
	case "groups":
		mode = frameGroups
	default:
		return nil, nil
	}
	p.advance()
	var start, end frameBound
	if p.peekKeyword() == "between" {
		p.advance()
		s, err := p.parseFrameBound()
		if err != nil {
			return nil, err
		}
		if err := p.expectKeyword("and"); err != nil {
			return nil, err
		}
		e, err := p.parseFrameBound()
		if err != nil {
			return nil, err
		}
		start, end = s, e
	} else {
		// A single bound is the frame START; the END defaults to CURRENT ROW.
		s, err := p.parseFrameBound()
		if err != nil {
			return nil, err
		}
		start, end = s, frameBound{Kind: frameCurrentRow}
	}
	exclude, err := p.parseFrameExclusion()
	if err != nil {
		return nil, err
	}
	return &windowFrame{Mode: mode, Start: start, End: end, Exclude: exclude}, nil
}

// parseFrameExclusion parses an optional `EXCLUDE { CURRENT ROW | GROUP | TIES | NO OTHERS }` clause
// (spec/design/window.md §6); absent → FrameExcludeNoOthers (drop nothing).
func (p *parser) parseFrameExclusion() (frameExclusion, error) {
	if p.peekKeyword() != "exclude" {
		return frameExcludeNoOthers, nil
	}
	p.advance()
	switch p.peekKeyword() {
	case "current":
		p.advance()
		if err := p.expectKeyword("row"); err != nil {
			return 0, err
		}
		return frameExcludeCurrentRow, nil
	case "group":
		p.advance()
		return frameExcludeGroup, nil
	case "ties":
		p.advance()
		return frameExcludeTies, nil
	case "no":
		p.advance()
		if err := p.expectKeyword("others"); err != nil {
			return 0, err
		}
		return frameExcludeNoOthers, nil
	default:
		return 0, newError(SyntaxError, "expected CURRENT ROW, GROUP, TIES, or NO OTHERS after EXCLUDE")
	}
}

// parseFrameBound parses one frame bound: `UNBOUNDED PRECEDING|FOLLOWING`, `CURRENT ROW`, or
// `expr PRECEDING|FOLLOWING` (spec/design/window.md §6).
func (p *parser) parseFrameBound() (frameBound, error) {
	switch p.peekKeyword() {
	case "unbounded":
		p.advance()
		switch p.peekKeyword() {
		case "preceding":
			p.advance()
			return frameBound{Kind: frameUnboundedPreceding}, nil
		case "following":
			p.advance()
			return frameBound{Kind: frameUnboundedFollowing}, nil
		default:
			return frameBound{}, newError(SyntaxError, "expected PRECEDING or FOLLOWING after UNBOUNDED")
		}
	case "current":
		p.advance()
		if err := p.expectKeyword("row"); err != nil {
			return frameBound{}, err
		}
		return frameBound{Kind: frameCurrentRow}, nil
	default:
		e, err := p.parseExpr()
		if err != nil {
			return frameBound{}, err
		}
		switch p.peekKeyword() {
		case "preceding":
			p.advance()
			return frameBound{Kind: framePreceding, Offset: e}, nil
		case "following":
			p.advance()
			return frameBound{Kind: frameFollowing, Offset: e}, nil
		default:
			return frameBound{}, newError(SyntaxError, "expected PRECEDING or FOLLOWING in frame bound")
		}
	}
}

// parseWindowOrderBy parses an OVER clause's optional `ORDER BY <key> ("," <key>)*` and returns the
// keys (nil when absent). Unlike the query parseOrderBy (column references only), each key is a
// general expression (`ORDER BY a + b`, `ORDER BY sum(x)`) followed by the shared sort suffix. A
// COLLATE binds tighter than the comparison/arithmetic that could appear in a key, so parseExpr
// already absorbs an inline `expr COLLATE "x"`; the trailing COLLATE here is the sort-key collation
// (the same two-level reading the query ORDER BY uses on a bare column). spec/design/window.md §5.1.
func (p *parser) parseWindowOrderBy() ([]windowOrderKey, error) {
	if p.peekKeyword() != "order" {
		return nil, nil
	}
	p.advance()
	if err := p.expectKeyword("by"); err != nil {
		return nil, err
	}
	var order []windowOrderKey
	for {
		expr, err := p.parseExpr()
		if err != nil {
			return nil, err
		}
		collation, descending, nullsFirst, err := p.parseSortSuffix()
		if err != nil {
			return nil, err
		}
		order = append(order, windowOrderKey{Expr: expr, Collation: collation, Descending: descending, NullsFirst: nullsFirst})
		if p.peek().Kind != tokComma {
			break
		}
		p.advance()
	}
	return order, nil
}

// parseColumnRef parses `column_ref ::= identifier ("." identifier)?` — a bare column name, or
// a qualified `rel.col` (the "." is TokDot). Returns (qualifier, name); qualifier is "" for a
// bare column (spec/grammar/grammar.ebnf `column_ref`, grammar.md §15).
func (p *parser) parseColumnRef() (string, string, error) {
	first, err := p.expectIdentifier()
	if err != nil {
		return "", "", err
	}
	if p.peek().Kind == tokDot {
		p.advance()
		second, err := p.expectIdentifier()
		if err != nil {
			return "", "", err
		}
		return first, second, nil
	}
	return "", first, nil
}

// parseQualifiedTableName parses `qualified_table ::= (identifier ".")? identifier` in DML-target
// position (attached-databases.md §3): an optional database qualifier followed by the table name.
// Returns (db, name) where db is nil for a bare (implicit-scope) name. The FROM-position analogue is
// inlined in parseTableRef (which must also disambiguate the function / derived-table forms).
func (p *parser) parseQualifiedTableName() (*string, string, error) {
	name, err := p.expectIdentifier()
	if err != nil {
		return nil, "", err
	}
	if p.peek().Kind == tokDot {
		p.advance() // .
		tbl, err := p.expectIdentifier()
		if err != nil {
			return nil, "", err
		}
		q := name
		return &q, tbl, nil
	}
	return nil, name, nil
}

// peek returns the current token without consuming it.
func (p *parser) peek() token { return p.tokens[p.pos] }

// peekKeyword returns the current token lowercased if it is a word, else "".
func (p *parser) peekKeyword() string {
	t := p.peek()
	if t.Kind == tokWord {
		return toLowerASCII(t.Word)
	}
	return ""
}

// peekKeywordAt returns the keyword (lowercased) offset tokens ahead of the cursor if that
// token is a word, else "". Used for the two-token NOT IN/BETWEEN/LIKE lookahead (a
// CLAUDE.md §8 determinism surface — byte-identical across the three parsers).
func (p *parser) peekKeywordAt(offset int) string {
	if p.pos+offset < len(p.tokens) {
		if t := p.tokens[p.pos+offset]; t.Kind == tokWord {
			return toLowerASCII(t.Word)
		}
	}
	return ""
}

// peekKindAt returns the token kind offset tokens ahead of the cursor, or TokEof past the end.
// Used by the EXISTS / scalar-subquery lookahead (grammar.md §26).
func (p *parser) peekKindAt(offset int) tokenKind {
	if p.pos+offset < len(p.tokens) {
		return p.tokens[p.pos+offset].Kind
	}
	return tokEof
}

// isWithClauseAtOffset reports whether a WITH clause (`WITH RECURSIVE …`, `WITH <name> ( …`, or
// `WITH <name> AS …`) begins at p.pos+offset (spec/design/cte.md §7), as opposed to an ordinary
// expression or a column named `with`. The shape-based lookahead keeps the recognition unambiguous
// even where `with` is a legal identifier (e.g. `x IN (with)` is a value list, not a nested WITH).
func (p *parser) isWithClauseAtOffset(offset int) bool {
	if p.peekKeywordAt(offset) != "with" {
		return false
	}
	if p.peekKeywordAt(offset+1) == "recursive" {
		return true
	}
	if p.peekKindAt(offset+1) == tokWord {
		return p.peekKindAt(offset+2) == tokLParen || p.peekKeywordAt(offset+2) == "as"
	}
	return false
}

// isQueryStartAtOffset reports whether a query expression — a SELECT or a nested WITH clause
// (cte.md §7) — begins at p.pos+offset. The §26 leading-SELECT lookahead, extended with WITH.
func (p *parser) isQueryStartAtOffset(offset int) bool {
	return p.peekKeywordAt(offset) == "select" || p.isWithClauseAtOffset(offset)
}

// atSubqueryStart reports whether the NEXT token begins a query expression (a SELECT or nested
// WITH) — the disambiguator at every subquery position.
func (p *parser) atSubqueryStart() bool { return p.isQueryStartAtOffset(0) }

// atWithClause reports whether the NEXT token begins a nested WITH clause (cte.md §7).
func (p *parser) atWithClause() bool { return p.isWithClauseAtOffset(0) }

// advance consumes and returns the current token.
func (p *parser) advance() token {
	t := p.tokens[p.pos]
	if p.pos+1 < len(p.tokens) {
		p.pos++
	}
	return t
}

// expect consumes the current token, requiring its kind to equal want.
func (p *parser) expect(want tokenKind) error {
	if got := p.advance(); got.Kind != want {
		return newError(SyntaxError, "unexpected token")
	}
	return nil
}

// expectKeyword consumes the current token, requiring it to be the given keyword
// (case-insensitive).
func (p *parser) expectKeyword(kw string) error {
	t := p.advance()
	if t.Kind == tokWord && toLowerASCII(t.Word) == kw {
		return nil
	}
	return newError(SyntaxError, fmt.Sprintf("expected keyword '%s'", kw))
}

// expectIdentifier consumes the current token, requiring it to be a bare word.
func (p *parser) expectIdentifier() (string, error) {
	t := p.advance()
	if t.Kind != tokWord {
		return "", newError(SyntaxError, "expected an identifier")
	}
	return t.Word, nil
}

// expectCollationName consumes a quoted collation name after COLLATE (spec/design/collation.md §1).
// The name is a double-quoted identifier — case-sensitive and kept verbatim ("C", "en-US") — so a
// bare word is not accepted (it would case-fold). An empty name ("") is a 42601 syntax error.
func (p *parser) expectCollationName() (string, error) {
	t := p.advance()
	if t.Kind != tokQuotedIdent {
		return "", newError(SyntaxError, "expected a quoted collation name after COLLATE")
	}
	if t.Word == "" {
		return "", newError(SyntaxError, "collation name may not be empty")
	}
	return t.Word, nil
}

// expectEof requires that all input has been consumed.
func (p *parser) expectEof() error {
	if p.peek().Kind != tokEof {
		return newError(SyntaxError, "unexpected trailing input")
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
func parseExpression(text string) (exprNode, error) {
	tokens, err := lex(text)
	if err != nil {
		return exprNode{}, err
	}
	p := &parser{tokens: tokens}
	expr, err := p.parseExpr()
	if err != nil {
		return exprNode{}, err
	}
	if err := p.expectEof(); err != nil {
		return exprNode{}, err
	}
	return expr, nil
}

// renderTokens re-renders a token slice as the persisted check-expression text: each token
// rendered by the closed table in spec/fileformat/format.md "Check-expression text", joined
// with single spaces. A byte contract — identical across every core (CLAUDE.md §8).
func renderTokens(tokens []token) string {
	parts := make([]string, len(tokens))
	for i, t := range tokens {
		parts[i] = renderToken(t)
	}
	return strings.Join(parts, " ")
}

// rewriteColumnIdentifier rewrites references to one table column in persisted expression text,
// leaving function/type/named-argument identifiers and unrelated composite fields untouched.
func rewriteColumnIdentifier(text, table, old, next string) (string, exprNode, error) {
	tokens, err := lex(text)
	if err != nil {
		return "", exprNode{}, err
	}
	for i := range tokens {
		if tokens[i].Kind != tokWord || !strings.EqualFold(tokens[i].Word, old) {
			continue
		}
		var prev, prev2, after token
		if i > 0 {
			prev = tokens[i-1]
		}
		if i > 1 {
			prev2 = tokens[i-2]
		}
		if i+1 < len(tokens) {
			after = tokens[i+1]
		}
		skip := after.Kind == tokLParen || after.Kind == tokFatArrow || after.Kind == tokStr ||
			prev.Kind == tokDoubleColon || (prev.Kind == tokWord && strings.EqualFold(prev.Word, "as")) ||
			(prev.Kind == tokDot && !(prev2.Kind == tokWord && strings.EqualFold(prev2.Word, table))) ||
			(after.Kind == tokDot && strings.EqualFold(old, table))
		if !skip {
			tokens[i].Word = next
		}
	}
	if len(tokens) > 0 && tokens[len(tokens)-1].Kind == tokEof {
		tokens = tokens[:len(tokens)-1]
	}
	rewritten := renderTokens(tokens)
	expr, err := parseExpression(rewritten)
	return rewritten, expr, err
}

func rewriteTableQualifier(text, old, next string) (string, exprNode, error) {
	tokens, err := lex(text)
	if err != nil {
		return "", exprNode{}, err
	}
	for i := 0; i+1 < len(tokens); i++ {
		if tokens[i].Kind == tokWord && strings.EqualFold(tokens[i].Word, old) && tokens[i+1].Kind == tokDot {
			tokens[i].Word = next
		}
	}
	if len(tokens) > 0 && tokens[len(tokens)-1].Kind == tokEof {
		tokens = tokens[:len(tokens)-1]
	}
	rewritten := renderTokens(tokens)
	expr, err := parseExpression(rewritten)
	return rewritten, expr, err
}

func renderToken(t token) string {
	switch t.Kind {
	case tokWord:
		return t.Word
	case tokInt:
		return strconv.FormatUint(t.Int, 10)
	case tokDecimal:
		// The digit string with '.' inserted `scale` digits from the right. The lexer
		// guarantees scale <= len(coeff) (every fractional digit is in the coefficient), so
		// the insertion point is in range; scale == len renders a leading-dot form (".5")
		// and scale == 0 a trailing-dot form ("1."), both of which re-lex as the same
		// decimal value (spec/fileformat/format.md "Check-expression text").
		split := len(t.Word) - int(t.Int)
		return t.Word[:split] + "." + t.Word[split:]
	case tokStr:
		return "'" + strings.ReplaceAll(t.Word, "'", "''") + "'"
	case tokQuotedIdent:
		// A double-quoted identifier round-trips verbatim with `"` doubled (collation names in a
		// persisted COLLATE expression, spec/design/collation.md §1).
		return "\"" + strings.ReplaceAll(t.Word, "\"", "\"\"") + "\""
	case tokParam:
		return "$" + strconv.FormatUint(t.Int, 10)
	case tokComma:
		return ","
	case tokDot:
		return "."
	case tokLParen:
		return "("
	case tokRParen:
		return ")"
	case tokLBracket:
		return "["
	case tokRBracket:
		return "]"
	case tokStar:
		return "*"
	case tokPlus:
		return "+"
	case tokMinus:
		return "-"
	case tokSlash:
		return "/"
	case tokPercent:
		return "%"
	case tokEq:
		return "="
	case tokNe:
		return "<>"
	case tokLt:
		return "<"
	case tokGt:
		return ">"
	case tokLe:
		return "<="
	case tokGe:
		return ">="
	case tokFatArrow:
		return "=>"
	case tokColon:
		return ":"
	case tokConcat:
		return "||"
	case tokContains:
		return "@>"
	case tokJsonPathExists:
		return "@?"
	case tokJsonPathMatch:
		return "@@"
	case tokContainedBy:
		return "<@"
	case tokOverlaps:
		return "&&"
	case tokStrictlyLeft:
		return "<<"
	case tokStrictlyRight:
		return ">>"
	case tokNotExtendRight:
		return "&<"
	case tokNotExtendLeft:
		return "&>"
	case tokAdjacent:
		return "-|-"
	case tokArrow:
		return "->"
	case tokArrowText:
		return "->>"
	case tokHashArrow:
		return "#>"
	case tokHashArrowText:
		return "#>>"
	case tokQuestion:
		return "?"
	case tokQuestionPipe:
		return "?|"
	case tokQuestionAmp:
		return "?&"
	case tokHashMinus:
		return "#-"
	case tokTilde:
		return "~"
	case tokTildeStar:
		return "~*"
	case tokBangTilde:
		return "!~"
	case tokBangTildeStar:
		return "!~*"
	default: // TokEof — never inside the parentheses
		return ""
	}
}
