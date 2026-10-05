//! Fast layer: Monte Carlo over seeds against the pool-state model.
//!
//! Subcommands:
//!   mc      --seed-start S --seeds N --horizon-days D --time-budget-secs T --threads K --out DIR
//!   replay  --seed S [--horizon-days D] [--quiet]
//!   sweep   --out DIR            grid over quote and pnl maths (vernier) with exhaustive small domains
//!
//! Output: a JSON summary (merged across seeds) and a markdown digest in the results directory. Only aggregates
//! and the first violations are written; the full path of any seed is reproducible with `replay`.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use brink_sim_core::invariants::Inv;
use brink_sim_core::model::USDC;
use brink_sim_core::{run_seed, Action, Outcome, ScenarioParams, SeedStats, Violation};

#[derive(Default, Clone)]
struct Totals {
    seeds: u64,
    ticks: u64,
    attempted: u64,
    accepted: u64,
    quotes: u64,
    by_kind_attempted: BTreeMap<&'static str, u64>,
    by_kind_accepted: BTreeMap<&'static str, u64>,
    by_error: BTreeMap<String, u64>,
    inv_pass: BTreeMap<Inv, u64>,
    inv_fail: BTreeMap<Inv, u64>,
    violations: Vec<Violation>,
    worst_drawdown: (u64, u64),          // (bp, seed)
    min_share_price: (u64, u64),         // (e6, seed)
    max_shortfall: (u128, u64, u64),     // (usdc units, bp of notional, seed)
    max_single_gain: (u64, u64),         // (bp of tvl, seed)
    max_util_total: (u32, u64),
    max_util_leg: (u16, u64),
    gains_paid: u128,
    losses_booked: u128,
    fees_total: u128,
    clamped_closes: u64,
    lp_short_n: u64,
    lp_short_sum: u128,
    tr_clamp_n: u64,
    tr_clamp_sum: u128,
    dead_n: u64,
    dead_sum: u128,
    closes: u64,
    early_closes: u64,
    early_transfer_sum: i128,
    early_transfer_abs_max: (u128, u64),
    near_maturity_liquidations: u64,
    settlements: u64,
    settle_mismatches: u64,
    settle_transfer_sum: i128,
    settle_transfer_abs_max: (u128, u64),
    seeds_tvl_zero: u64,
    seeds_with_drawdown_over_10pct: u64,
    seeds_with_drawdown_over_50pct: u64,
    stale_rejections: u64,
    withdraw_cap_rejections: u64,
    drawdown_hist: [u64; 8], // <1bp, <10, <100, <500, <1000, <2500, <5000, >=5000
}

