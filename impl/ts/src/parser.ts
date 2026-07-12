// Hand-written recursive-descent parser (CLAUDE.md §5, §10). Mirrors parser.go /
// parser.rs. Errors throw EngineError (42601). The lexer emits generic word tokens (no
// reserved-keyword table), so keywords are matched case-insensitively here.

import type {
  AlterColumnAction,
  AlterTableEdit,
  Assignment,
  BinaryOp,
  CheckDef,
  Cte,
  CteBody,
  DefaultDef,
  ExcludeDef,
  ForeignKeyDef,
  RefAction,
  UniqueDef,
  ColumnDef,
  Delete,
  Expr,
  GroupItem,
  IdentitySpec,
  IndexKeyElem,
  ConflictTarget,
  Insert,
  InsertValue,
  JoinClause,
  JoinKind,
  JsonOnBehavior,
  JsonPredicateKind,
  JsonTable,
  JsonWrapper,
  JtColumn,
  Literal,
  OnConflict,
  OrderKey,
  Overriding,
  QueryExpr,
  Select,
  SelectItem,
  SelectItems,
  SeqOptions,
  SeqRestart,
  SetOp,
  SetOpKind,
  Statement,
  SubscriptSpec,
  TableRef,
  TypeFieldDef,
  TypeMod,
  Update,
  WindowDef,
  WindowOrderKey,
  WindowFrame,
  FrameBound,
  FrameMode,
  FrameExclusion,
} from "./ast.ts";
import { emptySeqOptions, seqOptionsHasAny } from "./ast.ts";
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
    // USING introduces a join condition after the right table_ref (`JOIN b USING (k)`), so it
    // must not be swallowed as the right table's implicit alias (grammar.md §15).
    case "using":
    // NATURAL prefixes a join (`a NATURAL JOIN b`), so it must not be swallowed as the prior
    // relation's alias (grammar.md §15).
    case "natural":
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
    // WINDOW ends a SELECT core's FROM — it introduces the named-window clause and must not be
    // swallowed as an implicit table alias (`FROM t WINDOW w AS …`). window.md §5.
    case "window":
      return true;
    default:
      return false;
  }
}

// foldInt converts a lexed unsigned magnitude (<= 2^63) and a sign into a signed
// i64-range bigint, throwing 22003 when the result does not fit (a bare 2^63, or the
// not-negated 2^63). -(2^63) folds to i64's minimum (spec/design/grammar.md §4).
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

// classifyOrderKey classifies a parsed ORDER BY key expression into one of the three OrderKey modes
// (grammar.md §10). allowOrdinal matches PostgreSQL's rule that only a bare integer constant is an
// ordinal — and only in a query/set-operation ORDER BY: when set, an integer literal (positive, or
// negative via the parser's unary-minus-on-literal fold) is an ordinal; when clear (WITHIN GROUP), the
// same bare integer falls through to a constant expression key. A bare column reference — directly, or
// wrapped in a COLLATE that parseExpr absorbed (`ORDER BY name COLLATE "x"`) — is a column key carrying
// that collation, so it stays on the fast path (PK-scan elision, per-column collation); every other
// shape is a general expression key.
function classifyOrderKey(
  expr: Expr,
  collation: string | null,
  descending: boolean,
  nullsFirst: boolean,
  allowOrdinal: boolean,
): OrderKey {
  if (allowOrdinal && expr.kind === "literal" && expr.literal.kind === "int") {
    return {
      ordinal: Number(expr.literal.int),
      expr: null,
      qualifier: null,
      column: "",
      collation,
      descending,
      nullsFirst,
    };
  }
  if (expr.kind === "column") {
    return {
      ordinal: null,
      expr: null,
      qualifier: null,
      column: expr.name,
      collation,
      descending,
      nullsFirst,
    };
  }
  if (expr.kind === "qualifiedColumn") {
    return {
      ordinal: null,
      expr: null,
      qualifier: expr.qualifier,
      column: expr.name,
      collation,
      descending,
      nullsFirst,
    };
  }
  // parseExpr folds a trailing `COLLATE "x"` into the key (collation.md §1). When it wraps a bare
  // column, unwrap back to a column key carrying that explicit collation — exactly the column-only
  // OrderKey the old parser built, so the column fast path is byte-identical.
  if (expr.kind === "collate") {
    if (expr.inner.kind === "column") {
      return {
        ordinal: null,
        expr: null,
        qualifier: null,
        column: expr.inner.name,
        collation: expr.collation,
        descending,
        nullsFirst,
      };
    }
    if (expr.inner.kind === "qualifiedColumn") {
      return {
        ordinal: null,
        expr: null,
        qualifier: expr.inner.qualifier,
        column: expr.inner.name,
        collation: expr.collation,
        descending,
        nullsFirst,
      };
    }
  }
  return { ordinal: null, expr, qualifier: null, column: "", collation, descending, nullsFirst };
}

// parseSQL parses a single complete statement from sql.
// MAX_EXPR_DEPTH is the maximum expression / subquery / set-operation nesting depth a statement
// may reach (spec/design/cost.md §7; CLAUDE.md §13). The §13 native-stack-safety gate for
// untrusted input: the recursive-descent parser and the resolve/eval walks recurse to a
// statement's nesting depth, so deeply-nested SQL would overflow the call stack BEFORE the cost
// meter runs (54P01 cannot catch it; in this core an overflow is an uncatchable-by-design V8
// RangeError). Counting logical depth against this fixed bound — rather than PG's runtime
// stack-pointer probe — is deterministic and cross-core identical (§8): the constant is the SAME
// in every core (Rust / Go / TS). 256 sits with a >2× margin under the weakest core's native
// ceiling — THIS core, on a default Node/V8 stack, overflows at ~547 nested subqueries — yet far
// above any realistic query. Exceeding it throws 54001 statement_too_complex.
export const MAX_EXPR_DEPTH = 256;

