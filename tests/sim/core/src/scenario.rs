//! Scenario generator. A seed fixes every distribution draw, so the fast layer and the LiteSVM layer see the same
//! action stream for the same seed. The generator reads the model state (open swaps, capacity, LP holdings) when
//! it shapes an action, and never mutates it.
//!
//! Time is advanced in ticks of one hour (3,600 s, 9,000 slots at 400 ms). Every parameter below is drawn per
//! seed from the distribution documented next to it; `docs/audit/sim/REPORT.md` lists the same table.

use vernier::{Leg, Params, Tenor};

use crate::model::{Actor, Mode, Model, Setup, DEFAULT_PARAMS, SLOTS_PER_DAY, USDC};
use crate::rng::Rng;

pub const TICK_SECS: i64 = 3_600;
pub const TICK_SLOTS: u64 = SLOTS_PER_DAY / 24;
pub const TICKS_PER_DAY: u64 = 24;

/// How the publisher behaves when the true rate leaves the EMA band.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Clamp {
    /// Publishes `clamp(true, ema - band, ema + band)`: always accepted, tracks with lag.
    ToBand,
    /// Publishes the raw value: rejected `OutOfBand` until the true rate comes back.
    Raw,
}

#[derive(Clone, Debug)]
pub struct ScenarioParams {
    pub seed: u64,
    pub horizon_days: u64,
    // benchmark guards
    pub band_bp: u16,
    pub max_staleness_slots: u64,
    pub half_life_slots: u64,
    pub min_interval_slots: u64,
    // rate path (annualised bp), Ornstein-Uhlenbeck with jumps and regime shifts
    pub r0_bp: f64,
    pub mu_bp: f64,
    pub theta_per_day: f64,
    pub sigma_bp_per_sqrt_day: f64,
    pub jump_prob_per_day: f64,
    pub jump_sd_bp: f64,
    pub regime_prob_per_day: f64,
    pub publish_every_ticks: u64,
    pub stale_prob_per_day: f64,
    pub stale_len_ticks: u64,
    pub clamp: Clamp,
    // pool
    pub initial_tvl: u64,
    pub n_lps: usize,
    pub n_traders: usize,
    pub lp_deposit_per_day: f64,
    pub lp_withdraw_per_day: f64,
    pub trader_opens_per_day: f64,
    pub p_pay: f64,
    pub whale_prob: f64,
    pub notional_frac_log_mean: f64,
    pub notional_frac_log_sd: f64,
    pub tenor_weights: [f64; 4],
    pub cancel_per_swap_per_day: f64,
    pub settle_prob_per_tick: f64,
    pub liquidation_probes_per_tick: u64,
    pub third_party_crank_prob: f64,
    pub tight_limit_prob: f64,
    // governance and misc
    pub mode_event_per_day: f64,
    pub rush_withdraw_per_tick: f64,
    pub calibration_per_day: f64,
    pub donation_per_day: f64,
    pub sweep_per_day: f64,
    pub limited_mode_cap: u64,
    pub min_notional: u64,
}

