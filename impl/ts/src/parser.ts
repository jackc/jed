// Hand-written recursive-descent parser (CLAUDE.md §5, §10). Mirrors parser.go /
// parser.rs. Errors throw EngineError (42601). The lexer emits generic word tokens (no
// reserved-keyword table), so keywords are matched case-insensitively here.

import type {
  Assignment,
  BinaryOp,
  CheckDef,
  UniqueDef,
  ColumnDef,
  Delete,
  Expr,
  Insert,
  InsertValue,
  JoinClause,
  JoinKind,
  Literal,
  OrderKey,
  QueryExpr,
  Select,
  SelectItem,
  SelectItems,
  SetOpKind,
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
    // set operators end a SELECT core — they must not be swallowed as an implicit table alias
    // (`FROM a UNION ...` is a UNION, not a table `a` aliased `union`). §25.
    case "union":
    case "intersect":
    case "except":
    // RETURNING ends an INSERT ... SELECT source — it must not be swallowed as the source's
    // implicit table alias (`... SELECT v FROM t RETURNING v` is the INSERT's clause). §32;
    // PostgreSQL fully reserves the word.
    case "returning":
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
      // CREATE / DROP dispatch on the object keyword (TABLE vs [UNIQUE] INDEX — grammar.md
      // §30; UNIQUE needs no lookahead of its own — after CREATE the next word being
      // UNIQUE can only be CREATE UNIQUE INDEX).
      case "create":
        if (this.peekKeywordAt(1) === "index" || this.peekKeywordAt(1) === "unique") {
          return this.parseCreateIndex();
        }
        return this.parseCreateTable();
      case "drop":
        if (this.peekKeywordAt(1) === "index") return this.parseDropIndex();
        return this.parseDropTable();
      case "insert":
        return this.parseInsert();
      case "select":
        return this.parseQueryExpr();
      case "update":
        return this.parseUpdate();
      case "delete":
        return this.parseDelete();
      case "begin":
      case "start":
        return this.parseBegin();
      case "commit":
      case "end":
        return this.parseCommit();
      case "rollback":
        return this.parseRollback();
      case "":
        throw engineError("syntax_error", "expected a SQL statement");
      default:
        throw engineError("syntax_error", `unexpected keyword '${this.peekKeyword()}'`);
    }
  }

  // parseBegin parses `BEGIN [TRANSACTION|WORK] [READ ONLY|READ WRITE]` or `START TRANSACTION
  // [READ ONLY|READ WRITE]` — open an explicit transaction (spec/design/grammar.md §27). The
  // access mode defaults to READ WRITE.
  private parseBegin(): Statement {
    if (this.peekKeyword() === "start") {
      this.advance();
      this.expectKeyword("transaction");
    } else {
      this.advance(); // BEGIN
      const kw = this.peekKeyword();
      if (kw === "transaction" || kw === "work") this.advance();
    }
    return { kind: "begin", writable: this.parseAccessMode() };
  }

  // parseAccessMode parses the optional access mode after a transaction opener: `READ ONLY` →
  // false, `READ WRITE` → true, absent → null (unspecified — the executor applies the handle's
  // default: READ WRITE, or READ ONLY on a read-only handle; transactions.md §4.3, api.md §2.1).
  private parseAccessMode(): boolean | null {
    if (this.peekKeyword() !== "read") return null;
    this.advance(); // READ
    switch (this.peekKeyword()) {
      case "only":
        this.advance();
        return false;
      case "write":
        this.advance();
        return true;
      default:
        throw engineError(
          "syntax_error",
          `expected ONLY or WRITE after READ, found '${this.peekKeyword()}'`,
        );
    }
  }

  // parseCommit parses `COMMIT [TRANSACTION|WORK]` / `END [TRANSACTION|WORK]` (grammar.md §27).
  private parseCommit(): Statement {
    this.advance(); // COMMIT or END
    this.consumeTransactionOrWork();
    return { kind: "commit" };
  }

  // parseRollback parses `ROLLBACK [TRANSACTION|WORK]` (grammar.md §27).
  private parseRollback(): Statement {
    this.expectKeyword("rollback");
    this.consumeTransactionOrWork();
    return { kind: "rollback" };
  }

  // consumeTransactionOrWork consumes the optional trailing TRANSACTION / WORK noise word.
  private consumeTransactionOrWork(): void {
    const kw = this.peekKeyword();
    if (kw === "transaction" || kw === "work") this.advance();
  }

  // parseCreateTable parses `CREATE TABLE <name> ( <element> [, <element>]* )`, where
  // each <element> is a column definition or the table-level `PRIMARY KEY ( <col> [,
  // <col>]* )` constraint (spec/design/grammar.md §28). An element starting with the two
  // keywords PRIMARY KEY is the table constraint — nothing is lost, since a column named
  // "primary" would need a type named "key", which does not exist. Type names are kept as
  // written and resolved during execution; the constraint's member names are likewise
  // resolved there (42703/42701/42P16).
  private parseCreateTable(): Statement {
    this.expectKeyword("create");
    this.expectKeyword("table");
    const name = this.expectIdentifier();
    this.expect("lparen");

    const columns: ColumnDef[] = [];
    const tablePks: string[][] = [];
    const checks: CheckDef[] = [];
    const uniques: UniqueDef[] = [];
    for (;;) {
      if (this.peekKeyword() === "primary" && this.peekKeywordAt(1) === "key") {
        this.advance();
        this.advance();
        tablePks.push(this.parsePkColumnList());
      } else if (this.atCheckConstraint()) {
        checks.push(this.parseCheckConstraint());
      } else if (this.atUniqueTableConstraint()) {
        uniques.push(this.parseUniqueTableConstraint());
      } else {
        columns.push(this.parseColumnDef(checks, uniques));
      }
      const k = this.advance().kind;
      if (k === "comma") continue;
      if (k === "rparen") break;
      throw engineError("syntax_error", "expected ',' or ')'");
    }
    if (columns.length === 0) {
      throw engineError("syntax_error", "a table must have at least one column");
    }
    return { kind: "createTable", name, columns, tablePks, checks, uniques };
  }

  // atUniqueTableConstraint reports whether the cursor sits on a table-level UNIQUE
  // constraint: the keyword UNIQUE followed by "(", or CONSTRAINT <ident> UNIQUE
  // (spec/design/grammar.md §31). The keywords stay non-reserved — a column named
  // "unique" is followed by a type name (an identifier, never "("), so the lookahead
  // loses nothing.
  private atUniqueTableConstraint(): boolean {
    if (this.peekKeyword() === "unique" && this.peekKindAt(1) === "lparen") return true;
    return this.peekKeyword() === "constraint" && this.peekKeywordAt(2) === "unique";
  }

  // parseUniqueTableConstraint parses one table-level `[CONSTRAINT name] UNIQUE ( col [,
  // col]* )` (the cursor is verified by atUniqueTableConstraint). The member list reuses
  // the PRIMARY KEY list shape (spec/design/grammar.md §31).
  private parseUniqueTableConstraint(): UniqueDef {
    let name: string | null = null;
    if (this.peekKeyword() === "constraint") {
      this.advance();
      name = this.expectIdentifier();
    }
    this.expectKeyword("unique");
    return { name, columns: this.parsePkColumnList() };
  }

  // atCheckConstraint reports whether the cursor sits on a CHECK constraint: the keyword
  // CHECK followed by "(", or CONSTRAINT <ident> CHECK "(" (spec/design/grammar.md §29).
  // The keywords stay non-reserved — a column named "check"/"constraint" is followed by a
  // type name (an identifier, never "("), so the lookahead loses nothing.
  private atCheckConstraint(): boolean {
    if (this.peekKeyword() === "check" && this.peekKindAt(1) === "lparen") return true;
    return (
      this.peekKeyword() === "constraint" &&
      this.peekKeywordAt(2) === "check" &&
      this.peekKindAt(3) === "lparen"
    );
  }

  // parseCheckConstraint parses one `[CONSTRAINT name] CHECK ( expr )` (the cursor is
  // verified by atCheckConstraint). The token span between the parentheses is re-rendered
  // as the constraint's persisted text (spec/fileformat/format.md "Check-expression text").
  private parseCheckConstraint(): CheckDef {
    let name: string | null = null;
    if (this.peekKeyword() === "constraint") {
      this.advance();
      name = this.expectIdentifier();
    }
    this.expectKeyword("check");
    this.expect("lparen");
    const start = this.pos;
    const expr = this.parseExpr();
    const text = renderTokens(this.tokens.slice(start, this.pos));
    this.expect("rparen");
    return { name, expr, text };
  }

  // parsePkColumnList parses the parenthesized member list of a table-level PRIMARY KEY
  // constraint: `( <col> [, <col>]* )`. Must be non-empty — `PRIMARY KEY ()` is 42601 (the
  // first expectIdentifier rejects `)`).
  private parsePkColumnList(): string[] {
    this.expect("lparen");
    const cols = [this.expectIdentifier()];
    for (;;) {
      const k = this.advance().kind;
      if (k === "comma") {
        cols.push(this.expectIdentifier());
        continue;
      }
      if (k === "rparen") return cols;
      throw engineError("syntax_error", "expected ',' or ')'");
    }
  }

  private parseColumnDef(checks: CheckDef[], uniques: UniqueDef[]): ColumnDef {
    const name = this.expectIdentifier();
    const typeName = this.expectIdentifier();
    const typeMod = this.parseTypeMod();
    // Zero or more order-free column constraints: PRIMARY KEY, NOT NULL, DEFAULT <literal>,
    // [CONSTRAINT name] CHECK ( expr ), and [CONSTRAINT name] UNIQUE. A boolean constraint
    // may be repeated harmlessly; a repeated DEFAULT keeps the last; each CHECK is a
    // distinct constraint, collected into the statement-wide list in textual order (a
    // column-level check is semantically identical to a table-level one —
    // spec/design/constraints.md §4). A column-level UNIQUE collects the same way as the
    // one-member form (a repeat folds at execution — spec/design/constraints.md §5).
    let primaryKey = false;
    let notNull = false;
    let def: Literal | null = null;
    for (;;) {
      if (this.atCheckConstraint()) {
        checks.push(this.parseCheckConstraint());
        continue;
      }
      // CONSTRAINT <name> UNIQUE in column position (the named one-member form;
      // CONSTRAINT <name> CHECK ( was caught above).
      if (this.peekKeyword() === "constraint" && this.peekKeywordAt(2) === "unique") {
        this.advance();
        const cname = this.expectIdentifier();
        this.expectKeyword("unique");
        uniques.push({ name: cname, columns: [name] });
        continue;
      }
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
      } else if (kw === "unique") {
        this.advance();
        uniques.push({ name: null, columns: [name] });
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

  // parseCreateIndex parses `CREATE INDEX [name] ON <table> ( col [, col]* )`
  // (spec/design/grammar.md §30). The optional name needs one disambiguation because no
  // word is reserved: the word after INDEX is the index name UNLESS it is `ON` followed
  // by a word and then `(` — that exact three-token shape can only be the unnamed form's
  // `ON table (`. Key columns are bare identifiers (no expression/ordered/partial keys
  // this slice — a `(`/`ASC`/`DESC` after a key is the natural 42601).
  private parseCreateIndex(): Statement {
    this.expectKeyword("create");
    const unique = this.peekKeyword() === "unique";
    if (unique) this.advance();
    this.expectKeyword("index");
    const unnamed =
      this.peekKeyword() === "on" &&
      this.peekKindAt(1) === "word" &&
      this.peekKindAt(2) === "lparen";
    const name = unnamed ? null : this.expectIdentifier();
    this.expectKeyword("on");
    const table = this.expectIdentifier();
    this.expect("lparen");
    const columns: string[] = [];
    for (;;) {
      columns.push(this.expectIdentifier());
      const tok = this.advance();
      if (tok.kind === "comma") continue;
      if (tok.kind === "rparen") break;
      throw engineError("syntax_error", `expected ',' or ')', found ${tok.kind}`);
    }
    return { kind: "createIndex", name, table, columns, unique };
  }

  // parseDropIndex parses `DROP INDEX <name>` (spec/design/grammar.md §30). A missing
  // index (42704) or a table's name (42809) is rejected at execution time, not here.
  private parseDropIndex(): Statement {
    this.expectKeyword("drop");
    this.expectKeyword("index");
    const name = this.expectIdentifier();
    return { kind: "dropIndex", name };
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
      const returning = this.parseReturning();
      return { kind: "insert", table, columns, source: { kind: "select", select }, returning };
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
    const returning = this.parseReturning();
    return { kind: "insert", table, columns, source: { kind: "values", rows }, returning };
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

  // parseQueryExpr parses a top-level query expression (spec/design/grammar.md §25): one or more
  // SELECT cores combined by UNION/INTERSECT/EXCEPT, with an optional trailing ORDER BY/LIMIT/OFFSET
  // applying to the whole result. A lone query (no set operator) folds the trailing clauses back
  // onto the single Select, leaving the plain-query path untouched; otherwise a SetOp is returned.
  private parseQueryExpr(): Statement {
    const node = this.parseSetExpr();
    const orderBy = this.parseOrderBy();
    const { limit, offset } = this.parseLimitOffsetClauses();
    // Both Select and SetOp carry orderBy/limit/offset; the spread keeps the `kind` discriminant.
    return { ...node, orderBy, limit, offset };
  }

  // parseSubquery parses a parenthesized subquery's inner query_expr (grammar.md §26): a full
  // set-expression plus an optional trailing ORDER BY / LIMIT / OFFSET folded onto the node.
  // Mirrors parseQueryExpr but yields a QueryExpr. The caller has consumed the opening "(" and
  // consumes the closing ")".
  private parseSubquery(): QueryExpr {
    const node = this.parseSetExpr();
    const orderBy = this.parseOrderBy();
    const { limit, offset } = this.parseLimitOffsetClauses();
    return { ...node, orderBy, limit, offset };
  }

  // parseSetExpr parses the lower-precedence, left-associative UNION/EXCEPT level. INTERSECT binds
  // tighter (parsed inside parseIntersectExpr), so `a UNION b INTERSECT c` becomes
  // `a UNION (b INTERSECT c)`.
  private parseSetExpr(): QueryExpr {
    let left = this.parseIntersectExpr();
    for (;;) {
      const kw = this.peekKeyword();
      let op: SetOpKind;
      if (kw === "union") op = "union";
      else if (kw === "except") op = "except";
      else return left;
      this.advance(); // UNION | EXCEPT
      const all = this.parseSetOpQuantifier();
      const right = this.parseIntersectExpr();
      left = { kind: "setOp", op, all, lhs: left, rhs: right, orderBy: [], limit: null, offset: null };
    }
  }

  // parseIntersectExpr parses the higher-precedence, left-associative INTERSECT level.
  private parseIntersectExpr(): QueryExpr {
    let left: QueryExpr = this.parseSelectCore();
    while (this.peekKeyword() === "intersect") {
      this.advance(); // INTERSECT
      const all = this.parseSetOpQuantifier();
      const right = this.parseSelectCore();
      left = { kind: "setOp", op: "intersect", all, lhs: left, rhs: right, orderBy: [], limit: null, offset: null };
    }
    return left;
  }

  // parseSetOpQuantifier consumes the optional ALL (multiset) or DISTINCT (explicit default)
  // quantifier after a set operator, returning whether ALL was given.
  private parseSetOpQuantifier(): boolean {
    const kw = this.peekKeyword();
    if (kw === "all") {
      this.advance();
      return true;
    }
    if (kw === "distinct") {
      this.advance();
      return false;
    }
    return false;
  }

  // parseLimitOffsetClauses parses an optional `LIMIT <count>` / `OFFSET <count>` pair, in either
  // order, each at most once (§9). Returns nulls when absent.
  private parseLimitOffsetClauses(): { limit: bigint | null; offset: bigint | null } {
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
    return { limit, offset };
  }

  // parseSelect parses a complete SELECT with its own trailing ORDER BY/LIMIT/OFFSET — the form an
  // INSERT ... SELECT source takes (spec/design/grammar.md §24). Behaviorally identical to the
  // pre-set-operations parseSelect: a select_core plus the trailing clauses.
  private parseSelect(): Select {
    const sel = this.parseSelectCore();
    sel.orderBy = this.parseOrderBy();
    const { limit, offset } = this.parseLimitOffsetClauses();
    sel.limit = limit;
    sel.offset = offset;
    return sel;
  }

  // parseSelectCore parses a SELECT without a trailing ORDER BY/LIMIT/OFFSET — the operand form of
  // a set operation (spec/design/grammar.md §25). The returned Select has empty orderBy and null
  // limit/offset. The FROM clause is optional: with no `from` keyword the SELECT is FROM-less —
  // one virtual zero-column row (spec/design/grammar.md §34).
  private parseSelectCore(): Select {
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
    let from: TableRef | null = null;
    let joins: JoinClause[] = [];
    if (this.peekKeyword() === "from") {
      this.advance(); // FROM
      ({ from, joins } = this.parseFromClause());
    }

    const filter = this.parseOptionalWhere();

    const groupBy = this.parseGroupBy();

    const having = this.parseHaving();

    return {
      kind: "select",
      distinct,
      items,
      from,
      joins,
      filter,
      groupBy,
      having,
      orderBy: [],
      limit: null,
      offset: null,
    };
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

  // parseTableRef parses `table_ref ::= (identifier | table_function) ("AS"? identifier)?` where
  // `table_function ::= identifier "(" expr ("," expr)* ")"` — a base table name OR a
  // set-returning function call (generate_series(1, 5)) used as a row source, each with an
  // optional alias (grammar.md §15/§35). A `(` immediately after the leading identifier marks
  // the function form; the resolver owns arity/type errors. The alias logic is identical for
  // both forms. The stop-keyword set is a §8 cross-core surface.
  private parseTableRef(): TableRef {
    const name = this.expectIdentifier();
    // A `(` right after the name = a set-returning function call (no `*`/`DISTINCT`).
    let args: Expr[] | null = null;
    if (this.peek().kind === "lparen") {
      this.advance();
      args = [this.parseExpr()];
      while (this.peek().kind === "comma") {
        this.advance();
        args.push(this.parseExpr());
      }
      this.expect("rparen");
    }
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
    // The column-alias-list form `... AS g(n)` is a deferred narrowing (grammar.md §35): a `(`
    // after the alias is unambiguous (a base table never has one there) and rejected.
    if (alias !== null && this.peek().kind === "lparen") {
      throw engineError(
        "feature_not_supported",
        "column alias list on a table function is not supported yet",
      );
    }
    return { name, alias, args };
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
    const returning = this.parseReturning();
    return { kind: "update", table, assignments, filter, returning };
  }

  // parseDelete parses `DELETE FROM <table> [WHERE <pred>]`. No WHERE deletes all rows.
  private parseDelete(): Delete {
    this.expectKeyword("delete");
    this.expectKeyword("from");
    const table = this.expectIdentifier();
    const filter = this.parseOptionalWhere();
    const returning = this.parseReturning();
    return { kind: "delete", table, filter, returning };
  }

  // parseOptionalWhere parses an optional trailing `WHERE <expr>` (shared by
  // SELECT / UPDATE / DELETE). The expression must resolve to boolean (checked by the
  // executor).
  private parseOptionalWhere(): Expr | null {
    if (this.peekKeyword() !== "where") return null;
    this.advance();
    return this.parseExpr();
  }

  // parseReturning parses an optional terminal `RETURNING <select_items>` clause (shared by
  // INSERT/UPDATE/DELETE — spec/design/grammar.md §32). RETURNING is not reserved (§3): it is
  // a clause only in this trailing position (and it joins the table_ref implicit-alias stop
  // set, so an `INSERT ... SELECT` source never swallows it — §15). The item list is the
  // ordinary select-items production (`*` or expressions with optional AS labels); an empty
  // list fails in parseExpr (42601).
  private parseReturning(): SelectItems | null {
    if (this.peekKeyword() !== "returning") return null;
    this.advance(); // RETURNING
    return this.parseSelectItems();
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
      // `IN (SELECT ...)` is the uncorrelated IN-subquery (grammar.md §26), disambiguated by a
      // leading `SELECT`; otherwise a non-empty value list (`IN ()` is a 42601 syntax error).
      if (this.peekKeyword() === "select") {
        const query = this.parseSubquery();
        this.expect("rparen");
        return { kind: "inSubquery", lhs, query, negated: predNegated };
      }
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
      // minimum is representable and the literal range-checks against context. SUPPRESSED
      // when a `::` immediately follows: `::` binds tighter than unary minus (PostgreSQL),
      // so `-N::T` is `-(N::T)` — the cast applies to the unsigned magnitude first
      // (grammar.md §37). A one-token lookahead on the token AFTER the literal.
      if (this.peek().kind === "int" && this.tokens[this.pos + 1]?.kind !== "doubleColon") {
        const v = foldInt(this.advance().int!, true);
        return { kind: "literal", literal: { kind: "int", int: v } };
      }
      // Fold unary-minus of a decimal literal into one negative decimal literal (decimal
      // negation never overflows). Same `::` suppression.
      if (this.peek().kind === "decimal" && this.tokens[this.pos + 1]?.kind !== "doubleColon") {
        const t = this.advance();
        return { kind: "literal", literal: { kind: "decimal", dec: Decimal.fromDigitsScale(true, t.decDigits!, t.decScale!) } };
      }
      return { kind: "unary", op: "neg", operand: this.parseUnary() };
    }
    return this.parsePostfix();
  }

  // parsePostfix parses a primary optionally followed by one or more `::type` PostgreSQL typecasts
  // (grammar.md §37). `expr :: type` desugars to CAST(expr AS type) here at parse time — one
  // resolver / evaluator / cost path for both spellings — and casts chain left-associatively
  // (`x::int8::int2` = `(x::int8)::int2`). A typmod rides on the type name exactly as in CAST
  // (`x::numeric(10,2)`). `::` binds tighter than unary minus (handled by parseUnary above).
  private parsePostfix(): Expr {
    let expr = this.parsePrimary();
    while (this.peek().kind === "doubleColon") {
      this.advance();
      const typeName = this.expectIdentifier();
      const typeMod = this.parseTypeMod();
      expr = { kind: "cast", inner: expr, typeName, typeMod };
    }
    return expr;
  }

  // parsePrimary parses a parenthesized expression, CAST(...), a literal (integer,
  // TRUE/FALSE, NULL), or a column reference.
  private parsePrimary(): Expr {
    if (this.peek().kind === "lparen") {
      this.advance();
      // `(SELECT ...)` is a scalar subquery (grammar.md §26), disambiguated by a leading `SELECT`
      // after the `(`; otherwise this is a parenthesized expression.
      if (this.peekKeyword() === "select") {
        const query = this.parseSubquery();
        this.expect("rparen");
        return { kind: "scalarSubquery", query };
      }
      const e = this.parseExpr();
      this.expect("rparen");
      return e;
    }
    // `EXISTS ( SELECT ... )` — the existence predicate (grammar.md §26). Recognized only when an
    // open-paren + `SELECT` follows, so `exists` stays usable as a column / function name.
    if (
      this.peekKeyword() === "exists" &&
      this.tokens[this.pos + 1]?.kind === "lparen" &&
      this.peekKeywordAt(2) === "select"
    ) {
      this.advance(); // EXISTS
      this.expect("lparen");
      const query = this.parseSubquery();
      this.expect("rparen");
      return { kind: "exists", query };
    }
    // A typed string literal `type '...'` (grammar.md §36) — PostgreSQL's `type 'string'`, equal to
    // CAST('string' AS type) over a string-literal operand: ANY type-naming word immediately followed
    // by a string (`INTERVAL '1 day'`, `TIMESTAMP '...'`, `INTEGER '42'`, `BYTEA '\xDE'`, …).
    // Recognized only when the next token is a string — a one-token lookahead — so the word stays
    // usable as a column / function name otherwise. true/false/null are excluded (their own value
    // literals). The type name is resolved (and the string coerced to it) at resolve; unknown → 42704.
    const tlKw = this.peekKeyword();
    if (
      tlKw !== "" &&
      tlKw !== "null" &&
      tlKw !== "true" &&
      tlKw !== "false" &&
      this.tokens[this.pos + 1]?.kind === "str"
    ) {
      const nameTok = this.advance(); // the named type (original case; scalarFromName lowercases)
      const t = this.advance();
      return { kind: "typedLiteral", typeName: nameTok.word!, text: t.str! };
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
      // `current_timestamp` — the SQL-standard bare keyword (no parens), reserved like the value
      // literals above. Pure sugar: desugar to a `now()` call so resolution / execution / cost /
      // volatility are entirely shared (spec/design/functions.md §12). Not fired when followed by
      // `(` (a precision typmod, deferred) so that form resolves normally (42883).
      if (w === "current_timestamp" && this.tokens[this.pos + 1]?.kind !== "lparen") {
        this.advance();
        return { kind: "funcCall", name: "now", args: [], argNames: [], star: false };
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

  // parseFunctionCall parses
  // `function_call ::= identifier "(" ( "*" | function_arg ("," function_arg)* )? ")"` and
  // `function_arg ::= ( identifier "=>" )? expr` — the shared aggregate/scalar call syntax
  // (grammar.md §17). COUNT(*) is the star form; the argument list may be empty (a function whose
  // parameters all DEFAULT, e.g. make_interval()); otherwise it is a comma-separated list of
  // positional and/or NAMED (name => value) arguments. A positional argument may not follow a
  // named one (42601). argNames stays empty when every argument is positional. DISTINCT inside the
  // parens is deferred (rejected 42601). Resolution checks per-function arity and fills defaults.
  private parseFunctionCall(): Expr {
    const name = this.expectIdentifier();
    this.expect("lparen");
    // DISTINCT inside a function call (COUNT(DISTINCT x)) is deferred — reject at parse.
    if (this.peekKeyword() === "distinct") {
      throw engineError("syntax_error", "DISTINCT inside an aggregate is not supported yet");
    }
    const args: Expr[] = [];
    const names: (string | null)[] = [];
    let star = false;
    let anyNamed = false;
    if (this.peek().kind === "star") {
      this.advance();
      star = true;
    } else if (this.peek().kind !== "rparen") {
      // Empty parens (make_interval()) fall through with empty args.
      for (;;) {
        // A named argument is `identifier "=>" expr` (grammar.md §17); a two-token lookahead
        // (word then "=>") distinguishes it from a bare expr that starts with an identifier.
        let argName: string | null = null;
        if (this.peek().kind === "word" && this.peekKindAt(1) === "fatArrow") {
          argName = this.expectIdentifier();
          this.expect("fatArrow");
          anyNamed = true;
        } else if (anyNamed) {
          // A positional argument may not follow a named one (PostgreSQL, 42601).
          throw engineError("syntax_error", "positional argument cannot follow named argument");
        }
        args.push(this.parseExpr());
        names.push(argName);
        if (this.peek().kind !== "comma") break;
        this.advance();
      }
    }
    this.expect("rparen");
    // Keep argNames empty unless a name appeared (the all-positional sentinel — §8).
    const argNames = anyNamed ? names : [];
    return { kind: "funcCall", name, args, argNames, star };
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

  // peekKindAt returns the token kind `offset` tokens ahead of the cursor ("eof" past the
  // end). Used with peekKeywordAt for the CHECK-constraint lookahead (grammar.md §29).
  private peekKindAt(offset: number): TokenKind {
    const t = this.tokens[this.pos + offset];
    return t !== undefined ? t.kind : "eof";
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

// parseExpression parses a bare expression — the catalog-load path for a persisted CHECK
// expression (spec/design/constraints.md §4.5). The text was written by renderTokens, so
// it re-lexes to a value-identical token sequence; the caller maps a failure to XX001
// (the file claimed to be well-formed).
export function parseExpression(text: string): Expr {
  const p = new Parser(lex(text));
  const expr = p.parseExpr();
  p.expectEof();
  return expr;
}

// renderTokens re-renders a token slice as the persisted check-expression text: each token
// rendered by the closed table in spec/fileformat/format.md "Check-expression text",
// joined with single spaces. A byte contract — identical across every core (CLAUDE.md §8).
export function renderTokens(tokens: Token[]): string {
  return tokens.map(renderToken).join(" ");
}

function renderToken(t: Token): string {
  switch (t.kind) {
    case "word":
      return t.word!;
    case "int":
      return t.int!.toString();
    case "decimal": {
      // The digit string with "." inserted `scale` digits from the right. The lexer
      // guarantees scale <= coeff.length (every fractional digit is in the coefficient),
      // so the insertion point is in range; scale == length renders a leading-dot form
      // (".5") and scale == 0 a trailing-dot form ("1."), both of which re-lex as the same
      // decimal value (spec/fileformat/format.md "Check-expression text").
      const split = t.decDigits!.length - t.decScale!;
      return t.decDigits!.slice(0, split) + "." + t.decDigits!.slice(split);
    }
    case "str":
      return "'" + t.str!.replaceAll("'", "''") + "'";
    case "param":
      return "$" + t.paramIndex!.toString();
    case "comma":
      return ",";
    case "dot":
      return ".";
    case "lparen":
      return "(";
    case "rparen":
      return ")";
    case "star":
      return "*";
    case "plus":
      return "+";
    case "minus":
      return "-";
    case "slash":
      return "/";
    case "percent":
      return "%";
    case "eq":
      return "=";
    case "lt":
      return "<";
    case "gt":
      return ">";
    case "le":
      return "<=";
    case "ge":
      return ">=";
    default: // "eof" — never inside the parentheses
      return "";
  }
}
