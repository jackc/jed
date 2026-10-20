// Research only: a bounded emitter for hand-authored, integer-typed RExpr fixtures.
// No engine integration or production admission/type proof is claimed.
import assert from 'node:assert/strict';
import { performance } from 'node:perf_hooks';
import { evalExpr, evalArith, operatorCost } from '../../impl/ts/src/eval.ts';
import { Meter, LifetimeBudget } from '../../impl/ts/src/cost.ts';
import { intValue } from '../../impl/ts/src/value.ts';
import { resolveUnfetchedSelf } from '../../impl/ts/src/format.ts';

const col = index => ({ kind: 'column', index });
const lit = value => ({ kind: 'constInt', value });
const arith = (op, lhs, rhs, result = 'i64') => ({ kind: 'arith', op, result, lhs, rhs });
const cmp = (lhs, rhs) => ({ kind: 'compare', op: 'gt', lhs, rhs, collation: null });
const h = {
  intValue,
  column: (row, i) => row[i].kind === 'unfetched' ? resolveUnfetchedSelf(row[i].ref) : row[i],
  arith: (op, ty, a, b) => a.kind === 'null' || b.kind === 'null'
    ? { kind: 'null' } : evalArith(op, a.int, b.int, ty),
  compare: (a, b) => a.kind === 'null' || b.kind === 'null'
    ? { kind: 'null' } : { kind: 'bool', value: a.int > b.int },
};

function compile(root, id = 0) {
  const pool = [], lines = [];
  let nodes = 0;
  const data = v => { pool.push(v); return `p[${pool.length - 1}]`; };
  function emit(e, depth = 0) {
    if (++nodes > 192 || depth > 32) throw Error('probe compiler bound');
    const v = `v${nodes}`;
    lines.push('m.guard();');
    if (e.kind === 'column' || e.kind === 'param') {
      assert(Number.isSafeInteger(e.index) && e.index >= 0 && e.index < 256);
      lines.push(`const ${v} = ${e.kind === 'column' ? `h.column(row,${e.index})` : `env.params[${e.index}]`};`);
    } else if (e.kind === 'constInt') {
      lines.push(`const ${v} = h.intValue(${data(e.value)});`);
    } else if (e.kind === 'arith' || e.kind === 'compare') {
      assert(['add', 'sub', 'mul', 'div', 'mod', 'gt'].includes(e.op));
      if (e.kind === 'arith') assert(['i16', 'i32', 'i64'].includes(e.result));
      else assert(e.op === 'gt' && e.collation === null);
      lines.push(`m.charge(${data(operatorCost(e.op))});`);
      const a = emit(e.lhs, depth + 1), b = emit(e.rhs, depth + 1);
      // Integer comparisons retain even zero-charge checkpoints from evalCompareNode.
      if (e.kind === 'compare') lines.push('m.charge(0n); m.guard(); m.charge(0n); m.guard();');
      lines.push(`const ${v} = ${e.kind === 'arith'
        ? `h.arith(${data(e.op)},${data(e.result)},${a},${b})` : `h.compare(${a},${b})`};`);
    } else throw Error('unsupported fixture');
    return v;
  }
  const value = emit(root);
  assert(Number.isSafeInteger(id) && id >= 0);
  const source = `"use strict"; return function jedCompiled${id}(row,env,m) {\n${lines.join('\n')}\nreturn ${value};\n}`;
  assert(Buffer.byteLength(source) <= 32768);
  return { fn: new Function('h', 'p', source)(h, pool), sourceBytes: Buffer.byteLength(source) };
}

// CSP-compatible closure specialization: removes the big node-kind dispatch,
// but retains one indirect closure call per child. Same admitted fixture surface.
function closures(e) {
  if (e.kind === 'column') return (row, env, m) => { m.guard(); return h.column(row, e.index); };
  if (e.kind === 'param') return (row, env, m) => { m.guard(); return env.params[e.index]; };
  if (e.kind === 'constInt') return (row, env, m) => { m.guard(); return intValue(e.value); };
  const left = closures(e.lhs), right = closures(e.rhs), cost = operatorCost(e.op);
  return (row, env, m) => {
    m.guard(); m.charge(cost);
    const a = left(row, env, m), b = right(row, env, m);
    if (e.kind === 'arith') return h.arith(e.op, e.result, a, b);
    m.charge(0n); m.guard(); m.charge(0n); m.guard();
    return h.compare(a, b);
  };
}

