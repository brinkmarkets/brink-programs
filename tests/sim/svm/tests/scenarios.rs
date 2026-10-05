//! Differential replay: the shared scenario generator drives the pool-state model and the compiled programs on
//! LiteSVM with the same action stream. After every action the outcome (accepted, or rejected with the same error
//! name in the logs) and the full account state (pool, global, benchmark, vault, fee vault, share supply, LP
//! holdings, swap accounts) must agree, and the harness invariants are re-asserted from on-chain state.
//!
//! Environment:
//!   BRINK_SIM_SEEDS          comma-separated seeds (default "1,2,3,4,5,6")
//!   BRINK_SIM_HORIZON_DAYS   days per seed (default 60)
//!   BRINK_SIM_SO_DIR         directory with swap_amm.so, brink_index.so, brink_timelock.so (default programs/target/deploy)

use std::fmt::Write as _;

use brink_sim_core::invariants::{apply_and_check, Inv};
use brink_sim_core::model::Model;
use brink_sim_core::scenario::Action;
use brink_sim_core::{Scenario, ScenarioParams, SeedStats};
use brink_sim_svm::*;
use solana_signer::Signer;

fn seeds() -> Vec<u64> {
    std::env::var("BRINK_SIM_SEEDS").ok().map(|s| s.split(',').filter_map(|x| x.trim().parse().ok()).collect()).unwrap_or_else(|| vec![1, 2, 3, 4, 5, 6])
}
fn horizon_days() -> u64 {
    std::env::var("BRINK_SIM_HORIZON_DAYS").ok().and_then(|s| s.parse().ok()).unwrap_or(60)
}

struct Diff {
    seed: u64,
    chain_checks: u64,
    mismatches: Vec<String>,
}