impl ScenarioParams {
    /// Draws every parameter from its distribution. The seed is the only input.
    #[must_use]
    pub fn sample(seed: u64, horizon_days: u64) -> Self {
        let mut r = Rng::new(seed ^ 0x5CE4_A210_0000_0001);
        let band_bp = *r.pick(&[100u16, 200, 300, 500, 1_000]);
        let publish_every_ticks = *r.pick(&[1u64, 1, 2, 6, 24]);
        // staleness guard relative to the publish cadence: one in four configurations is stale-prone
        let staleness_mult = *r.pick(&[0.5f64, 1.5, 3.0, 10.0]);
        let max_staleness_slots = ((publish_every_ticks * TICK_SLOTS) as f64 * staleness_mult) as u64;
        let r0_bp = r.log_uniform(50.0, 2_500.0);
        let initial_tvl = (r.log_uniform(20_000.0, 200_000_000.0) * USDC as f64) as u64;
        Self {
            seed,
            horizon_days,
            band_bp,
            max_staleness_slots,
            half_life_slots: *r.pick(&[2_000u64, 10_000, 50_000, 216_000]),
            min_interval_slots: *r.pick(&[0u64, 150, 1_000]),
            r0_bp,
            mu_bp: (r0_bp * r.log_uniform(0.5, 2.0)).clamp(10.0, 5_000.0),
            theta_per_day: r.log_uniform(0.002, 0.2),
            sigma_bp_per_sqrt_day: r.log_uniform(2.0, 120.0),
            jump_prob_per_day: *r.pick(&[0.0, 0.005, 0.02, 0.1]),
            jump_sd_bp: r.log_uniform(50.0, 1_500.0),
            regime_prob_per_day: *r.pick(&[0.0, 0.002, 0.01]),
            publish_every_ticks,
            stale_prob_per_day: *r.pick(&[0.0, 0.0, 0.01, 0.05]),
            stale_len_ticks: r.range(2, 72),
            clamp: if r.chance(0.7) { Clamp::ToBand } else { Clamp::Raw },
            initial_tvl,
            n_lps: 4,
            n_traders: 6,
            lp_deposit_per_day: r.log_uniform(0.05, 3.0),
            lp_withdraw_per_day: r.log_uniform(0.05, 3.0),
            trader_opens_per_day: r.log_uniform(0.5, 40.0),
            p_pay: r.uniform(0.1, 0.9),
            whale_prob: *r.pick(&[0.0, 0.01, 0.05, 0.2]),
            notional_frac_log_mean: r.uniform(-7.0, -3.0),
            notional_frac_log_sd: r.uniform(0.5, 1.5),
            tenor_weights: [r.uniform(0.1, 1.0), r.uniform(0.1, 1.0), r.uniform(0.1, 1.0), r.uniform(0.1, 1.0)],
            cancel_per_swap_per_day: r.log_uniform(0.001, 0.05),
            settle_prob_per_tick: *r.pick(&[1.0, 0.5, 0.1, 0.02]),
            liquidation_probes_per_tick: *r.pick(&[0u64, 1, 2, 4]),
            third_party_crank_prob: r.uniform(0.0, 1.0),
            tight_limit_prob: 0.05,
            mode_event_per_day: *r.pick(&[0.0, 0.0, 0.01, 0.05]),
            rush_withdraw_per_tick: r.uniform(0.2, 2.0),
            calibration_per_day: *r.pick(&[0.0, 0.0, 0.01, 0.05]),
            donation_per_day: *r.pick(&[0.0, 0.0, 0.01]),
            sweep_per_day: 0.2,
            limited_mode_cap: (r.log_uniform(1_000.0, 1_000_000.0) * USDC as f64) as u64,
            min_notional: 1_000 * USDC,
        }
    }

    #[must_use]
    pub fn setup(&self) -> Setup {
        Setup {
            band_bp: self.band_bp,
            max_staleness_slots: self.max_staleness_slots,
            half_life_slots: self.half_life_slots,
            min_interval_slots: self.min_interval_slots,
            params: DEFAULT_PARAMS,
            param_delay_slots: 432_000,
            limited_mode_cap: self.limited_mode_cap,
            min_notional: self.min_notional,
            max_notional: 50_000_000 * USDC,
            n_lps: self.n_lps,
            n_traders: self.n_traders,
            start_slot: 1_000,
            start_ts: 1_790_000_000,
        }
    }
}

/// A fully concrete instruction (or clock move) that both layers can apply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Warp { slots: u64, secs: i64 },
    Publish { value_bp: u16 },
    Deposit { lp: u8, amount: u64, min_shares: u64 },
    Withdraw { lp: u8, shares: u64, min_amount: u64 },
    Open { trader: u8, id: u64, leg: Leg, tenor: u8, notional: u64, limit_bp: u16 },
    Cancel { signer: u8, id: u64, min_payout: u64 },
    Settle { id: u64, cranker: Option<u8> },
    Liquidate { id: u64, cranker: Option<u8> },
    Donate { amount: u64 },
    SyncVault,
    Sweep,
    SetMode { by: Actor, mode: Mode },
    QueueCalibration { params: Params },
}

impl Action {
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Action::Warp { .. } => "warp",
            Action::Publish { .. } => "publish",
            Action::Deposit { .. } => "lp_deposit",
            Action::Withdraw { .. } => "lp_withdraw",
            Action::Open { .. } => "trader_open_swap",
            Action::Cancel { .. } => "trader_cancel_swap",
            Action::Settle { .. } => "crank_settle_swap",
            Action::Liquidate { .. } => "crank_liquidate_swap",
            Action::Donate { .. } => "donate",
            Action::SyncVault => "sync_vault",
            Action::Sweep => "sweep_fees",
            Action::SetMode { .. } => "admin_set_mode",
            Action::QueueCalibration { .. } => "admin_queue_calibration",
        }
    }
}

/// A queued item: either a concrete action or one shaped from the model state at the moment it is popped (so
/// that quotes, capacities and holdings are read after the warp and the publish of the same tick).
#[derive(Clone, Debug)]
enum Planned {
    Ready(Action),
    Open,
    Withdraw { lo: f64, hi: f64 },
    Calibration,
    LiquidateRandom,
    CancelRandom,
}

