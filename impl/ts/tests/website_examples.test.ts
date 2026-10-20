// Execute the exact TypeScript files shown by the website's CodeTabs. Browser tests
// exercise the LiveSql panels; highlighting a host API snippet does not execute it.
import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import {
  copyFileSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readdirSync,
  rmSync,
  symlinkSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";
import { createDatabase, openDatabase, type Row } from "../src/async/node.ts";

const run = promisify(execFile);
const examples = fileURLToPath(new URL("../../../web/examples/", import.meta.url));
const packageRoot = fileURLToPath(new URL("../", import.meta.url));

type Example = {
  output: string[];
  database?: string;
  seed?: string;
  query?: string;
  rows?: Row[];
};

const cases: Record<string, Example> = {
  "open-database": {
    output: ["Ada", "Grace"],
    database: "people.jed",
    query: "SELECT name FROM person ORDER BY id",
    rows: [{ name: "Ada" }, { name: "Grace" }],
  },
  queries: {
    output: ["inserted 1 row", "balance = 100", "1: Ada"],
    database: "app.jed",
    seed: "CREATE TABLE account (id i32 PRIMARY KEY, name text, balance i64)",
  },
  transactions: {
    output: [],
    database: "bank.jed",
    seed: "CREATE TABLE account (id i32 PRIMARY KEY, balance i64); INSERT INTO account VALUES (1, 200), (2, 50)",
    query: "SELECT balance FROM account ORDER BY id",
    rows: [{ balance: 100n }, { balance: 150n }],
  },
  scripts: {
    output: ["ran 3 statements"],
    database: "app.jed",
    query: "SELECT balance FROM account ORDER BY id",
    rows: [{ balance: 100n }, { balance: 50n }],
  },
  authorization: {
    output: ["denied"],
    database: "app.jed",
    query: "SELECT body FROM report",
    rows: [{ body: "hello" }],
  },
  "resource-limits": {
    output: ["denied: 54P02", "spent: 3n", "admission: 54P02"],
  },
  "session-variables": {
    output: ["tenant: acme", "missing: null", "2024-01-15 07:00:00-05"],
  },
  "host-functions": { output: ["host functions verified"] },
};

test("every website TypeScript example has an execution fixture", () => {
  const topics = readdirSync(examples).filter((topic) =>
    existsSync(join(examples, topic, "ts.ts")),
  );
  assert.deepEqual(topics.sort(), Object.keys(cases).sort());
});

for (const [topic, example] of Object.entries(cases)) {
  test(`website TypeScript example: ${topic}`, { timeout: 30000 }, async () => {
    const directory = mkdtempSync(join(tmpdir(), "jed-website-example-"));
    try {
      // Resolve the documented package imports without rewriting any example source
      // or adding an installed dependency. Each process owns a fresh working directory.
      mkdirSync(join(directory, "node_modules"));
      symlinkSync(
        packageRoot,
        join(directory, "node_modules", "jed-ts"),
        process.platform === "win32" ? "junction" : "dir",
      );
      writeFileSync(join(directory, "package.json"), '{"type":"module"}\n');
      copyFileSync(join(examples, topic, "ts.ts"), join(directory, "example.ts"));

      if (example.seed) {
        const db = await createDatabase({ path: join(directory, example.database!) });
        try {
          await db.executeScript(example.seed);
        } finally {
          await db.close();
        }
      }

      // The host-function tab exports worker-local registration; it must be loaded
      // through extensionModule, not called in the application process.
      const launcher =
        topic === "host-functions"
          ? `import assert from "node:assert/strict";
import { createDatabase } from "jed-ts";
const db = await createDatabase({ extensionModule: new URL("./example.ts", import.meta.url).href });
try {
  assert.deepEqual(await db.get("SELECT discount(1250, 15) AS sale, with_tax(1250) AS taxed, discount(NULL::i64, 15) AS missing"),
    { sale: 1063n, taxed: 1350n, missing: null });
  console.log("host functions verified");
} finally {
  await db.close();
}
`
          : 'await import("./example.ts");\n';
      writeFileSync(join(directory, "run.mjs"), launcher);
      // A leaked execution worker keeps the child alive and fails the timeout.
      const { stdout } = await run(process.execPath, ["run.mjs"], {
        cwd: directory,
        timeout: 20000,
      });
      for (const expected of example.output) {
        assert.ok(stdout.includes(expected), `expected ${JSON.stringify(expected)} in ${stdout}`);
      }
      if (example.query) {
        const db = await openDatabase(join(directory, example.database!));
        try {
          assert.deepEqual(await db.all(example.query), example.rows);
        } finally {
          await db.close();
        }
      }
    } finally {
      rmSync(directory, { recursive: true, force: true });
    }
  });
}
