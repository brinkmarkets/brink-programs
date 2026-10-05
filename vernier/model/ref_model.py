#!/usr/bin/env python3
"""Exact-arithmetic reference model of the Brink quote maths (Python Fractions), written from
docs/audit/maths/SPEC.md, and a stream checker for the Rust `diff --emit` output.

Usage:
    ./target/release/diff --cases N --emit N | python3 ref_model.py
Reads lines of the form `inputs | implementation outputs`, recomputes every output exactly with Fractions
plus named rounding steps, and reports mismatch counts and up to 20 examples.
"""
import sys
from fractions import Fraction as F
from math import floor, ceil

BP = 10_000
I32_MAX = 2**31 - 1
I32_MIN = -(2**31)
U64_MAX = 2**64 - 1
CAP_LEG = 4_800
CAP_TOTAL = 8_000


def round_half_up(x: F) -> int:
    """floor(x + 1/2) for x >= 0."""
    assert x >= 0
    return floor(x + F(1, 2))


def demand(tvl, up, ur, pay, notional, k, cap):
    """SPEC section 3. Returns (status, demand, before, after, reduces). status 0 ok, 1 EmptyPool,
    2 MalformedUtilisation, 3 Overflow."""
    if tvl <= 0:
        return (1, 0, 0, 0, False)
    if up > BP or ur > BP:
        return (2, 0, 0, 0, False)
    before = up - ur
    d = floor(F(notional * BP, tvl))
    if d > I32_MAX:
        return (3, 0, 0, 0, False)
    after = before + d if pay else before - d
    if after > I32_MAX or after < I32_MIN:
        return (3, 0, 0, 0, False)
    reduces = abs(after) <= abs(before)
    dem = 0 if reduces else min(cap, round_half_up(F(k * abs(after), BP)))
    return (0, dem, before, after, reduces)


def quote(spot, ema, t, pay, notional, tvl, up, ur, p):
    status, dem, before, after, reduces = demand(tvl, up, ur, pay, notional, p["k"], p["cap"])
    if status:
        return (status, 0, 0, 0, 0, 0, 0, 0, 0)
    if pay:
        ref, model, s = max(spot, ema), p["mp"][t], 1
    else:
        ref, model, s = min(spot, ema), p["mr"][t], -1
    term = p["term"][t]
    fixed = ref + s * (model + dem + term)
    return (0, fixed, ref, s * model, s * dem, s * term, before, after, int(reduces))


def collateral(notional, t, p):
    return min(ceil(F(notional * p["coll"][t], BP)), U64_MAX)


def leg_capacity(tvl, up, ur, pay):
    used = up if pay else ur
    room = min(max(0, CAP_LEG - used), max(0, CAP_TOTAL - up - ur))
    return min(floor(F(room * tvl, BP)), U64_MAX)


def check_stream(lines):
    n = mism = 0
    shown = 0
    for line in lines:
        left, right = line.split("|")
        a = list(map(int, left.split()))
        got = list(map(int, right.split()))
        spot, ema, t, leg, notional, tvl, up, ur = a[:8]
        p = {"mp": a[8:12], "mr": a[12:16], "term": a[16:20], "k": a[20], "cap": a[21], "coll": a[22:26]}
        pay = leg == 0
        q = quote(spot, ema, t, pay, notional, tvl, up, ur, p)
        exp = list(q) + [collateral(notional, t, p), leg_capacity(tvl, up, ur, pay)]
        n += 1
        if exp != got:
            mism += 1
            if shown < 20:
                shown += 1
                print(f"MISMATCH line {n}: {line.strip()}\n  expected {exp}", file=sys.stderr)
    print(f"python_reference cases={n} mismatches={mism}")
    return mism


if __name__ == "__main__":
    sys.exit(1 if check_stream(sys.stdin) else 0)
