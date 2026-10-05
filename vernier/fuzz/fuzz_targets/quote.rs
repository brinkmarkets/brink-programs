#![no_main]
//! Coverage-guided fuzz of the pure library: it must never panic and the invariants must hold for any input.
use arbitrary::Arbitrary;
use vernier::{demand_bp, quote, Leg, Pool, Tenor, DEFAULT_PARAMS};
use libfuzzer_sys::fuzz_target;

#[derive(Arbitrary, Debug)]
struct Input { tvl: u64, pay: u16, rec: u16, spot: u16, ema: u16, tenor: u8, leg: bool, notional: u64 }

fuzz_target!(|i: Input| {
    let pool = Pool { tvl: i.tvl, util_pay_bp: i.pay, util_rec_bp: i.rec };
    let tenor = match i.tenor % 4 { 0 => Tenor::D28, 1 => Tenor::D60, 2 => Tenor::D90, _ => Tenor::D180 };
    let leg = if i.leg { Leg::Pay } else { Leg::Receive };
    if let Ok((d, before, after, reduces)) = demand_bp(&pool, leg, i.notional, &DEFAULT_PARAMS) {
        assert!(d <= DEFAULT_PARAMS.demand_cap_bp);
        if after.unsigned_abs() <= before.unsigned_abs() { assert!(reduces && d == 0); }
    }
    if let (Ok(p), Ok(r)) = (quote(i.spot, i.ema, tenor, Leg::Pay, i.notional, &pool, &DEFAULT_PARAMS), quote(i.spot, i.ema, tenor, Leg::Receive, i.notional, &pool, &DEFAULT_PARAMS)) {
        assert!(p.fixed_bp >= r.fixed_bp);
    }
});
