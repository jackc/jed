package abide

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
	case "insert":
		ins, err := p.parseInsert()
		if err != nil {
			return Statement{}, err
		}
		return Statement{Insert: ins}, nil
	case "select":
		sel, err := p.parseSelect()
		if err != nil {
			return Statement{}, err
		}
		return Statement{Select: sel}, nil
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
	primaryKey := false
	if p.peekKeyword() == "primary" {
		p.advance()
		if err := p.expectKeyword("key"); err != nil {
			return ColumnDef{}, err
		}
		primaryKey = true
	}
	return ColumnDef{Name: name, TypeName: typeName, PrimaryKey: primaryKey}, nil
}

// parseInsert parses `INSERT INTO <table> VALUES ( <literal> [, <literal>]* )`.
// Values map positionally to columns; the executor type-checks against the catalog.
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
	if err := p.expectKeyword("values"); err != nil {
		return nil, err
	}
	if err := p.expect(TokLParen); err != nil {
		return nil, err
	}

	var values []Literal
	for {
		lit, err := p.parseLiteral()
		if err != nil {
			return nil, err
		}
		values = append(values, lit)
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
		return nil, NewError(SyntaxError, "VALUES must have at least one value")
	}
	return &Insert{Table: table, Values: values}, nil
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
// `SELECT <items> FROM <table> [WHERE <predicate>] [ORDER BY <col> [ASC|DESC]]`,
// where <items> is `*` or a comma-separated list of column refs / CASTs.
func (p *Parser) parseSelect() (*Select, error) {
	if err := p.expectKeyword("select"); err != nil {
		return nil, err
	}
	items, err := p.parseSelectItems()
	if err != nil {
		return nil, err
	}
	if err := p.expectKeyword("from"); err != nil {
		return nil, err
	}
	from, err := p.expectIdentifier()
	if err != nil {
		return nil, err
	}

	sel := &Select{Items: items, From: from}

	filter, err := p.parseOptionalWhere()
	if err != nil {
		return nil, err
	}
	sel.Filter = filter

	if p.peekKeyword() == "order" {
		p.advance()
		if err := p.expectKeyword("by"); err != nil {
			return nil, err
		}
		col, err := p.expectIdentifier()
		if err != nil {
			return nil, err
		}
		descending := false
		switch p.peekKeyword() {
		case "asc":
			p.advance()
		case "desc":
			p.advance()
			descending = true
		}
		sel.OrderBy = &OrderBy{Column: col, Descending: descending}
	}

	return sel, nil
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
		if err := p.expect(TokRParen); err != nil {
			return Expr{}, err
		}
		return Expr{Kind: ExprCast, Cast: &CastExpr{Inner: inner, TypeName: typeName}}, nil
	}
	t := p.peek()
	switch {
	case t.Kind == TokInt:
		v, ok := foldInt(p.advance().Int, false)
		if !ok {
			// The only magnitude > MaxInt64 the lexer admits is 2^63, which fits no
			// signed integer type unless negated (handled by the unary-minus fold).
			return Expr{}, NewError(NumericValueOutOfRange,
				"value out of range: integer literal exceeds the maximum signed 64-bit value")
		}
		return Expr{Kind: ExprLiteral, Literal: &Literal{Kind: LiteralInt, Int: v}}, nil
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
		col, err := p.expectIdentifier()
		if err != nil {
			return Expr{}, err
		}
		return Expr{Kind: ExprColumn, Column: col}, nil
	default:
		return Expr{}, NewError(SyntaxError, "expected an expression")
	}
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
