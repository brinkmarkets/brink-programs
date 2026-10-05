// Differential check of the TypeScript mirror (packages/vernier/src/index.ts) against the Rust `diff --emit` stream.
// Build step bundles the TS with esbuild (already a dependency of the web workspace) into .ts-dist/.
// Usage: ./target/release/diff --cases N --emit N | node ts_check.mjs
import { build } from '../../../node_modules/esbuild/lib/main.js';
import { createInterface } from 'node:readline';
import fs from 'node:fs';

fs.mkdirSync('.ts-dist', { recursive: true });
await build({
  entryPoints: ['../../../packages/vernier/src/index.ts'],
  bundle: true, platform: 'node', format: 'esm', outfile: '.ts-dist/vernier.mjs', logLevel: 'error',
});
const V = await import('./.ts-dist/vernier.mjs');
const TENOR_DAYS = [28, 60, 90, 180];
const P53 = 2 ** 53;

const counts = {
  cases: 0,
  compared: 0,                 // Rust ok, TS compared field by field
  exact: 0,
  mismatch_total: 0,
  mismatch_precision: 0,       // notional·10^4 or tvl beyond 2^53 (double precision)
  mismatch_other: 0,
  rust_error_ts_value: 0,      // Rust reported an error, TS returned a number
  rust_error_ts_throw: 0,
  collateral_mismatch: 0,
  capacity_mismatch: 0,
  default_params: 0,
};
const examples = [];
const rl = createInterface({ input: process.stdin, crlfDelay: Infinity });
for await (const line of rl) {
  const [left, right] = line.split('|');
  const a = left.trim().split(/\s+/).map(Number);
  const g = right.trim().split(/\s+/).map(Number);
  const [spot, ema, t, leg, notional, tvl, up, ur] = a;
  const params = { modelPayBp: a.slice(8, 12), modelRecBp: a.slice(12, 16), termBp: a.slice(16, 20), demandKBp: a[20], demandCapBp: a[21] };
  const coll = a.slice(22, 26);
  const isDefault = JSON.stringify(coll) === JSON.stringify([120, 230, 330, 600]);
  const pool = { tvl, utilPayBp: up, utilRecBp: ur };
  const legKind = leg === 0 ? 'pay' : 'receive';
  counts.cases++;
  if (isDefault) counts.default_params++;
  const [status, fixed, ref, model, demand, term, before, after, reduces, rcoll, rcap] = g;
  let q;
  try { q = V.quoteBp(spot, ema, TENOR_DAYS[t], legKind, notional, pool, params); }
  catch (e) { q = null; }
  const cap = V.legCapacity(pool, legKind);
  if (cap !== rcap) counts.capacity_mismatch++;
  if (isDefault) {
    const c = V.collateralUsdc(notional, TENOR_DAYS[t]);
    if (c !== rcoll) counts.collateral_mismatch++;
  }
  if (status !== 0) {
    if (q === null) counts.rust_error_ts_throw++; else counts.rust_error_ts_value++;
    continue;
  }
  counts.compared++;
  const same = q && q.fixedBp === fixed && q.referenceBp === ref && q.modelBp === model && q.demandBp === demand
    && q.termBp === term && q.imbalanceBeforeBp === before && q.imbalanceAfterBp === after && Number(q.reducesImbalance) === reduces;
  if (same) { counts.exact++; continue; }
  counts.mismatch_total++;
  const precision = notional * 10_000 > P53 || tvl > P53 || notional > P53;
  if (precision) counts.mismatch_precision++; else counts.mismatch_other++;
  if (examples.length < 20) examples.push({ line: line.trim(), ts: q, precision });
}
console.log(JSON.stringify(counts, null, 1));
for (const e of examples) console.error('MISMATCH', e.line, '\n  ts', JSON.stringify(e.ts), e.precision ? '(beyond 2^53)' : '(other)');
process.exit(counts.mismatch_other > 0 ? 1 : 0);
