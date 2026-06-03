// Hand-written recursive-descent parser (CLAUDE.md §5, §10). Mirrors parser.go /
// parser.rs. Errors throw EngineError (42601). The lexer emits generic word tokens (no
// reserved-keyword table), so keywords are matched case-insensitively here.

import type {
  Assignment,
  BinaryOp,
  ColumnDef,
  Delete,
  Expr,
  Insert,
  Literal,
  OrderKey,
  Select,
  SelectItem,
  SelectItems,
  Statement,
  Update,
} from "./ast.ts";
import { engineError } from "./errors.ts";
import { lex } from "./lexer.ts";
import type { Token, TokenKind } from "./token.ts";

const I64_MIN = -9223372036854775808n;
const I64_MAX = 9223372036854775807n;

function lower(s: string): string {
  return s.toLowerCase();
}

// foldInt converts a lexed unsigned magnitude (<= 2^63) and a sign into a signed
// int64-range bigint, throwing 22003 when the result does not fit (a bare 2^63, or the
// not-negated 2^63). -(2^63) folds to int64's minimum (spec/design/grammar.md §4).
function foldInt(magnitude: bigint, negate: boolean): bigint {
  const v = negate ? -magnitude : magnitude;
  if (v < I64_MIN || v > I64_MAX) {
    throw engineError(
      "numeric_value_out_of_range",
      "value out of range: integer literal exceeds the maximum signed 64-bit value",
    );
  }
  return v;
}

// binaryExpr builds a binary-operator expression node.
function binaryExpr(op: BinaryOp, lhs: Expr, rhs: Expr): Expr {
  return { kind: "binary", op, lhs, rhs };
}

// parseSQL parses a single complete statement from sql.
export function parseSQL(sql: string): Statement {
  const p = new Parser(lex(sql));
  const stmt = p.parseStatement();
  p.expectEof();
  return stmt;
}

// Parser is a token cursor over a single statement.
class Parser {
  private tokens: Token[];
  private pos: number;

  constructor(tokens: Token[]) {
    this.tokens = tokens;
    this.pos = 0;
  }

  parseStatement(): Statement {
    switch (this.peekKeyword()) {
      case "create":
        return this.parseCreateTable();
      case "insert":
        return this.parseInsert();
      case "select":
        return this.parseSelect();
      case "update":
        return this.parseUpdate();
      case "delete":
        return this.parseDelete();
      case "":
        throw engineError("syntax_error", "expected a SQL statement");
      default:
        throw engineError("syntax_error", `unexpected keyword '${this.peekKeyword()}'`);
    }
  }

  // parseCreateTable parses `CREATE TABLE <name> ( <coldef> [, <coldef>]* )`, where
  // each <coldef> is `<name> <typename> [PRIMARY KEY]`. Type names are kept as written
  // and resolved during execution.
  private parseCreateTable(): Statement {
    this.expectKeyword("create");
    this.expectKeyword("table");
    const name = this.expectIdentifier();
    this.expect("lparen");

    const columns: ColumnDef[] = [];
    for (;;) {
      columns.push(this.parseColumnDef());
      const k = this.advance().kind;
      if (k === "comma") continue;
      if (k === "rparen") break;
      throw engineError("syntax_error", "expected ',' or ')'");
    }
    if (columns.length === 0) {
      throw engineError("syntax_error", "a table must have at least one column");
    }
    return { kind: "createTable", name, columns };
  }

  private parseColumnDef(): ColumnDef {
    const name = this.expectIdentifier();
    const typeName = this.expectIdentifier();
    let primaryKey = false;
    if (this.peekKeyword() === "primary") {
      this.advance();
      this.expectKeyword("key");
      primaryKey = true;
    }
    return { name, typeName, primaryKey };
  }

  // parseInsert parses `INSERT INTO <table> VALUES <row> [, <row>]*`, where each <row>
  // is `( <literal> [, <literal>]* )`. Values map positionally to columns; the executor
  // type-checks each row against the catalog and inserts all-or-nothing (grammar.md §12).
  private parseInsert(): Insert {
    this.expectKeyword("insert");
    this.expectKeyword("into");
    const table = this.expectIdentifier();
    this.expectKeyword("values");

    const rows: Literal[][] = [];
    for (;;) {
      rows.push(this.parseInsertRow());
      if (this.peek().kind === "comma") {
        this.advance();
        continue;
      }
      break;
    }
    return { kind: "insert", table, rows };
  }

  // parseInsertRow parses one parenthesized `( <literal> [, <literal>]* )` row.
  private parseInsertRow(): Literal[] {
    this.expect("lparen");
    const values: Literal[] = [];
    for (;;) {
      values.push(this.parseLiteral());
      const k = this.advance().kind;
      if (k === "comma") continue;
      if (k === "rparen") break;
      throw engineError("syntax_error", "expected ',' or ')'");
    }
    if (values.length === 0) {
      throw engineError("syntax_error", "a VALUES row must have at least one value");
    }
    return values;
  }

