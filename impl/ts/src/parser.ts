// Hand-written recursive-descent parser (CLAUDE.md §5, §10). Mirrors parser.go /
// parser.rs. Errors throw EngineError (42601). The lexer emits generic word tokens (no
// reserved-keyword table); the lexer folds every bare word to lowercase, so keywords match
// byte-exactly here (and a quoted identifier is never a keyword — grammar.md §3).

import type {
  AlterColumnAction,
  AlterTableEdit,
  Assignment,
  BinaryOp,
  CheckDef,
  CheckOption,
  Cte,
  CteBody,
  CycleClause,
  SearchClause,
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
  ConflictColumn,
  ConflictTarget,
  Insert,
  InsertValue,
  JoinClause,
  JoinKind,
  JsonBehavior,
  JsonOnBehavior,
  JsonPredicateKind,
  JsonPassing,
  JsonTable,
  JsonWrapper,
  JtColumn,
  Literal,
  OnConflict,
  OrderKey,
  Overriding,
  QueryExpr,
  ReindexKind,
  ReturningClause,
  Select,
  SelectItem,
  SelectItems,
  SeqOptions,
  SeqRestart,
  SetOpKind,
  Statement,
  SubscriptSpec,
  TableRef,
  SrcLoc,
  TableRefPos,
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
import { cteBodyAsQuery, emptySeqOptions, seqOptionsHasAny } from "./ast.ts";
import { Decimal } from "./decimal.ts";
import type { OnCommitAction } from "./catalog.ts";
import { EngineError, engineError } from "./errors.ts";
import { lex, lexWithPositions } from "./lexer.ts";
import type { Token, TokenKind } from "./token.ts";

const I64_MIN = -9223372036854775808n;
const I64_MAX = 9223372036854775807n;

// isIdentKind reports whether a token kind can be an identifier: a bare word (folded) or a quoted
// identifier (exact) — spec/design/grammar.md §3.
function isIdentKind(k: TokenKind | undefined): boolean {
  return k === "word" || k === "quotedIdent";
}

// identName is the name an identifier token spells (a bare word's folded text, a quoted
// identifier's exact content), or null for any other token.
function identName(t: Token | undefined): string | null {
  if (t === undefined) return null;
  if (t.kind === "word") return t.word!;
  if (t.kind === "quotedIdent") return t.str!;
  return null;
}

// BARE_LABEL_STOP_KEYWORDS holds the bare words (already folded) that may NOT be a select item's
// bare output label — after a complete expression they need `AS`. Exactly PostgreSQL's AS_LABEL
// keywords (kwlist.h): words that begin a clause after a select item, continue a type name, or
// introduce an interval field or a window/aggregate postfix. Any other bare word after an
// expression is the item's output label. This set is a CLAUDE.md §8 cross-core determinism
// surface (spec/design/grammar.md §8).
const BARE_LABEL_STOP_KEYWORDS: ReadonlySet<string> = new Set([
  "array",
  "as",
  "char",
  "character",
  "create",
  "day",
  "except",
  "fetch",
  "filter",
  "for",
  "from",
  "grant",
  "group",
  "having",
  "hour",
  "intersect",
  "into",
  "isnull",
  "limit",
  "minute",
  "month",
  "notnull",
  "offset",
  "on",
  "order",
  "over",
  "overlaps",
  "precision",
  "returning",
  "second",
  "to",
  "union",
  "varying",
  "where",
  "window",
  "with",
  "within",
  "without",
  "year",
]);

// isTableRefStopKeyword reports whether kw (already folded by the lexer) is a keyword that may legally
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

// joinGroupBody is the body of a parenthesized join (grammar.md §15): a `SELECT *` over its chain.
// Unaliased, the caller marks it joinGroup.
function joinGroupBody(from: TableRef, joins: JoinClause[]): Select {
  return {
    kind: "select",
    distinct: false,
    items: { kind: "all" },
    from,
    joins,
    filter: null,
    groupBy: [],
    having: null,
    orderBy: [],
    limit: null,
    offset: null,
    windows: [],
    parenJoin: true,
  };
}

// groupOuterJoinItem makes a later comma item whose chain holds a RIGHT or FULL join the unaliased
// parenthesized join it is (grammar.md §15): `FROM t, a RIGHT JOIN b ON p` is
// `FROM t, (a RIGHT JOIN b ON p)`. Flattened into the left-deep chain, `(t × a) RIGHT JOIN b` would
// keep an unmatched b row once rather than once per t row. seg indexes the item's comma join; the
// joins after it are its chain. An item of only inner and LEFT joins stays flattened — the same rows
// either way.
function groupOuterJoinItem(joins: JoinClause[], seg: number): void {
  const chain = joins.slice(seg + 1);
  if (!chain.some((j) => j.kind === "right" || j.kind === "full")) return;
  const head = joins[seg]!;
  const body = joinGroupBody(head.table, chain);
  body.joinGroup = true;
  joins.splice(seg, joins.length - seg, {
    ...head,
    table: { name: "", alias: null, args: null, subquery: body },
  });
}

// spliceLeadingGroup removes the parentheses from an unaliased parenthesized join that BEGINS a
// left-deep chain (spec/design/grammar.md §15): `(a JOIN b ON p) JOIN c ON q` is exactly
// `a JOIN b ON p JOIN c ON q`, the same join tree with the same ON scopes, so the group's chain
// becomes the chain's prefix and joins follow it. Any other item is returned unchanged.
function spliceLeadingGroup(
  item: TableRef,
  joins: JoinClause[],
): { from: TableRef; joins: JoinClause[] } {
  const group = item.subquery;
  if (
    item.alias !== null ||
    group === undefined ||
    group.kind !== "select" ||
    group.joinGroup !== true
  ) {
    return { from: item, joins };
  }
  return { from: group.from!, joins: [...group.joins, ...joins] };
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

// binaryExpr builds a binary-operator expression node; loc is the operator's source character
// position (spec/design/error-fields.md §10.2).
function binaryExpr(op: BinaryOp, lhs: Expr, rhs: Expr, loc: SrcLoc | undefined): Expr {
  return { kind: "binary", op, lhs, rhs, loc };
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
      loc: expr.loc,
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
      loc: expr.loc,
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
      loc: expr.loc,
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
        loc: expr.inner.loc,
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
        loc: expr.inner.loc,
        collation: expr.collation,
        descending,
        nullsFirst,
      };
    }
  }
  return { ordinal: null, expr, qualifier: null, column: "", collation, descending, nullsFirst };
}

// MAX_EXPR_DEPTH is the maximum expression / subquery / set-operation nesting depth a statement
// may reach (spec/design/cost.md §7; CLAUDE.md §13). The §13 native-stack-safety gate for
// untrusted input: the recursive-descent parser and the resolve/eval walks recurse to a
// statement's nesting depth, so deeply-nested SQL would overflow the call stack BEFORE the cost
// meter runs (54P01 cannot catch it; in this core an overflow is a V8 RangeError, catchable but
// carrying no SQLSTATE). Counting logical depth against this fixed bound — rather than PG's runtime
// stack-pointer probe — is deterministic and cross-core identical (§8): the constant is the SAME
// in every core (Rust / Go / TS). Far above any realistic query, 256 still fits this core's stack
// budget: every statement at the limit runs within 512 KB of V8 stack (the worst, nested WITH,
// needs ~410 KB; V8's default is ~984 KB — cost.md §7.1, enforced by tests/depth_limit.test.ts).
// Exceeding it throws 54001 statement_too_complex.
export const MAX_EXPR_DEPTH = 256;

// MAX_IDENTIFIER_LENGTH is enforced in the lexer (the identifier-token producer); re-exported here
// so the two parser-level limits sit together (spec/design/cost.md §7; CLAUDE.md §13).
export { MAX_IDENTIFIER_LENGTH } from "./lexer.ts";

// parseSQL parses one statement of host-written text. An error carries the character position of
// the token the parse stopped at (spec/design/error-fields.md §10.2/§10.3), except a class-54 program
// limit.
export function parseSQL(sql: string): Statement {
  const { tokens, positions } = lexWithPositions(sql);
  const p = new Parser(tokens, positions);
  try {
    const stmt = p.parseStatement();
    p.expectEof();
    return stmt;
  } catch (e) {
    if (e instanceof EngineError && !e.code().startsWith("54")) e.withPosition(p.furthestLoc());
    throw e;
  }
}

// SelectInto is the target of a `SELECT … INTO [TEMP | TEMPORARY] [TABLE] [db.]name`
// (create-table-as.md §6).
type SelectInto = { temp: boolean; db: string | undefined; name: string };

// unloggedNotSupported is the error of `CREATE UNLOGGED TABLE` and `SELECT … INTO UNLOGGED`
// (create-table-as.md §6): jed has no unlogged tables — every table is durable (CLAUDE.md §9) — so
// the form is recognized and refused.
function unloggedNotSupported(): EngineError {
  return engineError("feature_not_supported", "unlogged tables are not supported");
}

// Parser is a token cursor over a single statement.
class Parser {
  private tokens: Token[];
  private pos: number;
  // Current expression/query nesting depth (see MAX_EXPR_DEPTH). Incremented once per AST level
  // descended (deepen), restored on the way back up; left stale on the error path because a depth
  // error aborts the whole parse.
  private depth: number;
  // The deepest depth the parse has reached. A stored view body's is its nesting depth
  // (spec/design/views.md §4.1), which a reference adds to its own.
  maxDepth: number;
  // The token position just past the `)` of the most recent bare `(a, b, …)` row constructor (0 =
  // none). A bare row is not a parenthesized base, so parsePostfix refuses `.field` on it
  // (PostgreSQL: `(1,2).f1` is a syntax error, `((1,2)).f1` is field access).
  private bareRowEnd: number;
  // Per token index, whether a PARENTHESIZED QUERY begins there (isQueryStartAtOffset, grammar.md
  // §25). Precomputed once in one backward pass so every probe is O(1) — a long run of `(` costs
  // linear time, not time per nesting level.
  private parenQuery: boolean[];
  // Each token's 1-based character position in the statement text (spec/design/error-fields.md
  // §10.1), index-aligned with tokens; empty for a parse of stored text, which records none.
  private positions: number[];
  // The furthest token index the parse has examined through peek / advance — the token a parse
  // error is positioned at (spec/design/error-fields.md §10.3).
  private furthest: number;
  // SELECT … INTO (spec/design/create-table-as.md §6): whether the next selectCore is the leftmost
  // SELECT of the statement's own query, the one place an INTO clause may appear. Set for a query
  // statement (and a CREATE VIEW body, which refuses it); the first selectCore takes it, so every
  // later or nested one sees false.
  private intoOk = false;
  // Whether the statement's own query is being parsed (a query statement or a CREATE VIEW body), so
  // an INTO on a later set-operation arm is "only allowed on first SELECT" rather than "not allowed
  // here".
  private intoTop = false;
  // Whether the query is a CREATE VIEW body: its leftmost INTO is 0A000.
  private intoView = false;
  // How many query constructs (a SELECT's clauses, a CTE body) enclose the cursor within the
  // statement's own query: an INTO at depth 0 is on a set-operation arm.
  private intoNest = 0;
  // The INTO target the leftmost SELECT named, taken by the statement-level caller.
  private selectInto: SelectInto | null = null;

  // A parser over stored text passes no positions (error-fields.md §10.1); parseSQL passes the
  // host-written text's.
  constructor(tokens: Token[], positions: number[] = []) {
    this.tokens = tokens;
    this.positions = positions;
    this.furthest = 0;
    this.pos = 0;
    this.depth = 0;
    this.maxDepth = 0;
    this.bareRowEnd = 0;
    this.parenQuery = [];
    this.parenQuery = this.computeParenQuery();
  }

  // computeParenQuery builds the parenQuery table: the `(` at i begins a parenthesized query when
  // what follows it is a query start (a SELECT, a VALUES list, a WITH clause, or — recursively —
  // another parenthesized query) AND its matching `)` is followed by a token that can only continue
  // a query (isQueryFollowAtIndex). Computed back to front, so the recursion is a table lookup.
  private computeParenQuery(): boolean[] {
    const n = this.tokens.length;
    const closeAt: number[] = new Array<number>(n).fill(n);
    const open: number[] = [];
    for (let i = 0; i < n; i++) {
      const kind = this.tokens[i].kind;
      if (kind === "lparen") {
        open.push(i);
      } else if (kind === "rparen" && open.length > 0) {
        closeAt[open.pop()!] = i;
      }
    }
    const q: boolean[] = new Array<boolean>(n).fill(false);
    for (let i = n - 1; i >= 0; i--) {
      if (this.tokens[i].kind !== "lparen") continue;
      const inner =
        this.isSelectAtIndex(i + 1) ||
        this.isValuesAtIndex(i + 1) ||
        this.isWithClauseAtIndex(i + 1) ||
        (i + 1 < n && q[i + 1]);
      q[i] = inner && this.isQueryFollowAtIndex(closeAt[i] + 1);
    }
    return q;
  }