const expression = arith('add', arith('mul', col(0), { kind: 'param', index: 0 }), arith('sub', col(1), lit(7n)));
const jobs = [
  ['add', arith('add', col(0), col(1))],
  ['arithmetic', expression],
  ['predicate', cmp(expression, arith('mod', col(0), lit(11n)))],
];
const env = { params: [intValue(3n)] };
const rows = Array.from({ length: 1024 }, (_, i) => [intValue(BigInt(i)), intValue(BigInt(i * 3))]);
const encode = value => JSON.stringify(value, (_, v) => typeof v === 'bigint' ? `${v}n` : v);
let validationCount = 0;
function outcome(fn, row, limit, life) {
  const budget = new LifetimeBudget(BigInt(life));
  const m = new Meter(BigInt(limit), budget);
  let result;
  try { result = { value: fn(row, env, m) }; }
  catch (e) { result = { error: e.message, state: e.state, code: e.code }; }
  return encode({ ...result, cost: m.accrued, lifetime: budget.total });
}
function validate(e, fns) {
  const edgeRows = [...rows.slice(0, 8), [{ kind: 'null' }, intValue(3n)],
    [intValue(2n ** 63n - 1n), intValue(1n)], [intValue(-(2n ** 63n)), intValue(-1n)]];
  for (const param of [0n, 3n, -1n]) {
    env.params = [intValue(param)];
    for (const row of edgeRows) for (let limit = 0; limit <= 8; limit++) {
      for (const [statement, lifetime] of [[limit, 0], [0, limit]]) {
        const expected = outcome((r, env, m) => evalExpr(e, r, env, m), row, statement, lifetime);
        for (const fn of fns) {
          assert.equal(outcome(fn, row, statement, lifetime), expected);
          validationCount++;
        }
      }
    }
  }
  env.params = [intValue(3n)];
}
// Eager NULL operands, division by zero, narrow-width overflow, and i64 / -1.
for (const e of [arith('add', col(0), arith('div', lit(1n), lit(0n))),
  arith('add', col(0), lit(1n), 'i16'), arith('div', col(0), col(1))]) {
  validate(e, [compile(e).fn, closures(e)]);
}

let sink = 0;
function batch(fn, count) {
  const m = new Meter();
  let check = 0;
  for (let i = 0; i < count; i++) {
    const v = fn(rows[i & 1023], env, m);
    check += v.kind === 'bool' ? Number(v.value) : Number(v.int);
  }
  sink ^= check;
  return { check, cost: String(m.accrued) };
}
const observations = [];
for (const [id, [name, e]] of jobs.entries()) {
  const start = performance.now(), compiled = compile(e, id);
  const firstCompileUs = (performance.now() - start) * 1000;
  const fns = [(row, env, m) => evalExpr(e, row, env, m), closures(e), compiled.fn];
  validate(e, fns.slice(1));
  const firstStart = performance.now(); batch(compiled.fn, 1);
  const postValidationSingleCallUs = (performance.now() - firstStart) * 1000;
  const count = 100000;
  for (const fn of fns) batch(fn, 50000);
  const samples = [];
  for (let sample = 0; sample < 5; sample++) {
    for (const mode of sample % 2 ? [2, 1, 0] : [0, 1, 2]) {
      const t = performance.now(), answer = batch(fns[mode], count);
      samples.push({ mode: ['tree', 'closures', 'generated'][mode], ns: (performance.now() - t) * 1e6 / count, ...answer });
    }
  }
  assert(samples.every(s => s.check === samples[0].check && s.cost === samples[0].cost));
  validate(e, fns.slice(1));
  observations.push({ name, firstCompileUs, postValidationSingleCallUs, sourceBytes: compiled.sourceBytes, samples });
}
console.log(JSON.stringify({ node: process.version, v8: process.versions.v8, execArgv: process.execArgv,
  warmup: 50000, iterations: 100000, validationCount, observations, sink }));