  // parseLiteral parses a literal value for INSERT: an integer (with an optional leading
  // unary minus, folded here), or one of the keywords NULL / TRUE / FALSE. INSERT takes
  // literals only — not general expressions (spec/grammar/grammar.ebnf `literal`).
  private parseLiteral(): Literal {
    let negate = false;
    if (this.peek().kind === "minus") {
      this.advance();
      negate = true;
    }
    const t = this.advance();
    if (t.kind === "int") return { kind: "int", int: foldInt(t.int!, negate) };
    if (!negate && t.kind === "word") {
      const w = lower(t.word!);
      if (w === "null") return { kind: "null" };
      if (w === "true") return { kind: "bool", value: true };
      if (w === "false") return { kind: "bool", value: false };
    }
    throw engineError("syntax_error", "expected a literal value");
  }

  // parseSelect parses
  // `SELECT <items> FROM <table> [WHERE <predicate>] [ORDER BY <key> [, <key>]*]
  // [LIMIT <count>] [OFFSET <count>]`. LIMIT/OFFSET may appear in either order (§9).
  private parseSelect(): Select {
    this.expectKeyword("select");

    // DISTINCT is not reserved (a column may be named `distinct`), and it is the only
    // modifier before the select list, so it takes a two-token lookahead: the leading
    // `DISTINCT` is the modifier iff the next token is neither FROM nor end-of-input —
    // otherwise the word is a column named `distinct` (spec/design/grammar.md §11). This
    // rule must be byte-identical across cores.
    let distinct = false;
    if (this.peekKeyword() === "distinct") {
      const next = this.tokens[this.pos + 1]!;
      const modifier = next.kind !== "eof" && !(next.kind === "word" && lower(next.word!) === "from");
      if (modifier) {
        this.advance();
        distinct = true;
      }
    }

    const items = this.parseSelectItems();
    this.expectKeyword("from");
    const from = this.expectIdentifier();

    const filter = this.parseOptionalWhere();

    const orderBy = this.parseOrderBy();

    let limit: bigint | null = null;
    let offset: bigint | null = null;
    for (;;) {
      const kw = this.peekKeyword();
      if (kw === "limit") {
        if (limit !== null) throw engineError("syntax_error", "duplicate LIMIT clause");
        this.advance();
        limit = this.parseCount(true);
      } else if (kw === "offset") {
        if (offset !== null) throw engineError("syntax_error", "duplicate OFFSET clause");
        this.advance();
        offset = this.parseCount(false);
      } else {
        break;
      }
    }

    return { kind: "select", distinct, items, from, filter, orderBy, limit, offset };
  }

  // parseOrderBy parses an optional `ORDER BY <key> ("," <key>)*`, where each key is a bare
  // column with an optional ASC/DESC and an optional NULLS FIRST|LAST. nullsFirst is resolved
  // here: explicit if given, else the direction default (ASC -> last, DESC -> first). A bare
  // NULLS not followed by FIRST/LAST is a syntax error (42601). Returns [] when there is no
  // ORDER BY (spec/grammar/grammar.ebnf `order_by`).
  private parseOrderBy(): OrderKey[] {
    const keys: OrderKey[] = [];
    if (this.peekKeyword() !== "order") return keys;
    this.advance();
    this.expectKeyword("by");
    for (;;) {
      const column = this.expectIdentifier();
      let descending = false;
      if (this.peekKeyword() === "asc") {
        this.advance();
      } else if (this.peekKeyword() === "desc") {
        this.advance();
        descending = true;
      }
      // Default follows direction (grammar.md §10): NULL is the largest value
      // (PostgreSQL model), so ASC → NULLS LAST, DESC → NULLS FIRST.
      let nullsFirst = descending;
      if (this.peekKeyword() === "nulls") {
        this.advance();
        if (this.peekKeyword() === "first") {
          this.advance();
          nullsFirst = true;
        } else if (this.peekKeyword() === "last") {
          this.advance();
          nullsFirst = false;
        } else {
          throw engineError("syntax_error", "NULLS must be followed by FIRST or LAST");
        }
      }
      keys.push({ column, descending, nullsFirst });
      if (this.peek().kind === "comma") {
        this.advance();
        continue;
      }
      break;
    }
    return keys;
  }

