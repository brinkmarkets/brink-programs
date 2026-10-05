//! Differential driver: `vernier` versus the exact reference model.
//!
//! Usage (from programs/vernier/model):
//!   cargo run --release --bin diff -- --cases 10000000 [--seed 7] [--emit 2000000] [--valid-only]
//!
//! Without `--emit` it compares in-process and prints a summary. With `--emit N` it also streams the first
//! N cases as `inputs | implementation outputs` lines on stdout for the Python and node checkers.
use std::io::{BufWriter, Write};
use vernier_model::{bridge, gen};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut cases: u64 = 1_000_000;
    let mut seed: u64 = 7;
    let mut emit: u64 = 0;
    let mut valid_only = false;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--cases" => {
                cases = args[i + 1].parse().expect("cases");
                i += 1;
            }
            "--seed" => {
                seed = args[i + 1].parse().expect("seed");
                i += 1;
            }
            "--emit" => {
                emit = args[i + 1].parse().expect("emit");
                i += 1;
            }
            "--valid-only" => valid_only = true,
            other => panic!("unknown argument {other}"),
        }
        i += 1;
    }
    let stdout = std::io::stdout();
    let mut out = BufWriter::with_capacity(1 << 20, stdout.lock());
    let mut mismatches = 0u64;
    let mut shown = 0u64;
    let mut status_hist = [0u64; 4];
    let mut demand_nonzero = 0u64;
    let mut capped = 0u64;
    let mut neg_fixed = 0u64;
    let mut fixed_over_u16 = 0u64;
    for i in 0..cases {
        let c = gen::case(seed, i, valid_only);
        let a = bridge::run_impl(&c);
        let b = bridge::run_ref(&c);
        status_hist[usize::from(a.status)] += 1;
        if a.status == 0 {
            if a.demand != 0 {
                demand_nonzero += 1;
            }
            if a.demand.abs() == i128::from(c.params.demand_cap_bp) && a.demand != 0 {
                capped += 1;
            }
            if a.fixed < 0 {
                neg_fixed += 1;
            }
            if a.fixed > i128::from(u16::MAX) {
                fixed_over_u16 += 1;
            }
        }
        if a != b {
            mismatches += 1;
            if shown < 20 {
                shown += 1;
                eprintln!("MISMATCH case {i}: {}\n  impl {:?}\n  ref  {:?}", bridge::format_case(&c), a, b);
            }
        }
        if i < emit {
            writeln!(out, "{} | {}", bridge::format_case(&c), bridge::format_out(&a)).expect("write");
        }
    }
    out.flush().expect("flush");
    eprintln!(
        "cases={cases} seed={seed} mismatches={mismatches} ok={} empty_pool={} malformed_util={} overflow={} demand_nonzero={demand_nonzero} demand_at_cap={capped} negative_fixed={neg_fixed} fixed_over_u16={fixed_over_u16}",
        status_hist[0], status_hist[1], status_hist[2], status_hist[3]
    );
    if mismatches != 0 {
        std::process::exit(1);
    }
}
