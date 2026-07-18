import { createDatabase, ExtensionRegistry, intValue, render } from 'jed-ts';

// A host registers its own SCALAR FUNCTIONS over the built-in types. Build a registry, add
// functions, and hand it to createDatabase/openDatabase — the engine freezes it for the handle's
// lifetime and shares it into every session. It is a handle setting, never written to the file (a
// reopening host brings its own).
const registry = new ExtensionRegistry();

// discount(cents, pct) -> the price after a whole-percent discount. STRICT — a NULL argument
// short-circuits to NULL before the kernel runs, so the closure never sees one — and reached by an
// EXACT (i64, i64) signature (no implicit promotion; a built-in of the same signature would win).
// `cost: 2n` is charged once per call and gated against a session's maxCost, so the function stays
// inside the untrusted-query bound.
registry.registerFunction({
  name: 'discount',
  argTypes: ['i64', 'i64'],
  result: 'i64',
  volatility: 'immutable', // same inputs ⇒ same output
  crossCore: true, // results are byte-identical on every core
  cost: 2n,
  kernel: (args) => {
    // strict + resolved (i64, i64), so both args are non-null ints
    const cents = args[0] as { kind: 'int'; int: bigint };
    const pct = args[1] as { kind: 'int'; int: bigint };
    return intValue(cents.int - (cents.int * pct.int) / 100n);
  }
});

const db = createDatabase({ extensions: registry });

db.execute('CREATE TABLE product (id i32 PRIMARY KEY, name text, price_cents i64)');
db.execute("INSERT INTO product VALUES (1, 'Mug', 1250), (2, 'Notebook', 400)");

// Call it by name from SQL, exactly like a built-in.
for (const row of db.query(
  'SELECT name, discount(price_cents, 15) AS sale FROM product ORDER BY id'
)) {
  console.log(`${render(row[0])} -> ${render(row[1])}`); // Mug -> 1063, Notebook -> 340
}

db.close();