impl Totals {
    fn merge(&mut self, s: &SeedStats, ticks: u64) {
        self.seeds += 1;
        self.ticks += ticks;
        self.attempted += s.attempted;
        self.accepted += s.accepted;
        self.quotes += s.quotes_evaluated;
        for (k, v) in &s.by_kind_attempted {
            *self.by_kind_attempted.entry(k).or_default() += v;
        }
        for (k, v) in &s.by_kind_accepted {
            *self.by_kind_accepted.entry(k).or_default() += v;
        }
        for (k, v) in &s.by_error {
            *self.by_error.entry(k.clone()).or_default() += v;
        }
        for (k, v) in &s.inv_pass {
            *self.inv_pass.entry(*k).or_default() += v;
        }
        for (k, v) in &s.inv_fail {
            *self.inv_fail.entry(*k).or_default() += v;
        }
        for v in &s.violations {
            if self.violations.len() < 40 {
                self.violations.push(v.clone());
            }
        }
        if s.max_drawdown_bp > self.worst_drawdown.0 {
            self.worst_drawdown = (s.max_drawdown_bp, s.seed);
        }
        if s.share_price_min_e6 < self.min_share_price.0 || self.seeds == 1 {
            self.min_share_price = (s.share_price_min_e6, s.seed);
        }
        if s.max_collateral_shortfall > self.max_shortfall.0 {
            self.max_shortfall = (s.max_collateral_shortfall, s.max_collateral_shortfall_bp_notional, s.seed);
        }
        if s.max_single_gain_bp_of_tvl > self.max_single_gain.0 {
            self.max_single_gain = (s.max_single_gain_bp_of_tvl, s.seed);
        }
        if s.max_util_total > self.max_util_total.0 {
            self.max_util_total = (s.max_util_total, s.seed);
        }
        let leg = s.max_util_pay.max(s.max_util_rec);
        if leg > self.max_util_leg.0 {
            self.max_util_leg = (leg, s.seed);
        }
        self.gains_paid += s.gains_paid_from_tvl;
        self.losses_booked += s.losses_booked_to_tvl;
        self.fees_total += s.fees_total;
        self.clamped_closes += s.clamped_closes;
        self.lp_short_n += s.lp_absorbed_shortfall_closes;
        self.lp_short_sum += s.lp_absorbed_shortfall_sum;
        self.tr_clamp_n += s.trader_clamped_gain_closes;
        self.tr_clamp_sum += s.trader_clamped_gain_sum;
        self.dead_n += s.dead_capital_captures;
        self.dead_sum += s.dead_capital_captured_sum;
        self.closes += s.closes;
        self.early_closes += s.early_closes;
        self.early_transfer_sum += s.early_close_transfer;
        if s.early_close_transfer_abs_max > self.early_transfer_abs_max.0 {
            self.early_transfer_abs_max = (s.early_close_transfer_abs_max, s.seed);
        }
        self.near_maturity_liquidations += s.near_maturity_liquidations;
        self.settlements += s.settlements;
        self.settle_mismatches += s.settle_mismatches;
        self.settle_transfer_sum += s.settle_transfer;
        if s.settle_transfer_abs_max > self.settle_transfer_abs_max.0 {
            self.settle_transfer_abs_max = (s.settle_transfer_abs_max, s.seed);
        }
        if s.tvl_hit_zero {
            self.seeds_tvl_zero += 1;
        }
        if s.max_drawdown_bp >= 1_000 {
            self.seeds_with_drawdown_over_10pct += 1;
        }
        if s.max_drawdown_bp >= 5_000 {
            self.seeds_with_drawdown_over_50pct += 1;
        }
        self.stale_rejections += s.stale_quote_rejections;
        self.withdraw_cap_rejections += s.withdraw_cap_rejections;
        let d = s.max_drawdown_bp;
        let b = if d < 1 { 0 } else if d < 10 { 1 } else if d < 100 { 2 } else if d < 500 { 3 } else if d < 1_000 { 4 } else if d < 2_500 { 5 } else if d < 5_000 { 6 } else { 7 };
        self.drawdown_hist[b] += 1;
    }

