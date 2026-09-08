//! TEMPORARY profiling probe — delete.

use std::time::Instant;

use crate::multi::{Batch, MultiEngine, Policy, SERVERS};

const CLIENTS: usize = 100;
const QPS: f64 = 600.0;
const FRAME: f64 = 16.0;
const SEEDS: [u64; 4] = [0x5eed, 0xabc1, 0x77f3, 0x1234];

fn crowd() -> Batch {
    Batch { clients: CLIENTS, servers: SERVERS, reqs_per_client: 1 }
}

/// One frame, the interleaved way — pump to each due send before routing it.
fn interleaved(e: &mut MultiEngine, dt: f64, steps: &mut usize, pumps: &mut usize) {
    let t_end = e.probe_now() + dt;
    let states = e.probe_states();
    while let Some((client, at)) = e.probe_next_send(t_end) {
        e.probe_pump(at.min(t_end), &states, steps);
        *pumps += 1;
        e.probe_send(client, at);
    }
    e.probe_pump(t_end, &states, steps);
    *pumps += 1;
    e.probe_fold();
}

/// One frame in `n` sub-steps — the sends due inside a sub-step placed, then the world caught
/// up to its edge.
fn substepped(n: usize) -> impl FnMut(&mut MultiEngine, f64, &mut usize, &mut usize) {
    move |e, dt, steps, pumps| {
        let t0 = e.probe_now();
        let states = e.probe_states();
        for k in 1..=n {
            let edge = t0 + dt * k as f64 / n as f64;
            while let Some((client, at)) = e.probe_next_send(edge) {
                e.probe_send(client, at);
            }
            e.probe_pump(edge, &states, steps);
            *pumps += 1;
        }
        e.probe_fold();
    }
}

fn run_for(e: &mut MultiEngine, secs: f64, tick: &mut impl FnMut(&mut MultiEngine, f64, &mut usize, &mut usize), steps: &mut usize, pumps: &mut usize) {
    for _ in 0..(secs * 1000.0 / FRAME) as usize {
        tick(e, FRAME, steps, pumps);
    }
}

/// Refusals over five seconds of load past the first — the reading the routing tests take.
fn refusals(
    policy: Policy,
    seed: u64,
    tick: &mut impl FnMut(&mut MultiEngine, f64, &mut usize, &mut usize),
    steps: &mut usize,
    pumps: &mut usize,
) -> (usize, usize) {
    let mut e = MultiEngine::flowing(seed, crowd(), QPS, policy);
    run_for(&mut e, 1.0, tick, steps, pumps);
    let cold = e.answered();
    run_for(&mut e, 5.0, tick, steps, pumps);
    let now = e.answered();
    (now.refusals - cold.refusals, now.trips - cold.trips)
}

fn report(label: &str, mut tick: impl FnMut(&mut MultiEngine, f64, &mut usize, &mut usize)) {
    for policy in [Policy::AlwaysRandom, Policy::RepickOnQueueTimeout, Policy::PowerOfTwoVirtual] {
        let mut refused = 0;
        let mut trips = 0;
        let t = Instant::now();
        let (mut steps, mut pumps) = (0, 0);
        for seed in SEEDS {
            let (r, n) = refusals(policy, seed, &mut tick, &mut steps, &mut pumps);
            refused += r;
            trips += n;
        }
        let frames = SEEDS.len() as f64 * 6.0 * 1000.0 / FRAME;
        println!(
            "{label:>14} {policy:?}: refused {:.3}% ({refused}/{trips}) · {:6.3} ms/frame · {:5.1} pumps · {:6.1} steps",
            refused as f64 / trips as f64 * 100.0,
            t.elapsed().as_secs_f64() * 1000.0 / frames,
            pumps as f64 / frames,
            steps as f64 / frames,
        );
    }
}

#[test]
fn probe_tradeoff() {
    report("interleaved", interleaved);
    for n in [8, 4, 2, 1] {
        report(&format!("{n} sub-steps"), substepped(n));
    }
}

