//! Shared core of the Brink simulation harness: deterministic RNG, scenario generator, pool-state model and
//! invariant checker. Used by the fast layer (`../fast`, model only) and the slow layer (`../svm`, model next to
//! LiteSVM and the compiled programs).
#![forbid(unsafe_code)]

pub mod invariants;
pub mod model;
pub mod rng;
pub mod scenario;

pub use invariants::{apply_and_check, Effect, Inv, Outcome, SeedStats, Violation};
pub use model::{Actor, Err, Mode, Model, Setup};
pub use scenario::{Action, Scenario, ScenarioParams};

/// Runs one seed through the model alone. `max_actions` bounds the work for a time budget; `trace` receives
/// every action and outcome when supplied (replay mode).
pub fn run_seed(seed: u64, horizon_days: u64, mut trace: Option<&mut dyn FnMut(u64, u64, &Action, &Outcome, &Model)>) -> SeedStats {
    let params = ScenarioParams::sample(seed, horizon_days);
    let mut sc = Scenario::new(params.clone());
    let mut m = Model::new(&params.setup());
    let mut st = SeedStats::new(seed);
    st.initial_tvl = params.initial_tvl;
    let mut idx = 0u64;
    for a in sc.bootstrap() {
        let out = apply_and_check(&mut m, &a, &mut st, 0, idx);
        if let Some(t) = trace.as_deref_mut() {
            t(0, idx, &a, &out, &m);
        }
        idx += 1;
    }
    let ticks = sc.horizon_ticks();
    for tick in 1..=ticks {
        sc.begin_tick(&m);
        while let Some(a) = sc.next_action(&m) {
            let out = apply_and_check(&mut m, &a, &mut st, tick, idx);
            if let Some(t) = trace.as_deref_mut() {
                t(tick, idx, &a, &out, &m);
            }
            idx += 1;
        }
    }
    st
}