    fn merge_totals(&mut self, o: &Totals) {
        self.seeds += o.seeds;
        self.ticks += o.ticks;
        self.attempted += o.attempted;
        self.accepted += o.accepted;
        self.quotes += o.quotes;
        for (k, v) in &o.by_kind_attempted {
            *self.by_kind_attempted.entry(k).or_default() += v;
        }
        for (k, v) in &o.by_kind_accepted {
            *self.by_kind_accepted.entry(k).or_default() += v;
        }
        for (k, v) in &o.by_error {
            *self.by_error.entry(k.clone()).or_default() += v;
        }
        for (k, v) in &o.inv_pass {
            *self.inv_pass.entry(*k).or_default() += v;
        }
        for (k, v) in &o.inv_fail {
            *self.inv_fail.entry(*k).or_default() += v;
        }
        for v in &o.violations {
            if self.violations.len() < 40 {
                self.violations.push(v.clone());
            }
        }
        if o.worst_drawdown.0 > self.worst_drawdown.0 {
            self.worst_drawdown = o.worst_drawdown;
        }
        if o.seeds > 0 && (o.min_share_price.0 < self.min_share_price.0 || self.min_share_price.0 == 0) {
            self.min_share_price = o.min_share_price;
        }
        if o.max_shortfall.0 > self.max_shortfall.0 {
            self.max_shortfall = o.max_shortfall;
        }
        if o.max_single_gain.0 > self.max_single_gain.0 {
            self.max_single_gain = o.max_single_gain;
        }
        if o.max_util_total.0 > self.max_util_total.0 {
            self.max_util_total = o.max_util_total;
        }
        if o.max_util_leg.0 > self.max_util_leg.0 {
            self.max_util_leg = o.max_util_leg;
        }
        self.gains_paid += o.gains_paid;
        self.losses_booked += o.losses_booked;
        self.fees_total += o.fees_total;
        self.clamped_closes += o.clamped_closes;
        self.lp_short_n += o.lp_short_n;
        self.lp_short_sum += o.lp_short_sum;
        self.tr_clamp_n += o.tr_clamp_n;
        self.tr_clamp_sum += o.tr_clamp_sum;
        self.dead_n += o.dead_n;
        self.dead_sum += o.dead_sum;
        self.closes += o.closes;
        self.early_closes += o.early_closes;
        self.early_transfer_sum += o.early_transfer_sum;
        if o.early_transfer_abs_max.0 > self.early_transfer_abs_max.0 {
            self.early_transfer_abs_max = o.early_transfer_abs_max;
        }
        self.near_maturity_liquidations += o.near_maturity_liquidations;
        self.settlements += o.settlements;
        self.settle_mismatches += o.settle_mismatches;
        self.settle_transfer_sum += o.settle_transfer_sum;
        if o.settle_transfer_abs_max.0 > self.settle_transfer_abs_max.0 {
            self.settle_transfer_abs_max = o.settle_transfer_abs_max;
        }
        self.seeds_tvl_zero += o.seeds_tvl_zero;
        self.seeds_with_drawdown_over_10pct += o.seeds_with_drawdown_over_10pct;
        self.seeds_with_drawdown_over_50pct += o.seeds_with_drawdown_over_50pct;
        self.stale_rejections += o.stale_rejections;
        self.withdraw_cap_rejections += o.withdraw_cap_rejections;
        for i in 0..8 {
            self.drawdown_hist[i] += o.drawdown_hist[i];
        }
    }