#[test]
fn probe_pool_efficacy() {
    use crate::multi::QUEUE_TIME;
    let run = |policy: Policy, pool: Option<usize>| {
        let mut waits: Vec<f64> = Vec::new();
        let (mut refused, mut trips) = (0, 0);
        for seed in SEEDS {
            let mut e = MultiEngine::flowing(seed, crowd(), QPS, policy);
            if let Some(p) = pool {
                e.set_pool(p);
            }
            let (mut s, mut pu) = (0, 0);
            run_for(&mut e, 1.0, &mut interleaved, &mut s, &mut pu);
            let cold = e.answered();
            run_for(&mut e, 5.0, &mut interleaved, &mut s, &mut pu);
            let now = e.answered();
            refused += now.refusals - cold.refusals;
            trips += now.trips - cold.trips;
            e.with_records(|rs| {
                for r in rs {
                    waits.push(QUEUE_TIME.iter().filter_map(|sec| r.sections.get(sec)).sum());
                }
            });
        }
        waits.sort_by(f64::total_cmp);
        let at = |q: f64| waits[((waits.len() - 1) as f64 * q) as usize];
        (refused as f64 / trips as f64 * 100.0, at(0.5), at(0.9), at(0.99))
    };
    for (label, policy, pool) in [
        ("random", Policy::AlwaysRandom, None),
        ("virtual", Policy::PowerOfTwoVirtual, None),
        ("pool 2", Policy::PowerOfTwo, Some(2)),
        ("pool 3", Policy::PowerOfTwo, Some(3)),
        ("pool 5", Policy::PowerOfTwo, Some(5)),
        ("pool 10", Policy::PowerOfTwo, Some(10)),
    ] {
        let (refused, p50, p90, p99) = run(policy, pool);
        println!(
            "{label:>8}: refused {refused:.2}% · wait p50 {p50:5.1} p90 {p90:5.1} p99 {p99:6.1}"
        );
    }
}

#[test]
fn probe_pool_by_crowd() {
    use crate::multi::QUEUE_TIME;
    for clients in [10, 20, 50, 100] {
        let batch = Batch { clients, servers: SERVERS, reqs_per_client: 1 };
        for (label, policy, pool, ring) in [
            ("random", Policy::AlwaysRandom, None, false),
            ("pool 5", Policy::PowerOfTwo, Some(5), false),
            ("ring 3", Policy::PowerOfTwo, Some(3), true),
            ("ring 5", Policy::PowerOfTwo, Some(5), true),
            ("pool 10", Policy::PowerOfTwo, Some(10), false),
        ] {
            let mut waits: Vec<f64> = Vec::new();
            let (mut refused, mut trips) = (0, 0);
            for seed in SEEDS {
                let mut e = MultiEngine::flowing(seed, batch, QPS, policy);
                match (pool, ring) {
                    (Some(p), true) => e.set_pool_ring(p),
                    (Some(p), false) => e.set_pool(p),
                    _ => {}
                }
                let (mut s, mut pu) = (0, 0);
                run_for(&mut e, 1.0, &mut interleaved, &mut s, &mut pu);
                let cold = e.answered();
                run_for(&mut e, 5.0, &mut interleaved, &mut s, &mut pu);
                let now = e.answered();
                refused += now.refusals - cold.refusals;
                trips += now.trips - cold.trips;
                e.with_records(|rs| {
                    for r in rs {
                        waits.push(QUEUE_TIME.iter().filter_map(|sec| r.sections.get(sec)).sum());
                    }
                });
            }
            waits.sort_by(f64::total_cmp);
            let at = |q: f64| waits[((waits.len() - 1) as f64 * q) as usize];
            println!(
                "{clients:>3} clients {label:>8}: refused {:.2}% · wait p50 {:5.1} p90 {:5.1}",
                refused as f64 / trips as f64 * 100.0,
                at(0.5),
                at(0.9),
            );
        }
    }
}