/// Per-seed generator state.
#[derive(Clone, Debug)]
pub struct Scenario {
    pub p: ScenarioParams,
    rng: Rng,
    rate_rng: Rng,
    pub tick: u64,
    pub true_rate_bp: f64,
    mu_bp: f64,
    stale_until_tick: u64,
    next_id: u64,
    queue: Vec<Planned>,
    /// Pending calibration drift direction for collateral (so that queued calibrations trend, not jitter).
    calib_dir: i8,
}

impl Scenario {
    #[must_use]
    pub fn new(p: ScenarioParams) -> Self {
        let mut rng = Rng::new(p.seed);
        let rate_rng = rng.fork(1);
        Self {
            true_rate_bp: p.r0_bp,
            mu_bp: p.mu_bp,
            stale_until_tick: 0,
            next_id: 1,
            queue: Vec::new(),
            calib_dir: if rng.chance(0.5) { 1 } else { -1 },
            p,
            rng,
            rate_rng,
            tick: 0,
        }
    }

    #[must_use]
    pub fn horizon_ticks(&self) -> u64 {
        self.p.horizon_days * TICKS_PER_DAY
    }

    /// Bootstrap actions: first publish and the initial LP deposit.
    pub fn bootstrap(&mut self) -> Vec<Action> {
        let v = self.true_rate_bp.round().clamp(1.0, 30_000.0) as u16;
        vec![Action::Publish { value_bp: v }, Action::Deposit { lp: 0, amount: self.p.initial_tvl, min_shares: 0 }]
    }

    fn step_rate(&mut self) {
        let dt = 1.0 / TICKS_PER_DAY as f64;
        let r = &mut self.rate_rng;
        if r.chance(self.p.regime_prob_per_day * dt) {
            self.mu_bp = (self.mu_bp * r.log_uniform(0.4, 2.5)).clamp(10.0, 8_000.0);
        }
        let mut x = self.true_rate_bp;
        x += self.p.theta_per_day * (self.mu_bp - x) * dt + self.p.sigma_bp_per_sqrt_day * dt.sqrt() * r.normal();
        if r.chance(self.p.jump_prob_per_day * dt) {
            x += self.p.jump_sd_bp * r.normal();
        }
        self.true_rate_bp = x.clamp(1.0, 29_000.0);
    }

    fn pick_tenor(&mut self) -> u8 {
        self.rng.weighted(&self.p.tenor_weights) as u8
    }

    fn open_action(&mut self, m: &Model) -> Action {
        let leg = if self.rng.chance(self.p.p_pay) { Leg::Pay } else { Leg::Receive };
        let tenor_ix = self.pick_tenor();
        let tenor = tenor_from_ix(tenor_ix);
        let cap = vernier::leg_capacity(&m.pool.vernier(), leg);
        let notional = if self.rng.chance(self.p.whale_prob) {
            cap.max(self.p.min_notional)
        } else {
            let frac = (self.p.notional_frac_log_mean + self.p.notional_frac_log_sd * self.rng.normal()).exp();
            let n = (m.pool.tvl as f64 * frac) as u64;
            n.clamp(self.p.min_notional, cap.max(self.p.min_notional))
        };
        // Limit: the trader accepts the current quote; occasionally one bp better than the market (rejected).
        let limit_bp = match m.quote_fixed(tenor, leg, notional) {
            Ok(q) => {
                let q = q.clamp(0, i32::from(u16::MAX)) as u16;
                if self.rng.chance(self.p.tight_limit_prob) {
                    match leg {
                        Leg::Pay => q.saturating_sub(1),
                        Leg::Receive => q.saturating_add(1),
                    }
                } else {
                    q
                }
            }
            Err(_) => match leg {
                Leg::Pay => u16::MAX,
                Leg::Receive => 0,
            },
        };
        let trader = self.rng.below(self.p.n_traders as u64) as u8;
        let id = self.next_id;
        self.next_id += 1;
        Action::Open { trader, id, leg, tenor: tenor_ix, notional, limit_bp }
    }

    fn cranker(&mut self, m: &Model, id: u64) -> Option<u8> {
        if self.rng.chance(self.p.third_party_crank_prob) {
            let owner = m.swap(id).map(|s| s.trader).unwrap_or(0);
            // a different trader key acts as the keeper
            let k = (owner as usize + 1 + self.rng.below((self.p.n_traders - 1) as u64) as usize) % self.p.n_traders;
            Some(k as u8)
        } else {
            None
        }
    }

