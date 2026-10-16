import { createDatabase, Environment, intValue, render } from 'jed-ts';

// A host registers its own SCALAR FUNCTIONS over the built-in types on an EnvironmentBuilder, builds
// the (immutable) Environment, and hands it to createDatabase/openDatabase — every session of the
// handle shares it. It is never written to the file (a reopening host brings its own).
const builder = Environment.builder();

// discount(cents, pct) -> the price after a whole-percent discount. STRICT — a NULL argument
// short-circuits to NULL before the kernel runs, so the closure never sees one — and reached by an
// EXACT (i64, i64) signature (no implicit promotion; a built-in of the same signature would win).
// `cost: 2n` is charged once per call and gated against a session's maxCost, so the function stays
// inside the untrusted-query bound.
builder.registerFunction({
  name: 'discount',
  argTypes: ['i64', 'i64'],
  result: 'i64',
  volatility: 'immutable', // same inputs ⇒ same output
  crossCore: true, // results are byte-identical on every core
  cost: 2n,
  componentId: 'com.example/discount', // a stable identity for index-backing
  semanticVersion: 1, // bump when a formula change would invalidate stored index keys
  kernel: (args) => {
    // strict + resolved (i64, i64), so both args are non-null ints
    const cents = args[0] as { kind: 'int'; int: bigint };
    const pct = args[1] as { kind: 'int'; int: bigint };
    return intValue(cents.int - (cents.int * pct.int) / 100n);
  }
});

// with_tax(cents) -> cents plus 8% tax, as a BATCH kernel: it receives a whole column of rows per
// call (args[0][i] is row i's argument) and pushes one result per row to `out`. jed calls it once
// per chunk where it already holds the rows, and one row at a time elsewhere — same rows, cost, and
// errors either way.
builder.registerFunction({
  name: 'with_tax',
  argTypes: ['i64'],
  result: 'i64',
  volatility: 'immutable',
  batchKernel: (args, out) => {
    for (const v of args[0]) {
      const cents = (v as { kind: 'int'; int: bigint }).int;
      out.push(intValue(cents + (cents * 8n) / 100n));
    }
  }
});

const db = createDatabase({ environment: builder.build() });

db.execute('CREATE TABLE product (id i32 PRIMARY KEY, name text, price_cents i64)');
db.execute("INSERT INTO product VALUES (1, 'Mug', 1250), (2, 'Notebook', 400)");

// Because discount is IMMUTABLE and carries a component identity, it can back a persisted index.
// On reopen, if the environment supplies a different component/version, the index is skipped for
// reads (a correct heap scan) and refused for writes — never a silently stale result.
db.execute('CREATE INDEX ON product (discount(price_cents, 10))');

// Call it by name from SQL, exactly like a built-in.
for (const row of db.query(
  'SELECT name, discount(price_cents, 15) AS sale, with_tax(price_cents) AS total FROM product ORDER BY id'
)) {
  // Mug -> 1063 / 1350, Notebook -> 340 / 432
  console.log(`${render(row[0])} -> ${render(row[1])} / ${render(row[2])}`);
}

db.close();
