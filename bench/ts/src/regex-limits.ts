// Precompiled inputs and meter modes mirror the Rust and Go regex-limits benchmark.
import { withCancellationScope } from "../../../impl/ts/src/cancel.ts";
import { LifetimeBudget, Meter } from "../../../impl/ts/src/cost.ts";
import { compileRegex, regexIsMatch } from "../../../impl/ts/src/regex.ts";

const budget = 1_000_000_000n;
const flag = new Int32Array(new SharedArrayBuffer(4));
for (const [name, pattern, subject, hit, cost, iterations] of [
  ["hit", "(a)".repeat(12), "a".repeat(12), true, 311n, 200000],
  ["overlap", "(a)".repeat(11) + "(b)", "a".repeat(2048), false, 79712n, 1000],
] as const) {
  const program = compileRegex(pattern);
  const input = Array.from(subject, (ch) => ch.codePointAt(0) as number);
  for (const [mode, statement, lifetime, cancel] of [
    ["unlimited", 0n, 0n, false],
    ["statement", budget, 0n, false],
    ["lifetime", 0n, budget, false],
    ["both", budget, budget, false],
    ["both+cancel", budget, budget, true],
  ] as const) {
    const samples: number[] = [];
    for (let round = 0; round < 4; round++) {
      const life = new LifetimeBudget(lifetime);
      const m = new Meter(statement, life);
      withCancellationScope(cancel ? () => Atomics.load(flag, 0) !== 0 : undefined, () => {
        const start = process.hrtime.bigint();
        for (let i = 0; i < iterations; i++) {
          if (regexIsMatch(program, input, m) !== hit) throw Error(name);
        }
        const ns = Number(process.hrtime.bigint() - start) / iterations;
        if (round > 0) samples.push(ns);
      });
      if (m.accrued !== cost * BigInt(iterations) || life.total !== m.accrued)
        throw Error("meter drift");
    }
    console.log("TypeScript", name, mode, samples.sort((a, b) => a - b)[1].toFixed(1), "ns/match");
  }
}