    fn lp_withdraw_action(&mut self, m: &Model, frac_lo: f64, frac_hi: f64) -> Option<Action> {
        let holders: Vec<usize> = (0..self.p.n_lps).filter(|i| m.lp_shares[*i] > 0).collect();
        if holders.is_empty() {
            return None;
        }
        let lp = *self.rng.pick(&holders);
        let frac = self.rng.uniform(frac_lo, frac_hi);
        let shares = ((m.lp_shares[lp] as f64) * frac).max(1.0) as u64;
        Some(Action::Withdraw { lp: lp as u8, shares: shares.min(m.lp_shares[lp]), min_amount: 0 })
    }

    fn calibration_action(&mut self, m: &Model) -> Action {
        // Drift one or two tables inside the step bound (0.5x to 1.5x), collateral trending in one direction so
        // that long paths explore both thin and thick collateral.
        let cur = m.pool.params;
        let mut p = cur;
        let f = |r: &mut Rng, old: u16, dir: i8| -> u16 {
            let lo = old / 2;
            let hi = (old.saturating_mul(3) / 2).saturating_add(1);
            let v = if dir > 0 { r.range(u64::from(old), u64::from(hi)) } else { r.range(u64::from(lo), u64::from(old)) };
            (v as u16).max(1)
        };
        match self.rng.below(4) {
            0 => {
                for i in 0..4 {
                    p.collateral_bp[i] = f(&mut self.rng, cur.collateral_bp[i], self.calib_dir).clamp(10, 3_000);
                }
                if p.collateral_bp[3] >= 3_000 || p.collateral_bp[0] <= 10 {
                    self.calib_dir = -self.calib_dir;
                }
            }
            1 => {
                for i in 0..4 {
                    p.model_pay_bp[i] = f(&mut self.rng, cur.model_pay_bp[i], 1).min(400);
                    p.model_rec_bp[i] = f(&mut self.rng, cur.model_rec_bp[i], 1).min(400);
                }
            }
            2 => {
                p.demand_k_bp = f(&mut self.rng, cur.demand_k_bp, 1).min(500);
                p.demand_cap_bp = f(&mut self.rng, cur.demand_cap_bp, 1).min(500);
            }
            _ => {
                // deliberately outside the step bound: must be rejected
                p.term_bp[0] = cur.term_bp[0].saturating_mul(2).saturating_add(2);
            }
        }
        Action::QueueCalibration { params: p }
    }