    fn json(&self, seed_start: u64, seed_end: u64, horizon_days: u64, elapsed: Duration) -> String {
        let mut s = String::from("{\n");
        let _ = writeln!(s, "  \"seed_start\": {seed_start}, \"seed_end_exclusive\": {seed_end}, \"horizon_days\": {horizon_days},");
        let _ = writeln!(s, "  \"elapsed_secs\": {:.1}, \"seeds\": {}, \"ticks\": {},", elapsed.as_secs_f64(), self.seeds, self.ticks);
        let _ = writeln!(s, "  \"transitions_attempted\": {}, \"transitions_accepted\": {}, \"quotes_evaluated\": {},", self.attempted, self.accepted, self.quotes);
        let _ = writeln!(s, "  \"by_kind_attempted\": {},", map_json(&self.by_kind_attempted));
        let _ = writeln!(s, "  \"by_kind_accepted\": {},", map_json(&self.by_kind_accepted));
        let _ = writeln!(s, "  \"by_error\": {},", map_json_s(&self.by_error));
        s.push_str("  \"invariants\": {\n");
        for (i, inv) in Inv::ALL.iter().enumerate() {
            let _ = writeln!(
                s,
                "    \"{}\": {{\"pass\": {}, \"fail\": {}}}{}",
                inv.id(),
                self.inv_pass.get(inv).copied().unwrap_or(0),
                self.inv_fail.get(inv).copied().unwrap_or(0),
                if i + 1 < Inv::ALL.len() { "," } else { "" }
            );
        }
        s.push_str("  },\n");
        let _ = writeln!(s, "  \"worst_drawdown_bp\": {}, \"worst_drawdown_seed\": {},", self.worst_drawdown.0, self.worst_drawdown.1);
        let _ = writeln!(s, "  \"min_share_price_e6\": {}, \"min_share_price_seed\": {},", self.min_share_price.0, self.min_share_price.1);
        let _ = writeln!(s, "  \"max_collateral_shortfall_units\": {}, \"max_collateral_shortfall_bp_of_notional\": {}, \"max_collateral_shortfall_seed\": {},", self.max_shortfall.0, self.max_shortfall.1, self.max_shortfall.2);
        let _ = writeln!(s, "  \"max_single_gain_bp_of_tvl\": {}, \"max_single_gain_seed\": {},", self.max_single_gain.0, self.max_single_gain.1);
        let _ = writeln!(s, "  \"max_util_total_bp\": {}, \"max_util_total_seed\": {}, \"max_util_leg_bp\": {}, \"max_util_leg_seed\": {},", self.max_util_total.0, self.max_util_total.1, self.max_util_leg.0, self.max_util_leg.1);
        let _ = writeln!(s, "  \"gains_paid_units\": {}, \"losses_booked_units\": {}, \"fees_total_units\": {},", self.gains_paid, self.losses_booked, self.fees_total);
        let _ = writeln!(s, "  \"lp_absorbed_shortfall_closes\": {}, \"lp_absorbed_shortfall_units\": {}, \"trader_clamped_gain_closes\": {}, \"trader_clamped_gain_units\": {}, \"dead_capital_captures\": {}, \"dead_capital_units\": {},", self.lp_short_n, self.lp_short_sum, self.tr_clamp_n, self.tr_clamp_sum, self.dead_n, self.dead_sum);
        let _ = writeln!(s, "  \"closes\": {}, \"clamped_closes\": {}, \"early_closes\": {}, \"early_close_transfer_sum_units\": {}, \"early_close_transfer_abs_max_units\": {}, \"early_close_transfer_abs_max_seed\": {},", self.closes, self.clamped_closes, self.early_closes, self.early_transfer_sum, self.early_transfer_abs_max.0, self.early_transfer_abs_max.1);
        let _ = writeln!(s, "  \"near_maturity_liquidations\": {}, \"settlements\": {}, \"settle_mismatches\": {}, \"settle_transfer_sum_units\": {}, \"settle_transfer_abs_max_units\": {}, \"settle_transfer_abs_max_seed\": {},", self.near_maturity_liquidations, self.settlements, self.settle_mismatches, self.settle_transfer_sum, self.settle_transfer_abs_max.0, self.settle_transfer_abs_max.1);
        let _ = writeln!(s, "  \"seeds_tvl_hit_zero\": {}, \"seeds_drawdown_ge_10pct\": {}, \"seeds_drawdown_ge_50pct\": {}, \"stale_quote_rejections\": {}, \"withdraw_cap_rejections\": {},", self.seeds_tvl_zero, self.seeds_with_drawdown_over_10pct, self.seeds_with_drawdown_over_50pct, self.stale_rejections, self.withdraw_cap_rejections);
        let _ = writeln!(s, "  \"drawdown_histogram_bp\": {{\"lt1\": {}, \"lt10\": {}, \"lt100\": {}, \"lt500\": {}, \"lt1000\": {}, \"lt2500\": {}, \"lt5000\": {}, \"ge5000\": {}}},", self.drawdown_hist[0], self.drawdown_hist[1], self.drawdown_hist[2], self.drawdown_hist[3], self.drawdown_hist[4], self.drawdown_hist[5], self.drawdown_hist[6], self.drawdown_hist[7]);
        s.push_str("  \"violations\": [\n");
        for (i, v) in self.violations.iter().enumerate() {
            let _ = writeln!(
                s,
                "    {{\"inv\": \"{}\", \"seed\": {}, \"tick\": {}, \"action_index\": {}, \"action\": {}, \"detail\": {}}}{}",
                v.inv.id(),
                v.seed,
                v.tick,
                v.action_index,
                json_str(&v.action),
                json_str(&v.detail),
                if i + 1 < self.violations.len() { "," } else { "" }
            );
        }
        s.push_str("  ]\n}\n");
        s
    }

