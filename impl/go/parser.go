package abide

import "fmt"

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
		return Statement{}, NewError(FeatureNotSupported,
			"SQL statement parsing is not implemented yet (step-5 Phase A scaffold)")
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

// parseLiteral parses an integer literal or the keyword NULL.
func (p *Parser) parseLiteral() (Literal, error) {
	t := p.advance()
	switch {
	case t.Kind == TokInt:
		return Literal{Kind: LiteralInt, Int: t.Int}, nil
	case t.Kind == TokWord && toLowerASCII(t.Word) == "null":
		return Literal{Kind: LiteralNull}, nil
	default:
		return Literal{}, NewError(SyntaxError, "expected a literal value")
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
