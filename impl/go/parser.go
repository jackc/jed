package jed

import (
	"fmt"
	"math"
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
type Parser struct {
	tokens []Token
	pos    int
}

// NewParser builds a parser over the given tokens.
func NewParser(tokens []Token) *Parser {
	return &Parser{tokens: tokens}
}

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
	case "create":
		ct, err := p.parseCreateTable()
		if err != nil {
			return Statement{}, err
		}
		return Statement{CreateTable: ct}, nil
	case "drop":
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
	case "":
		return Statement{}, NewError(SyntaxError, "expected a SQL statement")
	default:
		return Statement{}, NewError(SyntaxError, fmt.Sprintf("unexpected keyword '%s'", p.peekKeyword()))
	}
}

// parseCreateTable parses `CREATE TABLE <name> ( <coldef> [, <coldef>]* )`, where
// each <coldef> is `<name> <typename> [PRIMARY KEY]`. Type names are kept as written
// and resolved during execution (the catalog owns the type lattice).
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
	for {
		col, err := p.parseColumnDef()
		if err != nil {
			return nil, err
		}
		columns = append(columns, col)
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
	return &CreateTable{Name: name, Columns: columns}, nil
}

func (p *Parser) parseColumnDef() (ColumnDef, error) {
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
	// Zero or more order-free column constraints: PRIMARY KEY, NOT NULL, and DEFAULT <literal>.
	// A boolean constraint may be repeated harmlessly; a repeated DEFAULT keeps the last.
	primaryKey := false
	notNull := false
	var def *Literal
	for {
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
			lit, err := p.parseLiteral()
			if err != nil {
				return ColumnDef{}, err
			}
			def = &lit
		default:
			return ColumnDef{Name: name, TypeName: typeName, TypeMod: typeMod, PrimaryKey: primaryKey, NotNull: notNull, Default: def}, nil
		}
	}
}