fn compare(env: &Env, m: &Model, seed: u64, tick: u64, idx: u64, a: &Action, d: &mut Diff) {
    let p = env.pool();
    let g = env.global();
    let b = env.bench();
    let mut e = String::new();
    macro_rules! eq {
        ($name:expr, $chain:expr, $model:expr) => {
            if $chain != $model {
                let _ = writeln!(e, "  {}: chain {:?} model {:?}", $name, $chain, $model);
            }
        };
    }
    eq!("pool.tvl", p.tvl, m.pool.tvl);
    eq!("pool.collateral_held", p.collateral_held, m.pool.collateral_held);
    eq!("pool.util_pay_bp", p.util_pay_bp, m.pool.util_pay_bp);
    eq!("pool.util_rec_bp", p.util_rec_bp, m.pool.util_rec_bp);
    eq!("pool.open_pay_notional", p.open_pay_notional, m.pool.open_pay_notional);
    eq!("pool.open_rec_notional", p.open_rec_notional, m.pool.open_rec_notional);
    eq!("pool.open_swaps", p.open_swaps, m.pool.open_swaps);
    eq!("pool.fees_lifetime", p.fees_lifetime, m.pool.fees_lifetime);
    eq!("pool.event_seq", p.event_seq, m.pool.event_seq);
    eq!("pool.params", p.params, to_params(&m.pool.params));
    eq!("pool.pending_effective_slot", p.pending_effective_slot, m.pool.pending_effective_slot);
    if m.pool.pending_effective_slot != 0 {
        eq!("pool.pending_params", p.pending_params, to_params(&m.pool.pending_params));
    }
    eq!("global.mode", to_mode(m.global.mode), g.mode);
    eq!("global.buyback_accrued", g.buyback_accrued, m.global.buyback_accrued);
    eq!("global.treasury_accrued", g.treasury_accrued, m.global.treasury_accrued);
    eq!("global.buyback_lifetime", g.buyback_lifetime, m.global.buyback_lifetime);
    eq!("global.treasury_lifetime", g.treasury_lifetime, m.global.treasury_lifetime);
    eq!("bench.value_bp", b.value_bp, m.bench.value_bp);
    eq!("bench.ema_bp", b.ema_bp, m.bench.ema_bp);
    eq!("bench.accrual_e18", b.accrual_e18, m.bench.accrual_e18);
    eq!("bench.slot", b.slot, m.bench.slot);
    eq!("bench.unix_ts", b.unix_ts, m.bench.unix_ts);
    eq!("bench.published", b.published, m.bench.published);
    eq!("vault.amount", env.token_amount(&env.vault), m.pool.vault);
    eq!("fee_vault.amount", env.token_amount(&env.fee_vault), m.global.fee_vault);
    eq!("share_mint.supply", env.mint_supply(&env.share_mint), m.pool.share_supply);
    for (i, lp) in env.lps.iter().enumerate() {
        eq!(format!("lp{i}.shares"), env.token_amount(&ata(&lp.pubkey(), &env.share_mint)), m.lp_shares[i]);
    }
    let t = env.treasury_owner.pubkey();
    eq!("treasury.amount", env.token_amount(&ata(&t, &USDC_DEVNET)), m.global.treasury_balance);
    eq!("buyback.amount", env.token_amount(&ata(&buyback_owner(&t), &USDC_DEVNET)), m.global.buyback_balance);
    // open swaps
    for s in &m.swaps {
        let k = env.swap_pda(&env.traders[usize::from(s.trader)].pubkey(), s.id);
        match env.try_acct::<Swap>("Swap", &k) {
            None => {
                let _ = writeln!(e, "  swap {} missing on chain", s.id);
            }
            Some(c) => {
                eq!(format!("swap{}.fixed_bp", s.id), c.fixed_bp, s.fixed_bp);
                eq!(format!("swap{}.collateral", s.id), c.collateral, s.collateral);
                eq!(format!("swap{}.notional", s.id), c.notional, s.notional);
                eq!(format!("swap{}.matures_ts", s.id), c.matures_ts, s.matures_ts);
                eq!(format!("swap{}.index_accrual_start", s.id), c.index_accrual_start, s.index_accrual_start);
                eq!(format!("swap{}.leg", s.id), c.leg, to_leg(s.leg));
                eq!(format!("swap{}.state", s.id), c.state, SwapState::Open);
            }
        }
    }
    // invariants from on-chain state alone (I2, I3, I4, I6, I7)
    let vault = env.token_amount(&env.vault);
    assert!(vault >= p.tvl + p.collateral_held, "seed {seed} tick {tick} #{idx}: I2 on chain: vault {vault} < tvl {} + collateral {}", p.tvl, p.collateral_held);
    if p.tvl > 0 {
        assert!(p.util_pay_bp <= 4_800 && p.util_rec_bp <= 4_800 && u32::from(p.util_pay_bp) + u32::from(p.util_rec_bp) <= 8_000, "seed {seed} tick {tick} #{idx}: I3 on chain: {} {}", p.util_pay_bp, p.util_rec_bp);
        assert_eq!(u128::from(p.util_pay_bp), u128::from(p.open_pay_notional) * 10_000 / u128::from(p.tvl), "seed {seed} tick {tick} #{idx}: I4 on chain");
    }
    assert_eq!(env.token_amount(&env.fee_vault), g.buyback_accrued + g.treasury_accrued, "seed {seed} tick {tick} #{idx}: I6 on chain");
    assert_eq!(g.buyback_lifetime + g.treasury_lifetime, p.fees_lifetime, "seed {seed} tick {tick} #{idx}: I6 lifetimes on chain");
    let holdings: u64 = env.lps.iter().map(|lp| env.token_amount(&ata(&lp.pubkey(), &env.share_mint))).sum();
    assert_eq!(env.mint_supply(&env.share_mint), holdings, "seed {seed} tick {tick} #{idx}: I7 on chain");
    d.chain_checks += 5;
    if !e.is_empty() {
        d.mismatches.push(format!("seed {seed} tick {tick} #{idx} after {a:?}:\n{e}"));
    }
}