    /// Fills the queue with this tick's actions, reading the model state at the start of the tick.
    pub fn begin_tick(&mut self, m: &Model) {
        self.tick += 1;
        let day = 1.0 / TICKS_PER_DAY as f64;
        self.queue.clear();
        self.queue.push(Planned::Ready(Action::Warp { slots: TICK_SLOTS, secs: TICK_SECS }));
        self.step_rate();

        // --- publisher ---
        if self.tick >= self.stale_until_tick && self.rng.chance(self.p.stale_prob_per_day * day) {
            self.stale_until_tick = self.tick + self.p.stale_len_ticks;
        }
        if self.tick % self.p.publish_every_ticks == 0 && self.tick >= self.stale_until_tick {
            let raw = self.true_rate_bp.round().clamp(0.0, 30_000.0) as u16;
            let v = match self.p.clamp {
                Clamp::ToBand if m.bench.published => {
                    let lo = m.bench.ema_bp.saturating_sub(m.bench.band_bp);
                    let hi = m.bench.ema_bp.saturating_add(m.bench.band_bp).min(30_000);
                    raw.clamp(lo, hi)
                }
                _ => raw,
            };
            self.queue.push(Planned::Ready(Action::Publish { value_bp: v }));
        }

        // --- governance ---
        if self.rng.chance(self.p.mode_event_per_day * day) {
            let (by, mode) = match self.rng.below(6) {
                0 => (Actor::Guardian, Mode::Limited),
                1 => (Actor::Guardian, Mode::WithdrawOnly),
                2 => (Actor::Guardian, Mode::Halted),
                3 => (Actor::Authority, Mode::Normal),
                4 => (Actor::Authority, Mode::Halted),
                _ => (Actor::Stranger, Mode::WithdrawOnly),
            };
            self.queue.push(Planned::Ready(Action::SetMode { by, mode }));
        }
        if m.global.mode == Mode::Halted && self.rng.chance(0.2) {
            // the authority does not leave the protocol halted for long
            self.queue.push(Planned::Ready(Action::SetMode { by: Actor::Authority, mode: Mode::Normal }));
        }
        if self.rng.chance(self.p.calibration_per_day * day) {
            self.queue.push(Planned::Calibration);
        }
        if self.rng.chance(self.p.donation_per_day * day) {
            let amount = self.rng.range(1, 1_000 * USDC);
            self.queue.push(Planned::Ready(Action::Donate { amount }));
            self.queue.push(Planned::Ready(Action::SyncVault));
        }
        if self.rng.chance(self.p.sweep_per_day * day) {
            self.queue.push(Planned::Ready(Action::Sweep));
        }

        // --- LP flow ---
        let n_dep = self.rng.poisson(self.p.lp_deposit_per_day * day);
        for _ in 0..n_dep {
            let lp = self.rng.below(self.p.n_lps as u64) as u8;
            let frac = self.rng.log_uniform(0.001, 0.5);
            let amount = ((self.p.initial_tvl as f64) * frac).max(1.0) as u64;
            self.queue.push(Planned::Ready(Action::Deposit { lp, amount, min_shares: 0 }));
        }
        let n_wd = self.rng.poisson(self.p.lp_withdraw_per_day * day);
        for _ in 0..n_wd {
            self.queue.push(Planned::Withdraw { lo: 0.01, hi: 0.5 });
        }
        if m.global.mode == Mode::WithdrawOnly {
            // withdrawal rush: several LPs try to leave with most of their holdings
            let n = self.rng.poisson(self.p.rush_withdraw_per_tick);
            for _ in 0..n {
                self.queue.push(Planned::Withdraw { lo: 0.5, hi: 1.0 });
            }
        }

        // --- closes: settlements of matured swaps, liquidation probes, cancels ---
        for s in &m.swaps {
            if m.ts + TICK_SECS >= s.matures_ts && self.rng.chance(self.p.settle_prob_per_tick) {
                let c = self.cranker(m, s.id);
                self.queue.push(Planned::Ready(Action::Settle { id: s.id, cranker: c }));
            }
        }
        if !m.swaps.is_empty() {
            for _ in 0..self.p.liquidation_probes_per_tick {
                self.queue.push(Planned::LiquidateRandom);
            }
            // keepers watch the pre-maturity window closely
            for s in &m.swaps {
                let to_maturity = s.matures_ts - (m.ts + TICK_SECS);
                if (0..6 * 3_600).contains(&to_maturity) && self.rng.chance(0.5) {
                    let c = self.cranker(m, s.id);
                    self.queue.push(Planned::Ready(Action::Liquidate { id: s.id, cranker: c }));
                }
            }
            let n_cancel = self.rng.poisson(self.p.cancel_per_swap_per_day * day * m.swaps.len() as f64);
            for _ in 0..n_cancel {
                self.queue.push(Planned::CancelRandom);
            }
        }

        // --- trader flow ---
        let n_open = self.rng.poisson(self.p.trader_opens_per_day * day);
        for _ in 0..n_open {
            self.queue.push(Planned::Open);
        }
        self.queue.reverse();
    }

    /// Next action of the current tick shaped from the current model state, or `None` when the tick is exhausted.
    pub fn next_action(&mut self, m: &Model) -> Option<Action> {
        loop {
            let planned = self.queue.pop()?;
            let a = match planned {
                Planned::Ready(a) => Some(a),
                Planned::Open => Some(self.open_action(m)),
                Planned::Withdraw { lo, hi } => self.lp_withdraw_action(m, lo, hi),
                Planned::Calibration => Some(self.calibration_action(m)),
                Planned::LiquidateRandom => {
                    if m.swaps.is_empty() {
                        None
                    } else {
                        let i = self.rng.below(m.swaps.len() as u64) as usize;
                        let id = m.swaps[i].id;
                        let c = self.cranker(m, id);
                        Some(Action::Liquidate { id, cranker: c })
                    }
                }
                Planned::CancelRandom => {
                    if m.swaps.is_empty() {
                        None
                    } else {
                        let i = self.rng.below(m.swaps.len() as u64) as usize;
                        let s = &m.swaps[i];
                        let signer = if self.rng.chance(0.02) { (s.trader + 1) % self.p.n_traders as u8 } else { s.trader };
                        Some(Action::Cancel { signer, id: s.id, min_payout: 0 })
                    }
                }
            };
            if a.is_some() {
                return a;
            }
        }
    }
}

#[must_use]
pub fn tenor_from_ix(ix: u8) -> Tenor {
    match ix {
        0 => Tenor::D28,
        1 => Tenor::D60,
        2 => Tenor::D90,
        _ => Tenor::D180,
    }
}