// parseTypeMod parses an optional parenthesized type modifier "(" integer ("," integer)? ")"
// after a type name (the first parameterized type, decimal — spec/grammar/grammar.ebnf
// type_name). The shape is accepted for any type name; whether a typmod is meaningful (decimal
// only) and in range is decided at resolve. Empty parens or a non-integer inside is 42601.
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
		return &Insert{Table: table, Columns: columns, Select: sel}, nil
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
	return &Insert{Table: table, Columns: columns, Rows: rows}, nil
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
// bind parameter ($N, bound at execute — spec/design/api.md §5), else a literal.
func (p *Parser) parseInsertValue() (InsertValue, error) {
	if p.peekKeyword() == "default" {
		p.advance()
		return InsertValue{IsDefault: true}, nil
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

// parseSetExpr parses the lower-precedence, left-associative UNION/EXCEPT level. INTERSECT binds
// tighter (parsed inside parseIntersectExpr), so `a UNION b INTERSECT c` becomes
// `a UNION (b INTERSECT c)`.
func (p *Parser) parseSetExpr() (QueryExpr, error) {
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
			return left, nil
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
	core, err := p.parseSelectCore()
	if err != nil {
		return QueryExpr{}, err
	}
	left := QueryExpr{Select: core}
	for p.peekKeyword() == "intersect" {
		p.advance() // INTERSECT
		all := p.parseSetOpQuantifier()
		right, err := p.parseSelectCore()
		if err != nil {
			return QueryExpr{}, err
		}
		left = QueryExpr{SetOp: &SetOp{Op: SetOpIntersect, All: all, Lhs: left, Rhs: QueryExpr{Select: right}}}
	}
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
	if err := p.expectKeyword("from"); err != nil {
		return nil, err
	}
	from, joins, err := p.parseFromClause()
	if err != nil {
		return nil, err
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

// parseTableRef parses `table_ref ::= identifier ("AS"? identifier)?` — a table name with an
// optional alias. An explicit AS takes the next identifier unconditionally; an implicit alias
// is taken only when the next token is a word that is NOT a clause/join keyword (so `FROM t
// WHERE` and `FROM t JOIN ...` keep no alias). The stop-keyword set is a §8 cross-core surface.
func (p *Parser) parseTableRef() (TableRef, error) {
	name, err := p.expectIdentifier()
	if err != nil {
		return TableRef{}, err
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
	return TableRef{Name: name, Alias: alias}, nil
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
		"union", "intersect", "except":
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
	return &Update{Table: table, Assignments: assignments, Filter: filter}, nil
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
	return &Delete{Table: table, Filter: filter}, nil
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
func (p *Parser) parseExpr() (Expr, error) { return p.parseOr() }

func (p *Parser) parseOr() (Expr, error) {
	lhs, err := p.parseAnd()
	if err != nil {
		return Expr{}, err
	}
	for p.peekKeyword() == "or" {
		p.advance()
		rhs, err := p.parseAnd()
		if err != nil {
			return Expr{}, err
		}
		lhs = binaryExpr(OpOr, lhs, rhs)
	}
	return lhs, nil
}

func (p *Parser) parseAnd() (Expr, error) {
	lhs, err := p.parseNot()
	if err != nil {
		return Expr{}, err
	}
	for p.peekKeyword() == "and" {
		p.advance()
		rhs, err := p.parseNot()
		if err != nil {
			return Expr{}, err
		}
		lhs = binaryExpr(OpAnd, lhs, rhs)
	}
	return lhs, nil
}

func (p *Parser) parseNot() (Expr, error) {
	if p.peekKeyword() == "not" {
		p.advance()
		operand, err := p.parseNot() // right-associative: NOT NOT x
		if err != nil {
			return Expr{}, err
		}
		return Expr{Kind: ExprUnary, Unary: &UnaryExpr{Op: OpNot, Operand: operand}}, nil
	}
	return p.parseComparison()
}

// parseComparison parses one comparison, a postfix IS [NOT] NULL, or
// IS [NOT] DISTINCT FROM, all non-associative: `a = b = c` is a syntax error, and
// `a + 1 IS NULL` binds as `(a + 1) IS NULL`. After the shared `IS` `NOT`? it dispatches
// on the NULL vs DISTINCT FROM keyword (spec/grammar/grammar.ebnf `comparison`).
func (p *Parser) parseComparison() (Expr, error) {
	lhs, err := p.parseAdditive()
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
		// IS [NOT] DISTINCT FROM <additive> — NULL-safe equality; else IS [NOT] NULL.
		if p.peekKeyword() == "distinct" {
			p.advance()
			if err := p.expectKeyword("from"); err != nil {
				return Expr{}, err
			}
			rhs, err := p.parseAdditive()
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
		// A non-empty value list (`IN ()` — parseAdditive on `)` is a 42601 syntax error).
		first, err := p.parseAdditive()
		if err != nil {
			return Expr{}, err
		}
		list := []Expr{first}
		for p.peek().Kind == TokComma {
			p.advance()
			elem, err := p.parseAdditive()
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
		// Both bounds parse at the ADDITIVE level, which never consumes `AND` (a looser level
		// owned by parseAnd). So the BETWEEN's structural `AND` is matched here and
		// `x BETWEEN a AND b AND c` parses as `(x BETWEEN a AND b) AND c` (grammar.md §21).
		lo, err := p.parseAdditive()
		if err != nil {
			return Expr{}, err
		}
		if err := p.expectKeyword("and"); err != nil {
			return Expr{}, err
		}
		hi, err := p.parseAdditive()
		if err != nil {
			return Expr{}, err
		}
		return Expr{Kind: ExprBetween, Between: &BetweenExpr{Lhs: lhs, Lo: lo, Hi: hi, Negated: predNegated}}, nil
	}
	if p.peekKeyword() == "like" {
		p.advance()
		rhs, err := p.parseAdditive()
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
	rhs, err := p.parseAdditive()
	if err != nil {
		return Expr{}, err
	}
	return binaryExpr(op, lhs, rhs), nil
}

func (p *Parser) parseAdditive() (Expr, error) {
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
			return lhs, nil
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
			return lhs, nil
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
		// minimum is representable and the literal range-checks against context.
		if p.peek().Kind == TokInt {
			v, ok := foldInt(p.advance().Int, true)
			if !ok {
				return Expr{}, NewError(NumericValueOutOfRange,
					"value out of range: integer literal exceeds the maximum signed 64-bit value")
			}
			return Expr{Kind: ExprLiteral, Literal: &Literal{Kind: LiteralInt, Int: v}}, nil
		}
		// Fold unary-minus of a decimal literal into one negative decimal literal (decimal
		// negation never overflows).
		if p.peek().Kind == TokDecimal {
			t := p.advance()
			return Expr{Kind: ExprLiteral, Literal: &Literal{
				Kind: LiteralDecimal, Dec: DecimalFromDigitsScale(true, t.Word, uint32(t.Int)),
			}}, nil
		}
		operand, err := p.parseUnary()
		if err != nil {
			return Expr{}, err
		}
		return Expr{Kind: ExprUnary, Unary: &UnaryExpr{Op: OpNeg, Operand: operand}}, nil
	}
	return p.parsePrimary()
}

// parsePrimary parses a parenthesized expression, CAST(...), a literal (integer,
// TRUE/FALSE, NULL), or a column reference.
func (p *Parser) parsePrimary() (Expr, error) {
	if p.peek().Kind == TokLParen {
		p.advance()
		e, err := p.parseExpr()
		if err != nil {
			return Expr{}, err
		}
		if err := p.expect(TokRParen); err != nil {
			return Expr{}, err
		}
		return e, nil
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
		if err := p.expect(TokRParen); err != nil {
			return Expr{}, err
		}
		return Expr{Kind: ExprCast, Cast: &CastExpr{Inner: inner, TypeName: typeName, TypeMod: typeMod}}, nil
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

// parseFunctionCall parses `function_call ::= identifier "(" ( "*" | expr ("," expr)* ) ")"` —
// the shared aggregate/scalar call syntax (grammar.md §17). COUNT(*) is the star form; every
// other call takes a comma-separated argument list (resolution checks the per-function arity).
// DISTINCT inside the parens is deferred (rejected 42601).
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
	if p.peek().Kind == TokStar {
		p.advance()
		fc.Star = true
	} else {
		for {
			arg, err := p.parseExpr()
			if err != nil {
				return Expr{}, err
			}
			fc.Args = append(fc.Args, &arg)
			if p.peek().Kind != TokComma {
				break
			}
			p.advance()
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