// MAX_IDENTIFIER_LENGTH is enforced in the lexer (the identifier-token producer); re-exported here
// so the two parser-level limits sit together (spec/design/cost.md §7; CLAUDE.md §13).
export { MAX_IDENTIFIER_LENGTH } from "./lexer.ts";

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
  // Current expression/query nesting depth (see MAX_EXPR_DEPTH). Incremented once per AST level
  // descended (deepen), restored on the way back up; left stale on the error path because a depth
  // error aborts the whole parse.
  private depth: number;

  constructor(tokens: Token[]) {
    this.tokens = tokens;
    this.pos = 0;
    this.depth = 0;
  }

  // deepen descends one nesting level, enforcing MAX_EXPR_DEPTH (spec/design/cost.md §7). Call at
  // every point the AST gains a level — a binary-chain step, a unary, a postfix, a re-entry into a
  // fresh sub-expression, a nested subquery, a set-op branch. The caller restores the depth with
  // undeepen on the success path (a throw short-circuits, leaving it stale, which is harmless: the
  // parse is aborting).
  private deepen(): void {
    this.depth++;
    if (this.depth > MAX_EXPR_DEPTH) {
      throw engineError(
        "statement_too_complex",
        `statement too complex: nesting depth exceeds the maximum of ${MAX_EXPR_DEPTH}`,
      );
    }
  }

  // undeepen restores one nesting level taken by deepen (success path only).
  private undeepen(): void {
    this.depth--;
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
        // CREATE TYPE — a 2-token lookahead keeps TYPE non-reserved (the CREATE UNIQUE INDEX
        // precedent — composite.md §1).
        if (this.peekKeywordAt(1) === "type") return this.parseCreateType();
        // CREATE SEQUENCE — a 2-token lookahead keeps SEQUENCE non-reserved (sequences.md §1).
        if (this.peekKeywordAt(1) === "sequence") return this.parseCreateSequence();
        return this.parseCreateTable();
      case "drop":
        if (this.peekKeywordAt(1) === "index") return this.parseDropIndex();
        if (this.peekKeywordAt(1) === "type") return this.parseDropType();
        if (this.peekKeywordAt(1) === "sequence") return this.parseDropSequence();
        return this.parseDropTable();
      // ALTER SEQUENCE — the only ALTER statement this slice (sequences.md §4). A 2-token lookahead
      // recognizes it; any other `ALTER …` (TABLE, SYSTEM, …) is not a statement keyword jed knows
      // and falls through to the generic unknown-keyword 42601 (the no-escape-hatch surface).
      case "alter":
        if (this.peekKeywordAt(1) === "sequence") return this.parseAlterSequence();
        if (this.peekKeywordAt(1) === "table") return this.parseAlterTable();
        throw engineError("syntax_error", "unexpected keyword 'alter'");
      case "insert":
        return this.parseInsert();
      case "select":
        return this.parseQueryExpr();
      // `WITH …` at statement start can only begin a query with common table expressions
      // (spec/design/cte.md). `with` is non-reserved but unambiguous here.
      case "with":
        return this.parseWithStatement();
      case "update":
        return this.parseUpdate();
      case "delete":
        return this.parseDelete();
      case "explain":
        return this.parseExplain();
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

  // parseExplain parses `EXPLAIN [ANALYZE] <statement>` (spec/design/explain.md). EXPLAIN is a
  // positional leading keyword — non-reserved, no lookahead — followed by an optional ANALYZE modifier
  // and then a restricted inner statement (a query or DML). ANALYZE is consumed positionally: no inner
  // statement begins with the word ANALYZE, so there is no ambiguity.
  private parseExplain(): Statement {
    this.advance(); // EXPLAIN
    let analyze = false;
    if (this.peekKeyword() === "analyze") {
      this.advance();
      analyze = true;
    }
    return { kind: "explain", analyze, inner: this.parseExplainInner() };
  }

  // parseExplainInner parses the statement EXPLAIN wraps — restricted to a query (SELECT / WITH) or a
  // DML statement (INSERT / UPDATE / DELETE). DDL, transaction control, and a nested EXPLAIN have no
  // query plan to render and are rejected 42601.
  private parseExplainInner(): Statement {
    switch (this.peekKeyword()) {
      case "select":
        return this.parseQueryExpr();
      case "with":
        return this.parseWithStatement();
      case "insert":
        return this.parseInsert();
      case "update":
        return this.parseUpdate();
      case "delete":
        return this.parseDelete();
      case "":
        throw engineError("syntax_error", "expected a statement after EXPLAIN");
      default:
        throw engineError("syntax_error", `EXPLAIN does not support '${this.peekKeyword()}'`);
    }
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
    // An optional table_scope between CREATE and TABLE makes the table TEMPORARY
    // (spec/design/temp-tables.md, grammar.ebnf `table_scope`). TEMP / TEMPORARY are NOT reserved (§3):
    // recognized positionally here — the word after TABLE is always the table name, so
    // `CREATE TABLE temp (...)` is an ordinary persistent table named "temp".
    const temp = this.peekKeyword() === "temp" || this.peekKeyword() === "temporary";
    if (temp) this.advance();
    this.expectKeyword("table");
    // An optional database qualifier `db.table` (attached-databases.md §3, Slice 1b): create the table
    // INTO the named database (`main` / `temp` / a host attachment). A bare name uses the implicit
    // scope. The `.` after the first identifier makes it the qualifier and the next the table name.
    const [db, name] = this.parseQualifiedTableName();
    this.expect("lparen");

    const columns: ColumnDef[] = [];
    const tablePks: string[][] = [];
    const checks: CheckDef[] = [];
    const uniques: UniqueDef[] = [];
    const fks: ForeignKeyDef[] = [];
    const excludes: ExcludeDef[] = [];
    for (;;) {
      if (this.peekKeyword() === "primary" && this.peekKeywordAt(1) === "key") {
        this.advance();
        this.advance();
        tablePks.push(this.parsePkColumnList());
      } else if (this.atCheckConstraint()) {
        checks.push(this.parseCheckConstraint());
      } else if (this.atUniqueTableConstraint()) {
        uniques.push(this.parseUniqueTableConstraint());
      } else if (this.atForeignKeyTableConstraint()) {
        fks.push(this.parseForeignKeyTableConstraint());
      } else if (this.atExclusionTableConstraint()) {
        excludes.push(this.parseExclusionTableConstraint());
      } else {
        columns.push(this.parseColumnDef(name, checks, uniques, fks));
      }
      const k = this.advance().kind;
      if (k === "comma") continue;
      if (k === "rparen") break;
      throw engineError("syntax_error", "expected ',' or ')'");
    }
    if (columns.length === 0) {
      throw engineError("syntax_error", "a table must have at least one column");
    }
    return {
      kind: "createTable",
      name,
      db,
      temp,
      columns,
      tablePks,
      checks,
      uniques,
      fks,
      excludes,
    };
  }

  // atExclusionTableConstraint reports whether the cursor sits on a table-level EXCLUDE constraint:
  // the keyword EXCLUDE (followed by USING or `(`), or CONSTRAINT <ident> EXCLUDE
  // (spec/design/gist.md §7). The keyword stays non-reserved — a column named "exclude" is followed
  // by a type name (an identifier), never USING or `(`, so the lookahead loses nothing.
  private atExclusionTableConstraint(): boolean {
    if (
      this.peekKeyword() === "exclude" &&
      (this.peekKeywordAt(1) === "using" || this.peekKindAt(1) === "lparen")
    ) {
      return true;
    }
    return this.peekKeyword() === "constraint" && this.peekKeywordAt(2) === "exclude";
  }

  // parseExclusionTableConstraint parses one `[CONSTRAINT name] EXCLUDE [USING method] ( col WITH op
  // [, col2 WITH op2 ...] )` (the cursor is verified by atExclusionTableConstraint). Each operand is
  // a bare column name; the WITH operator is captured as its source text (= / &&) and mapped to a
  // strategy at execution (spec/design/gist.md §7). The USING method (only gist) is captured verbatim.
  private parseExclusionTableConstraint(): ExcludeDef {
    let name: string | null = null;
    if (this.peekKeyword() === "constraint") {
      this.advance();
      name = this.expectIdentifier();
    }
    this.expectKeyword("exclude");
    let using: string | null = null;
    if (this.peekKeyword() === "using") {
      this.advance();
      using = this.expectIdentifier();
    }
    this.expect("lparen");
    const elements: { column: string; op: string }[] = [];
    for (;;) {
      const column = this.expectIdentifier();
      this.expectKeyword("with");
      // The operator is a single token (= / &&); render it to source text for execution.
      const start = this.pos;
      this.advance();
      const op = renderTokens(this.tokens.slice(start, this.pos));
      elements.push({ column, op });
      const k = this.advance().kind;
      if (k === "comma") continue;
      if (k === "rparen") break;
      throw engineError("syntax_error", "expected ',' or ')'");
    }
    return { name, using, elements };
  }

  // atForeignKeyTableConstraint reports whether the cursor sits on a table-level FOREIGN KEY
  // constraint: the two keywords FOREIGN KEY, or CONSTRAINT <ident> FOREIGN KEY
  // (spec/design/grammar.md §43). The keywords stay non-reserved — a column named "foreign"
  // would need a type named "key" (none exists), so the lookahead loses nothing (the PRIMARY KEY
  // precedent).
  private atForeignKeyTableConstraint(): boolean {
    if (this.peekKeyword() === "foreign" && this.peekKeywordAt(1) === "key") return true;
    return (
      this.peekKeyword() === "constraint" &&
      this.peekKeywordAt(2) === "foreign" &&
      this.peekKeywordAt(3) === "key"
    );
  }

  // parseForeignKeyTableConstraint parses one table-level `[CONSTRAINT name] FOREIGN KEY
  // ( col [, col]* ) references_clause` (the cursor is verified by
  // atForeignKeyTableConstraint). The local-column list reuses the PRIMARY KEY list shape
  // (spec/design/grammar.md §43).
  private parseForeignKeyTableConstraint(): ForeignKeyDef {
    let name: string | null = null;
    if (this.peekKeyword() === "constraint") {
      this.advance();
      name = this.expectIdentifier();
    }
    this.expectKeyword("foreign");
    this.expectKeyword("key");
    const columns = this.parsePkColumnList();
    const { refTable, refColumns, onDelete, onUpdate } = this.parseReferencesClause();
    return { name, columns, refTable, refColumns, onDelete, onUpdate };
  }

  // parseReferencesClause parses a references_clause from the REFERENCES keyword onward (shared
  // by the column-level and table-level forms — spec/design/grammar.md §43): the referenced
  // table, an optional referenced-column list (null defaults to the parent's primary key), and
  // the `ON DELETE` / `ON UPDATE` actions (each at most once, either order; a repeat is 42601).
  private parseReferencesClause(): {
    refTable: string;
    refColumns: string[] | null;
    onDelete: RefAction;
    onUpdate: RefAction;
  } {
    this.expectKeyword("references");
    const refTable = this.expectIdentifier();
    const refColumns = this.peek().kind === "lparen" ? this.parsePkColumnList() : null;
    let onDelete: RefAction = "noAction";
    let onUpdate: RefAction = "noAction";
    let seenDelete = false;
    let seenUpdate = false;
    while (this.peekKeyword() === "on") {
      this.advance();
      const kw = this.peekKeyword();
      if (kw === "delete") {
        this.advance();
        if (seenDelete) throw engineError("syntax_error", "ON DELETE specified more than once");
        seenDelete = true;
        onDelete = this.parseReferentialAction();
      } else if (kw === "update") {
        this.advance();
        if (seenUpdate) throw engineError("syntax_error", "ON UPDATE specified more than once");
        seenUpdate = true;
        onUpdate = this.parseReferentialAction();
      } else {
        throw engineError("syntax_error", "expected DELETE or UPDATE after ON");
      }
    }
    return { refTable, refColumns, onDelete, onUpdate };
  }

  // parseReferentialAction parses one referential_action (spec/design/grammar.md §43). All five
  // PG actions parse; CASCADE / SET NULL / SET DEFAULT are rejected later at CREATE TABLE (0A000).
  private parseReferentialAction(): RefAction {
    const kw = this.peekKeyword();
    if (kw === "no") {
      this.advance();
      this.expectKeyword("action");
      return "noAction";
    }
    if (kw === "restrict") {
      this.advance();
      return "restrict";
    }
    if (kw === "cascade") {
      this.advance();
      return "cascade";
    }
    if (kw === "set") {
      this.advance();
      const next = this.peekKeyword();
      if (next === "null") {
        this.advance();
        return "setNull";
      }
      if (next === "default") {
        this.advance();
        return "setDefault";
      }
      throw engineError("syntax_error", "expected NULL or DEFAULT after SET");
    }
    throw engineError(
      "syntax_error",
      "expected a referential action: NO ACTION / RESTRICT / CASCADE / SET NULL / SET DEFAULT",
    );
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

  // Whether the cursor sits on a table-level PRIMARY KEY constraint, including the named form.
  // ALTER TABLE uses this to keep the authoritative-but-deferred ADD PRIMARY KEY grammar on its
  // 0A000 path instead of parsing `primary key` as a column name and type.
  private atPrimaryKeyTableConstraint(): boolean {
    if (this.peekKeyword() === "primary" && this.peekKeywordAt(1) === "key") return true;
    return (
      this.peekKeyword() === "constraint" &&
      this.peekKeywordAt(2) === "primary" &&
      this.peekKeywordAt(3) === "key"
    );
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

  private parseColumnDef(
    tableName: string,
    checks: CheckDef[],
    uniques: UniqueDef[],
    fks: ForeignKeyDef[],
  ): ColumnDef {
    const name = this.expectIdentifier();
    const baseType = this.expectIdentifier();
    const typeMod = this.parseTypeMod();
    const typeName = this.consumeArrayBrackets() ? baseType + "[]" : baseType;
    // Zero or more order-free column constraints: PRIMARY KEY, NOT NULL, DEFAULT <literal>,
    // [CONSTRAINT name] CHECK ( expr ), and [CONSTRAINT name] UNIQUE. A boolean constraint
    // may be repeated harmlessly; a repeated DEFAULT keeps the last; each CHECK is a
    // distinct constraint, collected into the statement-wide list in textual order (a
    // column-level check is semantically identical to a table-level one —
    // spec/design/constraints.md §4). A column-level UNIQUE collects the same way as the
    // one-member form (a repeat folds at execution — spec/design/constraints.md §5).
    let primaryKey = false;
    let notNull = false;
    let def: DefaultDef | null = null;
    let identity: IdentitySpec | null = null;
    let collation: string | null = null;
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
      // CONSTRAINT <name> REFERENCES … in column position (the named one-member FK).
      if (this.peekKeyword() === "constraint" && this.peekKeywordAt(2) === "references") {
        this.advance();
        const cname = this.expectIdentifier();
        const { refTable, refColumns, onDelete, onUpdate } = this.parseReferencesClause();
        fks.push({
          name: cname,
          columns: [name],
          refTable,
          refColumns,
          onDelete,
          onUpdate,
        });
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
        // A DEFAULT takes any scalar expression (constraints.md §2). Capture the re-rendered
        // token span as the persisted text (format.md "Check-expression text"), as a CHECK
        // does — the executor classifies a bare literal (constant fast-path) vs an expression
        // (text-persisted).
        const start = this.pos;
        const expr = this.parseExpr();
        const text = renderTokens(this.tokens.slice(start, this.pos));
        def = { expr, text };
      } else if (kw === "generated") {
        // `GENERATED { ALWAYS | BY DEFAULT } AS IDENTITY [( seq_options )]`
        // (spec/design/sequences.md §13). Two identity specs on one column is 42601
        // ("multiple identity specifications"). The desugaring (owned sequence + nextval default +
        // NOT NULL + the type gate) is at execution.
        this.advance();
        let always: boolean;
        const akw = this.peekKeyword();
        if (akw === "always") {
          this.advance();
          always = true;
        } else if (akw === "by") {
          this.advance();
          this.expectKeyword("default");
          always = false;
        } else {
          throw engineError(
            "syntax_error",
            `expected ALWAYS or BY DEFAULT after GENERATED, found '${akw}'`,
          );
        }
        this.expectKeyword("as");
        this.expectKeyword("identity");
        const options =
          this.peek().kind === "lparen" ? this.parseSequenceOptions(true) : emptySeqOptions();
        if (identity !== null) {
          throw engineError(
            "syntax_error",
            `multiple identity specifications for column ${name} of table ${tableName}`,
          );
        }
        identity = { always, options };
      } else if (kw === "collate") {
        // COLLATE "name" in column position (spec/design/collation.md §1) — a quoted, case-sensitive
        // collation name. Validity (text-only 42804, loaded name 42704) is checked at execution. A
        // repeat keeps the last (like DEFAULT).
        this.advance();
        collation = this.expectCollationName();
      } else if (kw === "unique") {
        this.advance();
        uniques.push({ name: null, columns: [name] });
      } else if (kw === "references") {
        // The column-level one-member FK: `REFERENCES parent [(col)] [actions]`.
        // parseReferencesClause consumes the REFERENCES keyword itself.
        const { refTable, refColumns, onDelete, onUpdate } = this.parseReferencesClause();
        fks.push({
          name: null,
          columns: [name],
          refTable,
          refColumns,
          onDelete,
          onUpdate,
        });
      } else {
        break;
      }
    }
    return {
      name,
      typeName,
      typeMod,
      primaryKey,
      notNull,
      default: def,
      identity,
      collation,
    };
  }

  // parseTypeMod parses an optional parenthesized type modifier `"(" integer ("," integer)? ")"`
  // after a type name (the first parameterized type, decimal — spec/grammar/grammar.ebnf
  // type_name). The shape is accepted for any type name; whether a typmod is meaningful (decimal
  // only) and in range is decided at resolve. Empty parens or a non-integer inside is 42601.
  // consumeArrayBrackets consumes a trailing array type suffix `[]` (spec/design/array.md §1) after
  // a type name (and its optional typmod). Returns whether the type is an array. Multiple `[][]`
  // collapse to one array level — multidimensionality is a value property (§2). Only the empty
  // bracket form `[]` is accepted this slice.
  private consumeArrayBrackets(): boolean {
    let isArray = false;
    while (this.peek().kind === "lbracket") {
      this.advance(); // "["
      this.expect("rbracket");
      isArray = true;
    }
    return isArray;
  }

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

  // parseDropTable parses `DROP TABLE [IF EXISTS] <name> [, …] [CASCADE | RESTRICT]`.
  // Existence/dependency are resolved at execution time (42P01 — or a no-op when IF EXISTS is
  // present — and 2BP01), not here. A comma list collects several names; the trailing
  // CASCADE/RESTRICT keyword sets the FK-dependency mode (RESTRICT is the default)
  // (spec/design/grammar.md §13). IF EXISTS is recognized only when the next two keywords are
  // exactly IF EXISTS (the two-token lookahead the statement dispatch uses) — a lone `if` is an
  // ordinary non-reserved identifier, so `DROP TABLE if` drops a table named `if` (PG-faithful, §1).
  private parseDropTable(): Statement {
    this.expectKeyword("drop");
    this.expectKeyword("table");
    let ifExists = false;
    if (this.peekKeyword() === "if" && this.peekKeywordAt(1) === "exists") {
      this.advance(); // IF
      this.advance(); // EXISTS
      ifExists = true;
    }
    const names = [this.expectIdentifier()];
    while (this.peek().kind === "comma") {
      this.advance();
      names.push(this.expectIdentifier());
    }
    // The trailing dependency mode is optional; RESTRICT is the default (and the only mode the
    // bare form ever had). Anything else after the name list is trailing input (the dispatch's
    // end-of-statement check raises 42601).
    let cascade = false;
    if (this.peekKeyword() === "cascade") {
      this.advance();
      cascade = true;
    } else if (this.peekKeyword() === "restrict") {
      this.advance();
    }
    return { kind: "dropTable", names, ifExists, cascade };
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
    // The unnamed form is `INDEX ON <table> [USING <method>] (` — the word after INDEX is the
    // index name unless it is `ON` followed by a word and then `(` OR `USING` (the three-token
    // lookahead, extended for the optional USING clause — grammar.md §30, gin.md §3).
    const unnamed =
      this.peekKeyword() === "on" &&
      this.peekKindAt(1) === "word" &&
      (this.peekKindAt(2) === "lparen" || this.peekKeywordAt(2) === "using");
    const name = unnamed ? null : this.expectIdentifier();
    this.expectKeyword("on");
    // An optional database qualifier `db.table` on the target table (attached-databases.md §3, Slice
    // 1b): build the index ON a table in the named database (`main` / `temp` / a host attachment).
    const [db, table] = this.parseQualifiedTableName();
    // Optional `USING <method>` between the table name and the column list (PG order — gin.md §3,
    // grammar.md §30). Not reserved (positional); the method is resolved at execution (42704 if
    // unknown), not here.
    let using: string | undefined;
    if (this.peekKeyword() === "using") {
      this.advance();
      using = this.expectIdentifier();
    }
    this.expect("lparen");
    const keys: IndexKeyElem[] = [];
    for (;;) {
      keys.push(this.parseIndexElement());
      const tok = this.advance();
      if (tok.kind === "comma") continue;
      if (tok.kind === "rparen") break;
      throw engineError("syntax_error", `expected ',' or ')', found ${tok.kind}`);
    }
    // An optional trailing `WHERE predicate` makes the index PARTIAL (indexes.md §9). `where` is
    // recognized positionally after the closing `)` (non-reserved); its text is captured for the
    // canonical persisted form (like CHECK/DEFAULT).
    let predicate: { text: string; expr: Expr } | undefined;
    if (this.peekKeyword() === "where") {
      this.advance();
      const start = this.pos;
      const expr = this.parseExpr();
      predicate = { text: renderTokens(this.tokens.slice(start, this.pos)), expr };
    }
    return { kind: "createIndex", name, table, db, keys, unique, using, predicate };
  }

  // parseIndexElement parses one `index_element` (grammar.md §30, indexes.md §1): a bare column,
  // a bare function call (`lower(email)`), or a parenthesized expression (`(a + b)`). PostgreSQL's
  // `index_elem`: a general operator expression must be parenthesized (a bare `a + b` errors —
  // parsePrimary stops before the operator, so the element loop then sees an unexpected token); a
  // parenthesized bare column `(a)` normalizes to a column key. The token span between the delimiters
  // is re-rendered as the persisted canonical text (format.md "Check-expression text"), like CHECK.
  private parseIndexElement(): IndexKeyElem {
    if (this.peek().kind === "lparen") {
      // `( expr )` — any parenthesized expression.
      this.advance();
      const start = this.pos;
      const expr = this.parseExpr();
      const text = renderTokens(this.tokens.slice(start, this.pos));
      this.expect("rparen");
      return this.indexKeyFromExpr(expr, text);
    }
    if (this.peek().kind === "word" && this.peekKindAt(1) === "lparen") {
      // A bare function call `f(args)` — parse ONLY the primary, so a trailing operator
      // (`lower(x) + 1`) leaves `+` for the element loop to reject (PG requires parens).
      const start = this.pos;
      const expr = this.parsePrimary();
      const text = renderTokens(this.tokens.slice(start, this.pos));
      return this.indexKeyFromExpr(expr, text);
    }
    // A bare column name.
    return { kind: "column", name: this.expectIdentifier() };
  }

  // indexKeyFromExpr classifies a parsed index-element expression: a bare column reference (`a`,
  // `(a)`, `((a))`) becomes a column key (PG-matched), anything else an expression key carrying its
  // canonical text (rendered from the captured token span, like CHECK/DEFAULT).
  private indexKeyFromExpr(expr: Expr, text: string): IndexKeyElem {
    if (expr.kind === "column") return { kind: "column", name: expr.name };
    return { kind: "expr", text, expr };
  }

  // parseDropIndex parses `DROP INDEX <name>` (spec/design/grammar.md §30). A missing
  // index (42704) or a table's name (42809) is rejected at execution time, not here.
  private parseDropIndex(): Statement {
    this.expectKeyword("drop");
    this.expectKeyword("index");
    const name = this.expectIdentifier();
    return { kind: "dropIndex", name };
  }

  // parseCreateType parses `CREATE TYPE <name> AS ( <field> <type> [NOT NULL] [, …] )` — a
  // composite (row) type (spec/design/composite.md, grammar.md). At least one field (an empty
  // list is a syntax error); each field's type is a bare type name (built-in or a composite),
  // resolved at execution (42704 if unknown).
  private parseCreateType(): Statement {
    this.expectKeyword("create");
    this.expectKeyword("type");
    const name = this.expectIdentifier();
    this.expectKeyword("as");
    this.expect("lparen");
    const fields = this.parseFieldDefList();
    return { kind: "createType", name, fields };
  }

  // parseFieldDefList parses a `( field type [numeric(p,s)] [[]] [NOT NULL] [, …] )`
  // field-definition list — the body shared by `CREATE TYPE … AS (…)` (composite.md) and a
  // FROM-clause **column-definition list** `AS t(col type, …)` (C0, json-table.md §1). The caller
  // has consumed the opening `(`; this consumes through the matching `)`.
  private parseFieldDefList(): TypeFieldDef[] {
    const fields: TypeFieldDef[] = [];
    for (;;) {
      const fname = this.expectIdentifier();
      const baseType = this.expectIdentifier();
      const typeMod = this.parseTypeMod();
      // An array-typed field (`xs i32[]`) — the same `[]` suffix a column type takes
      // (spec/design/array.md §12); the canonical spelling carries the brackets.
      const typeName = this.consumeArrayBrackets() ? baseType + "[]" : baseType;
      let notNull = false;
      if (this.peekKeyword() === "not") {
        this.advance();
        this.expectKeyword("null");
        notNull = true;
      }
      fields.push({ name: fname, typeName, typeMod, notNull });
      const tok = this.advance();
      if (tok.kind === "comma") continue;
      if (tok.kind === "rparen") break;
      throw engineError("syntax_error", `expected ',' or ')', found ${tok.kind}`);
    }
    return fields;
  }

  // parseDropType parses `DROP TYPE [IF EXISTS] <name> [RESTRICT | CASCADE]`
  // (spec/design/composite.md §7). RESTRICT is the default and the only behavior this slice;
  // CASCADE is rejected (0A000) at parse. A missing type (42704) and dependents (2BP01) are
  // execution-time.
  private parseDropType(): Statement {
    this.expectKeyword("drop");
    this.expectKeyword("type");
    let ifExists = false;
    if (this.peekKeyword() === "if") {
      this.advance();
      this.expectKeyword("exists");
      ifExists = true;
    }
    const name = this.expectIdentifier();
    // Optional trailing RESTRICT / CASCADE (a keyword, consumed here; CASCADE is 0A000).
    let cascade = false;
    if (this.peekKeyword() === "restrict") {
      this.advance();
    } else if (this.peekKeyword() === "cascade") {
      this.advance();
      cascade = true;
    }
    if (cascade) {
      throw engineError("feature_not_supported", "DROP TYPE ... CASCADE is not supported");
    }
    return { kind: "dropType", name, ifExists };
  }

  // parseCreateSequence parses `CREATE SEQUENCE [IF NOT EXISTS] <name> [options]`
  // (spec/design/sequences.md §1). The options are order-free and each at most once (a repeat is
  // 42601); option values are signed integer literals. Validation of the resolved option set
  // (22023) and the namespace collision (42P07) are execution-time.
  private parseCreateSequence(): Statement {
    this.expectKeyword("create");
    this.expectKeyword("sequence");
    const ifNotExists = this.parseIfNotExists();
    const name = this.expectIdentifier();
    const options = this.parseSequenceOptions(false);
    return { kind: "createSequence", name, ifNotExists, options };
  }

  // parseSequenceOptions parses the order-free sequence-option set (`INCREMENT [BY] n`,
  // `MINVALUE`/`MAXVALUE` and their `NO` forms, `START [WITH] n`, `CACHE c`, `[NO] CYCLE`) shared by
  // CREATE SEQUENCE and an IDENTITY column's `( seq_options )` (spec/design/sequences.md §13). When
  // `parenthesized`, the options are wrapped in `( … )` and the loop stops at `)`; each option
  // appears at most once (a repeat is 42601 via dupCheck). Validation of the resolved set (22023) is
  // execution-time.
  private parseSequenceOptions(parenthesized: boolean): SeqOptions {
    return this.parseSeqOptionsInner(parenthesized, false).options;
  }

  // parseSeqOptionsInner is the shared option loop. When `allowRestart` (only on ALTER SEQUENCE,
  // never parenthesized), `RESTART [[WITH] n]` is also accepted as an interleavable pseudo-option and
  // returned separately (null = absent; `{ toStart: true }` = bare RESTART; `{ value: n }` = RESTART
  // WITH n); RESTART is invalid in CREATE/identity, where it ends the loop like any other keyword.
  private parseSeqOptionsInner(
    parenthesized: boolean,
    allowRestart: boolean,
  ): { options: SeqOptions; restart: SeqRestart | null } {
    if (parenthesized) this.expect("lparen");
    const seq = emptySeqOptions();
    let restart: SeqRestart | null = null;
    // Order-free option loop: dispatch on the leading keyword, each option at most once.
    for (;;) {
      switch (this.peekKeyword()) {
        // `RESTART [[WITH] n]` — only on ALTER; resets the counter (sequences.md §15).
        case "restart": {
          if (!allowRestart) {
            if (parenthesized) this.expect("rparen");
            return { options: seq, restart };
          }
          this.dupCheck(restart !== null, "RESTART");
          this.advance();
          if (
            this.peek().kind === "int" ||
            this.peek().kind === "minus" ||
            this.peekKeyword() === "with"
          ) {
            this.consumeKeyword("with");
            restart = { toStart: false, value: this.parseSignedIntLiteral() };
          } else {
            restart = { toStart: true };
          }
          break;
        }
        // `AS <type>` — the sequence value type (order-free, S5 — sequences.md §14). The raw type
        // name is stored; it is resolved (and a non-integer type rejected 22023) at execution.
        // Inside an IDENTITY column's `( … )` a set dataType is 42601.
        case "as": {
          this.dupCheck(seq.dataType !== null, "AS");
          this.advance();
          seq.dataType = this.expectIdentifier();
          break;
        }
        case "increment": {
          this.dupCheck(seq.increment !== null, "INCREMENT");
          this.advance();
          this.consumeKeyword("by");
          seq.increment = this.parseSignedIntLiteral();
          break;
        }
        case "minvalue": {
          this.dupCheck(seq.minValue !== null, "MINVALUE");
          this.advance();
          seq.minValue = { value: this.parseSignedIntLiteral() };
          break;
        }
        case "maxvalue": {
          this.dupCheck(seq.maxValue !== null, "MAXVALUE");
          this.advance();
          seq.maxValue = { value: this.parseSignedIntLiteral() };
          break;
        }
        case "start": {
          this.dupCheck(seq.start !== null, "START");
          this.advance();
          this.consumeKeyword("with");
          seq.start = this.parseSignedIntLiteral();
          break;
        }
        case "cache": {
          this.dupCheck(seq.cache !== null, "CACHE");
          this.advance();
          seq.cache = this.parseSignedIntLiteral();
          break;
        }
        case "cycle": {
          this.dupCheck(seq.cycle !== null, "CYCLE");
          this.advance();
          seq.cycle = true;
          break;
        }
        // `NO MINVALUE` / `NO MAXVALUE` / `NO CYCLE`.
        case "no": {
          this.advance();
          switch (this.peekKeyword()) {
            case "minvalue":
              this.dupCheck(seq.minValue !== null, "MINVALUE");
              this.advance();
              seq.minValue = { value: null };
              break;
            case "maxvalue":
              this.dupCheck(seq.maxValue !== null, "MAXVALUE");
              this.advance();
              seq.maxValue = { value: null };
              break;
            case "cycle":
              this.dupCheck(seq.cycle !== null, "CYCLE");
              this.advance();
              seq.cycle = false;
              break;
            default:
              throw engineError(
                "syntax_error",
                `expected MINVALUE, MAXVALUE, or CYCLE after NO, found '${this.peekKeyword()}'`,
              );
          }
          break;
        }
        default:
          if (parenthesized) this.expect("rparen");
          return { options: seq, restart };
      }
    }
  }

  // parseDropSequence parses `DROP SEQUENCE [IF EXISTS] <name> [, …] [RESTRICT | CASCADE]`
  // (spec/design/sequences.md §1). CASCADE is 0A000 at parse; a missing sequence (42P01) is
  // execution-time.
  private parseDropSequence(): Statement {
    this.expectKeyword("drop");
    this.expectKeyword("sequence");
    let ifExists = false;
    if (this.peekKeyword() === "if") {
      this.advance();
      this.expectKeyword("exists");
      ifExists = true;
    }
    const names = [this.expectIdentifier()];
    while (this.peek().kind === "comma") {
      this.advance();
      names.push(this.expectIdentifier());
    }
    let cascade = false;
    if (this.peekKeyword() === "restrict") {
      this.advance();
    } else if (this.peekKeyword() === "cascade") {
      this.advance();
      cascade = true;
    }
    if (cascade) {
      throw engineError("feature_not_supported", "DROP SEQUENCE ... CASCADE is not supported");
    }
    return { kind: "dropSequence", names, ifExists };
  }

  // Parse ALTER TABLE's authoritative grammar frame (alter.md §1). Slices 1-4 execute RENAME,
  // ADD/DROP COLUMN, catalog-only ALTER COLUMN edits, and ADD/DROP non-PK constraints.
  private parseAlterTable(): Statement {
    this.expectKeyword("alter");
    this.expectKeyword("table");
    let ifExists = false;
    if (this.peekKeyword() === "if") {
      this.advance();
      this.expectKeyword("exists");
      ifExists = true;
    }
    const [db, name] = this.parseQualifiedTableName();
    if (this.peekKeyword() === "rename") {
      this.advance();
      if (this.peekKeyword() === "to") {
        this.advance();
        return {
          kind: "alterTable",
          name,
          db,
          ifExists,
          action: { kind: "renameTable", newName: this.expectIdentifier() },
        };
      }
      if (this.peekKeyword() === "constraint") {
        this.advance();
        const oldName = this.expectIdentifier();
        this.expectKeyword("to");
        return {
          kind: "alterTable",
          name,
          db,
          ifExists,
          action: { kind: "renameConstraint", oldName, newName: this.expectIdentifier() },
        };
      }
      if (this.peekKeyword() === "column") this.advance();
      const oldName = this.expectIdentifier();
      this.expectKeyword("to");
      return {
        kind: "alterTable",
        name,
        db,
        ifExists,
        action: { kind: "renameColumn", oldName, newName: this.expectIdentifier() },
      };
    }
    const actions: AlterTableEdit[] = [];
    for (;;) {
      if (this.peekKeyword() === "add") {
        this.advance();
        const columnNoise = this.peekKeyword() === "column";
        if (columnNoise) this.advance();
        let addIfNotExists = false;
        if (this.peekKeyword() === "if") {
          this.advance();
          this.expectKeyword("not");
          this.expectKeyword("exists");
          addIfNotExists = true;
        }
        if (
          columnNoise ||
          addIfNotExists ||
          !(
            this.atCheckConstraint() ||
            this.atUniqueTableConstraint() ||
            this.atForeignKeyTableConstraint() ||
            this.atExclusionTableConstraint() ||
            this.atPrimaryKeyTableConstraint()
          )
        ) {
          const checks: CheckDef[] = [];
          const uniques: UniqueDef[] = [];
          const foreignKeys: ForeignKeyDef[] = [];
          const column = this.parseColumnDef(name, checks, uniques, foreignKeys);
          actions.push({
            kind: "addColumn",
            column,
            checks,
            uniques,
            foreignKeys,
            ifNotExists: addIfNotExists,
          });
          if (this.peek().kind !== "comma") break;
          this.advance();
          continue;
        }
        const constraint = this.atCheckConstraint()
          ? { kind: "check" as const, def: this.parseCheckConstraint() }
          : this.atUniqueTableConstraint()
            ? { kind: "unique" as const, def: this.parseUniqueTableConstraint() }
            : this.atForeignKeyTableConstraint()
              ? { kind: "foreignKey" as const, def: this.parseForeignKeyTableConstraint() }
              : this.atExclusionTableConstraint()
                ? { kind: "exclude" as const, def: this.parseExclusionTableConstraint() }
                : null;
        if (constraint === null)
          throw engineError(
            "feature_not_supported",
            "ALTER TABLE ... ADD PRIMARY KEY is not supported yet",
          );
        actions.push({ kind: "addConstraint", constraint });
      } else if (this.peekKeyword() === "drop") {
        this.advance();
        const constraint = this.peekKeyword() === "constraint";
        if (constraint || this.peekKeyword() === "column") this.advance();
        let dropIfExists = false;
        if (this.peekKeyword() === "if") {
          this.advance();
          this.expectKeyword("exists");
          dropIfExists = true;
        }
        const name = this.expectIdentifier();
        let cascade = false;
        if (this.peekKeyword() === "cascade") {
          this.advance();
          cascade = true;
        } else if (this.peekKeyword() === "restrict") this.advance();
        actions.push(
          constraint
            ? { kind: "dropConstraint", name, ifExists: dropIfExists, cascade }
            : { kind: "dropColumn", name, ifExists: dropIfExists, cascade },
        );
      } else {
        this.expectKeyword("alter");
        if (this.peekKeyword() === "column") this.advance();
        const column = this.expectIdentifier();
        let edit: AlterColumnAction;
        if (this.peekKeyword() === "set") {
          this.advance();
          if (this.peekKeyword() === "default") {
            this.advance();
            const start = this.pos;
            const expr = this.parseExpr();
            edit = {
              column,
              action: {
                kind: "setDefault",
                default: { expr, text: renderTokens(this.tokens.slice(start, this.pos)) },
              },
            };
          } else if (this.peekKeyword() === "data")
            throw engineError(
              "feature_not_supported",
              "ALTER COLUMN ... TYPE is not supported yet",
            );
          else {
            this.expectKeyword("not");
            this.expectKeyword("null");
            edit = { column, action: { kind: "setNotNull" } };
          }
        } else if (this.peekKeyword() === "drop") {
          this.advance();
          if (this.peekKeyword() === "default") {
            this.advance();
            edit = { column, action: { kind: "dropDefault" } };
          } else {
            this.expectKeyword("not");
            this.expectKeyword("null");
            edit = { column, action: { kind: "dropNotNull" } };
          }
        } else if (this.peekKeyword() === "type")
          throw engineError("feature_not_supported", "ALTER COLUMN ... TYPE is not supported yet");
        else throw engineError("syntax_error", "ALTER COLUMN requires SET or DROP");
        actions.push({ kind: "alterColumn", edit });
      }
      if (this.peek().kind !== "comma") break;
      this.advance();
    }
    return { kind: "alterTable", name, db, ifExists, action: { kind: "actions", actions } };
  }

  // parseAlterSequence parses `ALTER SEQUENCE [IF EXISTS] <name> <action>` (spec/design/sequences.md
  // §15). After the name the next keyword dispatches: RENAME → the rename form; OWNED/OWNER/SET →
  // 0A000; otherwise the order-free option loop (the CREATE options plus an interleavable RESTART),
  // requiring ≥ 1 option (a bare ALTER SEQUENCE s is 42601). AS is parsed into the option set and
  // rejected as 0A000 at execution.
  private parseAlterSequence(): Statement {
    this.expectKeyword("alter");
    this.expectKeyword("sequence");
    let ifExists = false;
    if (this.peekKeyword() === "if") {
      this.advance();
      this.expectKeyword("exists");
      ifExists = true;
    }
    const name = this.expectIdentifier();
    switch (this.peekKeyword()) {
      case "rename": {
        this.advance();
        this.expectKeyword("to");
        const newName = this.expectIdentifier();
        return {
          kind: "alterSequence",
          name,
          ifExists,
          action: { kind: "rename", newName },
        };
      }
      // The remaining unsupported ALTER actions are 0A000 (not syntax errors).
      case "owned":
      case "owner":
      case "set":
        throw engineError("feature_not_supported", "this ALTER SEQUENCE action is not supported");
      default: {
        const { options, restart } = this.parseSeqOptionsInner(false, true);
        // ≥ 1 action required: a bare ALTER SEQUENCE s (no option, no RESTART) is 42601.
        if (restart === null && !seqOptionsHasAny(options)) {
          throw engineError("syntax_error", "ALTER SEQUENCE requires at least one action");
        }
        return {
          kind: "alterSequence",
          name,
          ifExists,
          action: { kind: "setOptions", options, restart },
        };
      }
    }
  }

  // parseIfNotExists consumes an optional `IF NOT EXISTS` prefix, returning whether it was present.
  private parseIfNotExists(): boolean {
    if (this.peekKeyword() === "if") {
      this.advance();
      this.expectKeyword("not");
      this.expectKeyword("exists");
      return true;
    }
    return false;
  }

  // consumeKeyword consumes an optional noise keyword (e.g. the `BY` in `INCREMENT BY`, the
  // `WITH` in `START WITH`) when present.
  private consumeKeyword(kw: string): void {
    if (this.peekKeyword() === kw) this.advance();
  }

  // dupCheck raises 42601 when an option appeared twice.
  private dupCheck(already: boolean, opt: string): void {
    if (already) throw engineError("syntax_error", `${opt} specified more than once`);
  }

  // parseSignedIntLiteral parses a signed integer literal (`-? INT`) as a bigint — the sequence-
  // option value form. The lexer caps an `int` magnitude at 2^63, so the only out-of-range case is
  // a bare positive 2^63 (22003 — numeric_value_out_of_range); a negated 2^63 is i64::MIN (valid).
  private parseSignedIntLiteral(): bigint {
    let neg = false;
    if (this.peek().kind === "minus") {
      this.advance();
      neg = true;
    }
    const t = this.advance();
    if (t.kind !== "int") {
      throw engineError("syntax_error", `expected an integer, found ${t.kind}`);
    }
    const v = neg ? -t.int! : t.int!;
    if (v < -9223372036854775808n || v > 9223372036854775807n) {
      throw engineError("numeric_value_out_of_range", "sequence parameter out of i64 range");
    }
    return v;
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
    const [db, table] = this.parseQualifiedTableName();

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

    // Optional `OVERRIDING { SYSTEM | USER } VALUE` clause (spec/design/sequences.md §13), after
    // the column list and before the source. OVERRIDING / SYSTEM / USER / VALUE are non-reserved;
    // the clause is unambiguous against a VALUES/SELECT source.
    let overriding: Overriding | null = null;
    if (this.peekKeyword() === "overriding") {
      this.advance();
      const mkw = this.peekKeyword();
      if (mkw === "system") {
        overriding = "system";
      } else if (mkw === "user") {
        overriding = "user";
      } else {
        throw engineError(
          "syntax_error",
          `expected SYSTEM or USER after OVERRIDING, found '${mkw}'`,
        );
      }
      this.advance();
      this.expectKeyword("value");
    }

    // The source is EITHER a SELECT (INSERT ... SELECT — §24) OR a VALUES list. `VALUES` and
    // `SELECT` are disjoint leading keywords, so a peek decides without lookahead.
    if (this.peekKeyword() === "select") {
      const select = this.parseSelect();
      const onConflict = this.parseOnConflict();
      const returning = this.parseReturning();
      return {
        kind: "insert",
        table,
        db,
        columns,
        overriding,
        source: { kind: "select", select },
        onConflict,
        returning,
      };
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
    const onConflict = this.parseOnConflict();
    const returning = this.parseReturning();
    return {
      kind: "insert",
      table,
      db,
      columns,
      overriding,
      source: { kind: "values", rows },
      onConflict,
      returning,
    };
  }

  // parseOnConflict parses the optional `ON CONFLICT [target] action` clause (UPSERT —
  // spec/design/upsert.md), after the source and before RETURNING. ON / CONFLICT / DO / NOTHING /
  // CONSTRAINT are not reserved (§3); the clause is recognized by the `ON CONFLICT` two-keyword lead.
  private parseOnConflict(): OnConflict | null {
    if (this.peekKeyword() !== "on" || this.peekKeywordAt(1) !== "conflict") {
      return null;
    }
    this.advance(); // ON
    this.advance(); // CONFLICT

    // Optional conflict target: a `( col, … )` column list or `ON CONSTRAINT name`.
    let target: ConflictTarget | null = null;
    if (this.peek().kind === "lparen") {
      this.advance(); // '('
      const cols: string[] = [];
      for (;;) {
        cols.push(this.expectIdentifier());
        const k = this.advance().kind;
        if (k === "comma") continue;
        if (k === "rparen") break;
        throw engineError("syntax_error", "expected ',' or ')'");
      }
      target = { kind: "columns", columns: cols };
    } else if (this.peekKeyword() === "on") {
      this.advance(); // ON
      this.expectKeyword("constraint");
      target = { kind: "constraint", name: this.expectIdentifier() };
    }

    // The action: `DO NOTHING` or `DO UPDATE SET assignment [, …] [WHERE …]`.
    this.expectKeyword("do");
    const action = this.peekKeyword();
    if (action === "nothing") {
      this.advance();
      return { target, doUpdate: false, assignments: [], filter: null };
    }
    if (action === "update") {
      this.advance();
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
      const filter = this.parseOptionalWhere();
      return { target, doUpdate: true, assignments, filter };
    }
    throw engineError(
      "syntax_error",
      `expected NOTHING or UPDATE after ON CONFLICT DO, found '${action}'`,
    );
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
  // ROW(…) composite constructor (spec/design/composite.md §1), a bind parameter ($N, bound at
  // execute — spec/design/api.md §5), else a literal.
  private parseInsertValue(): InsertValue {
    if (this.peekKeyword() === "default") {
      this.advance();
      return { kind: "default" };
    }
    if (this.peekKeyword() === "row" && this.peekKindAt(1) === "lparen") {
      // ROW(field, field, …) — recurse on each field (a literal, a $N, or a nested ROW).
      this.advance(); // ROW
      this.expect("lparen");
      const fields: InsertValue[] = [];
      if (this.peek().kind !== "rparen") {
        for (;;) {
          fields.push(this.parseInsertValue());
          const t = this.advance();
          if (t.kind === "comma") continue;
          if (t.kind === "rparen") break;
          throw engineError("syntax_error", `expected ',' or ')', found ${t.kind}`);
        }
      } else {
        this.advance(); // the empty ROW() — consume ')'
      }
      return { kind: "row", fields };
    }
    if (this.peekKeyword() === "array" && this.peekKindAt(1) === "lbracket") {
      // ARRAY[elem, …] — recurse on each element (a literal or a $N).
      this.advance(); // ARRAY
      this.expect("lbracket");
      const elements: InsertValue[] = [];
      if (this.peek().kind !== "rbracket") {
        for (;;) {
          elements.push(this.parseInsertValue());
          const t = this.advance();
          if (t.kind === "comma") continue;
          if (t.kind === "rbracket") break;
          throw engineError("syntax_error", `expected ',' or ']', found ${t.kind}`);
        }
      } else {
        this.advance(); // the empty ARRAY[] — consume ']'
      }
      return { kind: "array", elements };
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
      return {
        kind: "decimal",
        dec: Decimal.fromDigitsScale(negate, t.decDigits!, t.decScale!),
      };
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
    return this.parseQueryExprNode();
  }

  // parseQueryExprNode parses a top-level query_expr as a QueryExpr node — a set expression plus an
  // optional trailing ORDER BY / LIMIT / OFFSET folded onto it. The shared core of parseQueryExpr
  // (which returns it as a Statement) and a WITH clause's main body. Unlike parseSubquery it opens
  // no new nesting level — the body is at the statement top level.
  private parseQueryExprNode(): Select | SetOp {
    const node = this.parseSetExpr();
    const orderBy = this.parseOrderBy(true);
    const { limit, offset } = this.parseLimitOffsetClauses();
    // Both Select and SetOp carry orderBy/limit/offset; the spread keeps the `kind` discriminant.
    return { ...node, orderBy, limit, offset };
  }

  // parseWithStatement parses `query_statement ::= with_clause? ( query_expr | insert | update |
  // delete )` — a top-level statement prefixed by a WITH clause defining common table expressions
  // (spec/design/cte.md, spec/design/writable-cte.md). WITH RECURSIVE (spec/design/recursive-cte.md)
  // sets the `recursive` flag and lets a CTE reference itself; the CTE bodies and the main body are
  // WITH-less cte_bodies (the top-level-only narrowing — a nested WITH surfaces as 42601 because a
  // body must begin with SELECT/INSERT/UPDATE/DELETE).
  private parseWithStatement(): Statement {
    this.expectKeyword("with");
    // `WITH RECURSIVE …` enables self-reference (recursive-cte.md). RECURSIVE in this position is
    // the keyword (PG reserves it), so a CTE may not be named `recursive` — a documented narrowing.
    // The flag governs the whole list; whether a given CTE is *actually* recursive is decided at
    // planning by whether its body references its own name.
    let recursive = false;
    if (this.peekKeyword() === "recursive") {
      this.advance();
      recursive = true;
    }
    const ctes: Cte[] = [];
    for (;;) {
      ctes.push(this.parseCte());
      if (this.peek().kind === "comma") {
        this.advance();
      } else {
        break;
      }
    }
    // The primary may be a data-modifying statement (spec/design/writable-cte.md): a leading
    // INSERT/UPDATE/DELETE keyword selects it, otherwise a WITH-less query_expr.
    const body = this.parseCteBody(false);
    return { kind: "with", ctes, body, recursive };
  }

  // parseCteBody parses a `cte_body` (spec/design/writable-cte.md): a data-modifying
  // INSERT/UPDATE/DELETE when one leads, otherwise a query. `parenthesized` is true for a CTE body
  // inside `( … )` (the closing `)` is the caller's), false for the WITH primary (it runs to end of
  // statement). A query body parsed here is the WITH-less query_expr (the top-level-only nested-WITH
  // narrowing — a nested `WITH` surfaces as a leftover 42601).
  private parseCteBody(parenthesized: boolean): CteBody {
    const kw = this.peekKeyword();
    if (kw === "insert" || kw === "update" || kw === "delete") {
      // A parenthesized data-modifying body counts one nesting level, like parseSubquery does for a
      // parenthesized query body (grammar.md §48); the primary (parenthesized = false) runs at the
      // statement top level and does not.
      if (parenthesized) {
        this.deepen();
      }
      let body: CteBody;
      if (kw === "insert") {
        body = this.parseInsert();
      } else if (kw === "update") {
        body = this.parseUpdate();
      } else {
        body = this.parseDelete();
      }
      if (parenthesized) {
        this.undeepen();
      }
      return body;
    } else if (parenthesized) {
      return this.parseSubquery();
    } else {
      return this.parseQueryExprNode();
    }
  }

  // parseCte parses one CTE: `identifier ("(" ident ("," ident)* ")")? "AS" ("NOT"? "MATERIALIZED")?
  // "(" cte_body ")"` (spec/design/cte.md, spec/design/writable-cte.md). The optional column list
  // renames the body's output columns; [NOT] MATERIALIZED is the explicit evaluation hint. The body
  // reuses parseCteBody (one nesting level, trailing clauses allowed) between its parens.
  private parseCte(): Cte {
    const name = this.expectIdentifier();
    let columns: string[] | null = null;
    if (this.peek().kind === "lparen") {
      this.advance();
      const cols = [this.expectIdentifier()];
      while (this.peek().kind === "comma") {
        this.advance();
        cols.push(this.expectIdentifier());
      }
      this.expect("rparen");
      columns = cols;
    }
    this.expectKeyword("as");
    let materialized: boolean | null = null;
    if (this.peekKeyword() === "materialized") {
      this.advance();
      materialized = true;
    } else if (this.peekKeyword() === "not" && this.peekKeywordAt(1) === "materialized") {
      this.advance();
      this.advance();
      materialized = false;
    }
    this.expect("lparen");
    const body = this.parseCteBody(true);
    this.expect("rparen");
    return { name, columns, materialized, body };
  }

  // parseSubquery parses a parenthesized subquery's inner query_expr (grammar.md §26): a full
  // set-expression plus an optional trailing ORDER BY / LIMIT / OFFSET folded onto the node.
  // Mirrors parseQueryExpr but yields a QueryExpr. The caller has consumed the opening "(" and
  // consumes the closing ")".
  private parseSubquery(): QueryExpr {
    // A nested scalar subquery / EXISTS / IN (SELECT …) is one query-nesting level deeper; the
    // guard also protects the parser's own stack against `(SELECT (SELECT … ))`.
    this.deepen();
    // A leading WITH begins a nested common-table-expression query (spec/design/cte.md §7).
    const node = this.atWithClause() ? this.parseWithQueryExpr() : this.parseSubqueryInner();
    this.undeepen();
    return node;
  }

  // parseSubqueryInner parses the non-WITH body of a subquery: a set-expression plus an optional
  // trailing ORDER BY / LIMIT / OFFSET folded onto the node. Split out so a nested WITH's main query
  // (parseWithQueryExpr) reuses it.
  private parseSubqueryInner(): QueryExpr {
    const node = this.parseSetExpr();
    const orderBy = this.parseOrderBy(true);
    const { limit, offset } = this.parseLimitOffsetClauses();
    return { ...node, orderBy, limit, offset };
  }

  // parseWithQueryExpr parses a nested `WITH [RECURSIVE] cte (, cte)* query_expr` into a WithExpr
  // (spec/design/cte.md §7). The CTE bodies reuse parseCte (so a CTE body may itself nest a WITH);
  // the main query is a WITH-less query_expr. A data-modifying CTE body parses here but is rejected
  // at planning (0A000, top-level-only — matching PostgreSQL).
  private parseWithQueryExpr(): QueryExpr {
    this.expectKeyword("with");
    let recursive = false;
    if (this.peekKeyword() === "recursive") {
      this.advance();
      recursive = true;
    }
    const ctes: Cte[] = [];
    for (;;) {
      ctes.push(this.parseCte());
      if (this.peek().kind === "comma") {
        this.advance();
        continue;
      }
      break;
    }
    const body = this.parseSubqueryInner();
    return { kind: "withExpr", ctes, recursive, body };
  }

  // isWithClauseAtOffset reports whether a WITH clause (`WITH RECURSIVE …`, `WITH <name> ( …`, or
  // `WITH <name> AS …`) begins at this.pos + offset (spec/design/cte.md §7), as opposed to an
  // ordinary expression or a column named `with`. The shape-based lookahead keeps the recognition
  // unambiguous even where `with` is a legal identifier (e.g. `x IN (with)` is a value list).
  private isWithClauseAtOffset(offset: number): boolean {
    if (this.peekKeywordAt(offset) !== "with") return false;
    if (this.peekKeywordAt(offset + 1) === "recursive") return true;
    if (this.peekKindAt(offset + 1) === "word") {
      return this.peekKindAt(offset + 2) === "lparen" || this.peekKeywordAt(offset + 2) === "as";
    }
    return false;
  }

  // isQueryStartAtOffset reports whether a query expression — a SELECT or a nested WITH clause
  // (cte.md §7) — begins at this.pos + offset. The §26 leading-SELECT lookahead, extended with WITH.
  private isQueryStartAtOffset(offset: number): boolean {
    return this.peekKeywordAt(offset) === "select" || this.isWithClauseAtOffset(offset);
  }

  // atSubqueryStart reports whether the NEXT token begins a query expression (a SELECT or nested
  // WITH) — the disambiguator at every subquery position.
  private atSubqueryStart(): boolean {
    return this.isQueryStartAtOffset(0);
  }

  // atWithClause reports whether the NEXT token begins a nested WITH clause (cte.md §7).
  private atWithClause(): boolean {
    return this.isWithClauseAtOffset(0);
  }

  // parseSetExpr parses the lower-precedence, left-associative UNION/EXCEPT level. INTERSECT binds
  // tighter (parsed inside parseIntersectExpr), so `a UNION b INTERSECT c` becomes
  // `a UNION (b INTERSECT c)`.
  private parseSetExpr(): Select | SetOp {
    const base = this.depth;
    let left = this.parseIntersectExpr();
    for (;;) {
      const kw = this.peekKeyword();
      let op: SetOpKind;
      if (kw === "union") op = "union";
      else if (kw === "except") op = "except";
      else {
        this.depth = base;
        return left;
      }
      this.deepen(); // each chained UNION/EXCEPT is one more set-op nesting level
      this.advance(); // UNION | EXCEPT
      const all = this.parseSetOpQuantifier();
      const right = this.parseIntersectExpr();
      left = {
        kind: "setOp",
        op,
        all,
        lhs: left,
        rhs: right,
        orderBy: [],
        limit: null,
        offset: null,
      };
    }
  }

  // parseIntersectExpr parses the higher-precedence, left-associative INTERSECT level.
  private parseIntersectExpr(): Select | SetOp {
    const base = this.depth;
    let left: QueryExpr = this.parseSelectCore();
    while (this.peekKeyword() === "intersect") {
      this.deepen(); // each chained INTERSECT is one more set-op nesting level
      this.advance(); // INTERSECT
      const all = this.parseSetOpQuantifier();
      const right = this.parseSelectCore();
      left = {
        kind: "setOp",
        op: "intersect",
        all,
        lhs: left,
        rhs: right,
        orderBy: [],
        limit: null,
        offset: null,
      };
    }
    this.depth = base;
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
  private parseLimitOffsetClauses(): {
    limit: bigint | null;
    offset: bigint | null;
  } {
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
    sel.orderBy = this.parseOrderBy(true);
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
      const modifier =
        next.kind !== "eof" && !(next.kind === "word" && lower(next.word!) === "from");
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

    // WINDOW name AS ( definition ) (, …) — named windows referenced by OVER name (window.md §5).
    const windows = this.parseWindowClause();

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
      windows,
    };
  }

  // parseWindowClause parses `window_clause ::= "WINDOW" identifier "AS" "(" window_definition ")"
  // ("," …)*` (window.md §5). Each entry is a full window definition (which may extend an earlier
  // entry via a leading base-window name — §5). Empty when no WINDOW keyword is present. WINDOW is
  // non-reserved. Each definition reuses parseWindowDefinition with the inline OVER.
  private parseWindowClause(): [string, WindowDef][] {
    if (this.peekKeyword() !== "window") return [];
    this.advance();
    const windows: [string, WindowDef][] = [];
    for (;;) {
      const name = this.expectIdentifier();
      this.expectKeyword("as");
      this.expect("lparen");
      const def = this.parseWindowDefinition();
      this.expect("rparen");
      windows.push([name, def]);
      if (this.peek().kind === "comma") {
        this.advance();
      } else {
        break;
      }
    }
    return windows;
  }

  // parseWindowDefinition parses a window definition body `[base] [PARTITION BY …] [ORDER BY …]
  // [frame]` between the already-consumed `(` and the closing `)` (spec/design/window.md §3, §5).
  // The optional leading base-window name (a bareword that is not a clause-introducing keyword)
  // marks a definition that extends a named window — the resolver merges it in (§5). Used by both
  // the inline `OVER ( … )` and the `WINDOW name AS ( … )` clause so the two spellings parse alike.
  private parseWindowDefinition(): WindowDef {
    const base = this.parseOptBaseWindowName();
    const partition: Expr[] = [];
    if (this.peekKeyword() === "partition") {
      this.advance();
      this.expectKeyword("by");
      // A PARTITION BY key is a general expression (`PARTITION BY a + b`), not just a column
      // (spec/design/window.md §5.1). A bare column resolves to its slot directly; a compound
      // expression is materialized into a synthetic window-key column before the window stage.
      for (;;) {
        partition.push(this.parseExpr());
        if (this.peek().kind === "comma") {
          this.advance();
        } else {
          break;
        }
      }
    }
    const order = this.parseWindowOrderBy();
    const frame = this.parseWindowFrame();
    return { base, partition, order, frame };
  }

  // parseOptBaseWindowName returns the optional leading base-window name of a window definition
  // (spec/design/window.md §5), else null. Present when the next token is a bareword that is not a
  // clause-introducing keyword (PARTITION/ORDER/ROWS/RANGE/GROUPS) — those start the definition's
  // own clauses, so an unquoted occurrence is the keyword, never a base name (matching PostgreSQL;
  // a window named like a keyword would need quoting, which jed's window names do not support).
  private parseOptBaseWindowName(): string | null {
    const t = this.peek();
    if (t.kind !== "word") return null;
    switch (lower(t.word!)) {
      case "partition":
      case "order":
      case "rows":
      case "range":
      case "groups":
        return null;
    }
    this.advance();
    return t.word!;
  }

  // parseHaving parses `having_clause ::= "HAVING" expr` (grammar.md §19), after GROUP BY and
  // before ORDER BY. `HAVING` is not reserved; the predicate is a general expression (it may
  // reference aggregates) checked for boolean at resolve.
  private parseHaving(): Expr | null {
    if (this.peekKeyword() !== "having") return null;
    this.advance(); // HAVING
    return this.parseExpr();
  }

  // parseGroupBy parses `group_by ::= "GROUP" "BY" group_item ("," group_item)*` (grammar.md §18),
  // after WHERE and before ORDER BY. Each term is an ordinary column, a parenthesized column group, or
  // ROLLUP/CUBE/GROUPING SETS (spec/design/aggregates.md §12); every grouping column is a
  // bare/qualified column. `GROUP` is not reserved, so it is a clause only when followed by `BY`.
  private parseGroupBy(): GroupItem[] {
    if (this.peekKeyword() !== "group") return [];
    this.advance(); // GROUP
    this.expectKeyword("by");
    const items: GroupItem[] = [];
    for (;;) {
      items.push(this.parseGroupItem());
      if (this.peek().kind === "comma") {
        this.advance();
        continue;
      }
      break;
    }
    return items;
  }

  // parseGroupItem parses one GROUP BY grouping term — a ROLLUP/CUBE/GROUPING SETS construct, or an
  // ordinary column group (a bare column, a parenthesized `(a, b)`, or the empty set `()`). Also used
  // for the elements of a GROUPING SETS list (which may nest these forms). ROLLUP/CUBE/GROUPING/SETS
  // are unreserved, recognized by lookahead only.
  private parseGroupItem(): GroupItem {
    switch (this.peekKeyword()) {
      case "rollup":
        this.advance();
        return { kind: "rollup", groups: this.parseGroupSetList() };
      case "cube":
        this.advance();
        return { kind: "cube", groups: this.parseGroupSetList() };
      case "grouping":
        if (this.peekKeywordAt(1) === "sets") {
          this.advance(); // GROUPING
          this.advance(); // SETS
          this.expect("lparen");
          const elems: GroupItem[] = [];
          for (;;) {
            elems.push(this.parseGroupItem());
            if (this.peek().kind === "comma") {
              this.advance();
              continue;
            }
            break;
          }
          this.expect("rparen");
          return { kind: "groupingSets", elems };
        }
        break;
    }
    return { kind: "set", cols: this.parseGroupSet() };
  }

  // parseGroupSetList parses the parenthesized `( group_set ("," group_set)* )` argument list of
  // ROLLUP / CUBE, where each element is a grouping expression group (spec/design/aggregates.md §12/§15).
  private parseGroupSetList(): Expr[][] {
    this.expect("lparen");
    const sets: Expr[][] = [];
    for (;;) {
      sets.push(this.parseGroupSet());
      if (this.peek().kind === "comma") {
        this.advance();
        continue;
      }
      break;
    }
    this.expect("rparen");
    return sets;
  }

  // parseGroupSet parses a single grouping "expression group": a parenthesized `( e, ... )` / empty
  // `()`, or a bare grouping term. Each member is a general expression — a bare/qualified column, a
  // select-list ordinal (a bare integer literal), an output alias, or any expression (aggregates.md
  // §15). A parenthesized list of two-or-more is a column group `(a, b)`; a single parenthesized
  // expression `(a + b)` is one term — both fall out of parsing a comma-list of expressions.
  private parseGroupSet(): Expr[] {
    if (this.peek().kind === "lparen") {
      this.advance();
      const cols: Expr[] = [];
      if (this.peek().kind !== "rparen") {
        for (;;) {
          cols.push(this.parseExpr());
          if (this.peek().kind === "comma") {
            this.advance();
            continue;
          }
          break;
        }
      }
      this.expect("rparen");
      return cols;
    }
    return [this.parseExpr()];
  }

  // parseFromClause parses `from_clause ::= table_ref join_clause*` (grammar.md §15): the first
  // table reference followed by a left-deep chain of zero or more joins. The join keywords are
  // not reserved (§3); the loop recognizes a join only by a leading join keyword, so any other
  // trailing word ends the FROM clause.
  private parseFromClause(): { from: TableRef; joins: JoinClause[] } {
    const from = this.parseTableRef();
    const joins: JoinClause[] = [];
    for (;;) {
      for (;;) {
        const j = this.parseJoinClause();
        if (j === null) break;
        joins.push(j);
      }
      // Comma-FROM (grammar.md §15): `FROM a, b` is an implicit CROSS JOIN. The comma separates
      // top-level FROM items, each its own join sub-chain; it binds LOOSER than JOIN, so the new
      // item begins a fresh ON-resolution segment (recorded by comma: true). The inner loop then
      // picks up any joins of the new item (`a, b JOIN c ON …`) before the next comma.
      if (this.peek().kind === "comma") {
        this.advance();
        const table = this.parseTableRef();
        joins.push({ kind: "cross", table, on: null, comma: true });
        continue;
      }
      break;
    }
    return { from, joins };
  }

  // parseTableRef parses `table_ref ::= derived_table derived_alias? | (identifier |
  // table_function) ("AS"? identifier)?` (grammar.md §15/§35/§42). A `(` at the START of a
  // table_ref, when a SELECT follows, begins a DERIVED TABLE — a parenthesized subquery used as a
  // relation (§42); any other leading `(` is a 42601 this slice (no parenthesized-join FROM).
  // Otherwise it is a base table name OR a set-returning function call, a `(` immediately after the
  // leading identifier marking the function form; the resolver owns arity/type errors. The alias
  // logic is shared. The stop-keyword set is a §8 cross-core surface.
  private parseTableRef(): TableRef {
    // An optional leading LATERAL (grammar.md §44) marks a derived table / table function as
    // correlated to the EARLIER FROM relations. LATERAL is non-reserved (§3), so it is the keyword
    // only when a derived table `(` or a function call `name(` follows (a two-token lookahead) —
    // otherwise it is an ordinary identifier (e.g. a table named `lateral`). A table function is
    // implicitly lateral regardless, so the keyword is redundant (but accepted) there.
    const lateral =
      this.peekKeyword() === "lateral" &&
      (this.peekKindAt(1) === "lparen" ||
        (this.peekKindAt(1) === "word" && this.peekKindAt(2) === "lparen"));
    if (lateral) {
      this.advance();
    }
    if (this.peek().kind === "lparen") {
      const tr = this.parseDerivedTable();
      tr.lateral = lateral;
      return tr;
    }
    // `JSON_TABLE(ctx, path [AS n] COLUMNS (…))` — a table source (json-table.md §3, T1), recognized
    // by the keyword followed by `(`.
    if (this.peekKeyword() === "json_table" && this.peekKindAt(1) === "lparen") {
      return this.parseJsonTable();
    }
    let name = this.expectIdentifier();
    // An optional DATABASE qualifier `db "." table` (spec/design/attached-databases.md §3): a `.`
    // after the first identifier makes it the database qualifier and the next identifier the table
    // name. A qualified name is a BASE TABLE only — never a set-returning function (no cross-database
    // SRF) — so the function `(` branch below is guarded off when a qualifier is present.
    let db: string | undefined;
    if (this.peek().kind === "dot") {
      this.advance(); // .
      db = name;
      name = this.expectIdentifier();
    }
    // A `(` right after the name = a set-returning function call (no `*`/`DISTINCT`).
    let args: Expr[] | null = null;
    if (this.peek().kind === "lparen") {
      if (db !== undefined) {
        throw engineError("syntax_error", "a database-qualified name cannot be a function call");
      }
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
    // A `(` after the alias is a FROM-clause list on a table function (a base table never has one
    // there). The TYPED column-definition list `AS t(col type, …)` (C0, json-table.md §1) — for the
    // record-returning functions — is parsed here; the rename-only form `AS g(col)` (no type) stays
    // a deferred narrowing (grammar.md §35).
    let columnDefs: TypeFieldDef[] | undefined;
    if (alias !== null && this.peek().kind === "lparen") {
      this.advance(); // (
      // Disambiguate: a col-def list has `name type`; a rename list has `name ,`/`name )`. After the
      // opening `(`, the current token is the first column name, so a `word` in the NEXT slot means a
      // type follows (col-def list).
      if (this.peekKindAt(1) !== "word") {
        throw engineError(
          "feature_not_supported",
          "column alias list on a table function is not supported yet",
        );
      }
      columnDefs = this.parseFieldDefList();
    }
    // An SRF is implicitly lateral; `lateral` records only whether the keyword was written.
    return { name, db, alias, args, columnDefs, lateral };
  }

  // parseDerivedTable parses a DERIVED TABLE — `"(" query_expr ")" derived_alias?` (grammar.md §42).
  // The caller has verified the next token is `(`. A derived table is recognized only when a SELECT
  // follows the `(` (the §26 leading-SELECT lookahead, a §8 cross-core surface); any other leading
  // `(` is a 42601 (no parenthesized-join FROM this slice). The alias is OPTIONAL (PostgreSQL 18
  // relaxed the old mandatory-alias rule): present, it is the label and may carry a column-rename
  // list; absent, the relation has no qualifier (its bare columns still resolve). name/alias carry
  // the alias ("" / null when none).
  private parseDerivedTable(): TableRef {
    // Consume the opening `(`. The body is EITHER a query_expr (a leading SELECT) OR a VALUES list
    // (a leading VALUES) — FROM (VALUES (e…),(e…)), a computed relation of literal rows
    // (grammar.md §42); any other leading `(` is rejected (a parenthesized-join FROM
    // `(a JOIN b ON …)` is a deferred narrowing).
    this.advance();
    let body: QueryExpr | undefined;
    let values: Expr[][] | undefined;
    if (this.peekKeyword() === "values") {
      values = this.parseValuesBody();
    } else if (this.atSubqueryStart()) {
      // A leading SELECT, or a nested WITH (cte.md §7), is a query_expr body.
      body = this.parseSubquery();
    } else {
      throw engineError(
        "syntax_error",
        "subquery in FROM must begin with SELECT or VALUES (a parenthesized join is not supported)",
      );
    }
    this.expect("rparen");
    // The alias is optional, parsed exactly like a base table's.
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
    // Optional column-rename list `(c1, c2, …)` — only when a table alias was given (PG: a column
    // list with no preceding alias name is a syntax error; the bare `(` falls through and a later
    // token check rejects it).
    let columnAliases: string[] | undefined;
    if (alias !== null && this.peek().kind === "lparen") {
      this.advance();
      columnAliases = [this.expectIdentifier()];
      while (this.peek().kind === "comma") {
        this.advance();
        columnAliases.push(this.expectIdentifier());
      }
      this.expect("rparen");
    }
    return {
      name: alias ?? "",
      alias,
      args: null,
      subquery: body,
      values,
      columnAliases,
    };
  }

  // parseValuesBody parses a VALUES-body's rows — VALUES "(" expr ("," expr)* ")" ("," …)*
  // (grammar.md §42), the body of a FROM (VALUES …) derived table. The caller has verified the next
  // keyword is VALUES (here consumed). Each row is a parenthesized list of GENERAL expressions
  // (unlike the INSERT … VALUES slot, which is a literal/$N/DEFAULT); arity equality across rows and
  // per-column type unification are resolve-time concerns (the executor's planValues). At least one
  // row, each with at least one value. NO trailing ORDER BY / LIMIT is consumed — the caller's `)`
  // follows the last row.
  private parseValuesBody(): Expr[][] {
    this.expectKeyword("values");
    const rows: Expr[][] = [];
    for (;;) {
      this.expect("lparen");
      const row: Expr[] = [this.parseExpr()];
      while (this.peek().kind === "comma") {
        this.advance();
        row.push(this.parseExpr());
      }
      this.expect("rparen");
      rows.push(row);
      if (this.peek().kind !== "comma") break;
      this.advance();
    }
    return rows;
  }

  // parseJsonTable parses `JSON_TABLE(ctx, path [AS n] COLUMNS (col, …)) [AS alias]` (json-table.md
  // §3, T1). The caller has verified the JSON_TABLE keyword + `(`.
  private parseJsonTable(): TableRef {
    this.advance(); // JSON_TABLE
    this.advance(); // (
    const ctx = this.parseExpr();
    this.skipFormatJson();
    this.expect("comma");
    const path = this.parseExpr();
    // An optional `AS name` for the root path (the path-name) is accepted and ignored (it only
    // matters with an explicit PLAN clause, the deferred T2).
    if (this.peekKeyword() === "as") {
      this.advance();
      this.expectIdentifier();
    }
    if (this.peekKeyword() === "passing") {
      throw engineError("feature_not_supported", "JSON_TABLE PASSING clause is not supported yet");
    }
    this.expectKeyword("columns");
    const columns = this.parseJtColumns();
    // An explicit PLAN clause is the deferred T2 slice.
    if (this.peekKeyword() === "plan") {
      throw engineError(
        "feature_not_supported",
        "JSON_TABLE explicit PLAN clause is not supported yet",
      );
    }
    this.expect("rparen");
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
    const jsonTable: JsonTable = { ctx, path, columns };
    return { name: alias ?? "json_table", alias, args: null, jsonTable, lateral: false };
  }

  // parseJtColumns parses a parenthesized JSON_TABLE COLUMNS list — `"(" jt_column ("," jt_column)*
  // ")"`.
  private parseJtColumns(): JtColumn[] {
    this.expect("lparen");
    const cols: JtColumn[] = [this.parseJtColumn()];
    while (this.peek().kind === "comma") {
      this.advance();
      cols.push(this.parseJtColumn());
    }
    this.expect("rparen");
    return cols;
  }

  // parseJtColumn parses one JSON_TABLE column: `NESTED [PATH] p [AS n] COLUMNS (…)`, `name FOR
  // ORDINALITY`, `name type EXISTS [PATH p] [ON ERROR]`, or a regular `name type [PATH p] [wrapper]
  // [quotes] [ON …]` column (json-table.md §3.3).
  private parseJtColumn(): JtColumn {
    if (this.peekKeyword() === "nested") {
      this.advance(); // NESTED
      if (this.peekKeyword() === "path") {
        this.advance();
      }
      const t = this.advance();
      if (t.kind !== "str") {
        throw engineError("syntax_error", "expected a string path after NESTED PATH");
      }
      const path = t.str!;
      if (this.peekKeyword() === "as") {
        this.advance();
        this.expectIdentifier();
      }
      this.expectKeyword("columns");
      const columns = this.parseJtColumns();
      return { kind: "nested", path, columns };
    }
    const name = this.expectIdentifier();
    // `name FOR ORDINALITY`.
    if (this.peekKeyword() === "for") {
      this.advance();
      this.expectKeyword("ordinality");
      return { kind: "ordinality", name };
    }
    // `name type …` — parse the type name + optional `[]`.
    const typeName = this.expectIdentifier();
    let array = false;
    if (this.peek().kind === "lbracket") {
      this.advance();
      this.expect("rbracket");
      array = true;
    }
    // `EXISTS` column.
    if (this.peekKeyword() === "exists") {
      this.advance();
      const path = this.parseJtPathClause();
      const onError = this.parseJsonOnErrorOnly();
      return { kind: "exists", name, typeName, path, onError };
    }
    // A regular column.
    this.skipFormatJson();
    const path = this.parseJtPathClause();
    const [wrapper, keepQuotes] = this.parseJsonWrapperQuotes();
    const [onEmpty, onError] = this.parseJsonOnClauses();
    return { kind: "regular", name, typeName, array, path, wrapper, keepQuotes, onEmpty, onError };
  }

  // parseJtPathClause parses an optional `PATH '<string>'` clause on a JSON_TABLE column.
  private parseJtPathClause(): string | null {
    if (this.peekKeyword() === "path") {
      this.advance();
      const t = this.advance();
      if (t.kind !== "str") {
        throw engineError("syntax_error", "expected a string after PATH");
      }
      return t.str!;
    }
    return null;
  }

  // parseJoinClause parses one join_clause if a join keyword begins here (returns null to end
  // the FROM chain). CROSS JOIN has no ON; the INNER/outer kinds require ON <expr> (a missing ON
  // is 42601). The outer kinds (LEFT/RIGHT/FULL [OUTER]) parse into the AST but are rejected at
  // execution (0A000) — spec/design/grammar.md §15.
  private parseJoinClause(): JoinClause | null {
    // An optional leading NATURAL (grammar.md §15) makes the join derive its USING list from the
    // common column names. It is non-reserved (in the table-ref stop set so it is not swallowed as
    // the prior relation's alias); once consumed it MUST be followed by a join (a NATURAL CROSS JOIN
    // / bare NATURAL <non-join> is 42601), and takes no ON/USING.
    const natural = this.peekKeyword() === "natural";
    if (natural) this.advance();
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
        if (natural) throw engineError("syntax_error", "NATURAL CROSS JOIN is not allowed");
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
      default:
        // After NATURAL a join keyword is required; otherwise the FROM chain just ends here.
        if (natural) throw engineError("syntax_error", "NATURAL must be followed by a join");
        return null;
    }
    const table = this.parseTableRef();
    // A non-CROSS, non-NATURAL join takes either `ON <expr>` or `USING (col, …)` (grammar.md §15).
    // A NATURAL join derives its condition (no ON/USING), and CROSS takes none. USING is not
    // reserved (§3): it is the join condition only as the keyword immediately following the right
    // table_ref. The column list has one or more names; an empty list is a 42601.
    let on: Expr | null = null;
    let using: string[] | undefined;
    if (isCross || natural) {
      // no condition (NATURAL derives it; CROSS has none)
    } else if (this.peekKeyword() === "using") {
      this.advance();
      this.expect("lparen");
      using = [this.expectIdentifier()];
      while (this.peek().kind === "comma") {
        this.advance();
        using.push(this.expectIdentifier());
      }
      this.expect("rparen");
    } else {
      this.expectKeyword("on");
      on = this.parseExpr();
    }
    return { kind, table, on, using, natural };
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

  // parseQualifiedTableName parses `qualified_table ::= (identifier ".")? identifier` in DML-target
  // position (spec/design/attached-databases.md §3): an optional database qualifier followed by the
  // table name. Returns [db, name] where db is undefined for a bare (implicit-scope) name. The
  // FROM-position analogue is inlined in parseTableRef (which must also disambiguate the function /
  // derived-table forms).
  private parseQualifiedTableName(): [string | undefined, string] {
    const first = this.expectIdentifier();
    if (this.peek().kind === "dot") {
      this.advance(); // .
      return [first, this.expectIdentifier()];
    }
    return [undefined, first];
  }

  // parseOrderBy parses an optional `ORDER BY <key> ("," <key>)*` (spec/grammar/grammar.ebnf
  // `order_by`). nullsFirst is resolved here: explicit if given, else the direction default (ASC ->
  // last, DESC -> first). A bare NULLS not followed by FIRST/LAST is a syntax error (42601). Returns []
  // when there is no ORDER BY.
  //
  // Each key is parsed as a general expression and classified into one of the three OrderKey modes
  // (grammar.md §10): a bare (optionally COLLATE-wrapped) column reference is a column key (kept on the
  // fast path so PK-scan elision + the column's collation still apply); anything else is a general
  // expression key. allowOrdinal governs the bare-integer case: when set (the query and set-operation
  // ORDER BY) a bare integer literal (the unary-minus fold makes `-1` one negative int) is an ordinal;
  // when clear (WITHIN GROUP) it is a constant expression key, matching PostgreSQL where a WITHIN GROUP
  // integer is a constant, not an ordinal.
  private parseOrderBy(allowOrdinal: boolean): OrderKey[] {
    const keys: OrderKey[] = [];
    if (this.peekKeyword() !== "order") return keys;
    this.advance();
    this.expectKeyword("by");
    for (;;) {
      const expr = this.parseExpr();
      const { collation, descending, nullsFirst } = this.parseSortSuffix();
      keys.push(classifyOrderKey(expr, collation, descending, nullsFirst, allowOrdinal));
      if (this.peek().kind === "comma") {
        this.advance();
        continue;
      }
      break;
    }
    return keys;
  }

  // parseSortSuffix parses the trailing modifiers shared by every sort key: an optional `COLLATE
  // "name"`, an optional `ASC`/`DESC` direction, and an optional `NULLS FIRST|LAST`. nullsFirst is
  // resolved here — explicit if given, else the direction default (ASC → NULLS LAST, DESC → NULLS
  // FIRST: NULL is the largest value, the PostgreSQL model, grammar.md §10). A bare `NULLS` not
  // followed by FIRST/LAST is 42601. Used by both the query ORDER BY (after a column ref) and the
  // window ORDER BY (after a general expression).
  private parseSortSuffix(): {
    collation: string | null;
    descending: boolean;
    nullsFirst: boolean;
  } {
    let collation: string | null = null;
    if (this.peekKeyword() === "collate") {
      this.advance();
      collation = this.expectCollationName();
    }
    let descending = false;
    if (this.peekKeyword() === "asc") {
      this.advance();
    } else if (this.peekKeyword() === "desc") {
      this.advance();
      descending = true;
    }
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
    return { collation, descending, nullsFirst };
  }

  // parseWindowOrderBy parses an OVER clause's optional `ORDER BY <key> ("," <key>)*` (nil when
  // absent). Unlike the query parseOrderBy (column references only), each key is a general expression
  // (`ORDER BY a + b`, `ORDER BY sum(x)`) followed by the shared sort suffix. A COLLATE binds tighter
  // than the comparison/arithmetic that could appear in a key, so parseExpr already absorbs an inline
  // `expr COLLATE "x"`; the trailing COLLATE here is the sort-key collation. spec/design/window.md §5.1.
  private parseWindowOrderBy(): WindowOrderKey[] {
    const keys: WindowOrderKey[] = [];
    if (this.peekKeyword() !== "order") return keys;
    this.advance();
    this.expectKeyword("by");
    for (;;) {
      const expr = this.parseExpr();
      const { collation, descending, nullsFirst } = this.parseSortSuffix();
      keys.push({ expr, collation, descending, nullsFirst });
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
  // (OFFSET), and a magnitude over i64's max throws 22003 (the value -0 folds to 0 and
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
    const [db, table] = this.parseQualifiedTableName();
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
    return { kind: "update", table, db, assignments, filter, returning };
  }

  // parseDelete parses `DELETE FROM <table> [WHERE <pred>]`. No WHERE deletes all rows.
  private parseDelete(): Delete {
    this.expectKeyword("delete");
    this.expectKeyword("from");
    const [db, table] = this.parseQualifiedTableName();
    const filter = this.parseOptionalWhere();
    const returning = this.parseReturning();
    return { kind: "delete", table, db, filter, returning };
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
      // `t.*` — a qualified star (all columns of the relation labeled `t`), a select-list /
      // RETURNING item MIXABLE with other items (grammar.md §15). Recognized by the three-token
      // shape `identifier "." "*"` before the general expr parser, so `t.col` (Dot then a word)
      // and `a * b` (no Dot) are untouched, and a bare `*` was already handled above. No `AS` alias.
      if (
        this.peek().kind === "word" &&
        this.peekKindAt(1) === "dot" &&
        this.peekKindAt(2) === "star"
      ) {
        const qualifier = this.expectIdentifier();
        this.advance(); // .
        this.advance(); // *
        items.push({ expr: { kind: "qualifiedStar", qualifier }, alias: null });
        if (this.peek().kind === "comma") {
          this.advance();
          continue;
        }
        break;
      }
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
    // A fresh sub-expression is one nesting level deeper (parens, ARRAY/ROW/CASE/function
    // operands, subscript indices all re-enter here). Bounds the recursive descent itself.
    this.deepen();
    const e = this.parseOr();
    this.undeepen();
    return e;
  }

  private parseOr(): Expr {
    const base = this.depth;
    let lhs = this.parseAnd();
    while (this.peekKeyword() === "or") {
      this.deepen(); // each chained OR is one more AST level
      this.advance();
      lhs = binaryExpr("or", lhs, this.parseAnd());
    }
    this.depth = base;
    return lhs;
  }

  private parseAnd(): Expr {
    const base = this.depth;
    let lhs = this.parseNot();
    while (this.peekKeyword() === "and") {
      this.deepen(); // each chained AND is one more AST level
      this.advance();
      lhs = binaryExpr("and", lhs, this.parseNot());
    }
    this.depth = base;
    return lhs;
  }

  private parseNot(): Expr {
    if (this.peekKeyword() === "not") {
      this.advance();
      // right-associative: NOT NOT x — each NOT is one more AST level (recursion here, so the
      // depth guard also protects the parser's own stack).
      this.deepen();
      const operand = this.parseNot();
      this.undeepen();
      return { kind: "unary", op: "not", operand };
    }
    return this.parseComparison();
  }

  // parseComparison parses one comparison, a postfix IS [NOT] NULL, or
  // IS [NOT] DISTINCT FROM, all non-associative: `a = b = c` is a syntax error, and
  // `a + 1 IS NULL` binds as `(a + 1) IS NULL`. After the shared `IS` `NOT`? it
  // dispatches on the NULL vs DISTINCT FROM keyword (spec/grammar/grammar.ebnf
  // `comparison`).
  private parseComparison(): Expr {
    const lhs = this.parseConcat();
    if (this.peekKeyword() === "is") {
      this.advance();
      let negated = false;
      if (this.peekKeyword() === "not") {
        this.advance();
        negated = true;
      }
      // IS [NOT] DISTINCT FROM <concat> — NULL-safe equality; else IS [NOT] NULL.
      if (this.peekKeyword() === "distinct") {
        this.advance();
        this.expectKeyword("from");
        return { kind: "isDistinct", lhs, rhs: this.parseConcat(), negated };
      }
      // IS [NOT] JSON [VALUE|SCALAR|ARRAY|OBJECT] [(WITH|WITHOUT) UNIQUE [KEYS]] — the SQL/JSON
      // well-formedness predicate (json-sql-functions.md §5).
      if (this.peekKeyword() === "json") {
        this.advance();
        let jsonKind: JsonPredicateKind = "value";
        switch (this.peekKeyword()) {
          case "value":
            this.advance();
            jsonKind = "value";
            break;
          case "scalar":
            this.advance();
            jsonKind = "scalar";
            break;
          case "array":
            this.advance();
            jsonKind = "array";
            break;
          case "object":
            this.advance();
            jsonKind = "object";
            break;
        }
        // The unique-keys clause: `(WITH|WITHOUT) UNIQUE [KEYS]`. Consume `WITH`/`WITHOUT` only when
        // `UNIQUE` follows (a two-token lookahead — `WITH` otherwise starts no expression-level
        // clause here). `KEYS` is optional.
        let uniqueKeys = false;
        const w = this.peekKeyword();
        if ((w === "with" || w === "without") && this.peekKeywordAt(1) === "unique") {
          this.advance(); // WITH / WITHOUT
          this.advance(); // UNIQUE
          if (this.peekKeyword() === "keys") this.advance();
          uniqueKeys = w === "with";
        }
        return { kind: "isJson", operand: lhs, negated, jsonKind, uniqueKeys };
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
        this.peekKeywordAt(1) === "like" ||
        this.peekKeywordAt(1) === "ilike");
    if (predNegated) {
      this.advance(); // NOT
    }
    if (this.peekKeyword() === "in") {
      this.advance();
      this.expect("lparen");
      // `IN (SELECT ...)` is the uncorrelated IN-subquery (grammar.md §26), disambiguated by a
      // leading `SELECT` (or a nested `WITH` — cte.md §7); otherwise a non-empty value list
      // (`IN ()` is a 42601 syntax error).
      if (this.atSubqueryStart()) {
        const query = this.parseSubquery();
        this.expect("rparen");
        return { kind: "inSubquery", lhs, query, negated: predNegated };
      }
      const list = [this.parseConcat()];
      while (this.peek().kind === "comma") {
        this.advance();
        list.push(this.parseConcat());
      }
      this.expect("rparen");
      return { kind: "in", lhs, list, negated: predNegated };
    }
    if (this.peekKeyword() === "between") {
      this.advance();
      // Both bounds parse at the CONCAT level (one tighter than comparison), which never
      // consumes `AND` (a looser level owned by parseAnd). So the BETWEEN's structural `AND` is
      // matched here and `x BETWEEN a AND b AND c` parses as `(x BETWEEN a AND b) AND c`
      // (grammar.md §21); a `||` bound still works.
      const lo = this.parseConcat();
      this.expectKeyword("and");
      const hi = this.parseConcat();
      return { kind: "between", lhs, lo, hi, negated: predNegated };
    }
    // LIKE / ILIKE (case-insensitive) — grammar.md §22. `ilike` is just another peeked keyword.
    if (this.peekKeyword() === "like" || this.peekKeyword() === "ilike") {
      const insensitive = this.peekKeyword() === "ilike";
      this.advance();
      const rhs = this.parseConcat();
      return { kind: "like", lhs, rhs, negated: predNegated, insensitive };
    }
    // `~` / `~*` / `!~` / `!~*` — regex match (grammar.md §22b, regex.md). Punctuation operators, so
    // `negated`/`insensitive` come from the token itself; there is no `NOT ~` keyword form (`NOT x ~ p`
    // is the prefix-NOT over the whole match, taken a level up). The pattern is one CONCAT expression.
    const rxKind = this.peek().kind;
    if (
      rxKind === "tilde" ||
      rxKind === "tildeStar" ||
      rxKind === "bangTilde" ||
      rxKind === "bangTildeStar"
    ) {
      const rxNegated = rxKind === "bangTilde" || rxKind === "bangTildeStar";
      const rxInsensitive = rxKind === "tildeStar" || rxKind === "bangTildeStar";
      this.advance();
      const rhs = this.parseConcat();
      return { kind: "regex", lhs, rhs, negated: rxNegated, insensitive: rxInsensitive };
    }
    let op: BinaryOp;
    switch (this.peek().kind) {
      case "eq":
        op = "eq";
        break;
      case "ne":
        op = "ne";
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
    // `op ANY/SOME/ALL ( array )` — a quantified array comparison (grammar.md §41): a quantifier
    // may stand in for the ordinary right operand. SOME folds to ANY.
    const kw = this.peekKeyword();
    if (kw === "all" || kw === "any" || kw === "some") {
      const all = kw === "all";
      this.advance(); // ANY / SOME / ALL
      this.expect("lparen");
      // A leading `SELECT` is the SUBQUERY form `op ANY/ALL(SELECT …)` — the subquery spelling of
      // IN (array-functions.md §11.6), the §26 leading-`SELECT` lookahead (or a nested `WITH` —
      // cte.md §7); anything else is the array operand (§11.1).
      if (this.atSubqueryStart()) {
        const query = this.parseSubquery();
        this.expect("rparen");
        return { kind: "quantifiedSubquery", op, all, lhs, query };
      }
      const array = this.parseExpr(); // a full expression resolving to an array
      this.expect("rparen");
      return { kind: "quantified", op, all, lhs, array };
    }
    return binaryExpr(op, lhs, this.parseConcat());
  }

  // parseConcat parses the "any other operator" level (grammar.md §39/§40, array-functions.md §8/§10):
  // one rung tighter than the comparisons, looser than additive, left-associative. It hosts `||` array
  // concatenation plus the `@>`/`<@`/`&&` array containment/overlap operators — all the same precedence
  // in PostgreSQL. Each operand is an additive expression, so `a + b || c` is `(a + b) || c`; chaining
  // mixes freely (`a || b @> c` is `(a || b) @> c`).
  private parseConcat(): Expr {
    const base = this.depth;
    let lhs = this.parseAdditive();
    for (;;) {
      let op: BinaryOp;
      const k = this.peek().kind;
      if (k === "concat") op = "concat";
      else if (k === "contains") op = "contains";
      // The `@?` jsonpath-exists operator (jsonpath.md §6) — same precedence level as `@>`.
      else if (k === "jsonPathExists") op = "jsonPathExists";
      // The `@@` jsonpath-match operator (jsonpath.md §6) — same precedence level as `@>`.
      else if (k === "jsonPathMatch") op = "jsonPathMatch";
      else if (k === "containedBy") op = "containedBy";
      else if (k === "overlaps") op = "overlaps";
      else if (k === "strictlyLeft") op = "strictlyLeft";
      else if (k === "strictlyRight") op = "strictlyRight";
      else if (k === "notExtendRight") op = "notExtendRight";
      else if (k === "notExtendLeft") op = "notExtendLeft";
      else if (k === "adjacent") op = "adjacent";
      // The jsonb accessor operators (json-sql-functions.md §1) — "any other operator" precedence,
      // same level as `@>`/`||`, left-associative (`doc -> 'a' -> 'b'`).
      else if (k === "arrow") op = "jsonGet";
      else if (k === "arrowText") op = "jsonGetText";
      else if (k === "hashArrow") op = "jsonGetPath";
      else if (k === "hashArrowText") op = "jsonGetPathText";
      // The jsonb key-existence operators (json-sql-functions.md §1, J5) — same precedence level.
      else if (k === "question") op = "jsonHasKey";
      else if (k === "questionPipe") op = "jsonHasAnyKey";
      else if (k === "questionAmp") op = "jsonHasAllKeys";
      // The jsonb delete-at-path operator (json-sql-functions.md §1, J6) — same precedence level.
      else if (k === "hashMinus") op = "jsonDeletePath";
      else {
        this.depth = base;
        return lhs;
      }
      this.deepen(); // each chained operator is one more AST level
      this.advance();
      lhs = binaryExpr(op, lhs, this.parseAdditive());
    }
  }

  private parseAdditive(): Expr {
    const base = this.depth;
    let lhs = this.parseMultiplicative();
    for (;;) {
      let op: BinaryOp;
      if (this.peek().kind === "plus") op = "add";
      else if (this.peek().kind === "minus") op = "sub";
      else {
        this.depth = base;
        return lhs;
      }
      this.deepen(); // each chained +/- is one more AST level (the `1+1+…` vector)
      this.advance();
      lhs = binaryExpr(op, lhs, this.parseMultiplicative());
    }
  }

  private parseMultiplicative(): Expr {
    const base = this.depth;
    let lhs = this.parseAtTimeZone();
    for (;;) {
      let op: BinaryOp;
      if (this.peek().kind === "star") op = "mul";
      else if (this.peek().kind === "slash") op = "div";
      else if (this.peek().kind === "percent") op = "mod";
      else {
        this.depth = base;
        return lhs;
      }
      this.deepen(); // each chained * / % is one more AST level
      this.advance();
      lhs = binaryExpr(op, lhs, this.parseAtTimeZone());
    }
  }

  // parseAtTimeZone parses the `AT TIME ZONE` rung (grammar.md §49, timezones.md §6): a
  // left-associative infix operator binding tighter than `* / %`, additive, and the comparisons,
  // looser than COLLATE / `::` / unary minus (PostgreSQL's %left AT). `value AT TIME ZONE zone`
  // desugars to the function call timezone(zone, value) — PostgreSQL's own implementation — so the
  // resolver/evaluator/cost have one path for the operator and the bare call. AT/TIME/ZONE are
  // non-reserved (matched as a three-token sequence), so a bare column named at/time/zone is unaffected.
  private parseAtTimeZone(): Expr {
    const base = this.depth;
    let lhs = this.parseUnary();
    while (
      this.peekKeyword() === "at" &&
      this.peekKeywordAt(1) === "time" &&
      this.peekKeywordAt(2) === "zone"
    ) {
      this.deepen(); // each chained AT TIME ZONE is one more AST level
      this.advance(); // AT
      this.advance(); // TIME
      this.advance(); // ZONE
      const zone = this.parseUnary();
      lhs = {
        kind: "funcCall",
        name: "timezone",
        args: [zone, lhs],
        argNames: [],
        star: false,
        distinct: false,
        variadic: false,
      };
    }
    this.depth = base;
    return lhs;
  }

  private parseUnary(): Expr {
    if (this.peek().kind === "minus") {
      this.advance();
      // Fold unary-minus-of-an-integer-literal into one negative literal, so i64's
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
        return {
          kind: "literal",
          literal: {
            kind: "decimal",
            dec: Decimal.fromDigitsScale(true, t.decDigits!, t.decScale!),
          },
        };
      }
      // each chained unary `-` is one more AST level (recursion here, so the depth guard also
      // protects the parser's own stack against `- - - … x`).
      this.deepen();
      const operand = this.parseUnary();
      this.undeepen();
      return { kind: "unary", op: "neg", operand };
    }
    return this.parsePostfix();
  }

  // parsePostfix parses a primary optionally followed by one or more postfix operators, applied
  // left-to-right in token order: a `::type` PostgreSQL typecast (grammar.md §37) or a `.field` /
  // `.*` composite field selection (spec/design/composite.md §S4). `expr :: type` desugars to
  // CAST(expr AS type) here at parse time — one resolver / evaluator / cost path for both spellings —
  // and casts chain left-associatively (`x::int8::int2` = `(x::int8)::int2`). A typmod rides on the
  // type name exactly as in CAST (`x::numeric(10,2)`).
  //
  // Field selection follows PostgreSQL's PARENS-REQUIRED rule: `.field` / `.*` applies ONLY to a
  // PARENTHESIZED base — `(home).zip`, `(t.home).zip`, `(ROW(1,2)).f1` — and chains on a prior field
  // access (`(c).a.b`). A bare `home.zip` / `t.home.zip` is a (multi-part) column reference, never
  // field access (PG raises 42P01 for the unparenthesized form). So `.field` fires only when the
  // primary started with `(` or after a previous `.field`; otherwise the `.` is left for the caller
  // (a trailing `.field` on a bare name is then a syntax error, like PG). NB: a bare `a.b` is consumed
  // as a single qualifiedColumn by parseColumnRef inside parsePrimary.
  private parsePostfix(): Expr {
    // Only a PARENTHESIZED primary is field-accessible (PG requires `(expr).field`). A subsequent
    // `.field` keeps the chain field-accessible (`(c).a.b`); a `::` cast does not.
    const base0 = this.depth;
    let fieldAccessible = this.peek().kind === "lparen";
    let expr = this.parsePrimary();
    for (;;) {
      const k = this.peek().kind;
      // each postfix `::`/`[…]`/`.field`/COLLATE wraps the base in one more AST level; deepen only
      // when a postfix actually follows. COLLATE shares this rung so it binds tighter than || and the
      // comparisons (PG precedence).
      const isCollate = k === "word" && this.peekKeyword() === "collate";
      const isPostfix =
        k === "doubleColon" || k === "lbracket" || (k === "dot" && fieldAccessible) || isCollate;
      if (!isPostfix) break;
      this.deepen();
      if (isCollate) {
        this.advance(); // COLLATE
        const collation = this.expectCollationName();
        expr = { kind: "collate", inner: expr, collation };
        fieldAccessible = false;
      } else if (k === "doubleColon") {
        this.advance();
        const baseType = this.expectIdentifier();
        const typeMod = this.parseTypeMod();
        const typeName = this.consumeArrayBrackets() ? baseType + "[]" : baseType;
        expr = { kind: "cast", inner: expr, typeName, typeMod };
        fieldAccessible = false;
      } else if (k === "lbracket") {
        // `base[..][..]` — array subscript (spec/design/array.md §6). Applies to ANY base (no parens
        // rule, unlike `.field`). Consecutive `[…]` brackets collect into ONE access (so `a[1][2]` is
        // a single multidim element read, not nested). Each spec is an index `[i]` or a slice `[m:n]`
        // (bounds optionally omitted). After a subscript a `.field` still needs parens (PG).
        const subscripts: SubscriptSpec[] = [];
        while (this.peek().kind === "lbracket") {
          this.advance(); // [
          // The lower bound / index is absent only before a `:` or `]` (`[:n]`, `[]`).
          let lower: Expr | null = null;
          if (this.peek().kind !== "colon" && this.peek().kind !== "rbracket") {
            lower = this.parseExpr();
          }
          if (this.peek().kind === "colon") {
            this.advance(); // :
            let upper: Expr | null = null;
            if (this.peek().kind !== "rbracket") {
              upper = this.parseExpr();
            }
            this.expect("rbracket");
            subscripts.push({ isSlice: true, lower, upper });
          } else {
            // Index form: a bare `[]` (no index, no colon) is a syntax error.
            if (lower === null) {
              throw engineError("syntax_error", "array subscript requires an index");
            }
            this.expect("rbracket");
            subscripts.push({ isSlice: false, index: lower });
          }
        }
        expr = { kind: "subscript", base: expr, subscripts };
        fieldAccessible = false;
      } else if (k === "dot" && fieldAccessible) {
        // `.field` / `.*` — composite field selection (spec/design/composite.md §S4),
        // parens-required: only on a parenthesized / chained-field base.
        this.advance();
        if (this.peek().kind === "star") {
          this.advance();
          expr = { kind: "fieldStar", base: expr };
          fieldAccessible = false; // `.*` is terminal
        } else {
          const field = this.expectIdentifier();
          expr = { kind: "fieldAccess", base: expr, field };
          // a field value may itself be composite → `(c).a.b` chains (fieldAccessible stays true)
        }
      } else {
        break; // unreachable: the isPostfix precheck already broke on a non-postfix token
      }
    }
    this.depth = base0;
    return expr;
  }

  // parsePrimary parses a parenthesized expression, CAST(...), a literal (integer,
  // TRUE/FALSE, NULL), or a column reference.
  private parsePrimary(): Expr {
    if (this.peek().kind === "lparen") {
      this.advance();
      // `(SELECT ...)` is a scalar subquery (grammar.md §26), disambiguated by a leading `SELECT`
      // (or a nested `WITH` — cte.md §7) after the `(`; otherwise a parenthesized expression.
      if (this.atSubqueryStart()) {
        const query = this.parseSubquery();
        this.expect("rparen");
        return { kind: "scalarSubquery", query };
      }
      const e = this.parseExpr();
      this.expect("rparen");
      return e;
    }
    // `EXISTS ( SELECT ... )` — the existence predicate (grammar.md §26). Recognized only when an
    // open-paren + a query start (`SELECT`, or a nested `WITH` — cte.md §7) follows, so `exists`
    // stays usable as a column / function name.
    if (
      this.peekKeyword() === "exists" &&
      this.tokens[this.pos + 1]?.kind === "lparen" &&
      this.isQueryStartAtOffset(2)
    ) {
      this.advance(); // EXISTS
      this.expect("lparen");
      const query = this.parseSubquery();
      this.expect("rparen");
      return { kind: "exists", query };
    }
    // `ROW(e1, e2, …)` composite constructor (spec/design/composite.md §1). Recognized when `ROW`
    // is immediately followed by `(`, so `row` stays usable as a column / function name otherwise.
    // The bare `(a, b)` form is deferred (0A000); only the keyword form parses.
    if (this.peekKeyword() === "row" && this.peekKindAt(1) === "lparen") {
      this.advance(); // ROW
      this.expect("lparen");
      const fields: Expr[] = [];
      if (this.peek().kind !== "rparen") {
        for (;;) {
          fields.push(this.parseExpr());
          const t = this.advance();
          if (t.kind === "comma") continue;
          if (t.kind === "rparen") break;
          throw engineError("syntax_error", `expected ',' or ')', found ${t.kind}`);
        }
      } else {
        this.advance(); // the empty ROW() — consume ')'
      }
      return { kind: "row", fields };
    }
    // `ARRAY[e1, e2, …]` array constructor (spec/design/array.md §1). Recognized when `ARRAY` is
    // immediately followed by `[`, so `array` stays usable as an identifier otherwise.
    if (this.peekKeyword() === "array" && this.peekKindAt(1) === "lbracket") {
      this.advance(); // ARRAY
      this.expect("lbracket");
      const elements: Expr[] = [];
      if (this.peek().kind !== "rbracket") {
        for (;;) {
          elements.push(this.parseExpr());
          const t = this.advance();
          if (t.kind === "comma") continue;
          if (t.kind === "rbracket") break;
          throw engineError("syntax_error", `expected ',' or ']', found ${t.kind}`);
        }
      } else {
        this.advance(); // the empty ARRAY[] — consume ']'
      }
      return { kind: "array", elements };
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
    // The SQL/JSON query functions `JSON_EXISTS` / `JSON_VALUE` / `JSON_QUERY`
    // (json-sql-functions.md §5, S2) — keyword-led primaries with sub-clauses, recognized when the
    // keyword is immediately followed by `(` (so the words stay usable as identifiers otherwise).
    {
      const kw = this.peekKeyword();
      if (
        (kw === "json_exists" || kw === "json_value" || kw === "json_query") &&
        this.peekKindAt(1) === "lparen"
      ) {
        this.advance(); // the function keyword
        this.advance(); // (
        const ctx = this.parseExpr();
        // `FORMAT JSON` after the context item is accepted (and ignored — a text/json/jsonb context
        // is coerced to jsonb regardless).
        this.skipFormatJson();
        this.expect("comma");
        const path = this.parseExpr();
        // `PASSING arg AS name, …` (the path-variable surface) is the deferred S2 follow-on.
        if (this.peekKeyword() === "passing") {
          throw engineError(
            "feature_not_supported",
            "JSON query function PASSING clause is not supported yet",
          );
        }
        let expr: Expr;
        if (kw === "json_exists") {
          const onError = this.parseJsonOnErrorOnly();
          expr = { kind: "jsonExists", ctx, path, onError };
        } else if (kw === "json_value") {
          const returning = this.parseJsonReturning();
          const [onEmpty, onError] = this.parseJsonOnClauses();
          expr = { kind: "jsonValue", ctx, path, returning, onEmpty, onError };
        } else {
          const returning = this.parseJsonReturning();
          const [wrapper, keepQuotes] = this.parseJsonWrapperQuotes();
          const [onEmpty, onError] = this.parseJsonOnClauses();
          expr = { kind: "jsonQuery", ctx, path, returning, wrapper, keepQuotes, onEmpty, onError };
        }
        this.expect("rparen");
        return expr;
      }
    }
    // `JSON(expr [(WITH|WITHOUT) UNIQUE [KEYS]])` — the SQL/JSON JSON() constructor
    // (json-sql-functions.md §5). Distinguished from the `json '...'` typed literal (handled above, a
    // string follows) and a generic call by being the JSON keyword immediately followed by `(`.
    if (this.peekKeyword() === "json" && this.peekKindAt(1) === "lparen") {
      this.advance(); // JSON
      this.advance(); // (
      const operand = this.parseExpr();
      // The unique-keys clause: `(WITH|WITHOUT) UNIQUE [KEYS]`. Consume `WITH`/`WITHOUT` only when
      // `UNIQUE` follows (a two-token lookahead); `KEYS` is optional.
      let uniqueKeys = false;
      const w = this.peekKeyword();
      if ((w === "with" || w === "without") && this.peekKeywordAt(1) === "unique") {
        this.advance(); // WITH / WITHOUT
        this.advance(); // UNIQUE
        if (this.peekKeyword() === "keys") this.advance();
        uniqueKeys = w === "with";
      }
      this.expect("rparen");
      return { kind: "jsonCtor", operand, uniqueKeys };
    }
    if (this.peekKeyword() === "cast") {
      this.advance();
      this.expect("lparen");
      const inner = this.parseExpr();
      this.expectKeyword("as");
      const baseType = this.expectIdentifier();
      const typeMod = this.parseTypeMod();
      const typeName = this.consumeArrayBrackets() ? baseType + "[]" : baseType;
      this.expect("rparen");
      return { kind: "cast", inner, typeName, typeMod };
    }
    // EXTRACT(field FROM source) (grammar.md §50, timezones.md §9.2). Recognized only when `extract`
    // is immediately followed by `(`, so `extract` stays usable as a column / function name otherwise
    // (the one-token lookahead, §8). The field is an identifier or a string literal (lowercased).
    if (this.peekKeyword() === "extract" && this.peekKindAt(1) === "lparen") {
      this.advance(); // EXTRACT
      this.expect("lparen");
      let field: string;
      if (this.peekKindAt(0) === "str") {
        field = this.advance().str!;
      } else {
        field = this.expectIdentifier();
      }
      this.expectKeyword("from");
      const source = this.parseExpr();
      this.expect("rparen");
      return { kind: "extract", field: field.toLowerCase(), source };
    }
    // `COALESCE(a, b, …)` — the first-non-NULL conditional (grammar.md §51). Recognized only
    // when COALESCE is immediately followed by `(` (the JSON(/EXTRACT( one-token lookahead), so
    // the word stays usable as a column name. At least one argument (an empty list is 42601 —
    // PostgreSQL's grammar has no empty form).
    if (this.peekKeyword() === "coalesce" && this.peekKindAt(1) === "lparen") {
      this.advance(); // COALESCE
      this.advance(); // (
      if (this.peekKindAt(0) === "rparen") {
        throw engineError("syntax_error", "COALESCE requires at least one argument");
      }
      const args: Expr[] = [];
      for (;;) {
        args.push(this.parseExpr());
        if (this.peekKindAt(0) !== "comma") break;
        this.advance(); // ,
      }
      this.expect("rparen");
      return { kind: "coalesce", args };
    }
    // `GREATEST(a, b, …)` / `LEAST(a, b, …)` — the variadic max/min (grammar.md §52). Recognized
    // only when the keyword is immediately followed by `(` (the same one-token lookahead), so the
    // words stay usable as column names. At least one argument (an empty list is 42601 —
    // PostgreSQL's grammar has no empty form).
    {
      const kw = this.peekKeyword();
      if ((kw === "greatest" || kw === "least") && this.peekKindAt(1) === "lparen") {
        const greatest = kw === "greatest";
        this.advance(); // GREATEST / LEAST
        this.advance(); // (
        if (this.peekKindAt(0) === "rparen") {
          throw engineError(
            "syntax_error",
            `${greatest ? "GREATEST" : "LEAST"} requires at least one argument`,
          );
        }
        const args: Expr[] = [];
        for (;;) {
          args.push(this.parseExpr());
          if (this.peekKindAt(0) !== "comma") break;
          this.advance(); // ,
        }
        this.expect("rparen");
        return { kind: "greatestLeast", args, greatest };
      }
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
      // The only magnitude > i64 max the lexer admits is 2^63, which fits no signed
      // integer type unless negated (handled by the unary-minus fold).
      const v = foldInt(this.advance().int!, false);
      return { kind: "literal", literal: { kind: "int", int: v } };
    }
    if (t.kind === "decimal") {
      this.advance();
      return {
        kind: "literal",
        literal: {
          kind: "decimal",
          dec: Decimal.fromDigitsScale(false, t.decDigits!, t.decScale!),
        },
      };
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
        return {
          kind: "funcCall",
          name: "now",
          args: [],
          argNames: [],
          star: false,
          distinct: false,
          variadic: false,
        };
      }
      // `current_date` — the SQL-standard bare keyword, desugared to the current_date() catalog
      // function (functions.md §12, date.md §6). Unlike current_timestamp there is no typmod form;
      // a following `(` is the explicit call spelling, which jed also resolves (PG rejects it as a
      // syntax error — a documented jed-lenient divergence).
      if (w === "current_date" && this.tokens[this.pos + 1]?.kind !== "lparen") {
        this.advance();
        return {
          kind: "funcCall",
          name: "current_date",
          args: [],
          argNames: [],
          star: false,
          distinct: false,
          variadic: false,
        };
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
    // A leading DISTINCT (`COUNT(DISTINCT x)`, aggregates.md §5) folds only the distinct argument
    // values. It is not reserved, but here — right after `(` — it is always the modifier.
    // `DISTINCT *` and `DISTINCT )` (no argument) are both 42601 syntax errors (PG); the resolver
    // rejects DISTINCT on a non-aggregate (42809) or a window function (0A000).
    let distinct = false;
    if (this.peekKeyword() === "distinct") {
      this.advance();
      if (this.peek().kind === "star") {
        throw engineError("syntax_error", "DISTINCT cannot be used with *");
      }
      if (this.peek().kind === "rparen") {
        throw engineError("syntax_error", "DISTINCT requires an aggregate argument");
      }
      distinct = true;
    }
    const args: Expr[] = [];
    const names: (string | null)[] = [];
    let star = false;
    let anyNamed = false;
    let variadic = false;
    if (this.peek().kind === "star") {
      this.advance();
      star = true;
    } else if (this.peek().kind !== "rparen") {
      // Empty parens (make_interval()) fall through with empty args.
      for (;;) {
        // The final argument may be `VARIADIC expr` (grammar.md §17, array-functions.md §12): the
        // array is passed directly to a variadic parameter. VARIADIC is a plain keyword (not
        // reserved) recognized only at the start of an argument; once seen, no further argument may
        // follow (42601) and it does not combine with a name.
        if (this.peekKeyword() === "variadic") {
          this.advance();
          variadic = true;
          args.push(this.parseExpr());
          names.push(null);
          // A VARIADIC argument must be the last (PostgreSQL, 42601).
          if (this.peek().kind === "comma") {
            throw engineError("syntax_error", "VARIADIC argument must be the last argument");
          }
          break;
        }
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
    // A trailing `WITHIN GROUP (ORDER BY <key>)` marks an ordered-set aggregate (mode /
    // percentile_cont / percentile_disc — aggregates.md §13). It comes between the argument list and
    // any FILTER / OVER (PG order). WITHIN/GROUP are not reserved; right after the call's `)` they are
    // always the clause. The order key reuses parseOrderBy with allowOrdinal OFF — a general expression
    // (`ORDER BY a + b`) but with a bare integer treated as a constant (not an ordinal), matching
    // PostgreSQL; the resolver enforces exactly one key (42883) and the per-name rules.
    let withinGroup: OrderKey[] | null = null;
    if (this.peekKeyword() === "within") {
      this.advance();
      this.expectKeyword("group");
      this.expect("lparen");
      if (this.peekKeyword() !== "order") {
        throw engineError("syntax_error", "WITHIN GROUP requires an ORDER BY clause");
      }
      withinGroup = this.parseOrderBy(false);
      this.expect("rparen");
    }
    // A trailing `FILTER (WHERE cond)` restricts which input rows feed THIS aggregate
    // (aggregates.md §11). PG syntax: `agg(args) FILTER (WHERE cond) [OVER (...)]` — FILTER binds to
    // the aggregate and precedes any OVER. FILTER is not reserved, but right after the call's `)` it
    // is always the modifier (PG: `count(*) filter` with no `(` is a syntax error, not an alias). The
    // resolver rejects FILTER on a non-aggregate (42809) or a window function (0A000), an aggregate
    // inside cond (42803), and a non-boolean cond (42804).
    let filter: Expr | null = null;
    if (this.peekKeyword() === "filter") {
      this.advance();
      this.expect("lparen");
      if (this.peekKeyword() !== "where") {
        throw engineError("syntax_error", "FILTER requires a WHERE clause");
      }
      this.advance();
      filter = this.parseExpr();
      this.expect("rparen");
    }
    // A trailing `OVER (...)` turns the call into a window-function call (spec/design/window.md,
    // grammar.ebnf `over_clause`). The inline `OVER ( [PARTITION BY cols] [ORDER BY ...] )` form
    // carries an inline definition; a named window `OVER name` (the WINDOW clause — window.md §5)
    // sets `overName` and is desugared to its definition at resolve.
    let over: WindowDef | null = null;
    let overName: string | null = null;
    if (this.peekKeyword() === "over") {
      this.advance();
      // `OVER name` references a named window (the WINDOW clause — window.md §5); `OVER (...)` is an
      // inline definition. A named reference is desugared to its definition at resolve.
      if (this.peek().kind !== "lparen") {
        overName = this.expectIdentifier();
        return {
          kind: "funcCall",
          name,
          args,
          argNames,
          star,
          distinct,
          filter,
          variadic,
          over: null,
          overName,
          withinGroup,
        };
      }
      this.expect("lparen");
      // `[base] [PARTITION BY cols] [ORDER BY …] [frame]` — the shared definition body. A leading
      // base-window name (window.md §5) extends a named window; merged at resolve.
      over = this.parseWindowDefinition();
      this.expect("rparen");
    }
    return {
      kind: "funcCall",
      name,
      args,
      argNames,
      star,
      distinct,
      filter,
      variadic,
      over,
      overName,
      withinGroup,
    };
  }

  // parseWindowFrame parses an optional window frame clause
  // `{ROWS|RANGE|GROUPS} frame_extent [EXCLUDE …]` (spec/design/window.md §6, grammar.ebnf
  // `frame_clause`). A single bound is the START (END = CURRENT ROW). `EXCLUDE` is rejected
  // `0A000` in S4. Returns null when no frame keyword is present (the default frame).
  private parseWindowFrame(): WindowFrame | null {
    let mode: FrameMode;
    switch (this.peekKeyword()) {
      case "rows":
        mode = "rows";
        break;
      case "range":
        mode = "range";
        break;
      case "groups":
        mode = "groups";
        break;
      default:
        return null;
    }
    this.advance();
    let start: FrameBound;
    let end: FrameBound;
    if (this.peekKeyword() === "between") {
      this.advance();
      start = this.parseFrameBound();
      this.expectKeyword("and");
      end = this.parseFrameBound();
    } else {
      // A single bound is the frame START; the END defaults to CURRENT ROW.
      start = this.parseFrameBound();
      end = { kind: "currentRow" };
    }
    const exclude = this.parseFrameExclusion();
    return { mode, start, end, exclude };
  }

  // parseFrameExclusion parses an optional `EXCLUDE { CURRENT ROW | GROUP | TIES | NO OTHERS }`
  // clause (spec/design/window.md §6); absent → "noOthers" (drop nothing).
  private parseFrameExclusion(): FrameExclusion {
    if (this.peekKeyword() !== "exclude") return "noOthers";
    this.advance();
    switch (this.peekKeyword()) {
      case "current":
        this.advance();
        this.expectKeyword("row");
        return "currentRow";
      case "group":
        this.advance();
        return "group";
      case "ties":
        this.advance();
        return "ties";
      case "no":
        this.advance();
        this.expectKeyword("others");
        return "noOthers";
      default:
        throw engineError(
          "syntax_error",
          "expected CURRENT ROW, GROUP, TIES, or NO OTHERS after EXCLUDE",
        );
    }
  }

  // parseFrameBound parses one frame bound: `UNBOUNDED PRECEDING|FOLLOWING`, `CURRENT ROW`, or
  // `expr PRECEDING|FOLLOWING` (spec/design/window.md §6).
  private parseFrameBound(): FrameBound {
    switch (this.peekKeyword()) {
      case "unbounded": {
        this.advance();
        const k = this.peekKeyword();
        if (k === "preceding") {
          this.advance();
          return { kind: "unboundedPreceding" };
        }
        if (k === "following") {
          this.advance();
          return { kind: "unboundedFollowing" };
        }
        throw engineError("syntax_error", "expected PRECEDING or FOLLOWING after UNBOUNDED");
      }
      case "current": {
        this.advance();
        this.expectKeyword("row");
        return { kind: "currentRow" };
      }
      default: {
        const offset = this.parseExpr();
        const k = this.peekKeyword();
        if (k === "preceding") {
          this.advance();
          return { kind: "preceding", offset };
        }
        if (k === "following") {
          this.advance();
          return { kind: "following", offset };
        }
        throw engineError("syntax_error", "expected PRECEDING or FOLLOWING in frame bound");
      }
    }
  }

  // --- cursor helpers ---

  // skipFormatJson skips an optional `FORMAT JSON [ENCODING …]` clause after a SQL/JSON context item
  // (json-sql-functions.md §5). The clause is accepted and ignored — a text/json/jsonb context is
  // coerced to jsonb regardless.
  private skipFormatJson(): void {
    if (this.peekKeyword() === "format" && this.peekKeywordAt(1) === "json") {
      this.advance(); // FORMAT
      this.advance(); // JSON
    }
  }

  // parseJsonReturning parses an optional `RETURNING <type> [FORMAT JSON]` clause → the type name
  // (resolved later).
  private parseJsonReturning(): string | null {
    if (this.peekKeyword() !== "returning") return null;
    this.advance(); // RETURNING
    const ty = this.expectIdentifier();
    this.skipFormatJson();
    return ty;
  }

  // parseJsonBehavior parses one constant SQL/JSON behavior word (`ERROR` / `NULL` / `TRUE` / `FALSE`
  // / `UNKNOWN` / `EMPTY [ARRAY|OBJECT]`). `DEFAULT expr` is the deferred S3 follow-on (0A000).
  private parseJsonBehavior(): JsonOnBehavior {
    switch (this.peekKeyword()) {
      case "error":
        this.advance();
        return "error";
      case "null":
        this.advance();
        return "null";
      case "true":
        this.advance();
        return "true";
      case "false":
        this.advance();
        return "false";
      case "unknown":
        this.advance();
        return "unknown";
      case "empty": {
        this.advance();
        switch (this.peekKeyword()) {
          case "object":
            this.advance();
            return "emptyObject";
          case "array":
            this.advance();
            return "emptyArray";
          // bare `EMPTY` defaults to `EMPTY ARRAY` (PostgreSQL).
          default:
            return "emptyArray";
        }
      }
      case "default":
        throw engineError(
          "feature_not_supported",
          "ON ERROR / ON EMPTY DEFAULT expr is not supported yet",
        );
      default:
        throw engineError("syntax_error", "expected a SQL/JSON ON ERROR/EMPTY behavior");
    }
  }

  // parseJsonOnErrorOnly parses JSON_EXISTS's single optional `<behavior> ON ERROR` clause.
  private parseJsonOnErrorOnly(): JsonOnBehavior | null {
    if (this.isJsonBehaviorStart() && this.peekOnClauseIs("error")) {
      const b = this.parseJsonBehavior();
      this.advance(); // ON
      this.advance(); // ERROR
      return b;
    }
    return null;
  }

  // parseJsonOnClauses parses the optional `<behavior> ON EMPTY` then `<behavior> ON ERROR` clauses
  // (in that order).
  private parseJsonOnClauses(): [JsonOnBehavior | null, JsonOnBehavior | null] {
    let onEmpty: JsonOnBehavior | null = null;
    let onError: JsonOnBehavior | null = null;
    if (this.isJsonBehaviorStart() && this.peekOnClauseIs("empty")) {
      const b = this.parseJsonBehavior();
      this.advance(); // ON
      this.advance(); // EMPTY
      onEmpty = b;
    }
    if (this.isJsonBehaviorStart() && this.peekOnClauseIs("error")) {
      const b = this.parseJsonBehavior();
      this.advance(); // ON
      this.advance(); // ERROR
      onError = b;
    }
    return [onEmpty, onError];
  }

  // parseJsonWrapperQuotes parses JSON_QUERY's optional
  // `[WITH [COND|UNCOND] [ARRAY] WRAPPER | WITHOUT [ARRAY] WRAPPER]` and
  // `[KEEP|OMIT QUOTES [ON SCALAR STRING]]` clauses.
  private parseJsonWrapperQuotes(): [JsonWrapper, boolean] {
    let wrapper: JsonWrapper = "without";
    switch (this.peekKeyword()) {
      case "with": {
        this.advance(); // WITH
        switch (this.peekKeyword()) {
          case "conditional":
            this.advance();
            wrapper = "conditional";
            break;
          case "unconditional":
            this.advance();
            wrapper = "unconditional";
            break;
          default:
            wrapper = "unconditional";
            break;
        }
        if (this.peekKeyword() === "array") this.advance();
        this.expectKeyword("wrapper");
        break;
      }
      case "without": {
        this.advance(); // WITHOUT
        if (this.peekKeyword() === "array") this.advance();
        this.expectKeyword("wrapper");
        break;
      }
      default:
        break;
    }
    let keepQuotes = true;
    switch (this.peekKeyword()) {
      case "keep":
        this.advance();
        this.expectKeyword("quotes");
        this.skipOnScalarString();
        break;
      case "omit":
        this.advance();
        this.expectKeyword("quotes");
        this.skipOnScalarString();
        keepQuotes = false;
        break;
      default:
        break;
    }
    return [wrapper, keepQuotes];
  }

  // skipOnScalarString skips an optional `ON SCALAR STRING` after a QUOTES clause.
  private skipOnScalarString(): void {
    if (this.peekKeyword() === "on" && this.peekKeywordAt(1) === "scalar") {
      this.advance(); // ON
      this.advance(); // SCALAR
      if (this.peekKeyword() === "string") this.advance();
    }
  }

  // isJsonBehaviorStart reports whether the cursor is at a SQL/JSON behavior word
  // (ERROR/NULL/TRUE/FALSE/UNKNOWN/EMPTY/DEFAULT).
  private isJsonBehaviorStart(): boolean {
    switch (this.peekKeyword()) {
      case "error":
      case "null":
      case "true":
      case "false":
      case "unknown":
      case "empty":
      case "default":
        return true;
      default:
        return false;
    }
  }

  // peekOnClauseIs reports whether the upcoming clause is `… ON <which>` (a one-or-two-token
  // lookahead past the behavior — the behavior is 1 token, or 2 for EMPTY ARRAY/OBJECT).
  private peekOnClauseIs(which: string): boolean {
    for (const skip of [1, 2]) {
      if (this.peekKeywordAt(skip) === "on" && this.peekKeywordAt(skip + 1) === which) {
        return true;
      }
    }
    return false;
  }

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

  // expectCollationName consumes a quoted collation name after COLLATE (spec/design/collation.md §1).
  // The name is a double-quoted identifier — case-sensitive and kept verbatim ("C", "en-US") — so a
  // bare word is not accepted (it would case-fold). An empty name ("") is a 42601 syntax error.
  private expectCollationName(): string {
    const t = this.advance();
    if (t.kind !== "quotedIdent") {
      throw engineError("syntax_error", "expected a quoted collation name after COLLATE");
    }
    if (t.str === "") {
      throw engineError("syntax_error", "collation name may not be empty");
    }
    return t.str!;
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

// Rewrite one column identifier in persisted expression text (alter.md §2.2). Canonical tokens let
// us distinguish bare/table-qualified columns from function/type/named-argument names and unrelated
// composite fields; the result is reparsed before entering the catalog.
export function rewriteColumnIdentifier(
  text: string,
  table: string,
  oldName: string,
  newName: string,
): { text: string; expr: Expr } {
  const tokens = lex(text);
  for (let i = 0; i < tokens.length; i++) {
    const t = tokens[i]!;
    if (t.kind !== "word" || lower(t.word!) !== lower(oldName)) continue;
    const prev = tokens[i - 1];
    const prev2 = tokens[i - 2];
    const next = tokens[i + 1];
    const skip =
      next?.kind === "lparen" ||
      next?.kind === "fatArrow" ||
      next?.kind === "str" ||
      prev?.kind === "doubleColon" ||
      (prev?.kind === "word" && lower(prev.word!) === "as") ||
      (prev?.kind === "dot" && !(prev2?.kind === "word" && lower(prev2.word!) === lower(table))) ||
      (next?.kind === "dot" && lower(oldName) === lower(table));
    if (!skip) t.word = newName;
  }
  if (tokens.at(-1)?.kind === "eof") tokens.pop();
  const rewritten = renderTokens(tokens);
  return { text: rewritten, expr: parseExpression(rewritten) };
}

export function rewriteTableQualifier(
  text: string,
  oldName: string,
  newName: string,
): { text: string; expr: Expr } {
  const tokens = lex(text);
  for (let i = 0; i + 1 < tokens.length; i++) {
    const t = tokens[i]!;
    if (t.kind === "word" && lower(t.word!) === lower(oldName) && tokens[i + 1]!.kind === "dot")
      t.word = newName;
  }
  if (tokens.at(-1)?.kind === "eof") tokens.pop();
  const rewritten = renderTokens(tokens);
  return { text: rewritten, expr: parseExpression(rewritten) };
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
    case "quotedIdent":
      // A double-quoted identifier round-trips verbatim with `"` doubled (collation names in a
      // persisted COLLATE expression, spec/design/collation.md §1).
      return '"' + t.str!.replaceAll('"', '""') + '"';
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
    case "lbracket":
      return "[";
    case "rbracket":
      return "]";
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
    case "ne":
      // The canonical not-equal spelling `<>` (its `!=` alias re-lexes to the same `ne` token —
      // lexer.ts; grammar.md §4). Missing this case previously rendered `<>` as empty in the
      // persisted check/default expression text — a cross-core divergence surfaced by
      // jed_constraints.expression (introspection.md §5.1).
      return "<>";
    case "lt":
      return "<";
    case "gt":
      return ">";
    case "le":
      return "<=";
    case "ge":
      return ">=";
    case "colon":
      return ":";
    case "concat":
      return "||";
    case "contains":
      return "@>";
    case "jsonPathExists":
      return "@?";
    case "jsonPathMatch":
      return "@@";
    case "containedBy":
      return "<@";
    case "overlaps":
      return "&&";
    case "strictlyLeft":
      return "<<";
    case "strictlyRight":
      return ">>";
    case "notExtendRight":
      return "&<";
    case "notExtendLeft":
      return "&>";
    case "adjacent":
      return "-|-";
    case "arrow":
      return "->";
    case "arrowText":
      return "->>";
    case "hashArrow":
      return "#>";
    case "hashArrowText":
      return "#>>";
    case "question":
      return "?";
    case "questionPipe":
      return "?|";
    case "questionAmp":
      return "?&";
    case "hashMinus":
      return "#-";
    case "tilde":
      return "~";
    case "tildeStar":
      return "~*";
    case "bangTilde":
      return "!~";
    case "bangTildeStar":
      return "!~*";
    default: // "eof" — never inside the parentheses
      return "";
  }
}
