// ../../../packages/vernier/src/index.ts
var TENOR_INDEX = { 28: 0, 60: 1, 90: 2, 180: 3 };
var DEFAULT_PARAMS = {
  modelPayBp: [11, 22, 31, 54],
  modelRecBp: [9, 12, 14, 19],
  termBp: [3, 5, 7, 12],
  demandKBp: 45,
  demandCapBp: 60
};
var assertInt = (v, name) => {
  if (!Number.isInteger(v)) throw new RangeError(`${name} must be an integer, got ${v}`);
};
function demandBp(pool, leg, notional, p = DEFAULT_PARAMS) {
  assertInt(notional, "notional");
  assertInt(pool.tvl, "tvl");
  if (pool.tvl <= 0) throw new RangeError("tvl must be positive");
  const before = pool.utilPayBp - pool.utilRecBp;
  const dBp = Math.floor(notional * 1e4 / pool.tvl);
  const after = leg === "pay" ? before + dBp : before - dBp;
  const reduces = Math.abs(after) <= Math.abs(before);
  const raw = Math.floor((p.demandKBp * Math.abs(after) + 5e3) / 1e4);
  const demand = reduces ? 0 : Math.min(p.demandCapBp, raw);
  return { demandBp: demand, imbalanceBeforeBp: before, imbalanceAfterBp: after, reducesImbalance: reduces };
}
function quoteBp(spotBp, emaBp, tenor, leg, notional, pool, p = DEFAULT_PARAMS) {
  assertInt(spotBp, "spotBp");
  assertInt(emaBp, "emaBp");
  const ti = TENOR_INDEX[tenor];
  const d = demandBp(pool, leg, notional, p);
  const term = p.termBp[ti];
  if (leg === "pay") {
    const reference2 = Math.max(spotBp, emaBp), model2 = p.modelPayBp[ti];
    return { ...d, referenceBp: reference2, modelBp: model2, demandBp: d.demandBp, termBp: term, fixedBp: reference2 + model2 + d.demandBp + term };
  }
  const reference = Math.min(spotBp, emaBp), model = p.modelRecBp[ti];
  return { ...d, referenceBp: reference, modelBp: -model, termBp: -term, fixedBp: reference - model - d.demandBp - term, demandBp: d.demandBp === 0 ? 0 : -d.demandBp };
}
var COLLATERAL_BP = { 28: 120, 60: 230, 90: 330, 180: 600 };
function collateralUsdc(notional, tenor) {
  assertInt(notional, "notional");
  return Math.ceil(notional * COLLATERAL_BP[tenor] / 1e4);
}
var OPENING_FEE_BP = 5;
var INCOME_FEE_PCT = 10;
var LP_EXIT_FEE_BP = 50;
var CAP_LEG_BP = 4800;
var CAP_TOTAL_BP = 8e3;
function legCapacity(pool, leg) {
  const used = leg === "pay" ? pool.utilPayBp : pool.utilRecBp;
  const legRoom = Math.max(0, CAP_LEG_BP - used);
  const totalRoom = Math.max(0, CAP_TOTAL_BP - pool.utilPayBp - pool.utilRecBp);
  return Math.floor(Math.min(legRoom, totalRoom) * pool.tvl / 1e4);
}
export {
  CAP_LEG_BP,
  CAP_TOTAL_BP,
  COLLATERAL_BP,
  DEFAULT_PARAMS,
  INCOME_FEE_PCT,
  LP_EXIT_FEE_BP,
  OPENING_FEE_BP,
  TENOR_INDEX,
  collateralUsdc,
  demandBp,
  legCapacity,
  quoteBp
};