    fn markdown(&self, seed_start: u64, seed_end: u64, horizon_days: u64, elapsed: Duration) -> String {
        let mut s = String::new();
        let _ = writeln!(s, "# Fast layer batch: seeds {seed_start}..{seed_end}, horizon {horizon_days} days\n");
        let _ = writeln!(s, "Elapsed {:.0} s, {} seeds, {} ticks, {} transitions attempted, {} accepted, {} quotes evaluated.\n", elapsed.as_secs_f64(), self.seeds, self.ticks, self.attempted, self.accepted, self.quotes);
        s.push_str("| Invariant | Pass | Fail |\n|---|---:|---:|\n");
        for inv in Inv::ALL {
            let _ = writeln!(s, "| {} {} | {} | {} |", inv.id(), inv.describe(), self.inv_pass.get(&inv).copied().unwrap_or(0), self.inv_fail.get(&inv).copied().unwrap_or(0));
        }
        s.push_str("\n| Metric | Value | Seed |\n|---|---:|---:|\n");
        let _ = writeln!(s, "| Worst LP share price drawdown (bp) | {} | {} |", self.worst_drawdown.0, self.worst_drawdown.1);
        let _ = writeln!(s, "| Minimum share price (1e6 = par) | {} | {} |", self.min_share_price.0, self.min_share_price.1);
        let _ = writeln!(s, "| Max collateral shortfall (USDC) | {:.2} ({} bp of notional) | {} |", self.max_shortfall.0 as f64 / USDC as f64, self.max_shortfall.1, self.max_shortfall.2);
        let _ = writeln!(s, "| Max single close gain as bp of tvl | {} | {} |", self.max_single_gain.0, self.max_single_gain.1);
        let _ = writeln!(s, "| Max total utilisation (bp) | {} | {} |", self.max_util_total.0, self.max_util_total.1);
        let _ = writeln!(s, "| Max single-leg utilisation (bp) | {} | {} |", self.max_util_leg.0, self.max_util_leg.1);
        let _ = writeln!(s, "| Gains paid from tvl (USDC) | {:.0} | |", self.gains_paid as f64 / USDC as f64);
        let _ = writeln!(s, "| Losses booked to tvl (USDC) | {:.0} | |", self.losses_booked as f64 / USDC as f64);
        let _ = writeln!(s, "| Fees (USDC) | {:.0} | |", self.fees_total as f64 / USDC as f64);
        let _ = writeln!(s, "| Closes / clamped at collateral | {} / {} | |", self.closes, self.clamped_closes);
        let _ = writeln!(s, "| Clamped on trader loss (LP absorbs excess): count / sum USDC | {} / {:.2} | |", self.lp_short_n, self.lp_short_sum as f64 / USDC as f64);
        let _ = writeln!(s, "| Clamped on trader gain (trader forgoes excess): count / sum USDC | {} / {:.2} | |", self.tr_clamp_n, self.tr_clamp_sum as f64 / USDC as f64);
        let _ = writeln!(s, "| Deposits capturing dead capital (supply 0, tvl > 0): count / sum USDC | {} / {:.2} | |", self.dead_n, self.dead_sum as f64 / USDC as f64);
        let _ = writeln!(s, "| Early closes / booked minus fair pnl sum (USDC) / abs max | {} / {:.0} / {:.0} | {} |", self.early_closes, self.early_transfer_sum as f64 / USDC as f64, self.early_transfer_abs_max.0 as f64 / USDC as f64, self.early_transfer_abs_max.1);
        let _ = writeln!(s, "| Near-maturity liquidations | {} | |", self.near_maturity_liquidations);
        let _ = writeln!(s, "| Settlements / with booked != fair / sum / abs max (USDC) | {} / {} / {:.0} / {:.0} | {} |", self.settlements, self.settle_mismatches, self.settle_transfer_sum as f64 / USDC as f64, self.settle_transfer_abs_max.0 as f64 / USDC as f64, self.settle_transfer_abs_max.1);
        let _ = writeln!(s, "| Seeds where tvl hit zero with shares outstanding | {} | |", self.seeds_tvl_zero);
        let _ = writeln!(s, "| Seeds with drawdown >= 10 pct / >= 50 pct | {} / {} | |", self.seeds_with_drawdown_over_10pct, self.seeds_with_drawdown_over_50pct);
        let _ = writeln!(s, "| Stale-benchmark rejections / withdrawals blocked by caps | {} / {} | |", self.stale_rejections, self.withdraw_cap_rejections);
        let _ = writeln!(s, "\nDrawdown histogram (bp): <1: {}, <10: {}, <100: {}, <500: {}, <1000: {}, <2500: {}, <5000: {}, >=5000: {}\n", self.drawdown_hist[0], self.drawdown_hist[1], self.drawdown_hist[2], self.drawdown_hist[3], self.drawdown_hist[4], self.drawdown_hist[5], self.drawdown_hist[6], self.drawdown_hist[7]);
        s.push_str("\n| Instruction | Attempted | Accepted |\n|---|---:|---:|\n");
        for (k, v) in &self.by_kind_attempted {
            let _ = writeln!(s, "| {k} | {v} | {} |", self.by_kind_accepted.get(k).copied().unwrap_or(0));
        }
        s.push_str("\n| Rejection | Count |\n|---|---:|\n");
        for (k, v) in &self.by_error {
            let _ = writeln!(s, "| {k} | {v} |");
        }
        if !self.violations.is_empty() {
            s.push_str("\n## Violations (first recorded)\n\n");
            for v in &self.violations {
                let _ = writeln!(s, "- {} seed {} tick {} action {} `{}`: {}", v.inv.id(), v.seed, v.tick, v.action_index, v.action, v.detail);
            }
        }
        s
    }
}