  // deepen descends one nesting level, enforcing MAX_EXPR_DEPTH (spec/design/cost.md §7). Call at
  // every point the AST gains a level — a binary-chain step, a unary, a postfix, a re-entry into a
  // fresh sub-expression, a nested subquery, a set-op branch. The caller restores the depth with
  // undeepen on the success path (a throw short-circuits, leaving it stale, which is harmless: the
  // parse is aborting).
  private deepen(): void {
    this.depth++;
    this.maxDepth = Math.max(this.maxDepth, this.depth);
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
      case "analyze":
        return this.parseAnalyze();
      case "reindex":
        return this.parseReindex();
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
        // CREATE VIEW (views.md §1, grammar.md §54) — VIEW is recognized positionally, so
        // `CREATE TABLE view (…)` still names a table. The deferred forms are recognized by their
        // keyword sequence and refused with 0A000 before the body is parsed (§12).
        if (this.atDeferredCreateView()) {
          throw engineError("feature_not_supported", "this form of CREATE VIEW is not supported");
        }
        if (this.atCreateView()) return this.parseCreateView();
        return this.parseCreateTable();
      case "drop":
        if (this.peekKeywordAt(1) === "index") return this.parseDropIndex();
        if (this.peekKeywordAt(1) === "type") return this.parseDropType();
        if (this.peekKeywordAt(1) === "sequence") return this.parseDropSequence();
        if (this.peekKeywordAt(1) === "view") return this.parseDropView();
        return this.parseDropTable();
      // ALTER SEQUENCE — the only ALTER statement this slice (sequences.md §4). A 2-token lookahead
      // recognizes it; any other `ALTER …` (TABLE, SYSTEM, …) is not a statement keyword jed knows
      // and falls through to the generic unknown-keyword 42601 (the no-escape-hatch surface).
      case "alter":
        if (this.peekKeywordAt(1) === "sequence") return this.parseAlterSequence();
        // ALTER VIEW … RENAME (views.md §7.1); its other actions are recognized and refused.
        if (this.peekKeywordAt(1) === "view") return this.parseAlterView();
        if (this.peekKeywordAt(1) === "table") return this.parseAlterTable();
        throw engineError("syntax_error", "unexpected keyword 'alter'");
      case "insert":
        return this.parseInsert();
      case "select":
        return this.parseIntoQuery(() => this.parseQueryExpr());
      // A top-level VALUES query (grammar.md §25): `VALUES (…), (…) [ORDER BY …]`.
      case "values":
        return this.parseIntoQuery(() => this.parseQueryExpr());
      // `WITH …` at statement start can only begin a query with common table expressions
      // (spec/design/cte.md). `with` is non-reserved but unambiguous here.
      case "with":
        return this.parseIntoQuery(() => this.parseWithStatement());
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
      case "savepoint":
        this.advance(); // SAVEPOINT
        return { kind: "savepoint", op: "define", name: this.expectIdentifier() };
      case "release":
        this.advance(); // RELEASE
        return { kind: "savepoint", op: "release", name: this.parseSavepointName() };
      case "":
        // A leading `(` can only begin a parenthesized query (grammar.md §25).
        if (this.peek().kind === "lparen") return this.parseIntoQuery(() => this.parseQueryExpr());
        throw engineError("syntax_error", "expected a SQL statement");
      default:
        throw engineError("syntax_error", `unexpected keyword '${this.peekKeyword()}'`);
    }
  }

  // parseIntoQuery parses a query statement whose leftmost SELECT may carry INTO
  // (spec/design/create-table-as.md §6). A query with an INTO target is the statement `CREATE [TEMP]
  // TABLE target AS query`.
  private parseIntoQuery(parse: () => Statement): Statement {
    this.intoOk = true;
    this.intoTop = true;
    const stmt = parse();
    this.intoOk = false;
    this.intoTop = false;
    const into = this.selectInto;
    if (into === null) return stmt;
    this.selectInto = null;
    let query: QueryExpr;
    if (stmt.kind === "select" || stmt.kind === "setOp") {
      query = stmt;
    } else if (stmt.kind === "with" && cteBodyAsQuery(stmt.body) !== null) {
      query = {
        kind: "withExpr",
        ctes: stmt.ctes,
        recursive: stmt.recursive,
        body: cteBodyAsQuery(stmt.body)!,
      };
    } else {
      throw engineError("syntax_error", "SELECT ... INTO is not allowed here");
    }
    return {
      kind: "createTable",
      name: into.name,
      db: into.db,
      temp: into.temp,
      ifNotExists: false,
      columns: [],
      tablePks: [],
      checks: [],
      uniques: [],
      fks: [],
      excludes: [],
      asQuery: { columnNames: null, query, withData: true },
    };
  }

  // parseSelectInto parses the `INTO [TEMPORARY | TEMP] [TABLE] [db.]name` of a SELECT … INTO, after
  // its select list (spec/design/create-table-as.md §6). owner is whether this SELECT is the
  // statement's leftmost one. Anywhere else the clause is 42601 — on a later set-operation arm "INTO
  // is only allowed on first SELECT", in a subquery, CTE body, or another statement's query "not
  // allowed here" — and in a CREATE VIEW body 0A000, as in PostgreSQL. TEMP, TEMPORARY, UNLOGGED,
  // and TABLE are words of the clause unless a `.` follows (a database qualifier).
  private parseSelectInto(owner: boolean): void {
    this.advance(); // INTO
    if (!owner) {
      const msg =
        this.intoTop && this.intoNest === 1
          ? "INTO is only allowed on first SELECT of UNION/INTERSECT/EXCEPT"
          : "SELECT ... INTO is not allowed here";
      throw engineError("syntax_error", msg);
    }
    if (this.intoView) {
      throw engineError("feature_not_supported", "views must not contain SELECT INTO");
    }
    const word = (w: string): boolean => this.peekKeyword() === w && this.peekKindAt(1) !== "dot";
    const temp = word("temp") || word("temporary");
    if (temp) this.advance();
    else if (word("unlogged")) throw unloggedNotSupported();
    if (word("table")) this.advance();
    const [db, name] = this.parseQualifiedTableName();
    this.selectInto = { temp, db, name };
  }

  // atDeferredCreateView reports whether the statement is one of the deferred `CREATE … VIEW` forms
  // (views.md §12): `CREATE [OR REPLACE] TEMP | TEMPORARY VIEW`, `CREATE [OR REPLACE] RECURSIVE
  // VIEW`, `CREATE MATERIALIZED VIEW`. Recognized by keyword sequence so each fails 0A000, not 42601.
  private atDeferredCreateView(): boolean {
    const at = this.peekKeywordAt(1) === "or" && this.peekKeywordAt(2) === "replace" ? 3 : 1;
    switch (this.peekKeywordAt(at)) {
      case "temp":
      case "temporary":
      case "recursive":
      case "materialized":
        return this.peekKeywordAt(at + 1) === "view";
      default:
        return false;
    }
  }

  // atCreateView reports whether the statement is `CREATE [OR REPLACE] VIEW` (views.md §1).
  private atCreateView(): boolean {
    return (
      this.peekKeywordAt(1) === "view" ||
      (this.peekKeywordAt(1) === "or" &&
        this.peekKeywordAt(2) === "replace" &&
        this.peekKeywordAt(3) === "view")
    );
  }

  // parseAlterView parses `ALTER VIEW [IF EXISTS] [db.]name RENAME { TO new | [COLUMN] old TO new }`
  // (views.md §7.1). The other ALTER VIEW actions (`ALTER [COLUMN]`, `OWNER TO`, `SET …`, `RESET …`)
  // are recognized by their first keyword and refused with 0A000.
  private parseAlterView(): Statement {
    this.expectKeyword("alter");
    this.expectKeyword("view");
    let ifExists = false;
    if (this.peekKeyword() === "if") {
      this.advance();
      this.expectKeyword("exists");
      ifExists = true;
    }
    const [db, name] = this.parseQualifiedTableName();
    switch (this.peekKeyword()) {
      case "rename":
        break;
      case "alter":
      case "owner":
      case "set":
      case "reset":
        throw engineError("feature_not_supported", "this form of ALTER VIEW is not supported");
      default:
        throw engineError("syntax_error", "expected RENAME after ALTER VIEW name");
    }
    this.advance(); // RENAME
    if (this.peekKeyword() === "to") {
      this.advance();
      return {
        kind: "alterView",
        name,
        db,
        ifExists,
        action: { kind: "renameTo", newName: this.expectIdentifier() },
      };
    }
    if (this.peekKeyword() === "column") this.advance();
    const oldName = this.expectIdentifier();
    this.expectKeyword("to");
    return {
      kind: "alterView",
      name,
      db,
      ifExists,
      action: { kind: "renameColumn", oldName, newName: this.expectIdentifier() },
    };
  }

  // parseCreateView parses `CREATE [OR REPLACE] VIEW [IF NOT EXISTS] [db.]name [(col, …)] AS
  // subquery_expr` (views.md §1, grammar.md §54). The body runs from the token after `AS` to the end
  // of the statement and is captured as its canonical token text. A trailing `WITH [CASCADED |
  // LOCAL] CHECK OPTION` is cut off before the body and gives the view a check option (§15.6). A
  // `WITH ( … )` option list after the name is a deferred form (0A000). `OR REPLACE` with `IF NOT
  // EXISTS` is 42601.
  private parseCreateView(): Statement {
    this.expectKeyword("create");
    const orReplace = this.peekKeyword() === "or";
    if (orReplace) {
      this.advance(); // OR
      this.expectKeyword("replace");
    }
    this.expectKeyword("view");
    // A two-token lookahead keeps a lone `if` an ordinary view name (`CREATE VIEW if AS …`).
    const ifNotExists =
      this.peekKeyword() === "if" && this.peekKeywordAt(1) === "not"
        ? this.parseIfNotExists()
        : false;
    if (orReplace && ifNotExists) {
      throw engineError(
        "syntax_error",
        "CREATE OR REPLACE VIEW cannot be combined with IF NOT EXISTS",
      );
    }
    const [db, name] = this.parseQualifiedTableName();
    if (this.peekKeyword() === "with" && this.peekKindAt(1) === "lparen") {
      throw engineError("feature_not_supported", "view options (WITH ( … )) are not supported");
    }
    let columns: string[] | null = null;
    if (this.peek().kind === "lparen") {
      this.advance();
      columns = [this.expectIdentifier()];
      while (this.peek().kind === "comma") {
        this.advance();
        columns.push(this.expectIdentifier());
      }
      this.expect("rparen");
    }
    this.expectKeyword("as");
    // A trailing `WITH [CASCADED | LOCAL] CHECK OPTION` is cut off before the body is parsed (an
    // unreserved `with` would otherwise be read as a table alias).
    const trailing = this.trailingCheckOption();
    if (trailing !== null) this.tokens[trailing.at] = { kind: "eof" };
    const start = this.pos;
    // The body's leftmost SELECT may not take INTO (0A000, create-table-as.md §6).
    this.intoOk = this.intoTop = this.intoView = true;
    this.parseSubquery();
    this.intoOk = this.intoTop = this.intoView = false;
    const end = this.pos;
    if (trailing !== null && end !== trailing.at) {
      throw engineError("syntax_error", "unexpected trailing input");
    }
    const body = renderTokens(this.tokens.slice(start, end));
    // renderTokens re-lexes to the same token sequence, so the body's tokens keep the statement's
    // positions (and its eof the position of the token after the body).
    const bodyLocs = this.positions.slice(start, end + 1);
    const checkOption = trailing === null ? null : trailing.option;
    return {
      kind: "createView",
      name,
      db,
      orReplace,
      ifNotExists,
      columns,
      body,
      bodyLocs,
      checkOption,
    };
  }

  // trailingCheckOption returns the index and option of a trailing `WITH [CASCADED | LOCAL] CHECK
  // OPTION` at the end of the token stream (views.md §15.6), or null if the statement does not end
  // with one. Bare `WITH CHECK OPTION` is CASCADED.
  private trailingCheckOption(): { at: number; option: CheckOption } | null {
    let end = this.tokens.length;
    if (this.tokens[end - 1]?.kind === "eof") end--;
    const word = (i: number, w: string): boolean => {
      const t = this.tokens[i];
      return t !== undefined && t.kind === "word" && t.word === w;
    };
    if (end < this.pos + 3 || !word(end - 1, "option") || !word(end - 2, "check")) return null;
    if (word(end - 3, "with")) return { at: end - 3, option: "cascaded" };
    if (end >= this.pos + 4 && word(end - 4, "with")) {
      if (word(end - 3, "cascaded")) return { at: end - 4, option: "cascaded" };
      if (word(end - 3, "local")) return { at: end - 4, option: "local" };
    }
    return null;
  }

  // parseDropView parses `DROP VIEW [IF EXISTS] [db.]name [, …] [CASCADE | RESTRICT]` (views.md §1).
  // IF EXISTS is a two-token lookahead, so `DROP VIEW if` drops a view named `if`.
  private parseDropView(): Statement {
    this.expectKeyword("drop");
    this.expectKeyword("view");
    const ifExists = this.peekKeyword() === "if" && this.peekKeywordAt(1) === "exists";
    if (ifExists) {
      this.advance(); // IF
      this.advance(); // EXISTS
    }
    const qualified = (): { db: string | undefined; name: string } => {
      const [db, name] = this.parseQualifiedTableName();
      return { db, name };
    };
    const names = [qualified()];
    while (this.peek().kind === "comma") {
      this.advance();
      names.push(qualified());
    }
    let cascade = false;
    if (this.peekKeyword() === "cascade") {
      this.advance();
      cascade = true;
    } else if (this.peekKeyword() === "restrict") {
      this.advance();
    }
    return { kind: "dropView", names, ifExists, cascade };
  }

  // parseAnalyze parses `ANALYZE qualified_table [(identifier [, identifier ...])]`.
  private parseAnalyze(): Statement {
    this.expectKeyword("analyze");
    const [db, name] = this.parseQualifiedTableName();
    const columns: string[] = [];
    if (this.peek().kind === "lparen") {
      this.advance();
      for (;;) {
        columns.push(this.expectIdentifier());
        if (this.peek().kind !== "comma") break;
        this.advance();
      }
      this.expect("rparen");
    }
    return { kind: "analyze", name, db, columns };
  }

  // parseReindex parses `REINDEX [(option, …)] {INDEX | TABLE} [CONCURRENTLY] [db.]name` or
  // `REINDEX [(option, …)] {DATABASE | SYSTEM} [CONCURRENTLY] [name]` (spec/design/indexes.md §10,
  // grammar.md §59). Options are `VERBOSE [boolean]` and `CONCURRENTLY [boolean]`, repeatable (the
  // last value wins, as PostgreSQL); an unknown option is 42601, `TABLESPACE` and `REINDEX SCHEMA`
  // are 0A000 (jed has neither tablespaces nor schemas). The positional `CONCURRENTLY` is recognized
  // only before a name for INDEX/TABLE, so a relation named `concurrently` stays nameable.
  private parseReindex(): Statement {
    this.expectKeyword("reindex");
    let concurrently = false;
    let verbose = false;
    if (this.peek().kind === "lparen") {
      this.advance();
      for (;;) {
        const name = this.peekKeyword();
        if (name === "") throw engineError("syntax_error", "expected a REINDEX option");
        this.advance();
        if (name === "verbose") {
          verbose = this.parseReindexBoolean(name);
        } else if (name === "concurrently") {
          concurrently = this.parseReindexBoolean(name);
        } else if (name === "tablespace") {
          throw engineError(
            "feature_not_supported",
            "REINDEX TABLESPACE is not supported: jed has no tablespaces",
          );
        } else {
          throw engineError("syntax_error", `unrecognized REINDEX option "${name}"`);
        }
        if (this.peek().kind === "rparen") {
          this.advance();
          break;
        }
        if (this.peek().kind !== "comma") {
          throw engineError("syntax_error", "expected ',' or ')' in REINDEX option list");
        }
        this.advance();
      }
    }
    let reindexKind: ReindexKind;
    switch (this.peekKeyword()) {
      case "index":
        reindexKind = "index";
        break;
      case "table":
        reindexKind = "table";
        break;
      case "database":
        reindexKind = "database";
        break;
      case "system":
        reindexKind = "system";
        break;
      case "schema":
        throw engineError(
          "feature_not_supported",
          "REINDEX SCHEMA is not supported: jed has no schemas",
        );
      default:
        throw engineError(
          "syntax_error",
          "expected INDEX, TABLE, SCHEMA, DATABASE, or SYSTEM after REINDEX",
        );
    }
    this.advance();
    const relation = reindexKind === "index" || reindexKind === "table";
    if (this.peekKeyword() === "concurrently" && (!relation || isIdentKind(this.peekKindAt(1)))) {
      this.advance();
      concurrently = true;
    }
    let db: string | undefined;
    let name: string | undefined;
    if (relation) {
      [db, name] = this.parseQualifiedTableName();
    } else if (isIdentKind(this.peek().kind)) {
      name = this.expectIdentifier();
    }
    return { kind: "reindex", reindexKind, db, name, concurrently, verbose };
  }

  // parseReindexBoolean parses a REINDEX option's optional boolean value (PostgreSQL's
  // defGetBoolean): omitted means TRUE; TRUE/FALSE/ON/OFF (bare or quoted) and the integers 1/0 are
  // accepted; anything else is 42601.
  private parseReindexBoolean(option: string): boolean {
    const t = this.peek();
    if (t.kind === "comma" || t.kind === "rparen") return true;
    let value: boolean | null = null;
    if (t.kind === "word" || t.kind === "str") {
      const w = (t.kind === "word" ? t.word! : t.str!).toLowerCase();
      if (w === "true" || w === "on") value = true;
      else if (w === "false" || w === "off") value = false;
    } else if (t.kind === "int") {
      if (t.int === 1n) value = true;
      else if (t.int === 0n) value = false;
    }
    if (value === null) throw engineError("syntax_error", `${option} requires a Boolean value`);
    this.advance();
    return value;
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
    // ROLLBACK [TRANSACTION|WORK] TO [SAVEPOINT] name (transactions.md §11).
    if (this.peekKeyword() === "to") {
      this.advance(); // TO
      return { kind: "savepoint", op: "rollbackTo", name: this.parseSavepointName() };
    }
    return { kind: "rollback" };
  }

  // parseSavepointName parses the [SAVEPOINT] name tail of RELEASE / ROLLBACK TO (grammar.md §27). The
  // noise word SAVEPOINT is consumed only when a name follows it, so `RELEASE savepoint` names a
  // savepoint called savepoint (PostgreSQL's grammar).
  private parseSavepointName(): string {
    const next = this.peekKindAt(1);
    if (this.peekKeyword() === "savepoint" && (next === "word" || next === "quotedIdent")) {
      this.advance(); // SAVEPOINT
    }
    return this.expectIdentifier();
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
    let verbose = false;
    let costs = true;
    let lane = false;
    const seen = new Set<string>();
    let optionList = false;
    // A `(` opens the option list unless a parenthesized query follows (`EXPLAIN (SELECT 1)`,
    // grammar.md §25): an option is a plain word, never `(`, SELECT, `VALUES (`, or a WITH clause.
    const parenQuery =
      this.peekKindAt(1) === "lparen" ||
      this.peekKeywordAt(1) === "select" ||
      this.isValuesAtIndex(this.pos + 1) ||
      this.isWithClauseAtOffset(1);
    if (this.peek().kind === "lparen" && !parenQuery) {
      optionList = true;
      this.advance();
      for (;;) {
        const name = this.peekKeyword();
        if (name !== "analyze" && name !== "verbose" && name !== "costs" && name !== "lane") {
          throw engineError(
            "syntax_error",
            name === "" ? "expected an EXPLAIN option" : `unrecognized EXPLAIN option: ${name}`,
          );
        }
        this.advance();
        if (seen.has(name)) {
          throw engineError("syntax_error", `EXPLAIN option specified more than once: ${name}`);
        }
        seen.add(name);
        let value = true;
        const bool = this.peekKeyword();
        if (bool === "true" || bool === "false" || bool === "on" || bool === "off") {
          this.advance();
          value = bool === "true" || bool === "on";
        }
        if (name === "analyze") analyze = value;
        else if (name === "verbose") verbose = value;
        else if (name === "costs") costs = value;
        else lane = value;
        if (this.peek().kind === "rparen") {
          this.advance();
          break;
        }
        if (this.peek().kind !== "comma") {
          throw engineError("syntax_error", "expected ',' or ')' in EXPLAIN option list");
        }
        this.advance();
      }
    }
    if (this.peekKeyword() === "analyze") {
      if (optionList) {
        throw engineError("syntax_error", "cannot mix EXPLAIN option list with positional ANALYZE");
      }
      this.advance();
      analyze = true;
    }
    return { kind: "explain", analyze, verbose, costs, lane, inner: this.parseExplainInner() };
  }

  // parseExplainInner parses the statement EXPLAIN wraps — restricted to a query (SELECT / VALUES /
  // WITH / a parenthesized query), a DML statement (INSERT / UPDATE / DELETE), or a CREATE TABLE … AS
  // (create-table-as.md §7). Other DDL, transaction
  // control, and a nested EXPLAIN have no query plan to render and are rejected 42601.
  private parseExplainInner(): Statement {
    switch (this.peekKeyword()) {
      case "select":
      case "values":
        return this.parseIntoQuery(() => this.parseQueryExpr());
      case "with":
        return this.parseIntoQuery(() => this.parseWithStatement());
      case "insert":
        return this.parseInsert();
      case "update":
        return this.parseUpdate();
      case "delete":
        return this.parseDelete();
      // `EXPLAIN CREATE TABLE … AS` (create-table-as.md §7); the column-definition form has no plan to
      // render.
      case "create": {
        if (this.atCreateView()) {
          throw engineError("syntax_error", "EXPLAIN does not support 'create'");
        }
        const ct = this.parseCreateTable();
        if (ct.kind === "createTable" && ct.asQuery === null) {
          throw engineError("syntax_error", "EXPLAIN does not support CREATE TABLE without AS");
        }
        return ct;
      }
      case "":
        if (this.peek().kind === "lparen") return this.parseIntoQuery(() => this.parseQueryExpr());
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
    else if (this.peekKeyword() === "unlogged") throw unloggedNotSupported();
    this.expectKeyword("table");
    // `IF NOT EXISTS` (grammar.md §13) is recognized by a two-token lookahead, so a lone `if` stays an
    // ordinary non-reserved table name: `CREATE TABLE if (...)` creates a table named `if` (the DROP
    // TABLE IF EXISTS precedent).
    const ifNotExists =
      this.peekKeyword() === "if" && this.peekKeywordAt(1) === "not" && this.parseIfNotExists();
    // An optional database qualifier `db.table` (attached-databases.md §3, Slice 1b): create the table
    // INTO the named database (`main` / `temp` / a host attachment). A bare name uses the implicit
    // scope. The `.` after the first identifier makes it the qualifier and the next the table name.
    const [db, name] = this.parseQualifiedTableName();
    if (this.atCreateTableAs()) {
      return this.parseCreateTableAs(name, db, temp, ifNotExists);
    }
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
    const onCommit = this.parseOnCommit();
    return {
      kind: "createTable",
      name,
      db,
      temp,
      ifNotExists,
      columns,
      tablePks,
      checks,
      uniques,
      fks,
      excludes,
      ...(onCommit === undefined ? {} : { onCommit }),
      asQuery: null,
    };
  }

  // atCreateTableAs reports whether the cursor, just past a CREATE TABLE's name, begins the `CREATE
  // TABLE … AS` form (spec/design/create-table-as.md §1): the keyword AS, the keyword ON (an ON COMMIT
  // clause precedes AS; the column-definition form needs `(` here), or a parenthesized list of bare
  // column names followed by AS or ON. A column definition always has a type after its name, so
  // `( name , … )` of single identifiers is never the column-definition form.
  private atCreateTableAs(): boolean {
    const kw = (i: number): boolean => {
      const w = this.peekKeywordAt(i);
      return w === "as" || w === "on";
    };
    if (kw(0)) return true;
    if (this.peekKindAt(0) !== "lparen") return false;
    let i = 1;
    for (;;) {
      const k = this.peekKindAt(i);
      if (k !== "word" && k !== "quotedIdent") return false;
      const next = this.peekKindAt(i + 1);
      if (next === "comma") {
        i += 2;
      } else if (next === "rparen") {
        return kw(i + 2);
      } else {
        return false;
      }
    }
  }

  // parseCreateTableAs parses the rest of `CREATE [TEMP] TABLE [IF NOT EXISTS] name [( col [, …] )]
  // [ON COMMIT …] AS query [WITH [NO] DATA]` (spec/design/create-table-as.md §1, grammar.md §13). The
  // query is a `subquery_expr`. A trailing `WITH [NO] DATA` is cut off before the query is parsed, as
  // CREATE VIEW cuts off its check option: an unreserved `with` would otherwise read as a table alias.
  private parseCreateTableAs(
    name: string,
    db: string | undefined,
    temp: boolean,
    ifNotExists: boolean,
  ): Statement {
    let columnNames: string[] | null = null;
    if (this.peek().kind === "lparen") {
      this.advance();
      columnNames = [this.expectIdentifier()];
      while (this.peek().kind === "comma") {
        this.advance();
        columnNames.push(this.expectIdentifier());
      }
      this.expect("rparen");
    }
    const onCommit = this.parseOnCommit();
    this.expectKeyword("as");
    const trailing = this.trailingWithData();
    if (trailing !== null) this.tokens[trailing.at] = { kind: "eof" };
    const query = this.parseSubquery();
    if (trailing !== null && this.pos !== trailing.at) {
      throw engineError("syntax_error", "unexpected trailing input");
    }
    return {
      kind: "createTable",
      name,
      db,
      temp,
      ifNotExists,
      columns: [],
      tablePks: [],
      checks: [],
      uniques: [],
      fks: [],
      excludes: [],
      ...(onCommit === undefined ? {} : { onCommit }),
      asQuery: { columnNames, query, withData: trailing === null || trailing.withData },
    };
  }

  // trailingWithData returns the token index of a trailing `WITH DATA` / `WITH NO DATA` ending the
  // statement and whether the table is filled (spec/design/create-table-as.md §1), or null when absent.
  private trailingWithData(): { at: number; withData: boolean } | null {
    let end = this.tokens.length;
    if (this.tokens[end - 1]?.kind === "eof") end--;
    const word = (i: number, w: string): boolean => {
      const t = this.tokens[i];
      return t !== undefined && t.kind === "word" && t.word === w;
    };
    if (end < this.pos + 2 || !word(end - 1, "data")) return null;
    if (word(end - 2, "with")) return { at: end - 2, withData: true };
    if (end >= this.pos + 3 && word(end - 2, "no") && word(end - 3, "with")) {
      return { at: end - 3, withData: false };
    }
    return null;
  }

  // parseOnCommit parses the optional trailing `ON COMMIT { PRESERVE ROWS | DELETE ROWS | DROP }` of a
  // CREATE TABLE (grammar.ebnf `on_commit`, spec/design/temp-tables.md §5.1), undefined when absent.
  // The words are recognized positionally after the closing parenthesis, so none is reserved.
  private parseOnCommit(): OnCommitAction | undefined {
    if (this.peekKeyword() !== "on") return undefined;
    this.advance();
    this.expectKeyword("commit");
    let action: OnCommitAction;
    switch (this.peekKeyword()) {
      case "preserve":
        action = "preserveRows";
        break;
      case "delete":
        action = "deleteRows";
        break;
      case "drop":
        this.advance();
        return "drop";
      default:
        throw engineError(
          "syntax_error",
          "expected PRESERVE ROWS, DELETE ROWS, or DROP after ON COMMIT",
        );
    }
    this.advance();
    this.expectKeyword("rows");
    return action;
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
    return { ...this.parseReferencesClause(), name, columns };
  }

  // parseReferencesClause parses a references_clause from the REFERENCES keyword onward (shared
  // by the column-level and table-level forms — spec/design/grammar.md §43): the referenced
  // table, an optional referenced-column list (null defaults to the parent's primary key), an
  // optional `MATCH FULL | SIMPLE` (`MATCH PARTIAL` is 0A000, as in PostgreSQL), and the
  // `ON DELETE` / `ON UPDATE` actions (each at most once, either order; a repeat is 42601). The
  // returned definition carries no name and no local columns; the caller fills them in.
  private parseReferencesClause(): ForeignKeyDef {
    this.expectKeyword("references");
    const refTable = this.expectIdentifier();
    const refColumns = this.peek().kind === "lparen" ? this.parsePkColumnList() : null;
    let matchFull = false;
    if (this.peekKeyword() === "match") {
      this.advance();
      const kw = this.peekKeyword();
      if (kw === "full") {
        matchFull = true;
      } else if (kw === "partial") {
        throw engineError("feature_not_supported", "MATCH PARTIAL not yet implemented");
      } else if (kw !== "simple") {
        throw engineError("syntax_error", "expected FULL, PARTIAL, or SIMPLE after MATCH");
      }
      this.advance();
    }
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
    return { name: null, columns: [], refTable, refColumns, matchFull, onDelete, onUpdate };
  }

  // parseReferentialAction parses one referential_action (spec/design/grammar.md §43). All five
  // All five PostgreSQL actions are resolved and persisted by CREATE/ALTER TABLE.
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
    const typeLoc = this.here();
    const baseType = this.expectTypeName();
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
        fks.push({ ...this.parseReferencesClause(), name: cname, columns: [name] });
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
        fks.push({ ...this.parseReferencesClause(), columns: [name] });
      } else {
        break;
      }
    }
    return {
      name,
      typeName,
      typeLoc,
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

  // parseCreateIndex parses `CREATE INDEX [IF NOT EXISTS name | name] ON <table> ( col [, col]* )`
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
    // `IF NOT EXISTS` (grammar.md §30) is recognized by a two-token lookahead, so a lone `if` stays an
    // ordinary index name (`CREATE INDEX if ON t (a)`). PostgreSQL requires a name after it, so the
    // unnamed lookahead below does not apply: `IF NOT EXISTS ON t (a)` is 42601.
    const ifNotExists =
      this.peekKeyword() === "if" && this.peekKeywordAt(1) === "not" && this.parseIfNotExists();
    // The unnamed form is `INDEX ON <table> [USING <method>] (` — the word after INDEX is the
    // index name unless it is `ON` followed by a word and then `(` OR `USING` (the three-token
    // lookahead, extended for the optional USING clause — grammar.md §30, gin.md §3).
    const unnamed =
      !ifNotExists &&
      this.peekKeyword() === "on" &&
      isIdentKind(this.peekKindAt(1)) &&
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
    return { kind: "createIndex", name, ifNotExists, table, db, keys, unique, using, predicate };
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
    if (isIdentKind(this.peek().kind) && this.peekKindAt(1) === "lparen") {
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
      const baseType = this.expectTypeName();
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
        default:
          if (!this.parseOneSeqOption(seq)) {
            if (parenthesized) this.expect("rparen");
            return { options: seq, restart };
          }
      }
    }
  }

  // parseOneSeqOption parses one order-free sequence option (`sequence_option` in grammar.ebnf) into
  // `seq`, or returns false when the next token starts none. Shared by the CREATE/ALTER SEQUENCE and
  // identity `( … )` option loops and by `ALTER TABLE … ALTER COLUMN … SET <sequence_option>`
  // (spec/design/alter.md §2.8).
  private parseOneSeqOption(seq: SeqOptions): boolean {
    switch (this.peekKeyword()) {
      // `AS <type>` — the sequence value type (order-free, S5 — sequences.md §14). The raw type
      // name is stored; it is resolved (and a non-integer type rejected 22023) at execution.
      // Inside an IDENTITY column's `( … )` a set dataType is 42601.
      case "as": {
        this.dupCheck(seq.dataType !== null, "AS");
        this.advance();
        seq.dataType = this.expectTypeName();
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
      // `SEQUENCE NAME name` — identity options only; the executor rejects it elsewhere (42601,
      // sequences.md §13.1).
      case "sequence": {
        if (this.peekKeywordAt(1) !== "name") return false;
        this.dupCheck(seq.sequenceName !== null, "SEQUENCE NAME");
        this.advance();
        this.advance();
        seq.sequenceName = this.expectIdentifier();
        break;
      }
      default:
        return false;
    }
    return true;
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

  // Parse ALTER TABLE's authoritative grammar frame (alter.md §1). Slices 1-5 execute the
  // complete planned surface other than identity management.
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
        if (this.atPrimaryKeyTableConstraint()) {
          if (this.peekKeyword() === "constraint") {
            this.advance();
            this.expectIdentifier(); // jed derives the PK handle.
          }
          this.expectKeyword("primary");
          this.expectKeyword("key");
          actions.push({ kind: "addPrimaryKey", columns: this.parsePkColumnList() });
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
        if (constraint === null) throw new Error("constraint lookahead");
        actions.push({ kind: "addConstraint", constraint });
      } else if (this.peekKeyword() === "drop") {
        this.advance();
        if (this.peekKeyword() === "primary") {
          this.advance();
          this.expectKeyword("key");
          let cascade = false;
          if (this.peekKeyword() === "cascade") {
            this.advance();
            cascade = true;
          } else if (this.peekKeyword() === "restrict") this.advance();
          actions.push({ kind: "dropPrimaryKey", cascade });
          if (this.peek().kind !== "comma") break;
          this.advance();
          continue;
        }
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
        const setTarget = this.peekKeywordAt(1);
        if (
          this.peekKeyword() === "restart" ||
          (this.peekKeyword() === "set" &&
            setTarget !== "default" &&
            setTarget !== "data" &&
            setTarget !== "not")
        ) {
          // The identity option list (alter.md §2.8): `SET GENERATED …`, `SET <sequence_option>`,
          // `RESTART …`, in any order.
          edit = { column, action: this.parseAlterIdentityOptions() };
        } else if (this.peekKeyword() === "add") {
          this.advance();
          this.expectKeyword("generated");
          const always = this.parseGeneratedWhen();
          this.expectKeyword("as");
          this.expectKeyword("identity");
          const options =
            this.peek().kind === "lparen" ? this.parseSequenceOptions(true) : emptySeqOptions();
          edit = { column, action: { kind: "addIdentity", identity: { always, options } } };
        } else if (this.peekKeyword() === "set") {
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
          } else if (this.peekKeyword() === "data") {
            this.advance();
            this.expectKeyword("type");
            edit = { column, action: this.parseAlterColumnType() };
          } else {
            this.expectKeyword("not");
            this.expectKeyword("null");
            edit = { column, action: { kind: "setNotNull" } };
          }
        } else if (this.peekKeyword() === "drop") {
          this.advance();
          if (this.peekKeyword() === "default") {
            this.advance();
            edit = { column, action: { kind: "dropDefault" } };
          } else if (this.peekKeyword() === "identity") {
            this.advance();
            let ifExists = false;
            if (this.peekKeyword() === "if") {
              this.advance();
              this.expectKeyword("exists");
              ifExists = true;
            }
            edit = { column, action: { kind: "dropIdentity", ifExists } };
          } else {
            this.expectKeyword("not");
            this.expectKeyword("null");
            edit = { column, action: { kind: "dropNotNull" } };
          }
        } else if (this.peekKeyword() === "type") {
          this.advance();
          edit = { column, action: this.parseAlterColumnType() };
        } else throw engineError("syntax_error", "ALTER COLUMN requires SET or DROP");
        actions.push({ kind: "alterColumn", edit });
      }
      if (this.peek().kind !== "comma") break;
      this.advance();
    }
    return { kind: "alterTable", name, db, ifExists, action: { kind: "actions", actions } };
  }

  // parseGeneratedWhen parses `ALWAYS` (true) or `BY DEFAULT` (false) after `GENERATED`.
  private parseGeneratedWhen(): boolean {
    const kw = this.peekKeyword();
    if (kw === "always") {
      this.advance();
      return true;
    }
    if (kw === "by") {
      this.advance();
      this.expectKeyword("default");
      return false;
    }
    throw engineError(
      "syntax_error",
      `expected ALWAYS or BY DEFAULT after GENERATED, found '${kw}'`,
    );
  }

  // parseAlterIdentityOptions parses the identity alteration list of `ALTER [COLUMN] c`
  // (spec/design/alter.md §2.8, PostgreSQL's `alter_identity_column_option_list`): one or more of
  // `SET GENERATED { ALWAYS | BY DEFAULT }`, `SET <sequence_option>`, and `RESTART [[WITH] n]`,
  // order-free, each at most once (42601). `SET AS`, `SET RESTART`, and `SET SEQUENCE NAME` are
  // 42601 ("not supported here").
  private parseAlterIdentityOptions(): Extract<
    AlterColumnAction["action"],
    { kind: "setIdentity" }
  > {
    let generated: boolean | null = null;
    const options = emptySeqOptions();
    let restart: SeqRestart | null = null;
    for (;;) {
      const kw = this.peekKeyword();
      if (kw === "restart") {
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
        continue;
      }
      const next = this.peekKeywordAt(1);
      if (kw !== "set" || next === "default" || next === "data" || next === "not") break;
      this.advance();
      const opt = this.peekKeyword();
      if (opt === "generated") {
        this.dupCheck(generated !== null, "SET GENERATED");
        this.advance();
        generated = this.parseGeneratedWhen();
      } else if (opt === "as" || opt === "restart") {
        throw engineError("syntax_error", `sequence option "${opt}" not supported here`);
      } else if (opt === "sequence") {
        throw engineError("syntax_error", "invalid sequence option SEQUENCE NAME");
      } else if (!this.parseOneSeqOption(options)) {
        throw engineError("syntax_error", `expected an identity option after SET, found '${opt}'`);
      }
    }
    return { kind: "setIdentity", generated, options, restart };
  }

  private parseAlterColumnType(): Extract<AlterColumnAction["action"], { kind: "setType" }> {
    let typeName = this.expectTypeName();
    const typeMod = this.parseTypeMod();
    if (this.consumeArrayBrackets()) typeName += "[]";
    let using: Expr | null = null;
    if (this.peekKeyword() === "using") {
      this.advance();
      using = this.parseExpr();
    }
    return { kind: "setType", typeName, typeMod, using };
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
    const tableLoc = this.here();
    const [db, table] = this.parseQualifiedTableName();
    // Optional target alias `AS alias` (spec/design/upsert.md §5). PostgreSQL requires the AS here
    // (no implicit alias), which also keeps a column list / source keyword unambiguous.
    let alias: string | undefined;
    if (this.peekKeyword() === "as") {
      this.advance();
      alias = this.expectIdentifier();
    }

    // Optional column list `( col [, col]* )` before VALUES. An empty `()` is rejected (the
    // first expectIdentifier errors 42601 on `)`). A `(` that begins a parenthesized query source
    // (`INSERT INTO t (SELECT …) UNION …` — grammar.md §24) is not a column list.
    let columns: string[] | null = null;
    const columnLocs: SrcLoc[] = [];
    if (this.peek().kind === "lparen" && !this.atInsertParenQuery()) {
      this.advance(); // '('
      const names: string[] = [];
      for (;;) {
        columnLocs.push(this.here() ?? 0);
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

    // The source is DEFAULT VALUES, a query (§24), or a VALUES list. DEFAULT VALUES is the
    // all-columns-omitted form, represented through the existing column-list machinery as one empty
    // row mapped by an empty synthetic list. PostgreSQL does not combine it with a column list or
    // OVERRIDING.
    if (this.peekKeyword() === "default") {
      if (columns !== null || overriding !== null) {
        throw engineError(
          "syntax_error",
          "DEFAULT VALUES cannot be combined with a column list or OVERRIDING",
        );
      }
      this.advance();
      this.expectKeyword("values");
      const onConflict = this.parseOnConflict();
      const returning = this.parseReturning();
      return {
        kind: "insert",
        table,
        tableLoc,
        db,
        ...(alias === undefined ? {} : { alias }),
        columns: [],
        overriding: null,
        source: { kind: "values", rows: [[]] },
        onConflict,
        returning,
      };
    }

    // A plain VALUES list, possibly parenthesized, is the INSERT VALUES source.
    const plainRows = this.parseInsertValuesSource();
    // A query source (§24): a SELECT, a VALUES query, a set operation, a parenthesized query, or a
    // WITH-prefixed query. After the column list a `(` can only begin a query.
    if (
      plainRows === null &&
      (this.isSelectAtIndex(this.pos) ||
        this.isValuesAtIndex(this.pos) ||
        this.atWithClause() ||
        this.peek().kind === "lparen")
    ) {
      const query = this.parseInsertQuery();
      const onConflict = this.parseOnConflict();
      const returning = this.parseReturning();
      return {
        kind: "insert",
        table,
        tableLoc,
        db,
        ...(alias === undefined ? {} : { alias }),
        columns,
        columnLocs,
        overriding,
        source: { kind: "query", query },
        onConflict,
        returning,
      };
    }

    // Neither a VALUES list nor a query: the error names the expected VALUES keyword (a VALUES here
    // was already parsed above, so this always fails).
    if (plainRows === null) {
      this.expectKeyword("values");
      throw engineError("syntax_error", "expected VALUES or a query");
    }
    const rows: InsertValue[][] = plainRows;
    const onConflict = this.parseOnConflict();
    const returning = this.parseReturning();
    return {
      kind: "insert",
      table,
      tableLoc,
      db,
      ...(alias === undefined ? {} : { alias }),
      columns,
      columnLocs,
      overriding,
      source: { kind: "values", rows },
      onConflict,
      returning,
    };
  }

  // parseConflictColumn parses one `ON CONFLICT ( … )` inference element (spec/design/upsert.md §2):
  // a column, an optional `COLLATE "name"`, an optional operator-class word (any word other than
  // ASC/DESC/NULLS — positional, so non-reserved), then PostgreSQL's ASC/DESC and NULLS FIRST|LAST,
  // which are recorded for resolution to reject (42P10).
  private parseConflictColumn(): ConflictColumn {
    const name = this.expectIdentifier();
    let collation: string | null = null;
    if (this.peekKeyword() === "collate") {
      this.advance();
      collation = this.expectCollationName();
    }
    let opclass: string | null = null;
    const t = this.peek();
    if (
      (t.kind === "word" && t.word !== "asc" && t.word !== "desc" && t.word !== "nulls") ||
      t.kind === "quotedIdent"
    ) {
      opclass = this.expectIdentifier();
    }
    let ordering = false;
    if (this.peekKeyword() === "asc" || this.peekKeyword() === "desc") {
      this.advance();
      ordering = true;
    }
    let nulls = false;
    if (this.peekKeyword() === "nulls") {
      this.advance();
      const k = this.peekKeyword();
      if (k !== "first" && k !== "last") {
        throw engineError("syntax_error", "expected FIRST or LAST after NULLS");
      }
      this.advance();
      nulls = true;
    }
    return { name, collation, opclass, ordering, nulls };
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

    // Optional conflict target: a `( col, … )` column list (with an optional `WHERE
    // index_predicate`) or `ON CONSTRAINT name`.
    let target: ConflictTarget | null = null;
    let indexPredicate: Expr | null = null;
    if (this.peek().kind === "lparen") {
      this.advance(); // '('
      const cols: ConflictColumn[] = [];
      for (;;) {
        cols.push(this.parseConflictColumn());
        const k = this.advance().kind;
        if (k === "comma") continue;
        if (k === "rparen") break;
        throw engineError("syntax_error", "expected ',' or ')'");
      }
      indexPredicate = this.parseOptionalWhere();
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
      return { target, indexPredicate, doUpdate: false, assignments: [], filter: null };
    }
    if (action === "update") {
      this.advance();
      this.expectKeyword("set");
      const assignments: Assignment[] = [];
      for (;;) {
        const loc = this.here();
        const column = this.expectIdentifier();
        this.expect("eq");
        const [isDefault, value] = this.parseAssignmentRhs();
        assignments.push({ column, loc, isDefault, value });
        if (this.peek().kind === "comma") {
          this.advance();
          continue;
        }
        break;
      }
      const filter = this.parseOptionalWhere();
      return { target, indexPredicate, doUpdate: true, assignments, filter };
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
      values.push(this.parseInsertSlot());
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

  // parseInsertSlot parses one top-level INSERT value slot (spec/design/grammar.md §16). The
  // literal-shaped forms (DEFAULT, a literal, $N, a literal-field ROW(…) / ARRAY[…]) keep their
  // contextual adaptation to the target column when the slot is exactly that form — the next token
  // closes the slot. Anything else (`1 + 1`, `now()`, `CASE …`, `(SELECT …)`, a constructor with an
  // expression field) re-parses from the slot's start as a general expression. A non-syntax error
  // from the literal form (an out-of-range integer, 22003) is the slot's own error.
  private parseInsertSlot(): InsertValue {
    const pos = this.pos;
    const depth = this.depth;
    try {
      const v = this.parseInsertValue();
      const k = this.peek().kind;
      if (k === "comma" || k === "rparen") return v;
    } catch (e) {
      if (!(e instanceof EngineError) || e.state !== "syntax_error") throw e;
    }
    this.pos = pos;
    this.depth = depth;
    return { kind: "expr", expr: this.parseExpr() };
  }

  // parseInsertValue parses one literal-shaped INSERT value: the DEFAULT keyword (not reserved —
  // §3), a ROW(…) composite constructor (spec/design/composite.md §1), a bind parameter ($N, bound
  // at execute — spec/design/api.md §5), else a literal.
  private parseInsertValue(): InsertValue {
    if (this.peekKeyword() === "default") {
      this.advance();
      return { kind: "default" };
    }
    if (this.peekKeyword() === "row" && this.peekKindAt(1) === "lparen") {
      // ROW(field, field, …) — recurse on each field (a literal, a $N, or a nested ROW).
      // Each constructor level is one nesting level (MAX_EXPR_DEPTH, cost.md §7).
      this.deepen();
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
      this.undeepen();
      return { kind: "row", fields };
    }
    if (this.peekKeyword() === "array" && this.peekKindAt(1) === "lbracket") {
      // ARRAY[elem, …] — recurse on each element (a literal or a $N).
      // Each constructor level is one nesting level (MAX_EXPR_DEPTH, cost.md §7).
      this.deepen();
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
      this.undeepen();
      return { kind: "array", elements };
    }
    if (this.peek().kind === "param") {
      return { kind: "param", index: this.advance().paramIndex! };
    }
    const loc = this.here();
    return { kind: "lit", lit: this.parseLiteral(), loc };
  }

  // parseLiteral parses a literal value for INSERT: an integer (with an optional leading
  // unary minus, folded here), or one of the keywords NULL / TRUE / FALSE
  // (spec/grammar/grammar.ebnf `literal`). A slot that is not exactly a literal is a general
  // expression (parseInsertSlot).
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
      const w = t.word!;
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
    const node = this.parseQueryExprNode();
    if (node.kind === "withExpr") {
      // A parenthesized `(WITH … query)` statement is the top-level WITH query itself (PostgreSQL:
      // the parentheses add nothing — grammar.md §25).
      return { kind: "with", ctes: node.ctes, body: node.body, recursive: node.recursive };
    }
    return node;
  }

  // parseQueryExprNode parses a top-level query_expr as a QueryExpr node — a set expression plus an
  // optional trailing ORDER BY / LIMIT / OFFSET folded onto it. The shared core of parseQueryExpr
  // (which returns it as a Statement) and a WITH clause's main body. Unlike parseSubquery it opens
  // no new nesting level — the body is at the statement top level.
  private parseQueryExprNode(): QueryExpr {
    const node = this.parseSetExpr();
    const orderBy = this.parseOrderBy(true);
    const { limit, offset } = this.parseLimitOffsetClauses();
    return foldTrailing(node, orderBy, limit, offset);
  }

  // parseWithStatement parses `query_statement ::= with_clause? ( query_expr | insert | update |
  // delete )` — a top-level statement prefixed by a WITH clause defining common table expressions
  // (spec/design/cte.md, spec/design/writable-cte.md). WITH RECURSIVE (spec/design/recursive-cte.md)
  // sets the `recursive` flag and lets a CTE reference itself. Parenthesized CTE query bodies are
  // subquery_exprs and may carry a nested WITH (cte.md §7); the top-level primary is unprefixed, so
  // a bare second WITH remains invalid.
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
    // INSERT/UPDATE/DELETE keyword selects it, otherwise an unprefixed query_expr. A data-modifying
    // primary's SELECT is another statement's query, where INTO is not allowed.
    const kw = this.peekKeyword();
    if (kw === "insert" || kw === "update" || kw === "delete") {
      this.intoOk = false;
      this.intoTop = false;
    }
    const body = this.parseCteBody(false);
    return { kind: "with", ctes, body, recursive };
  }

  // parseCteBody parses a `cte_body` (spec/design/writable-cte.md): a data-modifying
  // INSERT/UPDATE/DELETE when one leads, otherwise a query. `parenthesized` is true for a CTE body
  // inside `( … )` (the closing `)` is the caller's), false for the WITH primary (it runs to end of
  // statement). A parenthesized query body uses subquery_expr and may carry a nested WITH; the
  // top-level primary uses an unprefixed query_expr.
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
      const body = this.parseQueryExprNode();
      rejectSecondWith(body);
      return body;
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
    // A CTE body's SELECT never takes the statement's INTO (create-table-as.md §6).
    const intoOk = this.intoOk;
    this.intoOk = false;
    this.intoNest++;
    const body = this.parseCteBody(true);
    this.intoNest--;
    this.intoOk = intoOk;
    this.expect("rparen");
    const search = this.peekKeyword() === "search" ? this.parseSearchClause() : null;
    const cycle = this.peekKeyword() === "cycle" ? this.parseCycleClause() : null;
    return { name, columns, materialized, body, search, cycle };
  }

  // parseSearchClause parses `SEARCH { DEPTH | BREADTH } FIRST BY col, … SET seq_col`
  // (spec/design/recursive-cte.md §9).
  private parseSearchClause(): SearchClause {
    const loc = this.here();
    this.expectKeyword("search");
    const kw = this.peekKeyword();
    if (kw !== "depth" && kw !== "breadth") {
      throw engineError("syntax_error", "expected DEPTH or BREADTH after SEARCH");
    }
    this.advance();
    this.expectKeyword("first");
    this.expectKeyword("by");
    const columns = this.parseIdentifierList();
    this.expectKeyword("set");
    const setColumn = this.expectIdentifier();
    return { breadthFirst: kw === "breadth", columns, setColumn, loc };
  }

  // parseCycleClause parses `CYCLE col, … SET mark_col [TO value DEFAULT default] USING path_col`
  // (spec/design/recursive-cte.md §9).
  private parseCycleClause(): CycleClause {
    const loc = this.here();
    this.expectKeyword("cycle");
    const columns = this.parseIdentifierList();
    this.expectKeyword("set");
    const markColumn = this.expectIdentifier();
    let markValue: Expr | null = null;
    let markDefault: Expr | null = null;
    if (this.peekKeyword() === "to") {
      this.advance();
      markValue = this.parseCycleMarkConstant();
      this.expectKeyword("default");
      markDefault = this.parseCycleMarkConstant();
    }
    this.expectKeyword("using");
    const pathColumn = this.expectIdentifier();
    return { columns, markColumn, markValue, markDefault, pathColumn, loc };
  }

  // parseCycleMarkConstant parses a cycle mark constant (PostgreSQL's AexprConst): a number, string,
  // TRUE / FALSE / NULL, or a typed string literal (`date '…'`). A sign is not part of a constant, so
  // -1 is a syntax error, as in PostgreSQL.
  private parseCycleMarkConstant(): Expr {
    const k = this.peek().kind;
    if (k === "minus" || k === "lparen") {
      throw engineError("syntax_error", "expected a constant in a CYCLE clause");
    }
    const e = this.parsePrimary();
    if (e.kind !== "literal" && e.kind !== "typedLiteral") {
      throw engineError("syntax_error", "expected a constant in a CYCLE clause");
    }
    return e;
  }

  // parseIdentifierList parses a comma-separated list of one or more identifiers.
  private parseIdentifierList(): string[] {
    const names = [this.expectIdentifier()];
    while (this.peek().kind === "comma") {
      this.advance();
      names.push(this.expectIdentifier());
    }
    return names;
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
    return foldTrailing(node, orderBy, limit, offset);
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
    rejectSecondWith(body);
    return { kind: "withExpr", ctes, recursive, body };
  }

  // isWithClauseAtOffset reports whether a WITH clause (`WITH RECURSIVE …`, `WITH <name> ( …`, or
  // `WITH <name> AS …`) begins at this.pos + offset (spec/design/cte.md §7), as opposed to an
  // ordinary expression or a column named `with`. The shape-based lookahead keeps the recognition
  // unambiguous even where `with` is a legal identifier (e.g. `x IN (with)` is a value list).
  private isWithClauseAtOffset(offset: number): boolean {
    if (this.peekKeywordAt(offset) !== "with") return false;
    if (this.peekKeywordAt(offset + 1) === "recursive") return true;
    if (isIdentKind(this.peekKindAt(offset + 1))) {
      return this.peekKindAt(offset + 2) === "lparen" || this.peekKeywordAt(offset + 2) === "as";
    }
    return false;
  }

  // isQueryStartAtOffset reports whether a query expression — a SELECT, a VALUES list, a nested WITH
  // clause (cte.md §7), or a parenthesized query (grammar.md §25) — begins at this.pos + offset. The
  // §26 leading-SELECT lookahead, extended with `VALUES (` and WITH, and with a run of `(`s that ends
  // at a SELECT / `VALUES (` / WITH where EVERY `(` in the run is closed by a `)` followed by a token
  // that can only continue a query — UNION / INTERSECT / EXCEPT / ORDER / LIMIT / OFFSET or a further
  // `)`. Thus
  // `((SELECT 1) UNION SELECT 2)` and `((SELECT 1))` are subqueries but `((SELECT 1) + 1)` and
  // `((SELECT 1), 2)` are expressions. Precomputed (computeParenQuery), so a long run of `(` can
  // neither exhaust the stack nor cost time per probe; a §8 cross-core surface.
  private isQueryStartAtOffset(offset: number): boolean {
    const i = this.pos + offset;
    return (
      this.isSelectAtIndex(i) ||
      this.isValuesAtIndex(i) ||
      this.isWithClauseAtIndex(i) ||
      this.parenQuery[i] === true
    );
  }

  // isSelectAtIndex reports whether a SELECT keyword sits at token index i.
  private isSelectAtIndex(i: number): boolean {
    const t = this.tokens[i];
    return t !== undefined && t.kind === "word" && t.word === "select";
  }

  // isValuesAtIndex reports whether a VALUES list (`VALUES (`) begins at token index i (grammar.md
  // §25) — the VALUES keyword followed by a row's `(`, so a column named `values` stays an expression.
  private isValuesAtIndex(i: number): boolean {
    const t = this.tokens[i];
    const next = this.tokens[i + 1];
    return (
      t !== undefined &&
      t.kind === "word" &&
      t.word === "values" &&
      next !== undefined &&
      next.kind === "lparen"
    );
  }

  // isWithClauseAtIndex is isWithClauseAtOffset at an absolute token index.
  private isWithClauseAtIndex(i: number): boolean {
    return this.isWithClauseAtOffset(i - this.pos);
  }

  // isQueryFollowAtIndex reports whether the token at index i may follow a parenthesized query but
  // never a parenthesized expression: a set operator, a trailing ORDER/LIMIT/OFFSET, or a `)`.
  private isQueryFollowAtIndex(i: number): boolean {
    const t = this.tokens[i];
    if (t === undefined) return false;
    if (t.kind === "rparen") return true;
    if (t.kind !== "word") return false;
    switch (t.word) {
      case "union":
      case "intersect":
      case "except":
      case "order":
      case "limit":
      case "offset":
        return true;
    }
    return false;
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
  private parseSetExpr(): QueryExpr {
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
  private parseIntersectExpr(): QueryExpr {
    const base = this.depth;
    let left: QueryExpr = this.parseSetOperand();
    while (this.peekKeyword() === "intersect") {
      this.deepen(); // each chained INTERSECT is one more set-op nesting level
      this.advance(); // INTERSECT
      const all = this.parseSetOpQuantifier();
      const right = this.parseSetOperand();
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

  // parseSetOperand parses a set-operation operand (grammar.md §25): a select_core, a VALUES list, or
  // a parenthesized query — a full subquery_expr (its own WITH, set operations, and trailing ORDER
  // BY/LIMIT/OFFSET) between `(` and `)`, one nesting level deeper. In operand position a `(` can only
  // begin a query, so no lookahead is needed. A VALUES operand is the query `SELECT * FROM (VALUES …)`
  // over a *VALUES* relation (valuesQuery), so its columns are column1, column2, … and a trailing ORDER
  // BY folds onto it like a SELECT's.
  private parseSetOperand(): QueryExpr {
    if (this.peek().kind === "lparen") {
      this.advance();
      const q = this.parseSubquery();
      this.expect("rparen");
      return q;
    }
    if (this.peekKeyword() === "values") {
      // A VALUES operand is no SELECT, so a later arm cannot take the statement's INTO.
      this.intoOk = false;
      return valuesQuery(this.parseValuesBody(), [], null, null);
    }
    return this.parseSelectCore();
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

  // parseInsertValuesSource parses the plain VALUES source of an INSERT (grammar.md §12/§24): `VALUES
  // row [, row]*`, optionally wrapped in any number of parentheses (`INSERT INTO t (VALUES …)` is the
  // same source — PostgreSQL), whose slots take DEFAULT and adapt to their target columns. It is that
  // source only when the list stands alone: a set operator or a trailing ORDER BY / LIMIT / OFFSET
  // after it (inside or outside the parentheses) makes the whole source a query whose operand is a
  // VALUES query, so this rewinds and returns null for parseInsertQuery. null too when no VALUES list
  // begins here. Each parenthesis is one nesting level, as for a query.
  private parseInsertValuesSource(): InsertValue[][] | null {
    const start = this.pos;
    const depth = this.depth;
    const furthest = this.furthest;
    let parens = 0;
    while (this.peekKindAt(parens) === "lparen") parens++;
    if (this.peekKeywordAt(parens) !== "values") return null;
    for (let i = 0; i < parens; i++) {
      this.advance(); // '('
      this.deepen();
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
    // A set operator or ORDER/LIMIT/OFFSET (a query-follow token other than `)`) after the list or
    // after one of its closing parentheses makes it a query operand.
    const continuesQuery = (): boolean =>
      this.isQueryFollowAtIndex(this.pos) && this.peek().kind !== "rparen";
    let plain = true;
    for (let i = 0; i < parens; i++) {
      if (continuesQuery()) {
        plain = false;
        break;
      }
      this.expect("rparen");
    }
    if (plain && !continuesQuery()) {
      this.depth = depth;
      return rows;
    }
    // Rewind, forgetting what the abandoned parse examined so a later error is positioned by the
    // re-parse (error-fields.md §10.3).
    this.pos = start;
    this.depth = depth;
    this.furthest = furthest;
    return null;
  }

  // atInsertParenQuery reports whether the `(` at the current position begins a parenthesized query
  // source rather than an INSERT column list (grammar.md §24). A column list holds only identifiers,
  // so the `(` begins a query when it is followed by another `(`, by a WITH clause, or by a bare
  // SELECT that is not itself a listed column name (`(select)` / `(select, b)` stay column lists —
  // keywords are not reserved, §3). A §8 cross-core surface.
  private atInsertParenQuery(): boolean {
    if (this.peekKindAt(1) === "lparen") return true;
    if (this.isWithClauseAtOffset(1) || this.isValuesAtIndex(this.pos + 1)) return true;
    if (this.isSelectAtIndex(this.pos + 1)) {
      const next = this.peekKindAt(2);
      return next !== "comma" && next !== "rparen";
    }
    return false;
  }

  // parseInsertQuery parses an INSERT's query source (grammar.md §24): a subquery_expr at the
  // statement top level — a WITH-prefixed query, or a set expression (whose operands may be
  // parenthesized queries) with an optional trailing ORDER BY / LIMIT / OFFSET folded onto it. A lone
  // SELECT folds back to a Select node, exactly the pre-set-operations source.
  private parseInsertQuery(): QueryExpr {
    return this.atWithClause() ? this.parseWithQueryExpr() : this.parseQueryExprNode();
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
      const modifier = next.kind !== "eof" && !(next.kind === "word" && next.word! === "from");
      if (modifier) {
        this.advance();
        distinct = true;
      }
    }

    // Only the statement's leftmost SELECT may take an INTO (create-table-as.md §6), so this one owns
    // it and every SELECT nested in its clauses does not.
    const owner = this.intoOk;
    this.intoOk = false;
    this.intoNest++;
    const items = this.parseSelectItems();
    if (this.peekKeyword() === "into") this.parseSelectInto(owner);
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
    this.intoNest--;

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
    // A quoted name is never a keyword (grammar.md §3).
    if (t.kind === "quotedIdent") {
      this.advance();
      return t.str!;
    }
    if (t.kind !== "word") return null;
    switch (t.word!) {
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
    // A leading unaliased parenthesized join is the same left-deep chain without its parentheses.
    const { from, joins } = spliceLeadingGroup(this.parseTableRef(), []);
    // Where the current comma item's comma join sits in joins (-1 for the first item).
    let segment = -1;
    for (;;) {
      for (;;) {
        const j = this.parseJoinClause();
        if (j === null) break;
        joins.push(j);
      }
      if (segment >= 0) groupOuterJoinItem(joins, segment);
      // Comma-FROM (grammar.md §15): `FROM a, b` is an implicit CROSS JOIN. The comma separates
      // top-level FROM items, each its own join sub-chain; it binds LOOSER than JOIN, so the new
      // item begins a fresh ON-resolution segment (recorded by comma: true). The inner loop then
      // picks up any joins of the new item (`a, b JOIN c ON …`) before the next comma.
      if (this.peek().kind === "comma") {
        this.advance();
        // An unaliased parenthesized join after a comma starts the new segment the same way.
        const { from: table, joins: groupJoins } = spliceLeadingGroup(this.parseTableRef(), []);
        segment = joins.length;
        joins.push({ kind: "cross", table, on: null, comma: true });
        joins.push(...groupJoins);
        continue;
      }
      break;
    }
    return { from, joins };
  }

  // parseJoinGroup parses the inside of a parenthesized join `"(" table_ref join_clause+ ")"`
  // (spec/design/grammar.md §15) after its `(`, stopping before the `)`. The group may not contain a
  // comma, and must contain at least one join (`FROM (a)` is 42601, as in PostgreSQL). Its body is a
  // `SELECT *` over the chain; the caller marks it joinGroup when no alias follows.
  private parseJoinGroup(): Select {
    this.deepen();
    const { from, joins } = spliceLeadingGroup(this.parseTableRef(), []);
    for (;;) {
      const j = this.parseJoinClause();
      if (j === null) break;
      joins.push(j);
    }
    // `((a JOIN b ON p))` is a join (the inner group's chain); `(a)` is not.
    if (joins.length === 0) {
      throw engineError(
        "syntax_error",
        "a parenthesized FROM item must be a subquery, a VALUES list, or a join",
      );
    }
    this.undeepen();
    return joinGroupBody(from, joins);
  }

  // parseTableRef parses `table_ref ::= derived_table derived_alias? | joined_group derived_alias? |
  // table_function table_function_alias? | qualified_table ("AS"? identifier)?` (grammar.md
  // §15/§35/§42). A `(` at the START of a table_ref, when SELECT/WITH/VALUES follows, begins a
  // DERIVED TABLE — a parenthesized subquery used as a relation (§42); any other leading `(` begins a
  // PARENTHESIZED JOIN `(a JOIN b ON …)` (§15). Otherwise it is a base table name OR a set-returning
  // function call, a `(` immediately after the leading identifier marking the function form; the
  // resolver owns arity/type errors. The alias logic is shared. The stop-keyword set is a §8
  // cross-core surface.
  private parseTableRef(): TableRef {
    // An optional leading LATERAL (grammar.md §44) marks a derived table / table function as
    // correlated to the EARLIER FROM relations. LATERAL is non-reserved (§3), so it is the keyword
    // only when a derived table `(` or a function call `name(` follows (a two-token lookahead) —
    // otherwise it is an ordinary identifier (e.g. a table named `lateral`). A table function is
    // implicitly lateral regardless, so the keyword is redundant (but accepted) there.
    const lateral =
      this.peekKeyword() === "lateral" &&
      (this.peekKindAt(1) === "lparen" ||
        (isIdentKind(this.peekKindAt(1)) && this.peekKindAt(2) === "lparen"));
    if (lateral) {
      this.advance();
    }
    if (this.peek().kind === "lparen") {
      const tr = this.parseDerivedTable(lateral);
      tr.lateral = lateral;
      return tr;
    }
    // `JSON_TABLE(ctx, path [AS n] COLUMNS (…))` — a table source (json-table.md §3, T1), recognized
    // by the keyword followed by `(`.
    if (this.peekKeyword() === "json_table" && this.peekKindAt(1) === "lparen") {
      return this.parseJsonTable();
    }
    return this.parseNamedTableRef(lateral);
  }

  // parseNamedTableRef parses a FROM item that starts with a name: a base table or view, optionally
  // database-qualified, or a table function call (parseTableRef's last arm).
  private parseNamedTableRef(lateral: boolean): TableRef {
    let namePos = this.pos;
    const loc = this.here();
    let name = this.expectIdentifier();
    // An optional DATABASE qualifier `db "." table` (spec/design/attached-databases.md §3): a `.`
    // after the first identifier makes it the database qualifier and the next identifier the table
    // name. A qualified name is a BASE TABLE only — never a set-returning function (no cross-database
    // SRF) — so the function `(` branch below is guarded off when a qualifier is present.
    let db: string | undefined;
    if (this.peek().kind === "dot") {
      this.advance(); // .
      db = name;
      namePos = this.pos;
      name = this.expectIdentifier();
    }
    // A `(` right after the name = a set-returning function call (no `*`/`DISTINCT`). An argument
    // may be named (`name => value`, grammar.md §17); `argNames` stays undefined when every argument
    // is positional.
    let args: Expr[] | null = null;
    let argNames: (string | null)[] | undefined;
    if (this.peek().kind === "lparen") {
      if (db !== undefined) {
        throw engineError("syntax_error", "a database-qualified name cannot be a function call");
      }
      this.advance();
      args = [];
      const names: (string | null)[] = [];
      let anyNamed = false;
      for (;;) {
        let argName: string | null = null;
        if (isIdentKind(this.peek().kind) && this.peekKindAt(1) === "fatArrow") {
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
      this.expect("rparen");
      if (anyNamed) argNames = names;
    }
    let alias: string | null = null;
    let aliasPos: number | null = null;
    // `AS (c type, …)` — a table function's column-definition list without a relation alias
    // (PostgreSQL's func_alias_clause); the relation keeps the function's name.
    let bareDefs: TypeFieldDef[] | undefined;
    if (this.peekKeyword() === "as" && args !== null && this.peekKindAt(1) === "lparen") {
      this.advance(); // AS
      this.advance(); // (
      bareDefs = this.parseFieldDefList();
    } else if (this.peekKeyword() === "as") {
      this.advance();
      aliasPos = this.pos;
      alias = this.expectIdentifier();
    } else {
      const t = this.peek();
      if (t.kind === "quotedIdent") {
        alias = t.str!;
        aliasPos = this.pos;
        this.advance();
      } else if (t.kind === "word" && !isTableRefStopKeyword(t.word!)) {
        alias = t.word!;
        aliasPos = this.pos;
        this.advance();
      }
    }
    // A `(` after an alias is a column alias list. On a base table (or view) it is the rename-only
    // list `AS x(p, q)` (grammar.md §15). On a table function it is either the rename-only list
    // `AS g(c1, …)` (§35) or the typed column-definition list `AS g(c type, …)` used by
    // record-returning functions (C0, json-table.md §1).
    let columnAliases: string[] | undefined;
    let columnDefs: TypeFieldDef[] | undefined = bareDefs;
    let listPos: [number, number] | null = null;
    if (alias !== null && this.peek().kind === "lparen" && args === null) {
      const open = this.pos;
      this.advance(); // (
      columnAliases = [this.expectIdentifier()];
      while (this.peek().kind === "comma") {
        this.advance();
        columnAliases.push(this.expectIdentifier());
      }
      listPos = [open, this.pos];
      this.expect("rparen");
    } else if (alias !== null && this.peek().kind === "lparen") {
      this.advance(); // (
      // Disambiguate: a col-def list has `name type`; a rename list has `name ,`/`name )`. After the
      // opening `(`, the current token is the first column name, so a `word` in the NEXT slot means a
      // type follows (col-def list).
      if (isIdentKind(this.peekKindAt(1))) {
        columnDefs = this.parseFieldDefList();
      } else {
        columnAliases = [this.expectIdentifier()];
        while (this.peek().kind === "comma") {
          this.advance();
          columnAliases.push(this.expectIdentifier());
        }
        this.expect("rparen");
      }
    }
    // An SRF is implicitly lateral; `lateral` records only whether the keyword was written.
    const pos: TableRefPos = { name: namePos, alias: aliasPos, list: listPos, depth: this.depth };
    const tref: TableRef = { name, db, alias, args, columnAliases, columnDefs, lateral, pos, loc };
    if (argNames !== undefined) tref.argNames = argNames;
    return tref;
  }

  // parseDerivedTable parses a DERIVED TABLE — `"(" query_expr ")" derived_alias?` (grammar.md §42)
  // — or a PARENTHESIZED JOIN `"(" table_ref join_clause+ ")" derived_alias?` (§15). The caller has
  // verified the next token is `(`. A derived table is recognized when SELECT, WITH, or VALUES
  // follows the `(` (the §26 leading-SELECT lookahead, a §8 cross-core surface); any other leading
  // `(` is a parenthesized join, which may not follow LATERAL (42601). The alias is OPTIONAL
  // (PostgreSQL 18 relaxed the old mandatory-alias rule): present, it is the label and may carry a
  // column-rename list; absent, the relation has no qualifier (its bare columns still resolve). An
  // aliased join is the derived table `(SELECT * FROM <join>) AS alias`; an unaliased one keeps its
  // inner labels visible (joinGroup). name/alias carry the alias ("" / null when none).
  private parseDerivedTable(lateral: boolean): TableRef {
    // Consume the opening `(`. The body is a query_expr (a leading SELECT/WITH), a VALUES list
    // (a leading VALUES) — FROM (VALUES (e…),(e…)), a computed relation of literal rows
    // (grammar.md §42) — or a parenthesized join.
    this.advance();
    if (this.peekKeyword() === "values") {
      const v = this.parseDerivedValues();
      if (v !== null) return this.finishDerivedTable(v.body, v.values, undefined);
    }
    let body: QueryExpr;
    let group: Select | undefined;
    if (this.atSubqueryStart()) {
      // A leading SELECT, or a nested WITH (cte.md §7), is a query_expr body.
      body = this.parseSubquery();
    } else if (lateral) {
      throw engineError(
        "syntax_error",
        "LATERAL must be followed by a subquery or a function call",
      );
    } else {
      group = this.parseJoinGroup();
      body = group;
    }
    return this.finishDerivedTable(body, undefined, group);
  }

  // parseDerivedValues parses a derived table's VALUES body (parseDerivedTable's VALUES arm). A
  // trailing ORDER BY / LIMIT / OFFSET makes the body the query `SELECT * FROM (VALUES …) ORDER BY …
  // LIMIT …` (valuesQuery). A set operator after the VALUES list makes the body a query with a VALUES
  // operand (grammar.md §25): the cursor is rewound and null returned, to parse it as one.
  private parseDerivedValues(): { body?: QueryExpr; values?: Expr[][] } | null {
    const start = this.pos;
    const depth = this.depth;
    const furthest = this.furthest;
    const rows = this.parseValuesBody();
    const kw = this.peekKeyword();
    if (kw === "union" || kw === "intersect" || kw === "except") {
      this.pos = start;
      this.depth = depth;
      this.furthest = furthest;
      return null;
    }
    const orderBy = this.parseOrderBy(true);
    const { limit, offset } = this.parseLimitOffsetClauses();
    if (orderBy.length === 0 && limit === null && offset === null) return { values: rows };
    return { body: valuesQuery(rows, orderBy, limit, offset) };
  }

  // finishDerivedTable parses a derived table's closing `)` and optional alias, and builds it.
  private finishDerivedTable(
    body: QueryExpr | undefined,
    values: Expr[][] | undefined,
    group: Select | undefined,
  ): TableRef {
    this.expect("rparen");
    // The alias is optional, parsed exactly like a base table's.
    let alias: string | null = null;
    if (this.peekKeyword() === "as") {
      this.advance();
      alias = this.expectIdentifier();
    } else {
      const t = this.peek();
      if (t.kind === "quotedIdent") {
        alias = t.str!;
        this.advance();
      } else if (t.kind === "word" && !isTableRefStopKeyword(t.word!)) {
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
    // An unaliased join keeps its inner relation labels visible to the enclosing query.
    if (group !== undefined && alias === null) {
      group.joinGroup = true;
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
  // row, each with at least one value. A trailing ORDER BY / LIMIT / OFFSET is the caller's
  // (parseDerivedTable).
  private parseValuesBody(): Expr[][] {
    this.expectKeyword("values");
    const rows: Expr[][] = [];
    for (;;) {
      this.expect("lparen");
      const row: Expr[] = [this.parseValuesValue()];
      while (this.peek().kind === "comma") {
        this.advance();
        row.push(this.parseValuesValue());
      }
      this.expect("rparen");
      rows.push(row);
      if (this.peek().kind !== "comma") break;
      this.advance();
    }
    return rows;
  }

  // parseValuesValue parses one value of a VALUES-body row: a general expression. A value that is
  // exactly the bare word DEFAULT is the keyword, as in an INSERT slot, and is allowed only in the
  // plain INSERT … VALUES source: here it is 42601 (PostgreSQL: "DEFAULT is not allowed in this
  // context").
  private parseValuesValue(): Expr {
    const next = this.peekKindAt(1);
    if (this.peekKeyword() === "default" && (next === "comma" || next === "rparen")) {
      throw engineError("syntax_error", "DEFAULT is not allowed in this context");
    }
    return this.parseExpr();
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
    const passing = this.parseJsonPassing();
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
      if (t.kind === "quotedIdent") {
        alias = t.str!;
        this.advance();
      } else if (t.kind === "word" && !isTableRefStopKeyword(t.word!)) {
        alias = t.word!;
        this.advance();
      }
    }
    const jsonTable: JsonTable = { ctx, path, passing, columns };
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
      // Each NESTED level is one more nesting level of the statement (cost.md §7, json.md §6.4): a
      // deep NESTED chain recurses in the parser and the planner.
      this.deepen();
      const columns = this.parseJtColumns();
      this.undeepen();
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
    const typeName = this.expectTypeName();
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

  // Whether the cursor has the exact `identifier "." identifier` shape. Expression
  // disambiguation uses it so a keyword-spelled relation label wins over the keyword's literal,
  // unary, or keyword-led primary meaning (spec/design/grammar.md §3).
  private atQualifiedColumnRef(): boolean {
    return (
      isIdentKind(this.peekKindAt(0)) &&
      this.peekKindAt(1) === "dot" &&
      isIdentKind(this.peekKindAt(2))
    );
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

  // parseAssignmentRhs parses the RHS of one `SET col = …` assignment — shared by standalone UPDATE
  // and ON CONFLICT DO UPDATE. It returns [true, placeholder] for the DEFAULT form, which is special
  // only when it is the complete RHS: keywords are legal identifiers, so `SET x = default + 1` parses
  // `default` as a column reference (grammar.md §16). The placeholder is never resolved.
  private parseAssignmentRhs(): [boolean, Expr] {
    const nextKeyword = this.peekKeywordAt(1);
    const nextKind = this.peekKindAt(1);
    if (
      this.peekKeyword() === "default" &&
      (nextKind === "comma" ||
        nextKind === "rparen" ||
        nextKind === "eof" ||
        nextKeyword === "where" ||
        nextKeyword === "returning")
    ) {
      this.advance();
      return [true, { kind: "literal", literal: { kind: "null" } }];
    }
    return [false, this.parseExpr()];
  }

  // parseUpdate parses
  // `UPDATE <table> SET <col> = <operand> [, <col> = <operand>]* [WHERE <pred>]`.
  private parseUpdate(): Update {
    this.expectKeyword("update");
    const tableLoc = this.here();
    const [db, table] = this.parseQualifiedTableName();
    this.expectKeyword("set");

    const assignments: Assignment[] = [];
    for (;;) {
      const loc = this.here();
      const column = this.expectIdentifier();
      this.expect("eq");
      const [isDefault, value] = this.parseAssignmentRhs();
      assignments.push({ column, loc, isDefault, value });
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
    return { kind: "update", table, tableLoc, db, assignments, filter, returning };
  }

  // parseDelete parses `DELETE FROM <table> [WHERE <pred>]`. No WHERE deletes all rows.
  private parseDelete(): Delete {
    this.expectKeyword("delete");
    this.expectKeyword("from");
    const tableLoc = this.here();
    const [db, table] = this.parseQualifiedTableName();
    const filter = this.parseOptionalWhere();
    const returning = this.parseReturning();
    return { kind: "delete", table, tableLoc, db, filter, returning };
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
  // set, so an `INSERT ... SELECT` source never swallows it — §15). An optional
  // `WITH (OLD AS o, NEW AS n)` prefix aliases either row-version pseudo-relation; AS is mandatory
  // and each version may occur once. The item list is the ordinary select-items production (`*` or
  // expressions with optional AS labels); an empty list fails in parseExpr (42601).
  private parseReturning(): ReturningClause | null {
    if (this.peekKeyword() !== "returning") return null;
    this.advance(); // RETURNING
    let oldAlias: string | null = null;
    let newAlias: string | null = null;
    if (
      this.peekKeyword() === "with" &&
      this.peekKindAt(1) === "lparen" &&
      (this.peekKeywordAt(2) === "old" || this.peekKeywordAt(2) === "new") &&
      this.peekKeywordAt(3) === "as"
    ) {
      this.advance(); // WITH
      this.expect("lparen");
      for (;;) {
        const version = this.peekKeyword();
        if (version !== "old" && version !== "new") {
          throw engineError("syntax_error", "expected OLD or NEW in RETURNING WITH");
        }
        this.advance();
        this.expectKeyword("as");
        const alias = this.expectIdentifier();
        if (version === "old") {
          if (oldAlias !== null) {
            throw engineError("syntax_error", "OLD cannot be specified multiple times");
          }
          oldAlias = alias;
        } else {
          if (newAlias !== null) {
            throw engineError("syntax_error", "NEW cannot be specified multiple times");
          }
          newAlias = alias;
        }
        if (this.peek().kind === "comma") {
          this.advance();
          continue;
        }
        this.expect("rparen");
        break;
      }
    }
    return { oldAlias, newAlias, items: this.parseSelectItems() };
  }

  private parseSelectItems(): SelectItems {
    if (this.peek().kind === "star") {
      const pos = this.pos;
      this.advance();
      return { kind: "all", pos };
    }
    const items: SelectItem[] = [];
    for (;;) {
      // `t.*` — a qualified star (all columns of the relation labeled `t`), a select-list /
      // RETURNING item MIXABLE with other items (grammar.md §15). Recognized by the three-token
      // shape `identifier "." "*"` before the general expr parser, so `t.col` (Dot then a word)
      // and `a * b` (no Dot) are untouched, and a bare `*` was already handled above. No `AS` alias.
      if (
        isIdentKind(this.peek().kind) &&
        this.peekKindAt(1) === "dot" &&
        this.peekKindAt(2) === "star"
      ) {
        const pos = this.pos;
        const qualifier = this.expectIdentifier();
        this.advance(); // .
        this.advance(); // *
        items.push({ expr: { kind: "qualifiedStar", qualifier, pos }, alias: null });
        if (this.peek().kind === "comma") {
          this.advance();
          continue;
        }
        break;
      }
      const expr = this.parseExpr();
      // Optional output label, `AS alias` or a bare label (spec/grammar/grammar.ebnf
      // `select_item`). `AS` is not reserved, so it is taken as an alias marker only here,
      // after a complete expr. Without `AS`, a quoted identifier, or a bare word outside the
      // no-bare-label set, is the label (grammar.md §8). The alias never enters resolution.
      let alias: string | null = null;
      const t = this.peek();
      if (t.kind === "word" && t.word === "as") {
        this.advance();
        alias = this.expectIdentifier();
      } else if (
        t.kind === "quotedIdent" ||
        (t.kind === "word" && !BARE_LABEL_STOP_KEYWORDS.has(t.word!))
      ) {
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
  //
  // Each level runs on every nesting level of an expression, so each keeps a small frame
  // (spec/design/cost.md §7.1): a level parses its first operand, then hands any operator chain to a
  // separate *Chain method, whose locals are paid for only when the chain is there.

  // parseExpr is the entry point for WHERE, the SELECT list, and UPDATE assignment values.
  parseExpr(): Expr {
    // A fresh sub-expression is one nesting level deeper (parens, ARRAY/ROW/CASE/function
    // operands, subscript indices all re-enter here). Bounds the recursive descent itself.
    // The OR level is folded in here, one frame fewer per level.
    this.deepen();
    const lhs = this.parseAnd();
    const e = this.peekKeyword() === "or" ? this.parseOrChain(lhs) : lhs;
    this.undeepen();
    return e;
  }

  private parseOrChain(first: Expr): Expr {
    const base = this.depth;
    let lhs = first;
    while (this.peekKeyword() === "or") {
      this.deepen(); // each chained OR is one more AST level
      const loc = this.here();
      this.advance();
      lhs = binaryExpr("or", lhs, this.parseAnd(), loc);
    }
    this.depth = base;
    return lhs;
  }

  private parseAnd(): Expr {
    const lhs = this.parseNot();
    return this.peekKeyword() === "and" ? this.parseAndChain(lhs) : lhs;
  }

  private parseAndChain(first: Expr): Expr {
    const base = this.depth;
    let lhs = first;
    while (this.peekKeyword() === "and") {
      this.deepen(); // each chained AND is one more AST level
      const loc = this.here();
      this.advance();
      lhs = binaryExpr("and", lhs, this.parseNot(), loc);
    }
    this.depth = base;
    return lhs;
  }

  private parseNot(): Expr {
    // A qualified column has structural priority over keyword-led expressions (§3): `not.v`
    // starts with the relation label `not`, not the unary operator. The primary parser applies
    // the same rule before literals / CAST / CASE / current-date spellings.
    if (this.peekKeyword() === "not" && !this.atQualifiedColumnRef()) {
      const loc = this.here();
      this.advance();
      // right-associative: NOT NOT x — each NOT is one more AST level (recursion here, so the
      // depth guard also protects the parser's own stack).
      this.deepen();
      const operand = this.parseNot();
      this.undeepen();
      return { kind: "unary", op: "not", operand, loc };
    }
    return this.parseComparison();
  }

  // parseComparison parses one comparison, a postfix IS [NOT] NULL, or
  // IS [NOT] DISTINCT FROM, all non-associative: `a = b = c` is a syntax error, and
  // `a + 1 IS NULL` binds as `(a + 1) IS NULL`. After the shared `IS` `NOT`? it
  // dispatches on the NULL vs DISTINCT FROM keyword (spec/grammar/grammar.ebnf
  // `comparison`).
  private parseComparison(): Expr {
    return this.parseComparisonTail(this.parseConcat());
  }

  // parseComparisonTail parses whatever comparison follows lhs, if any.
  private parseComparisonTail(lhs: Expr): Expr {
    if (this.peekKeyword() === "is") return this.parseIsPredicate(lhs);
    // `NOT`? (`IN` (...) | `BETWEEN` lo `AND` hi) — a `NOT` here is consumed only when followed
    // by one of these postfix-predicate keywords (two-token lookahead; the prefix `NOT` was
    // already taken by parseNot). Non-associative, at the comparison level (grammar.md §20-§21).
    const predNegated =
      this.peekKeyword() === "not" &&
      (this.peekKeywordAt(1) === "in" ||
        this.peekKeywordAt(1) === "between" ||
        this.peekKeywordAt(1) === "like" ||
        this.peekKeywordAt(1) === "ilike");
    // The operator's position: the `NOT` of a negated form, else the keyword (PostgreSQL's).
    const opLoc = this.here();
    if (predNegated) {
      this.advance(); // NOT
    }
    if (this.peekKeyword() === "in") return this.parseInPredicate(lhs, predNegated, opLoc);
    if (this.peekKeyword() === "between") {
      this.advance();
      // Both bounds parse at the CONCAT level (one tighter than comparison), which never
      // consumes `AND` (a looser level owned by parseAnd). So the BETWEEN's structural `AND` is
      // matched here and `x BETWEEN a AND b AND c` parses as `(x BETWEEN a AND b) AND c`
      // (grammar.md §21); a `||` bound still works.
      const lo = this.parseConcat();
      this.expectKeyword("and");
      const hi = this.parseConcat();
      return { kind: "between", lhs, lo, hi, negated: predNegated, loc: opLoc };
    }
    // LIKE / ILIKE (case-insensitive) — grammar.md §22. `ilike` is just another peeked keyword.
    // An `ESCAPE e` clause desugars, as in PostgreSQL, to the pattern `like_escape(p, e)`; a bare NULL
    // or `$N` pattern/escape is typed text by the function's signature (grammar.md §22).
    if (this.peekKeyword() === "like" || this.peekKeyword() === "ilike")
      return this.parseLikePredicate(lhs, predNegated, opLoc);
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
      return {
        kind: "regex",
        lhs,
        rhs,
        negated: rxNegated,
        insensitive: rxInsensitive,
        loc: opLoc,
      };
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
    if (kw === "all" || kw === "any" || kw === "some")
      return this.parseQuantifiedComparison(lhs, op, kw, opLoc);
    return binaryExpr(op, lhs, this.parseConcat(), opLoc);
  }

  // parseIsPredicate parses the postfix `IS [NOT] …` predicates on lhs (a parseComparison arm).
  private parseIsPredicate(lhs: Expr): Expr {
    const isLoc = this.here();
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
      return { kind: "isDistinct", lhs, rhs: this.parseConcat(), negated, loc: isLoc };
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

  // parseInPredicate parses `[NOT] IN (…)` on lhs (a parseComparison arm).
  private parseInPredicate(lhs: Expr, predNegated: boolean, opLoc: SrcLoc | undefined): Expr {
    this.advance();
    this.expect("lparen");
    // `IN (SELECT ...)` is the uncorrelated IN-subquery (grammar.md §26), disambiguated by a
    // leading `SELECT` (or a nested `WITH` — cte.md §7); otherwise a non-empty value list
    // (`IN ()` is a 42601 syntax error).
    if (this.atSubqueryStart()) {
      const query = this.parseSubquery();
      this.expect("rparen");
      return { kind: "inSubquery", lhs, query, negated: predNegated, loc: opLoc };
    }
    const list = [this.parseConcat()];
    while (this.peek().kind === "comma") {
      this.advance();
      list.push(this.parseConcat());
    }
    this.expect("rparen");
    return { kind: "in", lhs, list, negated: predNegated, loc: opLoc };
  }

  // parseLikePredicate parses `[NOT] LIKE|ILIKE pattern [ESCAPE e]` on lhs (a parseComparison arm).
  private parseLikePredicate(lhs: Expr, predNegated: boolean, opLoc: SrcLoc | undefined): Expr {
    const insensitive = this.peekKeyword() === "ilike";
    const likeLoc = this.here();
    this.advance();
    let rhs = this.parseConcat();
    if (this.peekKeyword() === "escape") {
      this.advance();
      const escapeChar = this.parseConcat();
      rhs = {
        kind: "funcCall",
        name: "like_escape",
        args: [rhs, escapeChar],
        argNames: [],
        star: false,
        distinct: false,
        variadic: false,
        loc: likeLoc,
      };
    }
    return { kind: "like", lhs, rhs, negated: predNegated, insensitive, loc: opLoc };
  }

  // parseQuantifiedComparison parses `op ANY|SOME|ALL (…)` on lhs (a parseComparison arm).
  private parseQuantifiedComparison(
    lhs: Expr,
    op: BinaryOp,
    kw: string,
    opLoc: SrcLoc | undefined,
  ): Expr {
    const all = kw === "all";
    this.advance(); // ANY / SOME / ALL
    this.expect("lparen");
    // A leading `SELECT` is the SUBQUERY form `op ANY/ALL(SELECT …)` — the subquery spelling of
    // IN (array-functions.md §11.6), the §26 leading-`SELECT` lookahead (or a nested `WITH` —
    // cte.md §7); anything else is the array operand (§11.1).
    if (this.atSubqueryStart()) {
      const query = this.parseSubquery();
      this.expect("rparen");
      return { kind: "quantifiedSubquery", op, all, lhs, query, loc: opLoc };
    }
    const array = this.parseExpr(); // a full expression resolving to an array
    this.expect("rparen");
    return { kind: "quantified", op, all, lhs, array, loc: opLoc };
  }

  // parseConcat parses the "any other operator" level (grammar.md §39/§40, array-functions.md §8/§10):
  // one rung tighter than the comparisons, looser than additive, left-associative. It hosts `||` array
  // concatenation plus the `@>`/`<@`/`&&` array containment/overlap operators — all the same precedence
  // in PostgreSQL. Each operand is an additive expression, so `a + b || c` is `(a + b) || c`; chaining
  // mixes freely (`a || b @> c` is `(a || b) @> c`).
  private parseConcat(): Expr {
    return this.parseConcatChain(this.parseAdditive());
  }

  private parseConcatChain(first: Expr): Expr {
    const base = this.depth;
    let lhs = first;
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
      const loc = this.here();
      this.advance();
      lhs = binaryExpr(op, lhs, this.parseAdditive(), loc);
    }
  }

  private parseAdditive(): Expr {
    const lhs = this.parseMultiplicative();
    const k = this.peek().kind;
    return k === "plus" || k === "minus" ? this.parseAdditiveChain(lhs) : lhs;
  }

  private parseAdditiveChain(first: Expr): Expr {
    const base = this.depth;
    let lhs = first;
    for (;;) {
      let op: BinaryOp;
      if (this.peek().kind === "plus") op = "add";
      else if (this.peek().kind === "minus") op = "sub";
      else {
        this.depth = base;
        return lhs;
      }
      this.deepen(); // each chained +/- is one more AST level (the `1+1+…` vector)
      const loc = this.here();
      this.advance();
      lhs = binaryExpr(op, lhs, this.parseMultiplicative(), loc);
    }
  }

  private parseMultiplicative(): Expr {
    const lhs = this.parseAtTimeZone();
    const k = this.peek().kind;
    return k === "star" || k === "slash" || k === "percent"
      ? this.parseMultiplicativeChain(lhs)
      : lhs;
  }

  private parseMultiplicativeChain(first: Expr): Expr {
    const base = this.depth;
    let lhs = first;
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
      const loc = this.here();
      this.advance();
      lhs = binaryExpr(op, lhs, this.parseAtTimeZone(), loc);
    }
  }

  // parseAtTimeZone parses the `AT TIME ZONE` rung (grammar.md §49, timezones.md §6): a
  // left-associative infix operator binding tighter than `* / %`, additive, and the comparisons,
  // looser than COLLATE / `::` / unary minus (PostgreSQL's %left AT). `value AT TIME ZONE zone`
  // desugars to the function call timezone(zone, value) — PostgreSQL's own implementation — so the
  // resolver/evaluator/cost have one path for the operator and the bare call. AT/TIME/ZONE are
  // non-reserved (matched as a three-token sequence), so a bare column named at/time/zone is unaffected.
  private parseAtTimeZone(): Expr {
    const lhs = this.peek().kind === "minus" ? this.parseNegation() : this.parsePostfix();
    return this.peekKeyword() === "at" ? this.parseAtTimeZoneChain(lhs) : lhs;
  }

  private parseAtTimeZoneChain(first: Expr): Expr {
    const base = this.depth;
    let lhs = first;
    while (
      this.peekKeyword() === "at" &&
      this.peekKeywordAt(1) === "time" &&
      this.peekKeywordAt(2) === "zone"
    ) {
      this.deepen(); // each chained AT TIME ZONE is one more AST level
      const at = this.here();
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
        loc: at,
      };
    }
    this.depth = base;
    return lhs;
  }

  private parseUnary(): Expr {
    if (this.peek().kind === "minus") return this.parseNegation();
    return this.parsePostfix();
  }

  // parseNegation parses a unary `-` and its operand (the parseUnary arm).
  private parseNegation(): Expr {
    const minus = this.here();
    this.advance();
    // Fold unary-minus-of-an-integer-literal into one negative literal, so i64's
    // minimum is representable and the literal range-checks against context. SUPPRESSED
    // when a `::` immediately follows: `::` binds tighter than unary minus (PostgreSQL),
    // so `-N::T` is `-(N::T)` — the cast applies to the unsigned magnitude first
    // (grammar.md §37). A one-token lookahead on the token AFTER the literal.
    if (this.peek().kind === "int" && this.tokens[this.pos + 1]?.kind !== "doubleColon") {
      const v = foldInt(this.advance().int!, true);
      return { kind: "literal", literal: { kind: "int", int: v }, loc: minus };
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
        loc: minus,
      };
    }
    // each chained unary `-` is one more AST level (recursion here, so the depth guard also
    // protects the parser's own stack against `- - - … x`).
    this.deepen();
    const operand = this.parseUnary();
    this.undeepen();
    return { kind: "unary", op: "neg", operand, loc: minus };
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
    const parenthesized = this.peek().kind === "lparen";
    const expr = this.parsePrimary();
    return this.parsePostfixChain(expr, parenthesized && this.pos !== this.bareRowEnd);
  }

  private parsePostfixChain(primary: Expr, accessible: boolean): Expr {
    const base0 = this.depth;
    let fieldAccessible = accessible;
    let expr = primary;
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
        expr = this.parseCastSuffix(expr);
        fieldAccessible = false;
      } else if (k === "lbracket") {
        expr = this.parseSubscriptSuffix(expr);
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

  // parseCastSuffix parses the `:: type` postfix cast on expr (a parsePostfix arm).
  private parseCastSuffix(expr: Expr): Expr {
    const castLoc = this.here();
    this.advance();
    const typeLoc = this.here();
    const baseType = this.expectTypeName();
    const typeMod = this.parseTypeMod();
    const typeName = this.consumeArrayBrackets() ? baseType + "[]" : baseType;
    return { kind: "cast", inner: expr, typeName, typeMod, castLoc, typeLoc };
  }

  // parseSubscriptSuffix parses `base[..][..]` — array subscript (spec/design/array.md §6), a
  // parsePostfix arm. Applies to ANY base (no parens rule, unlike `.field`). Consecutive `[…]`
  // brackets collect into ONE access (so `a[1][2]` is a single multidim element read, not nested).
  // Each spec is an index `[i]` or a slice `[m:n]` (bounds optionally omitted). After a subscript a
  // `.field` still needs parens (PG).
  private parseSubscriptSuffix(base: Expr): Expr {
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
    return { kind: "subscript", base, subscripts };
  }

  // parsePrimary parses a parenthesized expression, CAST(...), a literal (integer,
  // TRUE/FALSE, NULL), or a column reference.
  private parsePrimary(): Expr {
    // Every word is legal as an identifier (§3), including spellings otherwise dispatched below
    // as literals or keyword-led expressions. Recognize the unambiguous `identifier.identifier`
    // shape first so a relation alias such as `null`, `cast`, or `case` remains usable.
    if (this.atQualifiedColumnRef()) return this.parseQualifiedPrimary();
    if (this.peek().kind === "lparen") return this.parseParenPrimary();
    // `EXISTS ( SELECT ... )` — the existence predicate (grammar.md §26). Recognized only when an
    // open-paren + a query start (`SELECT`, or a nested `WITH` — cte.md §7) follows, so `exists`
    // stays usable as a column / function name.
    if (
      this.peekKeyword() === "exists" &&
      this.tokens[this.pos + 1]?.kind === "lparen" &&
      this.isQueryStartAtOffset(2)
    )
      return this.parseExistsPrimary();
    // `ROW(e1, e2, …)` composite constructor (spec/design/composite.md §1). Recognized when `ROW`
    // is immediately followed by `(`, so `row` stays usable as a column / function name otherwise.
    // The bare `(a, b)` form is parsed with the parenthesized expression above.
    if (this.peekKeyword() === "row" && this.peekKindAt(1) === "lparen")
      return this.parseRowConstructor();
    // `ARRAY[e1, e2, …]` array constructor (spec/design/array.md §1). Recognized when `ARRAY` is
    // immediately followed by `[`, so `array` stays usable as an identifier otherwise.
    if (this.peekKeyword() === "array" && this.peekKindAt(1) === "lbracket")
      return this.parseArrayConstructor();
    // A typed string literal `type '...'` (grammar.md §36) — PostgreSQL's `type 'string'`, equal to
    // CAST('string' AS type) over a string-literal operand: ANY type-naming word immediately followed
    // by a string (`INTERVAL '1 day'`, `TIMESTAMP '...'`, `INTEGER '42'`, `BYTEA '\xDE'`, …).
    // Recognized only when the next token is a string — a one-token lookahead — so the word stays
    // usable as a column / function name otherwise. true/false/null are excluded (their own value
    // literals). The type name is resolved (and the string coerced to it) at resolve; unknown → 42704.
    const tlKw = this.peekKeyword();
    if (
      isIdentKind(this.peek().kind) &&
      tlKw !== "null" &&
      tlKw !== "true" &&
      tlKw !== "false" &&
      this.tokens[this.pos + 1]?.kind === "str"
    )
      return this.parseTypedLiteral();
    // The two-word `character varying '...'` typed literal (types.md §15).
    if (
      tlKw === "character" &&
      this.peekKeywordAt(1) === "varying" &&
      this.tokens[this.pos + 2]?.kind === "str"
    )
      return this.parseCharacterVaryingLiteral();
    // The SQL/JSON query functions `JSON_EXISTS` / `JSON_VALUE` / `JSON_QUERY`
    // (json-sql-functions.md §5, S2) — keyword-led primaries with sub-clauses, recognized when the
    // keyword is immediately followed by `(` (so the words stay usable as identifiers otherwise).
    {
      const kw = this.peekKeyword();
      if (
        (kw === "json_exists" || kw === "json_value" || kw === "json_query") &&
        this.peekKindAt(1) === "lparen"
      )
        return this.parseJsonQueryFunction(kw);
    }
    // `JSON(expr [(WITH|WITHOUT) UNIQUE [KEYS]])` — the SQL/JSON JSON() constructor
    // (json-sql-functions.md §5). Distinguished from the `json '...'` typed literal (handled above, a
    // string follows) and a generic call by being the JSON keyword immediately followed by `(`.
    if (this.peekKeyword() === "json" && this.peekKindAt(1) === "lparen")
      return this.parseJsonConstructor();
    if (this.peekKeyword() === "cast") return this.parseCastPrimary();
    // EXTRACT(field FROM source) (grammar.md §50, timezones.md §9.2). Recognized only when `extract`
    // is immediately followed by `(`, so `extract` stays usable as a column / function name otherwise
    // (the one-token lookahead, §8). The field is an identifier (bare or quoted) or a string literal
    // — PostgreSQL's extract_arg — ASCII-lowercased, as the unit lookup folds it (timezones.md §9.6).
    if (this.peekKeyword() === "extract" && this.peekKindAt(1) === "lparen")
      return this.parseExtractPrimary();
    // `COALESCE(a, b, …)` — the first-non-NULL conditional (grammar.md §51). Recognized only
    // when COALESCE is immediately followed by `(` (the JSON(/EXTRACT( one-token lookahead), so
    // the word stays usable as a column name. At least one argument (an empty list is 42601 —
    // PostgreSQL's grammar has no empty form).
    if (this.peekKeyword() === "coalesce" && this.peekKindAt(1) === "lparen")
      return this.parseCoalescePrimary();
    // `GREATEST(a, b, …)` / `LEAST(a, b, …)` — the variadic max/min (grammar.md §52). Recognized
    // only when the keyword is immediately followed by `(` (the same one-token lookahead), so the
    // words stay usable as column names. At least one argument (an empty list is 42601 —
    // PostgreSQL's grammar has no empty form).
    {
      const kw = this.peekKeyword();
      if ((kw === "greatest" || kw === "least") && this.peekKindAt(1) === "lparen")
        return this.parseGreatestLeast(kw);
    }
    // `NULLIF(a, b)` — NULL when a = b, else a (grammar.md §58). Recognized only when NULLIF is
    // immediately followed by `(` (the same one-token lookahead), so the word stays usable as a
    // column name. Exactly two arguments: any other count is 42601, as in PostgreSQL's grammar.
    if (this.peekKeyword() === "nullif" && this.peekKindAt(1) === "lparen") {
      const loc = this.here();
      this.advance(); // NULLIF
      this.advance(); // (
      const a = this.parseExpr();
      this.expect("comma");
      const b = this.parseExpr();
      this.expect("rparen");
      return { kind: "nullIf", args: [a, b], loc };
    }
    if (this.peekKeyword() === "case") return this.parseCasePrimary();
    if (this.atFunctionCall()) return this.parseFunctionCall();
    return this.parseLiteralOrName();
  }

  // atFunctionCall reports whether the cursor is on a function call: a bare or quoted identifier
  // IMMEDIATELY followed by "(" (grammar.md §17). The one-token lookahead keeps function names
  // non-reserved (a column may be named `count`); a qualified name is never a call. A bare null /
  // true / false is its literal. Checked before parseLiteralOrName, so the call's recursion does not
  // run in that function's frame.
  private atFunctionCall(): boolean {
    const t = this.peek();
    if (this.tokens[this.pos + 1]?.kind !== "lparen") return false;
    if (t.kind === "quotedIdent") return true;
    return t.kind === "word" && t.word !== "null" && t.word !== "true" && t.word !== "false";
  }

  // parseQualifiedPrimary parses a qualified column reference `q.name` (a parsePrimary arm).
  private parseQualifiedPrimary(): Expr {
    const loc = this.here();
    const qualifier = this.expectIdentifier();
    this.expect("dot");
    const name = this.expectIdentifier();
    return { kind: "qualifiedColumn", qualifier, name, loc };
  }

  // parseParenPrimary parses a parenthesized expression, scalar subquery, or bare row constructor (a parsePrimary arm).
  private parseParenPrimary(): Expr {
    const parenLoc = this.here();
    this.advance();
    // `(SELECT ...)` is a scalar subquery (grammar.md §26), disambiguated by a leading `SELECT`
    // (or a nested `WITH` — cte.md §7) after the `(`; otherwise a parenthesized expression.
    if (this.atSubqueryStart()) {
      const query = this.parseSubquery();
      this.expect("rparen");
      return { kind: "scalarSubquery", query, loc: parenLoc };
    }
    const e = this.parseExpr();
    // `(a, b, …)` — the bare row constructor (composite.md §1), PostgreSQL's implicit `ROW`
    // spelling: two or more comma-separated expressions in parentheses. Identical to
    // `ROW(a, b, …)`; a single parenthesized expression stays a grouping.
    if (this.peek().kind === "comma") {
      const fields: Expr[] = [e];
      while (this.peek().kind === "comma") {
        this.advance();
        fields.push(this.parseExpr());
      }
      this.expect("rparen");
      this.bareRowEnd = this.pos;
      return { kind: "row", fields, loc: parenLoc };
    }
    this.expect("rparen");
    return e;
  }

  // parseExistsPrimary parses `EXISTS ( query )` (a parsePrimary arm).
  private parseExistsPrimary(): Expr {
    const loc = this.here();
    this.advance(); // EXISTS
    this.expect("lparen");
    const query = this.parseSubquery();
    this.expect("rparen");
    return { kind: "exists", query, loc };
  }

  // parseRowConstructor parses `ROW(e1, e2, …)` (a parsePrimary arm).
  private parseRowConstructor(): Expr {
    const loc = this.here();
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
    return { kind: "row", fields, loc };
  }

  // parseArrayConstructor parses `ARRAY[e1, e2, …]` (a parsePrimary arm).
  private parseArrayConstructor(): Expr {
    const loc = this.here();
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
    return { kind: "array", elements, loc };
  }

  // parseTypedLiteral parses a typed string literal `type '...'` (a parsePrimary arm).
  private parseTypedLiteral(): Expr {
    const typeLoc = this.here();
    const typeName = this.expectIdentifier(); // the named type (folded or quoted — exact)
    const loc = this.here();
    const t = this.advance();
    return { kind: "typedLiteral", typeName, text: t.str!, loc, typeLoc };
  }

  // parseCharacterVaryingLiteral parses the `character varying '...'` typed literal (a parsePrimary arm).
  private parseCharacterVaryingLiteral(): Expr {
    const typeLoc = this.here();
    const typeName = this.expectTypeName();
    const loc = this.here();
    const t = this.advance();
    return { kind: "typedLiteral", typeName, text: t.str!, loc, typeLoc };
  }

  // parseJsonQueryFunction parses `JSON_EXISTS` / `JSON_VALUE` / `JSON_QUERY` (a parsePrimary arm).
  private parseJsonQueryFunction(kw: string): Expr {
    const loc = this.here();
    this.advance(); // the function keyword
    this.advance(); // (
    const ctx = this.parseExpr();
    // `FORMAT JSON` after the context item is accepted (and ignored — a text/json/jsonb context
    // is coerced to jsonb regardless).
    this.skipFormatJson();
    this.expect("comma");
    const path = this.parseExpr();
    const passing = this.parseJsonPassing();
    let expr: Expr;
    if (kw === "json_exists") {
      const onError = this.parseJsonOnErrorOnly();
      expr = { kind: "jsonExists", ctx, path, passing, onError, loc };
    } else if (kw === "json_value") {
      const returning = this.parseJsonReturning();
      const [onEmpty, onError] = this.parseJsonOnClauses();
      expr = { kind: "jsonValue", ctx, path, passing, returning, onEmpty, onError, loc };
    } else {
      const returning = this.parseJsonReturning();
      const [wrapper, keepQuotes] = this.parseJsonWrapperQuotes();
      const [onEmpty, onError] = this.parseJsonOnClauses();
      expr = {
        kind: "jsonQuery",
        ctx,
        path,
        passing,
        returning,
        wrapper,
        keepQuotes,
        onEmpty,
        onError,
        loc,
      };
    }
    this.expect("rparen");
    return expr;
  }

  // parseJsonConstructor parses the `JSON(expr …)` constructor (a parsePrimary arm).
  private parseJsonConstructor(): Expr {
    const loc = this.here();
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
    return { kind: "jsonCtor", operand, uniqueKeys, loc };
  }

  // parseCastPrimary parses `CAST(expr AS type)` (a parsePrimary arm).
  private parseCastPrimary(): Expr {
    const loc = this.here();
    this.advance();
    this.expect("lparen");
    const inner = this.parseExpr();
    this.expectKeyword("as");
    const typeLoc = this.here();
    const baseType = this.expectTypeName();
    const typeMod = this.parseTypeMod();
    const typeName = this.consumeArrayBrackets() ? baseType + "[]" : baseType;
    this.expect("rparen");
    return { kind: "cast", inner, typeName, typeMod, loc, castLoc: loc, typeLoc };
  }

  // parseExtractPrimary parses `EXTRACT(field FROM source)` (a parsePrimary arm).
  private parseExtractPrimary(): Expr {
    const loc = this.here();
    this.advance(); // EXTRACT
    this.expect("lparen");
    let field: string;
    if (this.peekKindAt(0) === "str") {
      field = this.advance().str!;
    } else if (this.peekKindAt(0) === "word" || this.peekKindAt(0) === "quotedIdent") {
      field = this.expectIdentifier();
    } else {
      throw engineError("syntax_error", "expected an EXTRACT field");
    }
    this.expectKeyword("from");
    const source = this.parseExpr();
    this.expect("rparen");
    return {
      kind: "extract",
      field: field.replace(/[A-Z]/g, (c) => c.toLowerCase()),
      source,
      loc,
    };
  }

  // parseCoalescePrimary parses `COALESCE(a, b, …)` (a parsePrimary arm).
  private parseCoalescePrimary(): Expr {
    const loc = this.here();
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
    return { kind: "coalesce", args, loc };
  }

  // parseGreatestLeast parses `GREATEST(…)` / `LEAST(…)` (a parsePrimary arm).
  private parseGreatestLeast(kw: string): Expr {
    const greatest = kw === "greatest";
    const loc = this.here();
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
    return { kind: "greatestLeast", args, greatest, loc };
  }

  // parseCasePrimary parses a `CASE` expression (a parsePrimary arm).
  private parseCasePrimary(): Expr {
    const caseLoc = this.here();
    this.advance();
    // Simple form has an operand between CASE and the first WHEN; the searched form starts
    // directly with WHEN (grammar.md §23).
    const operand = this.peekKeyword() === "when" ? null : this.parseExpr();
    const whens: { cond: Expr; result: Expr; loc?: SrcLoc }[] = [];
    while (this.peekKeyword() === "when") {
      // The WHEN keyword's position — a simple CASE's comparison is reported there (§10.2).
      const whenLoc = this.here();
      this.advance();
      const cond = this.parseExpr();
      this.expectKeyword("then");
      const result = this.parseExpr();
      whens.push({ cond, result, loc: whenLoc });
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
    return { kind: "case", operand, whens, els, loc: caseLoc };
  }

  // parseLiteralOrName parses a parameter, a literal, a function call, or a column reference (a parsePrimary arm).
  private parseLiteralOrName(): Expr {
    const t = this.peek();
    // The primary token's source character position (spec/design/error-fields.md §10.2).
    const primLoc = this.here();
    if (t.kind === "param") {
      return { kind: "param", index: this.advance().paramIndex!, loc: primLoc };
    }
    if (t.kind === "int") {
      // The only magnitude > i64 max the lexer admits is 2^63, which fits no signed
      // integer type unless negated (handled by the unary-minus fold).
      const v = foldInt(this.advance().int!, false);
      return { kind: "literal", literal: { kind: "int", int: v }, loc: primLoc };
    }
    if (t.kind === "decimal") {
      this.advance();
      return {
        kind: "literal",
        literal: {
          kind: "decimal",
          dec: Decimal.fromDigitsScale(false, t.decDigits!, t.decScale!),
        },
        loc: primLoc,
      };
    }
    if (t.kind === "str") {
      this.advance();
      return { kind: "literal", literal: { kind: "text", text: t.str! }, loc: primLoc };
    }
    if (t.kind === "word") {
      const w = t.word!;
      if (w === "null") {
        this.advance();
        return { kind: "literal", literal: { kind: "null" }, loc: primLoc };
      }
      if (w === "true") {
        this.advance();
        return { kind: "literal", literal: { kind: "bool", value: true }, loc: primLoc };
      }
      if (w === "false") {
        this.advance();
        return { kind: "literal", literal: { kind: "bool", value: false }, loc: primLoc };
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
          loc: primLoc,
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
          loc: primLoc,
        };
      }
      const loc = this.here();
      const [qualifier, name] = this.parseColumnRef();
      return qualifier !== null
        ? { kind: "qualifiedColumn", qualifier, name, loc }
        : { kind: "column", name, loc };
    }
    if (t.kind === "quotedIdent") {
      // A quoted identifier is never a keyword (grammar.md §3): a column reference (a call was taken
      // by atFunctionCall).
      const loc = this.here();
      const [qualifier, name] = this.parseColumnRef();
      return qualifier !== null
        ? { kind: "qualifiedColumn", qualifier, name, loc }
        : { kind: "column", name, loc };
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
    // An UNQUOTED `position` is only the SQL special form `POSITION(sub IN s)`.
    if (this.peek().kind === "word" && this.peek().word === "position")
      return this.parsePositionForm();
    // An UNQUOTED `substring` / `overlay` admits its SQL special form after the first argument
    // (below).
    const substringForm = this.peek().kind === "word" && this.peek().word === "substring";
    const overlayForm = this.peek().kind === "word" && this.peek().word === "overlay";
    const loc = this.here();
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
    // A trailing in-aggregate ORDER BY after the last argument (`json_agg(x ORDER BY y)` —
    // aggregates.md §22): parsed with allowOrdinal OFF, so a bare integer is a constant key
    // (PostgreSQL), and it ends the argument list. The resolver rejects it on a non-aggregate (42809)
    // or a window call (0A000).
    let aggOrder: OrderKey[] | null = null;
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
          if (this.peekKeyword() === "order") {
            aggOrder = this.parseOrderBy(false);
            break;
          }
          // A VARIADIC argument must be the last (PostgreSQL, 42601).
          if (this.peek().kind === "comma") {
            throw engineError("syntax_error", "VARIADIC argument must be the last argument");
          }
          break;
        }
        // A named argument is `identifier "=>" expr` (grammar.md §17); a two-token lookahead
        // (word then "=>") distinguishes it from a bare expr that starts with an identifier.
        let argName: string | null = null;
        if (isIdentKind(this.peek().kind) && this.peekKindAt(1) === "fatArrow") {
          argName = this.expectIdentifier();
          this.expect("fatArrow");
          anyNamed = true;
        } else if (anyNamed) {
          // A positional argument may not follow a named one (PostgreSQL, 42601).
          throw engineError("syntax_error", "positional argument cannot follow named argument");
        }
        args.push(this.parseExpr());
        names.push(argName);
        // `SUBSTRING(x FROM s [FOR l])` / `SUBSTRING(x FOR l [FROM s])` (grammar.ebnf
        // `substring_expr`, string-functions.md §4): a FROM/FOR keyword after the first positional
        // argument selects the special form.
        if (
          substringForm &&
          argName === null &&
          !distinct &&
          args.length === 1 &&
          (this.peekKeyword() === "from" || this.peekKeyword() === "for")
        ) {
          return this.parseSubstringTail(name, args[0]!, loc);
        }
        // `OVERLAY(s PLACING r FROM start [FOR count])` (grammar.ebnf `overlay_expr`,
        // string-functions.md §3): a PLACING keyword after the first positional argument selects
        // the special form.
        if (
          overlayForm &&
          argName === null &&
          !distinct &&
          args.length === 1 &&
          this.peekKeyword() === "placing"
        ) {
          return this.parseOverlayTail(name, args[0]!, loc);
        }
        if (this.peekKeyword() === "order") {
          aggOrder = this.parseOrderBy(false);
          break;
        }
        if (this.peek().kind !== "comma") break;
        this.advance();
      }
    }
    this.expect("rparen");
    // Keep argNames empty unless a name appeared (the all-positional sentinel — §8).
    return this.parseCallSuffix(
      name,
      args,
      anyNamed ? names : [],
      star,
      distinct,
      variadic,
      aggOrder,
      loc,
    );
  }

  // parseCallSuffix parses what may follow a call's argument list — WITHIN GROUP, FILTER, OVER — and
  // builds the funcCall (the tail of parseFunctionCall, kept out of its frame).
  private parseCallSuffix(
    name: string,
    args: Expr[],
    argNames: (string | null)[],
    star: boolean,
    distinct: boolean,
    variadic: boolean,
    aggOrder: OrderKey[] | null,
    loc: SrcLoc | undefined,
  ): Expr {
    // A trailing `WITHIN GROUP (ORDER BY <key>)` marks an ordered-set aggregate (mode /
    // percentile_cont / percentile_disc — aggregates.md §13). It comes between the argument list and
    // any FILTER / OVER (PG order). WITHIN/GROUP are not reserved; right after the call's `)` they are
    // always the clause. The order key reuses parseOrderBy with allowOrdinal OFF — a general expression
    // (`ORDER BY a + b`) but with a bare integer treated as a constant (not an ordinal), matching
    // PostgreSQL; the resolver enforces exactly one key (42883) and the per-name rules.
    let withinGroup: OrderKey[] | null = null;
    if (this.peekKeyword() === "within") {
      // The in-aggregate ORDER BY and WITHIN GROUP both order the aggregated input; PostgreSQL
      // rejects the pair in its grammar (42601).
      if (aggOrder !== null) {
        throw engineError("syntax_error", "cannot use multiple ORDER BY clauses with WITHIN GROUP");
      }
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
          // Present only when written, so an ordinary call's AST shape is unchanged.
          ...(aggOrder !== null ? { aggOrder } : {}),
          loc,
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
      ...(aggOrder !== null ? { aggOrder } : {}),
      loc,
    };
  }

  // parsePositionForm parses the SQL special form `POSITION(substring IN string)` (grammar.ebnf
  // `position_expr`, string-functions.md §3), the cursor on the unquoted `position`. Desugars to the
  // ordinary call `position(string, substring)` — PostgreSQL's rewrite, arguments swapped. Each
  // operand is an "any other operator" (parseConcat) expression, so the operand's own IN is never
  // taken as an IN predicate; anything else after the first operand (a comma) is 42601, as in
  // PostgreSQL.
  private parsePositionForm(): Expr {
    const loc = this.here();
    const name = this.expectIdentifier();
    this.expect("lparen");
    this.deepen();
    const sub = this.parseConcat();
    this.expectKeyword("in");
    const str = this.parseConcat();
    this.undeepen();
    this.expect("rparen");
    return {
      kind: "funcCall",
      name,
      args: [str, sub],
      argNames: [],
      star: false,
      distinct: false,
      variadic: false,
      loc,
    };
  }

  // parseOverlayTail parses the tail of the SQL special form `OVERLAY(s PLACING r FROM start [FOR
  // count])` (grammar.ebnf `overlay_expr`), the cursor on PLACING. Desugars to the ordinary call
  // `overlay(s, r, start[, count])` — PostgreSQL's rewrite. FROM is required. No FILTER / OVER may
  // follow (the form is not a function application).
  private parseOverlayTail(name: string, source: Expr, loc: SrcLoc | undefined): Expr {
    this.expectKeyword("placing");
    const replacement = this.parseExpr();
    this.expectKeyword("from");
    const args: Expr[] = [source, replacement, this.parseExpr()];
    if (this.peekKeyword() === "for") {
      this.advance();
      args.push(this.parseExpr());
    }
    this.expect("rparen");
    return {
      kind: "funcCall",
      name,
      args,
      argNames: [],
      star: false,
      distinct: false,
      variadic: false,
      loc,
    };
  }

  // parseSubstringTail parses the tail of the SQL special form `SUBSTRING(x FROM s [FOR l])` /
  // `SUBSTRING(x FOR l [FROM s])` (grammar.ebnf `substring_expr`), the cursor on FROM or FOR.
  // Desugars to the ordinary call `substring(x, s[, l])` — PostgreSQL's rewrite — with a FOR-only
  // form starting at 1. No FILTER / OVER may follow (the form is not a function application).
  private parseSubstringTail(name: string, source: Expr, loc: SrcLoc | undefined): Expr {
    let start: Expr | null = null;
    let count: Expr | null = null;
    if (this.peekKeyword() === "from") {
      this.advance();
      start = this.parseExpr();
      if (this.peekKeyword() === "for") {
        this.advance();
        count = this.parseExpr();
      }
    } else {
      this.expectKeyword("for");
      count = this.parseExpr();
      if (this.peekKeyword() === "from") {
        this.advance();
        start = this.parseExpr();
      }
    }
    this.expect("rparen");
    const args: Expr[] = [source, start ?? { kind: "literal", literal: { kind: "int", int: 1n } }];
    if (count !== null) args.push(count);
    return {
      kind: "funcCall",
      name,
      args,
      argNames: [],
      star: false,
      distinct: false,
      variadic: false,
      loc,
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
  // (json-sql-functions.md §5) and reports whether one was there. After a context item the clause is
  // accepted and ignored — a text/json/jsonb context is coerced to jsonb regardless.
  private skipFormatJson(): boolean {
    if (this.peekKeyword() === "format" && this.peekKeywordAt(1) === "json") {
      this.advance(); // FORMAT
      this.advance(); // JSON
      return true;
    }
    return false;
  }

  // parseJsonPassing parses an optional `PASSING value [FORMAT JSON [ENCODING name]] AS name [, …]` clause of a
  // SQL/JSON query function or `JSON_TABLE` (json-sql-functions.md §5.4) → its arguments (empty when
  // absent). The name is any identifier, folded like one (`AS X` binds `$x`; `AS "X"` binds `$X`).
  private parseJsonPassing(): JsonPassing[] {
    const passing: JsonPassing[] = [];
    if (this.peekKeyword() !== "passing") return passing;
    this.advance(); // PASSING
    for (;;) {
      const expr = this.parseExpr();
      const formatJson = this.skipFormatJson();
      let encoding: string | null = null;
      if (formatJson && this.peekKeyword() === "encoding") {
        this.advance(); // ENCODING
        encoding = jsonEncodingName(this.expectIdentifier());
      }
      this.expectKeyword("as");
      const name = this.expectIdentifier();
      passing.push({ expr, formatJson, encoding, name });
      if (this.peek().kind !== "comma") return passing;
      this.advance();
    }
  }

  // parseJsonReturning parses an optional `RETURNING <type> [FORMAT JSON]` clause → the type name
  // (resolved later).
  private parseJsonReturning(): string | null {
    if (this.peekKeyword() !== "returning") return null;
    this.advance(); // RETURNING
    const ty = this.expectTypeName();
    this.skipFormatJson();
    return ty;
  }

  // parseJsonBehavior parses one constant SQL/JSON behavior word (`ERROR` / `NULL` / `TRUE` / `FALSE`
  // / `UNKNOWN` / `EMPTY [ARRAY|OBJECT]`). `DEFAULT expr` is parsed by parseJsonOnClauses.
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
          "syntax_error",
          "invalid ON ERROR behavior: only ERROR, TRUE, FALSE, or UNKNOWN is allowed in ON ERROR for JSON_EXISTS()",
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
  // (in that order). A behavior is a constant word or `DEFAULT expr` (json-sql-functions.md §5.3);
  // the expression ends at the `ON`.
  private parseJsonOnClauses(): [JsonBehavior | null, JsonBehavior | null] {
    let onEmpty: JsonBehavior | null = null;
    let onError: JsonBehavior | null = null;
    if (this.peekKeyword() === "default") {
      const [b, which] = this.parseJsonDefaultBehavior();
      if (which === "error") return [null, b];
      onEmpty = b;
    } else if (this.isJsonBehaviorStart() && this.peekOnClauseIs("empty")) {
      const kind = this.parseJsonBehavior();
      this.advance(); // ON
      this.advance(); // EMPTY
      onEmpty = { kind, default: null };
    }
    if (this.peekKeyword() === "default") {
      const [b, which] = this.parseJsonDefaultBehavior();
      if (which === "empty")
        throw engineError("syntax_error", "ON EMPTY must come before ON ERROR");
      onError = b;
    } else if (this.isJsonBehaviorStart() && this.peekOnClauseIs("error")) {
      const kind = this.parseJsonBehavior();
      this.advance(); // ON
      this.advance(); // ERROR
      onError = { kind, default: null };
    }
    return [onEmpty, onError];
  }

  // parseJsonDefaultBehavior parses `DEFAULT expr ON EMPTY|ERROR` (the cursor at DEFAULT) → the
  // behavior and which clause it answers.
  private parseJsonDefaultBehavior(): [JsonBehavior, "empty" | "error"] {
    this.advance(); // DEFAULT
    const expr = this.parseExpr();
    this.expectKeyword("on");
    const which = this.peekKeyword();
    if (which !== "empty" && which !== "error") {
      throw engineError("syntax_error", "expected EMPTY or ERROR after ON");
    }
    this.advance();
    return [{ kind: "default", default: expr }, which];
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
    this.note(this.pos);
    return this.tokens[this.pos]!;
  }

  // note records that the parse examined token i (error-fields.md §10.3). Only peek and advance
  // note: multi-token lookahead (peekKeywordAt / peekKindAt / tokens[pos + n]) and the precomputed
  // parenQuery table do not.
  private note(i: number): void {
    if (i > this.furthest) this.furthest = i;
  }

  // here is the current token's character position, or undefined for stored text (SrcLoc).
  private here(): SrcLoc | undefined {
    return this.positions[this.pos];
  }

  // furthestLoc is the character position of the furthest token examined (0 for stored text).
  furthestLoc(): number {
    return this.positions[this.furthest] ?? 0;
  }

  // peekKeyword returns the current token if it is a bare word (already folded by the lexer), else
  // "". A quoted identifier is never a keyword (grammar.md §3).
  private peekKeyword(): string {
    const t = this.peek();
    return t.kind === "word" ? t.word! : "";
  }

  // peekKeywordAt returns the keyword (a bare word, already folded) `offset` tokens ahead of the cursor if that
  // token is a word, else "". Used for the two-token NOT IN/BETWEEN/LIKE lookahead (a
  // CLAUDE.md §8 determinism surface — byte-identical across the three parsers).
  private peekKeywordAt(offset: number): string {
    const t = this.tokens[this.pos + offset];
    return t !== undefined && t.kind === "word" ? t.word! : "";
  }

  // peekKindAt returns the token kind `offset` tokens ahead of the cursor ("eof" past the
  // end). Used with peekKeywordAt for the CHECK-constraint lookahead (grammar.md §29).
  private peekKindAt(offset: number): TokenKind {
    const t = this.tokens[this.pos + offset];
    return t !== undefined ? t.kind : "eof";
  }

  private advance(): Token {
    this.note(this.pos);
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
    if (t.kind === "word" && t.word! === kw) return;
    throw engineError("syntax_error", `expected keyword '${kw}'`);
  }

  // expectTypeName consumes a type name in any type position (column/field definition, ALTER …
  // TYPE, CAST, `::`, a typed literal, RETURNING, a JSON_TABLE column, CREATE SEQUENCE … AS): an
  // identifier, or PostgreSQL's two-word `character varying` (types.md §15). The two-word form is
  // recognized only as two bare words — `character` then `varying` (a quoted word is an ordinary
  // identifier) — and returns the alias "character varying", which the catalog resolves to text
  // exactly like varchar. Any other `character` stays a one-word name (unknown type → 42704 at
  // resolve). A CLAUDE.md §8 cross-core determinism surface (spec/design/grammar.md §14).
  private expectTypeName(): string {
    if (this.peekKeyword() === "character" && this.peekKeywordAt(1) === "varying") {
      this.advance(); // CHARACTER
      this.advance(); // VARYING
      return "character varying";
    }
    return this.expectIdentifier();
  }

  // expectIdentifier consumes an identifier — a bare word (folded) or a quoted identifier (exact) —
  // and returns the name (grammar.md §3).
  private expectIdentifier(): string {
    const t = this.advance();
    const name = identName(t);
    if (name === null) {
      throw engineError("syntax_error", "expected an identifier");
    }
    return name;
  }

  // expectCollationName consumes a quoted collation name after COLLATE (spec/design/collation.md §1).
  // The name is a double-quoted identifier — case-sensitive and kept verbatim ("C", "en-US") — so a
  // bare word is not accepted (it would case-fold). An empty name ("") is a 42601 syntax error.
  private expectCollationName(): string {
    const t = this.advance();
    if (t.kind !== "quotedIdent") {
      throw engineError("syntax_error", "expected a quoted collation name after COLLATE");
    }
    // The lexer rejects a zero-length `""` (42601), so a quoted name is never empty.
    return t.str!;
  }

  // parseViewBody parses a whole token stream's `subquery_expr` (parseViewDefinition's entry).
  parseViewBody(): QueryExpr {
    return this.parseSubquery();
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

// parseViewDefinition parses a stored view definition (spec/design/views.md §3): the canonical body
// text as a `subquery_expr`, with the body's nesting depth (§4.1) — the deepest level its parse
// reached, the body's own `subquery_expr` level included. Every token position the AST records
// (TableRef.pos, a star's pos) is an index into lex(text), which is what the rename rewrites and `*`
// freezing edit.
export function parseViewDefinition(text: string): { body: QueryExpr; depth: number } {
  return parseViewDefinitionAt(text, []);
}

// parseViewDefinitionAt is parseViewDefinition for a CREATE VIEW body being validated: `positions`
// are the body's tokens' character positions in the statement the host wrote (CreateView.bodyLocs),
// recorded on the AST so a resolution error in the body carries Position
// (spec/design/error-fields.md §10.1). They are used only when they align with lex(text); otherwise
// none are recorded.
export function parseViewDefinitionAt(
  text: string,
  positions: number[],
): { body: QueryExpr; depth: number } {
  const tokens = lex(text);
  const p = new Parser(tokens, positions.length === tokens.length ? positions : []);
  const body = p.parseViewBody();
  p.expectEof();
  return { body, depth: p.maxDepth };
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
    if (!tokenNames(t, oldName)) continue;
    const prev = tokens[i - 1];
    const prev2 = tokens[i - 2];
    const next = tokens[i + 1];
    const skip =
      next?.kind === "lparen" ||
      next?.kind === "fatArrow" ||
      next?.kind === "str" ||
      prev?.kind === "doubleColon" ||
      (prev?.kind === "word" && prev.word! === "as") ||
      (prev?.kind === "dot" && !tokenNames(prev2, table)) ||
      (next?.kind === "dot" && oldName === table);
    if (!skip) tokens[i] = nameToken(newName);
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
    if (tokenNames(t, oldName) && tokens[i + 1]!.kind === "dot") tokens[i] = nameToken(newName);
  }
  if (tokens.at(-1)?.kind === "eof") tokens.pop();
  const rewritten = renderTokens(tokens);
  return { text: rewritten, expr: parseExpression(rewritten) };
}

// parseNameText parses a name-valued TEXT argument as exactly one identifier — the sequence
// functions' `nextval('s')` regclass text form (spec/design/grammar.md §3), PostgreSQL's
// SplitIdentifierString rule: surrounding whitespace (space, tab, LF, CR, FF) is ignored; a
// double-quoted name is exact (`""` an embedded quote; `'""'` is the empty name, which never
// exists); otherwise the run of characters up to whitespace or `.` is a bare name, folded to
// lowercase (ASCII). Empty / all-whitespace text, an unterminated quote, anything after the name
// (internal whitespace, trailing characters), or a dotted name is 42602 invalid_name — jed has no
// schemas and does not resolve a qualified name from text. Not length-checked (cost.md §7).
export function parseNameText(text: string): string {
  const invalid = (): EngineError => engineError("invalid_name", "invalid name syntax");
  const isWs = (c: string): boolean =>
    c === " " || c === "\t" || c === "\n" || c === "\r" || c === "\f";
  let i = 0;
  const n = text.length;
  while (i < n && isWs(text[i]!)) i++;
  let name: string;
  if (i < n && text[i] === '"') {
    i++;
    name = "";
    for (;;) {
      if (i >= n) throw invalid();
      const c = text[i]!;
      if (c === '"') {
        if (i + 1 < n && text[i + 1] === '"') {
          name += '"';
          i += 2;
          continue;
        }
        i++;
        break;
      }
      name += c;
      i++;
    }
  } else {
    const start = i;
    while (i < n && !isWs(text[i]!) && text[i] !== ".") i++;
    if (i === start) throw invalid();
    name = asciiLower(text.slice(start, i));
  }
  while (i < n && isWs(text[i]!)) i++;
  if (i < n) throw invalid();
  return name;
}

// asciiLower folds ASCII A-Z only (a non-ASCII character is left untouched — PostgreSQL's
// downcase_identifier for a UTF-8 database).
function asciiLower(s: string): string {
  return s.replace(/[A-Z]+/g, (m) => m.toLowerCase());
}

// tokenNames reports whether an identifier token (bare word or quoted) spells exactly `name`
// (byte-exact after the lexer's fold, grammar.md §3).
function tokenNames(t: Token | undefined, name: string): boolean {
  return identName(t) === name;
}

// isPlainIdentifier reports whether `name` survives the lexer written bare: `[a-z_][a-z0-9_]*`
// (grammar.md §3's quoting rule). Any other name must be rendered double-quoted.
export function isPlainIdentifier(name: string): boolean {
  return /^[a-z_][a-z0-9_]*$/.test(name);
}

// nameToken is the token a RENAME rewrite substitutes into persisted expression text: ALWAYS a
// quoted identifier, even for a plain lowercase name — jed has no reserved keywords, so a bare
// `null` / `true` would re-lex as the literal (grammar.md §3, alter.md §2.2). Untouched tokens keep
// their stored spelling.
function nameToken(name: string): Token {
  return { kind: "quotedIdent", str: name };
}

// quoteIdentifier renders `name` as SQL identifier text by grammar.md §3's quoting rule: bare when
// it matches `[a-z_][a-z0-9_]*`, else double-quoted with each `"` doubled.
export function quoteIdentifier(name: string): string {
  return isPlainIdentifier(name) ? name : '"' + name.replaceAll('"', '""') + '"';
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
      // A double-quoted identifier round-trips verbatim with `"` doubled (format v37; a bare word
      // renders as its folded lowercase spelling).
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
    case "doubleColon":
      return "::";
    case "fatArrow":
      return "=>";
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
    case "eof": // never inside the parentheses
      return "";
  }
}

// VALUES_LABEL is the label of a VALUES query's relation (PostgreSQL's "*VALUES*").
const VALUES_LABEL = "*VALUES*";

// valuesQuery builds a VALUES query (grammar.md §25) — a statement, a set-operation operand, or a
// VALUES body with trailing ORDER BY / LIMIT / OFFSET (§42) — as the query `SELECT * FROM (VALUES …)
// ORDER BY … LIMIT … OFFSET …` over an inner VALUES relation labeled *VALUES* (PostgreSQL's name for
// it): the keys resolve against column1, column2, … by name, as "*VALUES*".column1, or by ordinal,
// and an enclosing derived table's alias and column-rename list apply to the query's output unchanged
// (v.column1 in the keys is 42P01, as in PostgreSQL).
function valuesQuery(
  rows: Expr[][],
  orderBy: OrderKey[],
  limit: bigint | null,
  offset: bigint | null,
): Select {
  const inner: TableRef = { name: VALUES_LABEL, alias: VALUES_LABEL, args: null, values: rows };
  return {
    kind: "select",
    distinct: false,
    items: { kind: "all" },
    from: inner,
    joins: [],
    filter: null,
    groupBy: [],
    having: null,
    orderBy,
    limit,
    offset,
    windows: [],
  };
}

// foldTrailing folds a query expression's trailing ORDER BY / LIMIT / OFFSET onto its node
// (grammar.md §25). A parenthesized query is the SAME query as its contents (PostgreSQL), so its own
// trailing clauses are already set: giving one of them a second time — `(SELECT … ORDER BY a) ORDER
// BY b`, `(SELECT … LIMIT 2) LIMIT 1` — is 42601, while a clause the contents lack is simply added. A
// parenthesized nested WITH folds onto its main query.
function foldTrailing(
  node: QueryExpr,
  orderBy: OrderKey[],
  limit: bigint | null,
  offset: bigint | null,
): QueryExpr {
  if (node.kind === "withExpr") {
    return { ...node, body: foldTrailing(node.body, orderBy, limit, offset) };
  }
  if (orderBy.length > 0 && node.orderBy.length > 0) {
    throw engineError("syntax_error", "multiple ORDER BY clauses not allowed");
  }
  if (offset !== null && node.offset !== null) {
    throw engineError("syntax_error", "multiple OFFSET clauses not allowed");
  }
  if (limit !== null && node.limit !== null) {
    throw engineError("syntax_error", "multiple LIMIT clauses not allowed");
  }
  // Both Select and SetOp carry orderBy/limit/offset; the spread keeps the `kind` discriminant.
  return {
    ...node,
    orderBy: orderBy.length > 0 ? orderBy : node.orderBy,
    limit: limit ?? node.limit,
    offset: offset ?? node.offset,
  };
}

// rejectSecondWith throws 42601 for a WITH clause whose main query is itself a parenthesized WITH
// query — `WITH a AS (…) (WITH b AS (…) SELECT …)` (PostgreSQL: both clauses would attach to one
// query). A parenthesized WITH query as a set-operation operand is a separate query and is fine.
function rejectSecondWith(body: QueryExpr): void {
  if (body.kind === "withExpr") {
    throw engineError("syntax_error", "multiple WITH clauses not allowed");
  }
}

// jsonEncodingName is the canonical name of a SQL/JSON ENCODING clause's encoding
// (json-sql-functions.md §5.4): UTF8, UTF16, or UTF32, matched case-insensitively as PostgreSQL's
// makeJsonEncoding does. Any other name is 22023.
function jsonEncodingName(name: string): string {
  switch (name.toLowerCase()) {
    case "utf8":
      return "UTF8";
    case "utf16":
      return "UTF16";
    case "utf32":
      return "UTF32";
  }
  throw engineError("invalid_parameter_value", `unrecognized JSON encoding: ${name}`);
}
