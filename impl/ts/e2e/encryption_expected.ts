// The Node side of the real-browser encryption e2e (e2e/opfs.spec.ts): run with node, it prints the
// request the browser worker (browser/workers/encryption_worker.ts) checks itself against — the
// seeded workload and the bytes the Node file host's node:crypto codec writes for it, and each
// encrypted_* golden with a probe and the rows the Node host reads from it.
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { createDatabase, Environment, openDatabase } from "../src/lib.ts";
import { render } from "../src/value.ts";

const SEED = 0x4a45_4442n;
const KEY = new Uint8Array(32).fill(0x4a);
const PAGE = 1024;
const WORKLOAD = [
  "CREATE TABLE t (id i64 PRIMARY KEY, note text, blob bytea, span i32range)",
  "CREATE INDEX t_note ON t (note)",
  "CREATE INDEX t_span ON t USING gist (span)",
  "INSERT INTO t SELECT g, 'secret-' || g, NULL, i32range(g, g + 3) FROM generate_series(1, 300) g",
  "UPDATE t SET blob = decode(repeat(md5(id::text), 60), 'hex') WHERE id % 40 = 0",
  "DELETE FROM t WHERE id % 3 = 0",
  "BEGIN",
  "INSERT INTO t VALUES (1000, 'in a block', NULL, NULL)",
  "UPDATE t SET note = 'changed' WHERE id = 10",
  "COMMIT",
  "INSERT INTO t VALUES (2000, 'tail', NULL, NULL)",
];
const GOLDENS = [
  { name: "encrypted_pk_table.jed", probe: "SELECT count(*), sum(id), sum(v), count(v) FROM t" },
  {
    name: "encrypted_overflow_table.jed",
    probe: "SELECT id, md5(body), md5(blob) FROM t ORDER BY id",
  },
  {
    name: "encrypted_index_table.jed",
    probe:
      "SELECT count(*) FILTER (WHERE r && i32range(40, 44)), count(*) FILTER (WHERE v = 300), sum(v) FROM t",
  },
  { name: "encrypted_catalog.jed", probe: "SELECT count(*) FROM t29" },
];

const dir = mkdtempSync(join(tmpdir(), "jed-e2e-encryption-"));
try {
  const path = join(dir, "control.jed");
  const db = createDatabase({
    path,
    pageSize: PAGE,
    skipFsync: true,
    environment: Environment.builder().insecureSeededKeyEntropy(SEED).build(),
    credential: { key: KEY },
    locking: "none",
  });
  const s = db.session();
  for (const sql of WORKLOAD) s.execute(sql);
  s.close();
  db.close();
  const expected = [...readFileSync(path)];

  const fixtures = new URL("../../../spec/fileformat/fixtures/", import.meta.url);
  const goldens = GOLDENS.map((g) => {
    const file = new URL(g.name, fixtures);
    const gdb = openDatabase(file.pathname, {
      credential: { key: KEY },
      readOnly: true,
      locking: "none",
    });
    const gs = gdb.session();
    const want = [...gs.query(g.probe)].map((row) => row.map(render).join("|"));
    gs.close();
    gdb.close();
    return { name: g.name, bytes: [...readFileSync(file)], probe: g.probe, want };
  });
  process.stdout.write(
    JSON.stringify({
      seed: SEED.toString(),
      key: [...KEY],
      pageSize: PAGE,
      goldens,
      workload: WORKLOAD,
      expected,
      timing: { rows: 20000, pageSize: 4096 },
    }),
  );
} finally {
  rmSync(dir, { recursive: true, force: true });
}
