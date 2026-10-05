#!/usr/bin/env python3
"""Sums the fast-layer batch digests in results/ into one markdown table set (stdout)."""
import glob
import json
import sys

USDC = 1_000_000
files = sorted(glob.glob("results/mc_*.json"))
if len(sys.argv) > 1:
    files = sys.argv[1:]
tot = {}
inv = {}
by_err = {}
by_kind_a = {}
by_kind_ok = {}
extremes = {}
rows = []
for f in files:
    d = json.load(open(f))
    rows.append(d)
    for k in ("seeds", "ticks", "transitions_attempted", "transitions_accepted", "quotes_evaluated", "closes", "clamped_closes",
              "early_closes", "settlements", "settle_mismatches", "near_maturity_liquidations", "seeds_tvl_hit_zero",
              "seeds_drawdown_ge_10pct", "seeds_drawdown_ge_50pct", "stale_quote_rejections", "withdraw_cap_rejections",
              "gains_paid_units", "losses_booked_units", "fees_total_units", "elapsed_secs",
              "lp_absorbed_shortfall_closes", "lp_absorbed_shortfall_units", "trader_clamped_gain_closes",
              "trader_clamped_gain_units", "dead_capital_captures", "dead_capital_units"):
        tot[k] = tot.get(k, 0) + d.get(k, 0)
    for k, v in d["invariants"].items():
        p, fl = inv.get(k, (0, 0))
        inv[k] = (p + v["pass"], fl + v["fail"])
    for k, v in d["by_error"].items():
        by_err[k] = by_err.get(k, 0) + v
    for k, v in d["by_kind_attempted"].items():
        by_kind_a[k] = by_kind_a.get(k, 0) + v
    for k, v in d["by_kind_accepted"].items():
        by_kind_ok[k] = by_kind_ok.get(k, 0) + v
    for k, seedk, better in (("min_share_price_e6", "min_share_price_seed", min), ("max_collateral_shortfall_units", "max_collateral_shortfall_seed", max),
                             ("max_single_gain_bp_of_tvl", "max_single_gain_seed", max), ("settle_transfer_abs_max_units", "settle_transfer_abs_max_seed", max),
                             ("early_close_transfer_abs_max_units", "early_close_transfer_abs_max_seed", max)):
        cur = extremes.get(k)
        if cur is None or better(cur[0], d[k]) == d[k]:
            extremes[k] = (d[k], d[seedk], f)

print("| Batch file | Seeds | Elapsed s | Transitions attempted | Accepted | Quotes |")
print("|---|---:|---:|---:|---:|---:|")
for d in rows:
    print(f"| mc_{d['seed_start']}_{d['seed_end_exclusive']}_{d['horizon_days']}d | {d['seeds']} | {d['elapsed_secs']:.0f} | {d['transitions_attempted']:,} | {d['transitions_accepted']:,} | {d['quotes_evaluated']:,} |")
print(f"| **Total** | {tot['seeds']:,} | {tot['elapsed_secs']:.0f} | {tot['transitions_attempted']:,} | {tot['transitions_accepted']:,} | {tot['quotes_evaluated']:,} |")
print()
print("| Invariant | Pass | Fail |")
print("|---|---:|---:|")
for k in sorted(inv, key=lambda s: int(s.split()[0][1:])):
    p, fl = inv[k]
    print(f"| {k} | {p:,} | {fl:,} |")
print()
print("| Instruction | Attempted | Accepted |")
print("|---|---:|---:|")
for k in sorted(by_kind_a):
    print(f"| {k} | {by_kind_a[k]:,} | {by_kind_ok.get(k, 0):,} |")
print()
print("| Rejection (instruction:error) | Count |")
print("|---|---:|")
for k in sorted(by_err):
    print(f"| {k} | {by_err[k]:,} |")
print()
print("| Metric | Value |")
print("|---|---:|")
print(f"| Ticks (hours) simulated | {tot['ticks']:,} |")
print(f"| Closes / clamped at collateral | {tot['closes']:,} / {tot['clamped_closes']:,} |")
print(f"| Settlements / booked differs from fair | {tot['settlements']:,} / {tot['settle_mismatches']:,} |")
print(f"| Early closes | {tot['early_closes']:,} |")
print(f"| Gains paid from LP capital (USDC) | {tot['gains_paid_units'] / USDC:,.0f} |")
print(f"| Losses booked to LP capital (USDC) | {tot['losses_booked_units'] / USDC:,.0f} |")
print(f"| Fees (USDC) | {tot['fees_total_units'] / USDC:,.0f} |")
print(f"| Seeds where tvl hit zero with shares outstanding | {tot['seeds_tvl_hit_zero']} |")
print(f"| Seeds with drawdown >= 10 pct / >= 50 pct | {tot['seeds_drawdown_ge_10pct']} / {tot['seeds_drawdown_ge_50pct']} |")
print(f"| Stale-benchmark rejections / withdrawals blocked by caps | {tot['stale_quote_rejections']:,} / {tot['withdraw_cap_rejections']:,} |")
if tot.get("lp_absorbed_shortfall_closes"):
    print(f"| Clamped on trader loss (LP absorbs excess): count / USDC | {tot['lp_absorbed_shortfall_closes']:,} / {tot['lp_absorbed_shortfall_units'] / USDC:,.0f} |")
    print(f"| Clamped on trader gain (trader forgoes excess): count / USDC | {tot['trader_clamped_gain_closes']:,} / {tot['trader_clamped_gain_units'] / USDC:,.0f} |")
    print(f"| Deposits capturing dead capital: count / USDC | {tot['dead_capital_captures']:,} / {tot['dead_capital_units'] / USDC:,.0f} |")
print()
print("| Extreme | Value | Seed | Batch |")
print("|---|---:|---:|---|")
for k, (v, s, f) in extremes.items():
    print(f"| {k} | {v:,} | {s} | {f.split('/')[-1]} |")
