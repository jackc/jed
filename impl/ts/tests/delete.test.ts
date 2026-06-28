// DELETE: by predicate, no-WHERE (all rows), Kleene (NULL rows not matched), and the
// load-bearing no-PK rowid fix — DELETE then INSERT must not collide on a freed rowid.

import assert from "node:assert/strict";
import { test } from "node:test";
import { Engine, execute } from "../src/lib.ts";
import { errCode } from "./util.ts";

test("delete from a missing table traps 42P01", () => {
  assert.equal(
    errCode(() => execute(new Engine(), "DELETE FROM nope")),
    "42P01",
  );
});