fn run_one(seed: u64, horizon_days: u64) -> (SeedStats, Diff, Vec<(String, bool, u64)>) {
    let params = ScenarioParams::sample(seed, horizon_days);
    let setup = params.setup();
    let mut env = Env::new(&setup);
    let mut sc = Scenario::new(params.clone());
    let mut m = Model::new(&setup);
    let mut st = SeedStats::new(seed);
    let mut d = Diff { seed, chain_checks: 0, mismatches: Vec::new() };
    let mut idx = 0u64;
    let step = |a: &Action, tick: u64, idx: u64, m: &mut Model, env: &mut Env, st: &mut SeedStats, d: &mut Diff| {
        let model_out = apply_and_check(m, a, st, tick, idx);
        let chain_out = env.apply(a);
        match (&model_out, &chain_out) {
            (Ok(_), Ok(_)) => {}
            (Err(me), Err(ce)) => {
                if !ce.contains(me.name()) {
                    d.mismatches.push(format!("seed {seed} tick {tick} #{idx} {a:?}: model error {me:?} ({}) but chain logs:\n{ce}", me.name()));
                }
            }
            (Ok(eff), Err(ce)) => d.mismatches.push(format!("seed {seed} tick {tick} #{idx} {a:?}: model accepted ({eff:?}) but chain rejected:\n{ce}")),
            (Err(me), Ok(logs)) => d.mismatches.push(format!("seed {seed} tick {tick} #{idx} {a:?}: model rejected {me:?} but chain accepted:\n{}", logs.join("\n"))),
        }
        if !matches!(a, Action::Warp { .. }) {
            compare(env, m, seed, tick, idx, a, d);
        }
        assert!(d.mismatches.len() < 5, "too many mismatches for seed {seed}:\n{}", d.mismatches.join("\n"));
    };
    for a in sc.bootstrap() {
        step(&a, 0, idx, &mut m, &mut env, &mut st, &mut d);
        idx += 1;
    }
    let ticks = sc.horizon_ticks();
    for tick in 1..=ticks {
        sc.begin_tick(&m);
        while let Some(a) = sc.next_action(&m) {
            step(&a, tick, idx, &mut m, &mut env, &mut st, &mut d);
            idx += 1;
        }
    }
    (st, d, env.cu_log)
}

#[test]
fn differential_replay_model_vs_programs() {
    let horizon = horizon_days();
    let mut report = String::new();
    let mut all_cu: Vec<(String, bool, u64)> = Vec::new();
    let mut any_mismatch = false;
    let _ = writeln!(report, "# Slow layer: differential replay, horizon {horizon} days\n");
    let _ = writeln!(report, "| Seed | Transitions attempted | Accepted | Chain state checks | Mismatches | Invariant failures (model) |\n|---:|---:|---:|---:|---:|---|");
    for seed in seeds() {
        let (st, d, cu) = run_one(seed, horizon);
        let fails: Vec<String> = Inv::ALL.iter().filter(|i| st.inv_fail.get(i).copied().unwrap_or(0) > 0).map(|i| format!("{} x{}", i.id(), st.inv_fail[i])).collect();
        let _ = writeln!(report, "| {seed} | {} | {} | {} | {} | {} |", st.attempted, st.accepted, d.chain_checks, d.mismatches.len(), if fails.is_empty() { "none".to_string() } else { fails.join(", ") });
        if !d.mismatches.is_empty() {
            any_mismatch = true;
            let _ = writeln!(report, "\nMismatches for seed {seed}:\n");
            for mm in &d.mismatches {
                let _ = writeln!(report, "```\n{mm}\n```");
            }
        }
        for v in &st.violations {
            let _ = writeln!(report, "\n- model invariant {} failed at tick {} action {} `{}`: {}", v.inv.id(), v.tick, v.action_index, v.action, v.detail);
        }
        all_cu.extend(cu);
    }
    let _ = writeln!(report, "\n## Compute units (scenario replay)\n\n{}", cu_table(&all_cu));
    let dir = results_dir();
    let _ = std::fs::create_dir_all(&dir);
    std::fs::write(dir.join("svm_scenarios.md"), &report).expect("write report");
    println!("{report}");
    assert!(!any_mismatch, "model and programs diverged; see results/svm_scenarios.md");
}
