// discount.ts — a trusted Node worker extension module. It exports configure;
// the application passes this module's file URL as the extensionModule option.
import { intValue } from 'jed-ts';
import type { EnvironmentBuilder } from 'jed-ts/internal';

export function configure(builder: EnvironmentBuilder): void {
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
}

// In a separate application module:
// const db = await createDatabase({
//   path: 'shop.jed',
//   extensionModule: new URL('./discount.ts', import.meta.url).href
// });
// await db.get('SELECT discount(1250, 15) AS sale'); // { sale: 1063n }
// await db.close();
