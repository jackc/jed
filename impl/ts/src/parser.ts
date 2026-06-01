// Hand-written recursive-descent parser (CLAUDE.md §5, §10). Mirrors parser.go /
// parser.rs. Errors throw EngineError (42601). The lexer emits generic word tokens (no
// reserved-keyword table), so keywords are matched case-insensitively here.

import type {
  Assignment,
  ColumnDef,
  CompareOp,
  Delete,
  Insert,
  Literal,
  Operand,
  OrderBy,
  Predicate,
  Select,
  SelectExpr,
  SelectItems,
  Statement,
  Update,
} from "./ast.ts";
import { engineError } from "./errors.ts";
import { lex } from "./lexer.ts";
import type { Token, TokenKind } from "./token.ts";

function lower(s: string): string {
  return s.toLowerCase();
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

  // parseInsert parses `INSERT INTO <table> VALUES ( <literal> [, <literal>]* )`.
  private parseInsert(): Insert {
    this.expectKeyword("insert");
    this.expectKeyword("into");
    const table = this.expectIdentifier();
    this.expectKeyword("values");
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
      throw engineError("syntax_error", "VALUES must have at least one value");
    }
    return { kind: "insert", table, values };
  }

  // parseLiteral parses an integer literal or the keyword NULL.
  private parseLiteral(): Literal {
    const t = this.advance();
    if (t.kind === "int") return { kind: "int", int: t.int! };
    if (t.kind === "word" && lower(t.word!) === "null") return { kind: "null" };
    throw engineError("syntax_error", "expected a literal value");
  }

  // parseSelect parses
  // `SELECT <items> FROM <table> [WHERE <predicate>] [ORDER BY <col> [ASC|DESC]]`.
  private parseSelect(): Select {
    this.expectKeyword("select");
    const items = this.parseSelectItems();
    this.expectKeyword("from");
    const from = this.expectIdentifier();

    const filter = this.parseOptionalWhere();

    let orderBy: OrderBy | null = null;
    if (this.peekKeyword() === "order") {
      this.advance();
      this.expectKeyword("by");
      const column = this.expectIdentifier();
      let descending = false;
      if (this.peekKeyword() === "asc") {
        this.advance();
      } else if (this.peekKeyword() === "desc") {
        this.advance();
        descending = true;
      }
      orderBy = { column, descending };
    }

    return { kind: "select", items, from, filter, orderBy };
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
      const value = this.parseOperand();
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

  // parseOptionalWhere parses an optional trailing `WHERE <predicate>` (shared by
  // SELECT / UPDATE / DELETE).
  private parseOptionalWhere(): Predicate | null {
    if (this.peekKeyword() !== "where") return null;
    this.advance();
    return this.parsePredicate();
  }

  private parseSelectItems(): SelectItems {
    if (this.peek().kind === "star") {
      this.advance();
      return { kind: "all" };
    }
    const items: SelectExpr[] = [];
    for (;;) {
      items.push(this.parseSelectExpr());
      if (this.peek().kind === "comma") {
        this.advance();
        continue;
      }
      break;
    }
    return { kind: "list", items };
  }

  // parseSelectExpr parses `CAST ( <expr> AS <type> )`, a bare integer literal, or a
  // bare column name.
  private parseSelectExpr(): SelectExpr {
    if (this.peekKeyword() === "cast") {
      this.advance();
      this.expect("lparen");
      const inner = this.parseSelectExpr();
      this.expectKeyword("as");
      const typeName = this.expectIdentifier();
      this.expect("rparen");
      return { kind: "cast", inner, typeName };
    }
    if (this.peek().kind === "int") {
      const n = this.advance().int!;
      return { kind: "literal", literal: { kind: "int", int: n } };
    }
    const name = this.expectIdentifier();
    return { kind: "column", name };
  }

  // parsePredicate parses `<col> IS [NOT] NULL` or `<col> <cmp> <operand>`.
  private parsePredicate(): Predicate {
    const column = this.expectIdentifier();
    if (this.peekKeyword() === "is") {
      this.advance();
      let negated = false;
      if (this.peekKeyword() === "not") {
        this.advance();
        negated = true;
      }
      this.expectKeyword("null");
      return { kind: "isNull", column, negated };
    }
    let op: CompareOp;
    switch (this.advance().kind) {
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
        throw engineError("syntax_error", "expected a comparison operator");
    }
    const rhs = this.parseOperand();
    return { kind: "compare", column, op, rhs };
  }

  // parseOperand parses a comparison's right-hand side: a literal (integer or NULL) or
  // a column reference.
  private parseOperand(): Operand {
    const t = this.peek();
    if (t.kind === "int") {
      this.advance();
      return { kind: "literal", literal: { kind: "int", int: t.int! } };
    }
    if (t.kind === "word" && lower(t.word!) === "null") {
      this.advance();
      return { kind: "literal", literal: { kind: "null" } };
    }
    if (t.kind === "word") {
      const name = this.expectIdentifier();
      return { kind: "column", name };
    }
    throw engineError("syntax_error", "expected an operand");
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