  // parseCount parses a LIMIT/OFFSET count: a non-negative integer literal. The sign is
  // folded as in parseLiteral; a negative value is rejected with 2201W (LIMIT) / 2201X
  // (OFFSET), and a magnitude over int64's max throws 22003 (the value -0 folds to 0 and
  // is accepted). isLimit selects which structured error to raise.
  private parseCount(isLimit: boolean): bigint {
    let negate = false;
    if (this.peek().kind === "minus") {
      this.advance();
      negate = true;
    }
    const t = this.advance();
    if (t.kind !== "int") {
      throw engineError("syntax_error", "expected an integer count");
    }
    const v = foldInt(t.int!, negate);
    if (v < 0n) {
      throw isLimit
        ? engineError("invalid_row_count_in_limit_clause", "LIMIT must not be negative")
        : engineError("invalid_row_count_in_offset_clause", "OFFSET must not be negative");
    }
    return v;
  }

  // parseUpdate parses
  // `UPDATE <table> SET <col> = <operand> [, <col> = <operand>]* [WHERE <pred>]`.
  private parseUpdate(): Update {
    this.expectKeyword("update");
    const table = this.expectIdentifier();
    this.expectKeyword("set");

    const assignments: Assignment[] = [];
    for (;;) {
      const column = this.expectIdentifier();
      this.expect("eq");
      const value = this.parseExpr();
      assignments.push({ column, value });
      if (this.peek().kind === "comma") {
        this.advance();
        continue;
      }
      break;
    }
    if (assignments.length === 0) {
      throw engineError("syntax_error", "UPDATE must set at least one column");
    }

    const filter = this.parseOptionalWhere();
    return { kind: "update", table, assignments, filter };
  }

  // parseDelete parses `DELETE FROM <table> [WHERE <pred>]`. No WHERE deletes all rows.
  private parseDelete(): Delete {
    this.expectKeyword("delete");
    this.expectKeyword("from");
    const table = this.expectIdentifier();
    const filter = this.parseOptionalWhere();
    return { kind: "delete", table, filter };
  }

  // parseOptionalWhere parses an optional trailing `WHERE <expr>` (shared by
  // SELECT / UPDATE / DELETE). The expression must resolve to boolean (checked by the
  // executor).
  private parseOptionalWhere(): Expr | null {
    if (this.peekKeyword() !== "where") return null;
    this.advance();
    return this.parseExpr();
  }

  private parseSelectItems(): SelectItems {
    if (this.peek().kind === "star") {
      this.advance();
      return { kind: "all" };
    }
    const items: SelectItem[] = [];
    for (;;) {
      const expr = this.parseExpr();
      // Optional `AS alias` output label. `AS` is not reserved, so it is taken as an
      // alias marker only here, after a complete expr (spec/grammar/grammar.ebnf
      // `select_item`). The alias never enters resolution (grammar.md §8).
      let alias: string | null = null;
      if (this.peekKeyword() === "as") {
        this.advance();
        alias = this.expectIdentifier();
      }
      items.push({ expr, alias });
      if (this.peek().kind === "comma") {
        this.advance();
        continue;
      }
      break;
    }
    return { kind: "list", items };
  }

  // --- expression precedence ladder (spec/grammar/grammar.ebnf `expr`) ---------
  // Loosest to tightest: OR < AND < NOT < comparison/IS NULL < additive <
  // multiplicative < unary minus < primary. One method per level keeps the grammar
  // legible (CLAUDE.md §10). The precedence is authored data (spec/functions/catalog.toml);
  // this ladder must agree with it.

  // parseExpr is the entry point for WHERE, the SELECT list, and UPDATE assignment values.
  parseExpr(): Expr {
    return this.parseOr();
  }

  private parseOr(): Expr {
    let lhs = this.parseAnd();
    while (this.peekKeyword() === "or") {
      this.advance();
      lhs = binaryExpr("or", lhs, this.parseAnd());
    }
    return lhs;
  }

  private parseAnd(): Expr {
    let lhs = this.parseNot();
    while (this.peekKeyword() === "and") {
      this.advance();
      lhs = binaryExpr("and", lhs, this.parseNot());
    }
    return lhs;
  }

  private parseNot(): Expr {
    if (this.peekKeyword() === "not") {
      this.advance();
      // right-associative: NOT NOT x
      return { kind: "unary", op: "not", operand: this.parseNot() };
    }
    return this.parseComparison();
  }

