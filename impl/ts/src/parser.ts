// Hand-written recursive-descent parser (CLAUDE.md §5, §10). Mirrors parser.go /
// parser.rs. Errors throw EngineError (42601). The lexer emits generic word tokens (no
// reserved-keyword table), so keywords are matched case-insensitively here.

import type {
  Assignment,
  BinaryOp,
  CheckDef,
  Cte,
  CteBody,
  DefaultDef,
  ForeignKeyDef,
  RefAction,
  UniqueDef,
  ColumnDef,
  Delete,
  Expr,
  IdentitySpec,
  ConflictTarget,
  Insert,
  InsertValue,
  JoinClause,
  JoinKind,
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
    // An optional table_scope between CREATE and TABLE makes the table TEMPORARY
    // (spec/design/temp-tables.md, grammar.ebnf `table_scope`). SHARED / TEMP / TEMPORARY are NOT
    // reserved (§3): recognized positionally here — the word after TABLE is always the table name, so
    // `CREATE TABLE temp (...)` / `CREATE TABLE shared (...)` are ordinary persistent tables. A leading
    // SHARED makes a database-wide shared temp table (§4) and MUST be immediately followed by
    // TEMP/TEMPORARY (a stray `CREATE SHARED TABLE …` is 42601); so shared always has temp===true.
    const shared = this.peekKeyword() === "shared";
    if (shared) this.advance();
    const temp = this.peekKeyword() === "temp" || this.peekKeyword() === "temporary";
    if (temp) this.advance();
    if (shared && !temp) {
      throw engineError("syntax_error", "SHARED must be followed by TEMP or TEMPORARY");
    }
    this.expectKeyword("table");
    const name = this.expectIdentifier();
    this.expect("lparen");

    const columns: ColumnDef[] = [];
    const tablePks: string[][] = [];
    const checks: CheckDef[] = [];
    const uniques: UniqueDef[] = [];
    const fks: ForeignKeyDef[] = [];
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
      temp,
      shared,
      columns,
      tablePks,
      checks,
      uniques,
      fks,
    };
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
    // The unnamed form is `INDEX ON <table> [USING <method>] (` — the word after INDEX is the
    // index name unless it is `ON` followed by a word and then `(` OR `USING` (the three-token
    // lookahead, extended for the optional USING clause — grammar.md §30, gin.md §3).
    const unnamed =
      this.peekKeyword() === "on" &&
      this.peekKindAt(1) === "word" &&
      (this.peekKindAt(2) === "lparen" || this.peekKeywordAt(2) === "using");
    const name = unnamed ? null : this.expectIdentifier();
    this.expectKeyword("on");
    const table = this.expectIdentifier();
    // Optional `USING <method>` between the table name and the column list (PG order — gin.md §3,
    // grammar.md §30). Not reserved (positional); the method is resolved at execution (42704 if
    // unknown), not here.
    let using: string | undefined = undefined;
    if (this.peekKeyword() === "using") {
      this.advance();
      using = this.expectIdentifier();
    }
    this.expect("lparen");
    const columns: string[] = [];
    for (;;) {
      columns.push(this.expectIdentifier());
      const tok = this.advance();
      if (tok.kind === "comma") continue;
      if (tok.kind === "rparen") break;
      throw engineError("syntax_error", `expected ',' or ')', found ${tok.kind}`);
    }
    return { kind: "createIndex", name, table, columns, unique, using };
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
    return { kind: "createType", name, fields };
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
    const orderBy = this.parseOrderBy();
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
    const orderBy = this.parseOrderBy();
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
      keys.push(
        qualifier !== null
          ? { kind: "qualifiedColumn", qualifier, name }
          : { kind: "column", name },
      );
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
    // An SRF is implicitly lateral; `lateral` records only whether the keyword was written.
    return { name, alias, args, lateral };
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
      // Optional `COLLATE "name"` on the sort key (spec/design/collation.md §1), between the column
      // and the ASC/DESC direction (PG order).
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
      keys.push({ qualifier, column, collation, descending, nullsFirst });
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
    if (this.peekKeyword() === "like") {
      this.advance();
      const rhs = this.parseConcat();
      return { kind: "like", lhs, rhs, negated: predNegated };
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
      else if (k === "containedBy") op = "containedBy";
      else if (k === "overlaps") op = "overlaps";
      else if (k === "strictlyLeft") op = "strictlyLeft";
      else if (k === "strictlyRight") op = "strictlyRight";
      else if (k === "notExtendRight") op = "notExtendRight";
      else if (k === "notExtendLeft") op = "notExtendLeft";
      else if (k === "adjacent") op = "adjacent";
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
    let lhs = this.parseUnary();
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
      lhs = binaryExpr(op, lhs, this.parseUnary());
    }
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
    // DISTINCT inside a function call (COUNT(DISTINCT x)) is deferred — reject at parse.
    if (this.peekKeyword() === "distinct") {
      throw engineError("syntax_error", "DISTINCT inside an aggregate is not supported yet");
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
    return { kind: "funcCall", name, args, argNames, star, variadic };
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
    default: // "eof" — never inside the parentheses
      return "";
  }
}