#[test]
fn probe_edge() {
    use crate::multi::QUEUE_TIME;
    let run = |policy: Policy, clients: usize, lbs: usize, lifetime_ms: f64| {
        let mut totals: Vec<f64> = Vec::new();
        let mut waits: Vec<f64> = Vec::new();
        let mut shakes: Vec<f64> = Vec::new();
        let (mut refused, mut trips) = (0, 0);
        for seed in SEEDS {
            let mut e = MultiEngine::edged(seed, QPS, clients, lbs, lifetime_ms, policy);
            let (mut s, mut pu) = (0, 0);
            run_for(&mut e, 1.0, &mut interleaved, &mut s, &mut pu);
            let cold = e.answered();
            run_for(&mut e, 5.0, &mut interleaved, &mut s, &mut pu);
            let now = e.answered();
            refused += now.refusals - cold.refusals;
            trips += now.trips - cold.trips;
            e.with_records(|rs| {
                for r in rs {
                    totals.push(r.total_ms);
                    waits.push(QUEUE_TIME.iter().filter_map(|sec| r.sections.get(sec)).sum());
                    shakes.push(
                        r.sections.get(&crate::multi::Section::Handshake).copied().unwrap_or(0.0),
                    );
                }
            });
        }
        totals.sort_by(f64::total_cmp);
        waits.sort_by(f64::total_cmp);
        let at = |xs: &[f64], q: f64| xs[((xs.len() - 1) as f64 * q) as usize];
        let shake = shakes.iter().sum::<f64>() / shakes.len() as f64;
        (
            refused as f64 / trips as f64 * 100.0,
            at(&totals, 0.5),
            at(&totals, 0.9),
            at(&totals, 0.99),
            at(&waits, 0.9),
            shake,
        )
    };
    for (label, policy, clients, lbs, life) in [
        ("random lb", Policy::AlwaysRandom, 100, 20, 1000.0),
        ("p2 lb", Policy::PowerOfTwo, 100, 20, 1000.0),
        ("p2 400c", Policy::PowerOfTwo, 400, 20, 1000.0),
        ("p2 5lb", Policy::PowerOfTwo, 100, 5, 1000.0),
        ("p2 1lb", Policy::PowerOfTwo, 100, 1, 1000.0),
        ("p2 200ms", Policy::PowerOfTwo, 100, 20, 200.0),
        ("p2 10s", Policy::PowerOfTwo, 100, 20, 10_000.0),
    ] {
        let (refused, p50, p90, p99, w90, shake) = run(policy, clients, lbs, life);
        println!(
            "{label:>9}: refused {refused:.2}% · total p50 {p50:5.1} p90 {p90:5.1} p99 {p99:6.1} · wait p90 {w90:5.1} · handshake mean {shake:4.1}"
        );
    }
}

#[test]
fn probe_lines_cost() {
    let mut e = MultiEngine::flowing(0x5eed, crowd(), QPS, Policy::PowerOfTwoVirtual);
    for _ in 0..60 {
        e.tick(FRAME);
    }
    // Twenty sends a frame, and each one rebuilds the fleet's handles.
    let t = Instant::now();
    let n = 2000;
    let mut acc = 0;
    for _ in 0..n {
        acc += e.probe_lines();
    }
    println!(
        "lines() ×20 (one frame's sends): {:.4} ms · {acc}",
        t.elapsed().as_secs_f64() * 1000.0 / (n as f64 / 20.0),
    );
    let t = Instant::now();
    for _ in 0..120 {
        e.tick(FRAME);
    }
    println!("tick: {:.3} ms/frame", t.elapsed().as_secs_f64() * 1000.0 / 120.0);
}

#[test]
fn probe_edge_flip() {
    let mut e = MultiEngine::edged(0x5eed, QPS, 100, 20, 1000.0, Policy::AlwaysRandom);
    let (mut s, mut p) = (0, 0);
    run_for(&mut e, 3.0, &mut interleaved, &mut s, &mut p);
    let a = e.answered();
    run_for(&mut e, 5.0, &mut interleaved, &mut s, &mut p);
    let b = e.answered();
    e.set_policy(Policy::PowerOfTwo);
    run_for(&mut e, 2.0, &mut interleaved, &mut s, &mut p);
    let c = e.answered();
    run_for(&mut e, 5.0, &mut interleaved, &mut s, &mut p);
    let d = e.answered();
    println!(
        "random {:.2}% -> p2 {:.2}%",
        (b.refusals - a.refusals) as f64 / (b.trips - a.trips) as f64 * 100.0,
        (d.refusals - c.refusals) as f64 / (d.trips - c.trips) as f64 * 100.0,
    );
}
