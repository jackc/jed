import { createDatabase } from '/home/jack/.codex/worktrees/b8e6/jed/impl/ts/src/file.ts';
import { intValue } from '/home/jack/.codex/worktrees/b8e6/jed/impl/ts/src/value.ts';
import { drainOutcome } from '/home/jack/.codex/worktrees/b8e6/jed/impl/ts/src/tooling.ts';

const database = createDatabase();
const session = database.session();
const setup = [
  'CREATE TABLE p9_left (id i32 PRIMARY KEY, k i32)',
  'CREATE TABLE p9_right (id i32 PRIMARY KEY, k i32)',
  'INSERT INTO p9_left VALUES (1,0),(2,0),(3,0),(4,0),(5,0),(6,0),(7,0),(8,0),(9,1),(10,NULL)',
  'INSERT INTO p9_right VALUES (1,0),(2,0),(3,1),(4,1),(5,2),(6,NULL)',
  'ANALYZE p9_left (k)', 'ANALYZE p9_right (k)',
  'CREATE INDEX p9_left_k ON p9_left(k)'
];
for (const sql of setup) session.execute(sql, []);
const observations = [];
for (const value of [0, 1, 2]) {
  for (const mode of ['literal', 'parameter']) {
    const sql = `SELECT l.id, r.id FROM p9_left l JOIN p9_right r ON l.k = r.k WHERE l.k = ${mode === 'literal' ? value : '$1'}`;
    for (const statement of [`EXPLAIN ${sql}`, `${sql} ORDER BY l.id, r.id`]) {
      const result = drainOutcome(session.query(statement, mode === 'literal' ? [] : [intValue(BigInt(value))]));
      observations.push({mode, value, sql: statement, result});
    }
  }
}
console.log(JSON.stringify({setup, observations}, (_, value) => typeof value === 'bigint' ? value.toString() : value, 2));
session.close();
database.close();
