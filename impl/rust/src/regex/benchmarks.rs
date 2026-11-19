use super::compile;
use crate::cancel::CancellationToken;
use crate::cost::{Lifetime, Meter};
use std::{cell::Cell, hint::black_box, rc::Rc, time::Instant};

#[test]
#[ignore = "run with mise run bench:regex-limits"]
fn grouped_literal_meter_benchmark() {
    let budget = 1_000_000_000;
    for (name, pattern, input, hit, cost, iterations) in [
        ("hit", "(a)".repeat(12), "a".repeat(12), true, 311, 200_000),
        (
            "overlap",
            "(a)".repeat(11) + "(b)",
            "a".repeat(2048),
            false,
            79712,
            1000,
        ),
    ] {
        let program = compile(&pattern).unwrap();
        let input: Vec<_> = input.chars().collect();
        for (mode, statement, lifetime, cancel) in [
            ("unlimited", 0, 0, false),
            ("statement", budget, 0, false),
            ("lifetime", 0, budget, false),
            ("both", budget, budget, false),
            ("both+cancel", budget, budget, true),
        ] {
            let mut samples = Vec::new();
            for round in 0..4 {
                let life = Lifetime {
                    total: Rc::new(Cell::new(0)),
                    limit: lifetime,
                };
                let mut m = Meter::for_session(
                    statement,
                    life.clone(),
                    cancel.then(CancellationToken::new),
                );
                let start = Instant::now();
                for _ in 0..iterations {
                    assert_eq!(program.is_match(black_box(&input), &mut m).unwrap(), hit);
                }
                let ns = start.elapsed().as_nanos() as f64 / iterations as f64;
                assert_eq!(
                    (m.accrued, life.total.get()),
                    (cost * iterations, cost * iterations)
                );
                if round > 0 {
                    samples.push(ns);
                }
            }
            samples.sort_by(f64::total_cmp);
            println!("Rust {name} {mode} {:.1} ns/match", samples[1]);
        }
    }
}
