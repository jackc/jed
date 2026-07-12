import assert from "node:assert/strict";
import { test } from "node:test";
import { parseSQL } from "../src/parser.ts";

// Keywords are legal identifiers in jed (a deliberate PostgreSQL divergence). UPDATE's DEFAULT
// special form must therefore yield to an ordinary expression when the RHS continues.
test("UPDATE default keyword with a continuing RHS is a column", () => {
  const stmt = parseSQL("UPDATE t SET result = default + 1");
  assert.equal(stmt.kind, "update");
  if (stmt.kind !== "update") return;
  const assignment = stmt.assignments[0]!;
  assert.equal(assignment.isDefault, false);
  assert.equal(assignment.value.kind, "binary");
  if (assignment.value.kind !== "binary") return;
  assert.deepEqual(assignment.value.lhs, { kind: "column", name: "default" });
});
