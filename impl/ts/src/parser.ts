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
  InsertValue,
  JoinClause,
  JoinKind,
  Literal,
  OrderKey,
  Select,
  SelectItem,
  SelectItems,
  Statement,
  TableRef,
  TypeMod,
  Update,
} from "./ast.ts";
import { Decimal } from "./decimal.ts";
import { engineError } from "./errors.ts";
import { lex } from "./lexer.ts";
import type { Token, TokenKind } from "./token.ts";

const I64_MIN = -9223372036854775808n;
const I64_MAX = 9223372036854775807n;

function lower(s: string): string {
  return s.toLowerCase();
}

// isTableRefStopKeyword reports whether kw (already lower-cased) is a keyword that may legally
// follow a table_ref, and so must NOT be swallowed as an implicit table alias: a trailing clause
// keyword (where/order/limit/offset) or any join-machinery keyword
// (join/inner/cross/left/right/full/outer/on). `as` is handled separately. This set is a
// CLAUDE.md §8 cross-core determinism surface (spec/design/grammar.md §15).
function isTableRefStopKeyword(kw: string): boolean {
  switch (kw) {
    case "where":
    case "group":
    case "having":
    case "order":
    case "limit":
    case "offset":
    case "join":
    case "inner":
    case "cross":
    case "left":
    case "right":
    case "full":
    case "outer":
    case "on":
    case "as":
      return true;
    default:
      return false;
  }
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
      case "drop":
        return this.parseDropTable();
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
    const typeMod = this.parseTypeMod();
    // Zero or more order-free column constraints: PRIMARY KEY, NOT NULL, and DEFAULT <literal>.
    // A boolean constraint may be repeated harmlessly; a repeated DEFAULT keeps the last.
    let primaryKey = false;
    let notNull = false;
    let def: Literal | null = null;
    for (;;) {
      const kw = this.peekKeyword();
      if (kw === "primary") {
        this.advance();
        this.expectKeyword("key");
        primaryKey = true;
      } else if (kw === "not") {
        this.advance();
        this.expectKeyword("null");
        notNull = true;
      } else if (kw === "default") {
        this.advance();
        def = this.parseLiteral();
      } else {
        break;
      }
    }
    return { name, typeName, typeMod, primaryKey, notNull, default: def };
  }

  // parseTypeMod parses an optional parenthesized type modifier `"(" integer ("," integer)? ")"`
  // after a type name (the first parameterized type, decimal — spec/grammar/grammar.ebnf
  // type_name). The shape is accepted for any type name; whether a typmod is meaningful (decimal
  // only) and in range is decided at resolve. Empty parens or a non-integer inside is 42601.
  private parseTypeMod(): TypeMod | null {
    if (this.peek().kind !== "lparen") return null;
    this.advance(); // "("
    const precision = this.expectTypmodInt();
    let scale: bigint | null = null;
    if (this.peek().kind === "comma") {
      this.advance();
      scale = this.expectTypmodInt();
    }
    this.expect("rparen");
    return { precision, scale };
  }

  private expectTypmodInt(): bigint {
    const t = this.advance();
    if (t.kind !== "int") throw engineError("syntax_error", "expected an integer type modifier");
    return t.int!;
  }

  // parseDropTable parses `DROP TABLE <name>`. A missing table is rejected at execution
  // time (42P01), not here. Single table; no IF EXISTS, no CASCADE/RESTRICT this slice
  // (spec/design/grammar.md §13).
  private parseDropTable(): Statement {
    this.expectKeyword("drop");
    this.expectKeyword("table");
    const name = this.expectIdentifier();
    return { kind: "dropTable", name };
  }

  // parseInsert parses `INSERT INTO <table> [( <col> [, <col>]* )] ( VALUES <row> [, <row>]* |
  // <select> )`. The source is either a VALUES list (each <row> is `( <value> [, <value>]* )`,
  // each <value> a literal or the DEFAULT keyword) or a SELECT (INSERT ... SELECT — §24). The
  // optional column list names the target columns; unlisted columns take their default. The
  // executor resolves names + type-checks each row and inserts all-or-nothing (grammar.md §12 /
  // §24, constraints.md §2).
  private parseInsert(): Insert {
    this.expectKeyword("insert");
    this.expectKeyword("into");
    const table = this.expectIdentifier();

    // Optional column list `( col [, col]* )` before VALUES. An empty `()` is rejected (the
    // first expectIdentifier errors 42601 on `)`).
    let columns: string[] | null = null;
    if (this.peek().kind === "lparen") {
      this.advance(); // '('
      const names: string[] = [];
      for (;;) {
        names.push(this.expectIdentifier());
        const k = this.advance().kind;
        if (k === "comma") continue;
        if (k === "rparen") break;
        throw engineError("syntax_error", "expected ',' or ')'");
      }
      columns = names;
    }

    // The source is EITHER a SELECT (INSERT ... SELECT — §24) OR a VALUES list. `VALUES` and
    // `SELECT` are disjoint leading keywords, so a peek decides without lookahead.
    if (this.peekKeyword() === "select") {
      const select = this.parseSelect();
      return { kind: "insert", table, columns, source: { kind: "select", select } };
    }

    this.expectKeyword("values");

    const rows: InsertValue[][] = [];
    for (;;) {
      rows.push(this.parseInsertRow());
      if (this.peek().kind === "comma") {
        this.advance();
        continue;
      }
      break;
    }
    return { kind: "insert", table, columns, source: { kind: "values", rows } };
  }

  // parseInsertRow parses one parenthesized `( <value> [, <value>]* )` row.
  private parseInsertRow(): InsertValue[] {
    this.expect("lparen");
    const values: InsertValue[] = [];
    for (;;) {
      values.push(this.parseInsertValue());
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

  // parseInsertValue parses one INSERT value slot: the DEFAULT keyword (not reserved — §3), a
  // bind parameter ($N, bound at execute — spec/design/api.md §5), else a literal.
  private parseInsertValue(): InsertValue {
    if (this.peekKeyword() === "default") {
      this.advance();
      return { kind: "default" };
    }
    if (this.peek().kind === "param") {
      return { kind: "param", index: this.advance().paramIndex! };
    }
    return { kind: "lit", lit: this.parseLiteral() };
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
    if (t.kind === "decimal") {
      // A decimal literal carries the unscaled coefficient + scale; the leading unary minus
      // (if any) folds into the sign. Cap checks are at resolve.
      return { kind: "decimal", dec: Decimal.fromDigitsScale(negate, t.decDigits!, t.decScale!) };
    }
    if (!negate && t.kind === "str") return { kind: "text", text: t.str! };
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
    const { from, joins } = this.parseFromClause();

    const filter = this.parseOptionalWhere();

    const groupBy = this.parseGroupBy();

    const having = this.parseHaving();

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

    return { kind: "select", distinct, items, from, joins, filter, groupBy, having, orderBy, limit, offset };
  }

  // parseHaving parses `having_clause ::= "HAVING" expr` (grammar.md §19), after GROUP BY and
  // before ORDER BY. `HAVING` is not reserved; the predicate is a general expression (it may
  // reference aggregates) checked for boolean at resolve.
  private parseHaving(): Expr | null {
    if (this.peekKeyword() !== "having") return null;
    this.advance(); // HAVING
    return this.parseExpr();
  }

  // parseGroupBy parses `group_by ::= "GROUP" "BY" column_ref ("," column_ref)*` (grammar.md
  // §18), after WHERE and before ORDER BY. Each key is a bare/qualified column (never an
  // expression/alias/ordinal). `GROUP` is not reserved, so it is a clause only when immediately
  // followed by `BY`.
  private parseGroupBy(): Expr[] {
    if (this.peekKeyword() !== "group") return [];
    this.advance(); // GROUP
    this.expectKeyword("by");
    const keys: Expr[] = [];
    for (;;) {
      const [qualifier, name] = this.parseColumnRef();
      keys.push(qualifier !== null ? { kind: "qualifiedColumn", qualifier, name } : { kind: "column", name });
      if (this.peek().kind === "comma") {
        this.advance();
        continue;
      }
      break;
    }
    return keys;
  }

  // parseFromClause parses `from_clause ::= table_ref join_clause*` (grammar.md §15): the first
  // table reference followed by a left-deep chain of zero or more joins. The join keywords are
  // not reserved (§3); the loop recognizes a join only by a leading join keyword, so any other
  // trailing word ends the FROM clause.
  private parseFromClause(): { from: TableRef; joins: JoinClause[] } {
    const from = this.parseTableRef();
    const joins: JoinClause[] = [];
    for (;;) {
      const j = this.parseJoinClause();
      if (j === null) break;
      joins.push(j);
    }
    return { from, joins };
  }

  // parseTableRef parses `table_ref ::= identifier ("AS"? identifier)?` — a table name with an
  // optional alias. An explicit AS takes the next identifier unconditionally; an implicit alias
  // is taken only when the next token is a word that is NOT a clause/join keyword (so `FROM t
  // WHERE` and `FROM t JOIN ...` keep no alias). The stop-keyword set is a §8 cross-core surface.
  private parseTableRef(): TableRef {
    const name = this.expectIdentifier();
    let alias: string | null = null;
    if (this.peekKeyword() === "as") {
      this.advance();
      alias = this.expectIdentifier();
    } else {
      const t = this.peek();
      if (t.kind === "word" && !isTableRefStopKeyword(lower(t.word!))) {
        alias = t.word!;
        this.advance();
      }
    }
    return { name, alias };
  }

  // parseJoinClause parses one join_clause if a join keyword begins here (returns null to end
  // the FROM chain). CROSS JOIN has no ON; the INNER/outer kinds require ON <expr> (a missing ON
  // is 42601). The outer kinds (LEFT/RIGHT/FULL [OUTER]) parse into the AST but are rejected at
  // execution (0A000) — spec/design/grammar.md §15.
  private parseJoinClause(): JoinClause | null {
    const kw = this.peekKeyword();
    let kind: JoinKind;
    let isCross = false;
    switch (kw) {
      case "join": // a bare JOIN is INNER
        this.advance();
        kind = "inner";
        break;
      case "inner":
        this.advance();
        this.expectKeyword("join");
        kind = "inner";
        break;
      case "cross":
        this.advance();
        this.expectKeyword("join");
        kind = "cross";
        isCross = true;
        break;
      case "left":
      case "right":
      case "full":
        this.advance();
        if (this.peekKeyword() === "outer") this.advance(); // optional OUTER
        this.expectKeyword("join");
        kind = kw;
        break;
      default: // not a join keyword: the FROM chain ends here
        return null;
    }
    const table = this.parseTableRef();
    let on: Expr | null = null;
    if (!isCross) {
      this.expectKeyword("on");
      on = this.parseExpr();
    }
    return { kind, table, on };
  }

  // parseColumnRef parses `column_ref ::= identifier ("." identifier)?` — a bare column name, or
  // a qualified `rel.col` (the "." is the "dot" token). Returns [qualifier, name]; qualifier is
  // null for a bare column (spec/grammar/grammar.ebnf `column_ref`, grammar.md §15).
  private parseColumnRef(): [string | null, string] {
    const first = this.expectIdentifier();
    if (this.peek().kind === "dot") {
      this.advance();
      return [first, this.expectIdentifier()];
    }
    return [null, first];
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
      const [qualifier, column] = this.parseColumnRef();
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
      keys.push({ qualifier, column, descending, nullsFirst });
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
    // `NOT`? (`IN` (...) | `BETWEEN` lo `AND` hi) — a `NOT` here is consumed only when followed
    // by one of these postfix-predicate keywords (two-token lookahead; the prefix `NOT` was
    // already taken by parseNot). Non-associative, at the comparison level (grammar.md §20-§21).
    const predNegated =
      this.peekKeyword() === "not" &&
      (this.peekKeywordAt(1) === "in" ||
        this.peekKeywordAt(1) === "between" ||
        this.peekKeywordAt(1) === "like");
    if (predNegated) {
      this.advance(); // NOT
    }
    if (this.peekKeyword() === "in") {
      this.advance();
      this.expect("lparen");
      // A non-empty value list (`IN ()` — parseAdditive on `)` is a 42601 syntax error).
      const list = [this.parseAdditive()];
      while (this.peek().kind === "comma") {
        this.advance();
        list.push(this.parseAdditive());
      }
      this.expect("rparen");
      return { kind: "in", lhs, list, negated: predNegated };
    }
    if (this.peekKeyword() === "between") {
      this.advance();
      // Both bounds parse at the ADDITIVE level, which never consumes `AND` (a looser level
      // owned by parseAnd). So the BETWEEN's structural `AND` is matched here and
      // `x BETWEEN a AND b AND c` parses as `(x BETWEEN a AND b) AND c` (grammar.md §21).
      const lo = this.parseAdditive();
      this.expectKeyword("and");
      const hi = this.parseAdditive();
      return { kind: "between", lhs, lo, hi, negated: predNegated };
    }
    if (this.peekKeyword() === "like") {
      this.advance();
      const rhs = this.parseAdditive();
      return { kind: "like", lhs, rhs, negated: predNegated };
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
      // Fold unary-minus of a decimal literal into one negative decimal literal (decimal
      // negation never overflows).
      if (this.peek().kind === "decimal") {
        const t = this.advance();
        return { kind: "literal", literal: { kind: "decimal", dec: Decimal.fromDigitsScale(true, t.decDigits!, t.decScale!) } };
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
      const typeMod = this.parseTypeMod();
      this.expect("rparen");
      return { kind: "cast", inner, typeName, typeMod };
    }
    if (this.peekKeyword() === "case") {
      this.advance();
      // Simple form has an operand between CASE and the first WHEN; the searched form starts
      // directly with WHEN (grammar.md §23).
      const operand = this.peekKeyword() === "when" ? null : this.parseExpr();
      const whens: { cond: Expr; result: Expr }[] = [];
      while (this.peekKeyword() === "when") {
        this.advance();
        const cond = this.parseExpr();
        this.expectKeyword("then");
        const result = this.parseExpr();
        whens.push({ cond, result });
      }
      if (whens.length === 0) {
        throw engineError("syntax_error", "CASE requires at least one WHEN clause");
      }
      let els: Expr | null = null;
      if (this.peekKeyword() === "else") {
        this.advance();
        els = this.parseExpr();
      }
      this.expectKeyword("end");
      return { kind: "case", operand, whens, els };
    }
    const t = this.peek();
    if (t.kind === "param") {
      return { kind: "param", index: this.advance().paramIndex! };
    }
    if (t.kind === "int") {
      // The only magnitude > int64 max the lexer admits is 2^63, which fits no signed
      // integer type unless negated (handled by the unary-minus fold).
      const v = foldInt(this.advance().int!, false);
      return { kind: "literal", literal: { kind: "int", int: v } };
    }
    if (t.kind === "decimal") {
      this.advance();
      return { kind: "literal", literal: { kind: "decimal", dec: Decimal.fromDigitsScale(false, t.decDigits!, t.decScale!) } };
    }
    if (t.kind === "str") {
      this.advance();
      return { kind: "literal", literal: { kind: "text", text: t.str! } };
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
      // Function call: a BARE identifier IMMEDIATELY followed by "(" is a call (the engine's
      // first call syntax — grammar.md §17). The one-token lookahead keeps function names
      // non-reserved (a column may be named `count`); a qualified name is never a call. Only
      // aggregates resolve (42883 otherwise).
      if (this.tokens[this.pos + 1]?.kind === "lparen") {
        return this.parseFunctionCall();
      }
      const [qualifier, name] = this.parseColumnRef();
      return qualifier !== null
        ? { kind: "qualifiedColumn", qualifier, name }
        : { kind: "column", name };
    }
    throw engineError("syntax_error", "expected an expression");
  }

  // parseFunctionCall parses `function_call ::= identifier "(" ( "*" | expr ("," expr)* ) ")"` —
  // the shared aggregate/scalar call syntax (grammar.md §17). COUNT(*) is the star form; every
  // other call takes a comma-separated argument list (resolution checks the per-function arity).
  // DISTINCT inside the parens is deferred (rejected 42601).
  private parseFunctionCall(): Expr {
    const name = this.expectIdentifier();
    this.expect("lparen");
    // DISTINCT inside a function call (COUNT(DISTINCT x)) is deferred — reject at parse.
    if (this.peekKeyword() === "distinct") {
      throw engineError("syntax_error", "DISTINCT inside an aggregate is not supported yet");
    }
    const args: Expr[] = [];
    let star = false;
    if (this.peek().kind === "star") {
      this.advance();
      star = true;
    } else {
      args.push(this.parseExpr());
      while (this.peek().kind === "comma") {
        this.advance();
        args.push(this.parseExpr());
      }
    }
    this.expect("rparen");
    return { kind: "funcCall", name, args, star };
  }

  // --- cursor helpers ---

  private peek(): Token {
    return this.tokens[this.pos]!;
  }

  private peekKeyword(): string {
    const t = this.peek();
    return t.kind === "word" ? lower(t.word!) : "";
  }

  // peekKeywordAt returns the keyword (lowercased) `offset` tokens ahead of the cursor if that
  // token is a word, else "". Used for the two-token NOT IN/BETWEEN/LIKE lookahead (a
  // CLAUDE.md §8 determinism surface — byte-identical across the three parsers).
  private peekKeywordAt(offset: number): string {
    const t = this.tokens[this.pos + offset];
    return t !== undefined && t.kind === "word" ? lower(t.word!) : "";
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