fn map_json(m: &BTreeMap<&'static str, u64>) -> String {
    let mut s = String::from("{");
    for (i, (k, v)) in m.iter().enumerate() {
        if i > 0 {
            s.push_str(", ");
        }
        let _ = write!(s, "\"{k}\": {v}");
    }
    s.push('}');
    s
}

fn map_json_s(m: &BTreeMap<String, u64>) -> String {
    let mut s = String::from("{");
    for (i, (k, v)) in m.iter().enumerate() {
        if i > 0 {
            s.push_str(", ");
        }
        let _ = write!(s, "\"{k}\": {v}");
    }
    s.push('}');
    s
}

fn json_str(x: &str) -> String {
    let mut s = String::from("\"");
    for c in x.chars() {
        match c {
            '"' => s.push_str("\\\""),
            '\\' => s.push_str("\\\\"),
            '\n' => s.push_str("\\n"),
            c => s.push(c),
        }
    }
    s.push('"');
    s
}

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
}

fn arg_u64(args: &[String], name: &str, default: u64) -> u64 {
    arg(args, name).and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn cmd_mc(args: &[String]) {
    let seed_start = arg_u64(args, "--seed-start", 1);
    let seeds = arg_u64(args, "--seeds", u64::MAX);
    let horizon_days = arg_u64(args, "--horizon-days", 365);
    let budget = arg_u64(args, "--time-budget-secs", 1_200);
    let threads = arg_u64(args, "--threads", 2).max(1) as usize;
    let out_dir = arg(args, "--out").unwrap_or_else(|| "results".to_string());
    std::fs::create_dir_all(&out_dir).expect("results dir");
    let started = Instant::now();
    let deadline = started + Duration::from_secs(budget);
    let next = AtomicU64::new(seed_start);
    let stop = AtomicBool::new(false);
    let merged: Mutex<Totals> = Mutex::new(Totals::default());
    let ticks_per_seed = horizon_days * 24;
    std::thread::scope(|sc| {
        for _ in 0..threads {
            sc.spawn(|| {
                let mut local = Totals::default();
                loop {
                    if stop.load(Ordering::Relaxed) || Instant::now() >= deadline {
                        break;
                    }
                    let seed = next.fetch_add(1, Ordering::Relaxed);
                    if seed >= seed_start.saturating_add(seeds) {
                        stop.store(true, Ordering::Relaxed);
                        break;
                    }
                    let st = run_seed(seed, horizon_days, None);
                    local.merge(&st, ticks_per_seed);
                    if local.seeds % 50 == 0 {
                        merged.lock().unwrap().merge_totals(&local);
                        local = Totals::default();
                    }
                }
                merged.lock().unwrap().merge_totals(&local);
            });
        }
    });
    let elapsed = started.elapsed();
    let t = merged.into_inner().unwrap();
    let seed_end = next.load(Ordering::Relaxed).min(seed_start.saturating_add(seeds));
    let stem = format!("{out_dir}/mc_{seed_start}_{seed_end}_{horizon_days}d");
    std::fs::write(format!("{stem}.json"), t.json(seed_start, seed_end, horizon_days, elapsed)).expect("write json");
    std::fs::write(format!("{stem}.md"), t.markdown(seed_start, seed_end, horizon_days, elapsed)).expect("write md");
    println!("{}", t.markdown(seed_start, seed_end, horizon_days, elapsed));
    println!("written {stem}.json and {stem}.md");
}

fn cmd_replay(args: &[String]) {
    let seed = arg_u64(args, "--seed", 1);
    let horizon_days = arg_u64(args, "--horizon-days", 365);
    let quiet = args.iter().any(|a| a == "--quiet");
    let only_fail = args.iter().any(|a| a == "--rejections");
    let p = ScenarioParams::sample(seed, horizon_days);
    println!("{p:#?}");
    let mut printed = 0u64;
    let mut cb = |tick: u64, idx: u64, a: &Action, out: &Outcome, m: &brink_sim_core::Model| {
        if quiet || matches!(a, Action::Warp { .. }) {
            return;
        }
        if only_fail && out.is_ok() {
            return;
        }
        printed += 1;
        let o = match out {
            Ok(e) => format!("ok {e:?}"),
            Err(e) => format!("rejected {e:?}"),
        };
        println!(
            "t{tick} #{idx} slot {} ts {} | {a:?} -> {o} | tvl {} col {} vault {} util {}/{} open {} supply {} mode {:?} bench {} ema {}",
            m.slot, m.ts, m.pool.tvl, m.pool.collateral_held, m.pool.vault, m.pool.util_pay_bp, m.pool.util_rec_bp, m.pool.open_swaps, m.pool.share_supply, m.global.mode, m.bench.value_bp, m.bench.ema_bp
        );
    };
    let st = run_seed(seed, horizon_days, Some(&mut cb));
    let mut t = Totals::default();
    t.merge(&st, horizon_days * 24);
    println!("{}", t.markdown(seed, seed + 1, horizon_days, Duration::from_secs(0)));
}

/// Exhaustive sweep of the pure maths: every quote input on a coarse grid and every pnl input on a coarse grid,
/// checking the vernier bounds hold (quote within reference plus caps; pnl bounded by collateral; both legs sum
/// to zero before the bound).
fn cmd_sweep(args: &[String]) {
    use vernier::{quote, Leg, Params, Pool, Tenor, DEFAULT_PARAMS};
    let out_dir = arg(args, "--out").unwrap_or_else(|| "results".to_string());
    std::fs::create_dir_all(&out_dir).expect("results dir");
    let started = Instant::now();
    let mut evaluated: u64 = 0;
    let mut quote_bound_fail = 0u64;
    let mut quote_err = 0u64;
    let mut max_fixed = 0i32;
    let mut min_fixed = i32::MAX;
    let mut pay_below_receive = 0u64;
    let tenors = [Tenor::D28, Tenor::D60, Tenor::D90, Tenor::D180];
    let spots: Vec<u16> = (0..=30_000u32).step_by(250).map(|v| v as u16).collect();
    let emas: Vec<u16> = (0..=30_000u32).step_by(750).map(|v| v as u16).collect();
    let tvls: [u64; 6] = [1_000 * USDC, 100_000 * USDC, 10_000_000 * USDC, 1_000_000_000 * USDC, 100_000_000_000 * USDC, u64::MAX / 4];
    let utils: [u16; 6] = [0, 1_000, 2_400, 4_000, 4_799, 4_800];
    let mut params_set: Vec<Params> = vec![DEFAULT_PARAMS];
    let mut wide = DEFAULT_PARAMS;
    for i in 0..4 {
        wide.model_pay_bp[i] = 400;
        wide.model_rec_bp[i] = 400;
        wide.term_bp[i] = 500;
    }
    wide.demand_cap_bp = 500;
    wide.demand_k_bp = 500;
    params_set.push(wide);
    for params in &params_set {
        for spot in &spots {
            for ema in &emas {
                for tvl in tvls {
                    for up in utils {
                        for ur in utils {
                            if u32::from(up) + u32::from(ur) > 8_000 {
                                continue;
                            }
                            let pool = Pool { tvl, util_pay_bp: up, util_rec_bp: ur };
                            for t in tenors {
                                let notional_cases = [1_000 * USDC, tvl / 100, tvl / 10];
                                for n in notional_cases {
                                    let mut fixed_pay = None;
                                    let mut fixed_rec = None;
                                    for leg in [Leg::Pay, Leg::Receive] {
                                        evaluated += 1;
                                        match quote(*spot, *ema, t, leg, n, &pool, params) {
                                            Ok(q) => {
                                                let f = q.fixed_bp;
                                                let i = tenor_index(t);
                                                let (reference, model) = match leg {
                                                    Leg::Pay => ((*spot).max(*ema), params.model_pay_bp[i]),
                                                    Leg::Receive => ((*spot).min(*ema), params.model_rec_bp[i]),
                                                };
                                                let bound = i32::from(model) + i32::from(params.demand_cap_bp) + i32::from(params.term_bp[i]);
                                                let side = match leg {
                                                    Leg::Pay => f >= i32::from(reference),
                                                    Leg::Receive => f <= i32::from(reference),
                                                };
                                                if (f - i32::from(reference)).abs() > bound || !side {
                                                    quote_bound_fail += 1;
                                                }
                                                max_fixed = max_fixed.max(f);
                                                min_fixed = min_fixed.min(f);
                                                match leg {
                                                    Leg::Pay => fixed_pay = Some(f),
                                                    Leg::Receive => fixed_rec = Some(f),
                                                }
                                            }
                                            Err(_) => quote_err += 1,
                                        }
                                    }
                                    if let (Some(p), Some(r)) = (fixed_pay, fixed_rec) {
                                        if p < r {
                                            pay_below_receive += 1;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    // pnl grid
    let mut pnl_evaluated = 0u64;
    let mut pnl_bound_fail = 0u64;
    let mut pnl_err = 0u64;
    let mut antisym_fail = 0u64;
    let diffs: Vec<i64> = (-30_000i64..=30_000).step_by(500).collect();
    let notionals: [u64; 7] = [1, 1_000 * USDC, 1_000_000 * USDC, 50_000_000 * USDC, 1_000_000_000 * USDC, u64::MAX / 1_000, u64::MAX];
    let days: [u16; 7] = [0, 1, 14, 28, 60, 90, 180];
    let collaterals: [u64; 5] = [0, 1, 1_000 * USDC, 1_000_000 * USDC, u64::MAX];
    for d in &diffs {
        for n in notionals {
            for day in days {
                for c in collaterals {
                    pnl_evaluated += 1;
                    match brink_sim_core::model::pnl_bounded(*d, n, day, c) {
                        Ok(p) => {
                            if p.unsigned_abs() > c {
                                pnl_bound_fail += 1;
                            }
                            if let Ok(q) = brink_sim_core::model::pnl_bounded(-*d, n, day, c) {
                                if p != -q {
                                    antisym_fail += 1;
                                }
                            }
                        }
                        Err(_) => pnl_err += 1,
                    }
                }
            }
        }
    }
    let elapsed = started.elapsed();
    let md = format!(
        "# Maths sweep\n\nElapsed {:.1} s.\n\n| Check | Evaluated | Failures | Note |\n|---|---:|---:|---|\n| quote within reference +/- (model + demand cap + term), correct side | {} | {} | {} quote errors (bad utilisation inputs), fixed range [{}, {}] |\n| pay quote >= receive quote | {} pairs | {} | |\n| pnl bounded by collateral | {} | {} | {} overflow errors (u64::MAX-scale inputs) |\n| pnl antisymmetric in the rate difference | {} | {} | |\n",
        elapsed.as_secs_f64(),
        evaluated,
        quote_bound_fail,
        quote_err,
        min_fixed,
        max_fixed,
        evaluated / 2,
        pay_below_receive,
        pnl_evaluated,
        pnl_bound_fail,
        pnl_err,
        pnl_evaluated,
        antisym_fail
    );
    std::fs::write(format!("{out_dir}/sweep.md"), &md).expect("write sweep");
    println!("{md}");
}

fn tenor_index(t: vernier::Tenor) -> usize {
    match t {
        vernier::Tenor::D28 => 0,
        vernier::Tenor::D60 => 1,
        vernier::Tenor::D90 => 2,
        vernier::Tenor::D180 => 3,
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("mc") => cmd_mc(&args),
        Some("replay") => cmd_replay(&args),
        Some("sweep") => cmd_sweep(&args),
        _ => {
            eprintln!("usage: brink_sim_fast mc|replay|sweep [options]");
            std::process::exit(2);
        }
    }
}