  // parseComparison parses one comparison, a postfix IS [NOT] NULL, or
  // IS [NOT] DISTINCT FROM, all non-associative: `a = b = c` is a syntax error, and
  // `a + 1 IS NULL` binds as `(a + 1) IS NULL`. After the shared `IS` `NOT`? it
  // dispatches on the NULL vs DISTINCT FROM keyword (spec/grammar/grammar.ebnf
  // `comparison`).
  private parseComparison(): Expr {
    const lhs = this.parseAdditive();
    if (this.peekKeyword() === "is") {
      this.advance();
      let negated = false;
      if (this.peekKeyword() === "not") {
        this.advance();
        negated = true;
      }
      // IS [NOT] DISTINCT FROM <additive> — NULL-safe equality; else IS [NOT] NULL.
      if (this.peekKeyword() === "distinct") {
        this.advance();
        this.expectKeyword("from");
        return { kind: "isDistinct", lhs, rhs: this.parseAdditive(), negated };
      }
      this.expectKeyword("null");
      return { kind: "isNull", operand: lhs, negated };
    }
    let op: BinaryOp;
    switch (this.peek().kind) {
      case "eq":
        op = "eq";
        break;
      case "lt":
        op = "lt";
        break;
      case "gt":
        op = "gt";
        break;
      case "le":
        op = "le";
        break;
      case "ge":
        op = "ge";
        break;
      default:
        return lhs;
    }
    this.advance();
    return binaryExpr(op, lhs, this.parseAdditive());
  }

  private parseAdditive(): Expr {
    let lhs = this.parseMultiplicative();
    for (;;) {
      let op: BinaryOp;
      if (this.peek().kind === "plus") op = "add";
      else if (this.peek().kind === "minus") op = "sub";
      else return lhs;
      this.advance();
      lhs = binaryExpr(op, lhs, this.parseMultiplicative());
    }
  }

  private parseMultiplicative(): Expr {
    let lhs = this.parseUnary();
    for (;;) {
      let op: BinaryOp;
      if (this.peek().kind === "star") op = "mul";
      else if (this.peek().kind === "slash") op = "div";
      else if (this.peek().kind === "percent") op = "mod";
      else return lhs;
      this.advance();
      lhs = binaryExpr(op, lhs, this.parseUnary());
    }
  }

  private parseUnary(): Expr {
    if (this.peek().kind === "minus") {
      this.advance();
      // Fold unary-minus-of-an-integer-literal into one negative literal, so int64's
      // minimum is representable and the literal range-checks against context.
      if (this.peek().kind === "int") {
        const v = foldInt(this.advance().int!, true);
        return { kind: "literal", literal: { kind: "int", int: v } };
      }
      return { kind: "unary", op: "neg", operand: this.parseUnary() };
    }
    return this.parsePrimary();
  }

  // parsePrimary parses a parenthesized expression, CAST(...), a literal (integer,
  // TRUE/FALSE, NULL), or a column reference.
  private parsePrimary(): Expr {
    if (this.peek().kind === "lparen") {
      this.advance();
      const e = this.parseExpr();
      this.expect("rparen");
      return e;
    }
    if (this.peekKeyword() === "cast") {
      this.advance();
      this.expect("lparen");
      const inner = this.parseExpr();
      this.expectKeyword("as");
      const typeName = this.expectIdentifier();
      this.expect("rparen");
      return { kind: "cast", inner, typeName };
    }
    const t = this.peek();
    if (t.kind === "int") {
      // The only magnitude > int64 max the lexer admits is 2^63, which fits no signed
      // integer type unless negated (handled by the unary-minus fold).
      const v = foldInt(this.advance().int!, false);
      return { kind: "literal", literal: { kind: "int", int: v } };
    }
    if (t.kind === "word") {
      const w = lower(t.word!);
      if (w === "null") {
        this.advance();
        return { kind: "literal", literal: { kind: "null" } };
      }
      if (w === "true") {
        this.advance();
        return { kind: "literal", literal: { kind: "bool", value: true } };
      }
      if (w === "false") {
        this.advance();
        return { kind: "literal", literal: { kind: "bool", value: false } };
      }
      return { kind: "column", name: this.expectIdentifier() };
    }
    throw engineError("syntax_error", "expected an expression");
  }

  // --- cursor helpers ---

  private peek(): Token {
    return this.tokens[this.pos]!;
  }

  private peekKeyword(): string {
    const t = this.peek();
    return t.kind === "word" ? lower(t.word!) : "";
  }

  private advance(): Token {
    const t = this.tokens[this.pos]!;
    if (this.pos + 1 < this.tokens.length) this.pos++;
    return t;
  }

  private expect(want: TokenKind): void {
    if (this.advance().kind !== want) {
      throw engineError("syntax_error", "unexpected token");
    }
  }

  private expectKeyword(kw: string): void {
    const t = this.advance();
    if (t.kind === "word" && lower(t.word!) === kw) return;
    throw engineError("syntax_error", `expected keyword '${kw}'`);
  }

  private expectIdentifier(): string {
    const t = this.advance();
    if (t.kind !== "word") {
      throw engineError("syntax_error", "expected an identifier");
    }
    return t.word!;
  }

  expectEof(): void {
    if (this.peek().kind !== "eof") {
      throw engineError("syntax_error", "unexpected trailing input");
    }
  }
}
