//! **Sim 7 · The balancer tier** — a churning crowd, the balancers that stand for it, and
//! five percentiles scrolling as the reader turns the knobs.
//!
//! The pool card ends on a wall: a client's counters are only as fresh as its own answer
//! rate, and a big crowd cannot buy freshness at any pool size. This card puts the fix on
//! stage. Clients are short-lived and dumb — one balancer each, picked at birth, random —
//! and the balancers hold the warm lines and the heard tables, fed by the whole crowd's
//! traffic. Every setting is a slider because every claim in the prose is a ratio: crowd
//! size against balancer count, lifetime against handshake, and the one toggle that turns
//! the tier's information on.
//!
//! There are no frozen columns here. The comparison is time itself: percentile rows scroll
//! like a latency dashboard, a changed setting drops a faint rule on the timeline, and what
//! the change did *is the line moving*.

use std::collections::VecDeque;

use idyll::{live_view, Ctx, MutableVec, Rect, Setup, Shape, Signal};
use idyll_styles::styles;

use crate::atoms::button::styles as bstyles;
use crate::atoms::client_strip::{ink, ink_frame, ClientStrip};
use crate::atoms::cut::ramp;
use crate::atoms::lanes::{Ink, LaneTier, WIRES};
use crate::atoms::server_box::{group, server_name, styles as sbstyles, ServerBox};
use crate::atoms::wires::{
    bow_down, set_rect, standing, styles as wstyles, wires_down, Bow, IDLE,
};
use crate::atoms::sim_card::{styles as card, SimHeader};
use crate::atoms::slider::{fmt_qps, fmt_speed, raw_from_speed, speed_from_raw, Name, Scale, Slider};
use crate::engine::NET_MS;
use crate::atoms::strips::{
    mark_path, note, slot_x, y_of, StripRow, Strips, POINTS, SAMPLE_MS,
};
use crate::atoms::toggle::{knob, Tone, ToggleGroup, ToggleItem};
use crate::engine::ServerCounts;
use crate::flow::styles as fstyles;
use crate::multi::{
    sim_seed, Leg, MultiEngine, Policy, EDGE_RTT_MS, MAX_CLIENTS, MAX_LBS, QUEUE_TIME, SERVERS,
};
use blog_core::SimKey;

#[derive(Debug)]
pub enum LbMsg {
    Tick(f64),
    Toggle,
    Qps(f64),
    Clients(f64),
    Lbs(f64),
    Servers(f64),
    Lifetime(f64),
    /// Dilate the engine's clock — slow motion. Fronts stretch with it; afterglow does not.
    Speed(f64),
    /// Which policy the balancers run — the card's whole question.
    Policy(usize),
    /// Which reading the rows show. A view change: the lines redraw from the same history,
    /// and no rule is dropped, because nothing happened to the machine.
    Metric(usize),
    /// The tier stage's laid-out rect — the origin the client wires are drawn against.
    Stage(Rect),
    /// A client circle's rect, one end of its wire.
    ClientDot(usize, Rect),
    /// A balancer box's rect — the far end of hop one, the near end of hop two.
    Chip(usize, Rect),
    /// A server box's rect, where hop two lands.
    Server(usize, Rect),
}

/// Where the card opens: ¶52's own example — a hundred clients through twenty balancers —
/// on the chapter's ten-machine fleet at its loaded-but-serving rate, lives of a second.
const OPENS_QPS: f64 = 600.0;
const OPENS_CLIENTS: usize = 100;
const OPENS_LBS: usize = MAX_LBS;
const OPENS_LIFE_MS: f64 = 1000.0;

/// The most virtual time one frame may advance — the same clamp the flow cards wear.
const MAX_STEP_MS: f64 = 32.0;

/// How many answered trips a sample is cut from — the same window, for the same burst-noise
/// reasons, as a policy column keeps.
const WINDOW: usize = 2000;

/// Headroom past the tallest reading, so p99 does not ride the ceiling.
const AXIS_HEADROOM: f64 = 1.05;

/// The rows, highest cut last so the tail sits at the bottom nearest the axis label.
const CUTS: [(&str, f64); 5] =
    [("p25", 25.0), ("p50", 50.0), ("p75", 75.0), ("p90", 90.0), ("p99", 99.0)];

/// The balancers' two minds, on the pill.
const POLICIES: [(&str, Policy); 2] =
    [("Random", Policy::AlwaysRandom), ("Power of Two", Policy::PowerOfTwo)];

/// The two readings of the same trips: what the client felt, and the slice routing decides.
const METRICS: [&str; 2] = ["round trip", "queue wait"];

/// One sample: the five cuts of both metrics, taken together from one window so switching
/// metrics rereads history instead of restarting it.
#[derive(Clone, Copy)]
struct Sample {
    total: [f64; 5],
    wait: [f64; 5],
}

impl Sample {
    fn of(self, metric: usize) -> [f64; 5] {
        match metric {
            0 => self.total,
            _ => self.wait,
        }
    }
}

pub(crate) async fn run(
    ctx: Ctx<Setup, LbMsg>,
    _seed: crate::PageSeed,
    key: SimKey,
) -> idyll::Result {
    let mut engine = MultiEngine::edged(
        sim_seed(&key),
        OPENS_QPS,
        OPENS_CLIENTS,
        OPENS_LBS,
        OPENS_LIFE_MS,
        Policy::AlwaysRandom,
    );

    let running = ctx.mutable_signal(false);
    ctx.frames(&running.read(), LbMsg::Tick);
    let run_label = {
        let running = running.read();
        ctx.computed(move |cx| match running.get(cx) {
            true => "❚❚ Pause".to_string(),
            false => "▶ Run".to_string(),
        })
        .read()
    };

    let qps = ctx.mutable_signal(OPENS_QPS);
    let clients = ctx.mutable_signal(OPENS_CLIENTS as f64);
    let lbs = ctx.mutable_signal(OPENS_LBS as f64);
    let servers = ctx.mutable_signal(SERVERS as f64);
    let lifetime = ctx.mutable_signal(OPENS_LIFE_MS);
    let served = ctx.mutable_signal(group(0));
    let refused = ctx.mutable_signal(group(0));
    let aggs = vec![("served", served.read()), ("retried", refused.read())];

    // Ten boxes always: the fleet is built once, and the slider decides who is in rotation.
    // A box out of rotation greys rather than leaves — a drained machine is still a machine.
    let active = ctx.mutable_signal(SERVERS);
    let counts: Vec<_> =
        (0..SERVERS).map(|_| ctx.mutable_signal(ServerCounts::default())).collect();
    let boxes: Vec<(usize, String, Signal<ServerCounts>, Signal<bool>)> = (0..SERVERS)
        .map(|i| {
            let active = active.read();
            let idle = ctx.computed(move |cx| i >= active.get(cx)).read();
            (i, server_name(i), counts[i].read(), idle)
        })
        .collect();

    let policy = ctx.mutable_signal(0usize);
    let policy_items: Vec<ToggleItem> = POLICIES
        .iter()
        .enumerate()
        .map(|(i, (label, _))| {
            let chosen = policy.read();
            (i, (*label).into(), ctx.computed(move |cx| chosen.get(cx) == i).read(), Tone::Normal)
        })
        .collect();
    let policy_knob = knob(&ctx, &policy_items);

    // The crowd itself: one circle per client above the tier it talks to, orange while its
    // answer is owed.
    let ink_signals: Vec<_> = (0..MAX_CLIENTS).map(|_| ctx.mutable_signal(ink(false))).collect();
    let strip: Vec<(usize, Signal<String>)> =
        ink_signals.iter().enumerate().map(|(c, s)| (c, s.read())).collect();
    let strip_count = {
        let clients = clients.read();
        ctx.computed(move |cx| clients.get(cx) as usize).read()
    };
    let dot_measured = ctx.callback(|(c, rect): (usize, Rect)| LbMsg::ClientDot(c, rect));
    // Each client's wire to the balancer it lives on — hop one's warm lines, following the
    // homes so a rebirth moves only its own — and hop two's, balancers to machines in
    // rotation, one lattice. What travels is two-edge pulses on each hop's own lanes:
    // ten-millisecond edge fronts, six-millisecond machine fronts, both on the one canvas.
    let hops: MutableVec<Shape> = ctx.mutable_vec();
    let traffic: MutableVec<Shape> = ctx.mutable_vec();
    let picture = ctx.mutable_vec_of(vec![hops.clone(), traffic.clone()]);
    let mut edge_tier = LaneTier::new(WIRES, EDGE_RTT_MS / 2.0);
    let mut hop_tier = LaneTier::new(WIRES, NET_MS);
    let speed = ctx.mutable_signal(raw_from_speed(1.0));
    let speed_at = speed.read();

    // The balancer boxes: twenty always, greyed out of rotation like the servers — a tier's
    // machines are machines, in the fleet's own grid with the fleet's own labelled rows.
    // A forwarder has two numbers: who lives on it, and what is passing through it.
    let lbs_active = ctx.mutable_signal(OPENS_LBS);
    let lb_counts: Vec<_> = (0..MAX_LBS).map(|_| ctx.mutable_signal((0usize, 0usize))).collect();
    #[allow(clippy::type_complexity)]
    let lb_chips: Vec<(
        usize,
        String,
        Signal<String>,
        Signal<bool>,
        Signal<String>,
        Signal<bool>,
        Signal<bool>,
    )> = (0..MAX_LBS)
        .map(|i| {
            let active = lbs_active.read();
            let idle = ctx.computed(move |cx| i >= active.get(cx)).read();
            let row = |get: fn(&(usize, usize)) -> usize| {
                let counts = lb_counts[i].read();
                let text = {
                    let counts = counts.clone();
                    ctx.computed(move |cx| group(get(&counts.get(cx)))).read()
                };
                let zero = ctx.computed(move |cx| get(&counts.get(cx)) == 0).read();
                (text, zero)
            };
            let (clients, clients_zero) = row(|c| c.0);
            let (inflight, inflight_zero) = row(|c| c.1);
            (i, format!("lb-{:02}", i + 1), clients, clients_zero, inflight, inflight_zero, idle)
        })
        .collect();

    let metric = ctx.mutable_signal(0usize);
    let metric_items: Vec<ToggleItem> = METRICS
        .iter()
        .enumerate()
        .map(|(i, label)| {
            let chosen = metric.read();
            (i, (*label).into(), ctx.computed(move |cx| chosen.get(cx) == i).read(), Tone::Normal)
        })
        .collect();
    let metric_knob = knob(&ctx, &metric_items);

    let rows = ctx.mutable_signal(strip_rows(&VecDeque::new(), 0, 1.0));
    let marks = ctx.mutable_signal(String::new());

    let picked_policy = ctx.callback(LbMsg::Policy);
    let picked_metric = ctx.callback(LbMsg::Metric);
    let qps_at = qps.read();
    let clients_at = clients.read();
    let lbs_at = lbs.read();
    let servers_at = servers.read();
    let lifetime_at = lifetime.read();
    let rows_sig = rows.read();
    let marks_sig = marks.read();
    let title = "Load balancers · clients → balancers → servers";

    let mut ctx = ctx.render(live_view! {
        div css=[card::CARD] role=("group") aria_label=(title) {
            SimHeader title=(title) aggs=(aggs)
            div css=[card::CTRLS] {
                button css=[bstyles::BASE, bstyles::CTA]
                    onclick=>(|_| Some(LbMsg::Toggle)) { $run_label }
                Slider name=(Name::new("arrivals", 52)) scale=(Scale::new(50, 900, 10))
                    at=(qps_at) fmt=(fmt_qps) moved=>(|n| LbMsg::Qps(n))
                Slider name=(Name::new("lifetime", 52)) scale=(Scale::new(200, 10_000, 200))
                    at=(lifetime_at) fmt=(fmt_life) moved=>(|n| LbMsg::Lifetime(n))
                Slider name=(Name::new("speed", 38)) scale=(Scale::new(0, 100, 1))
                    at=(speed_at) fmt=(fmt_speed) moved=>(|n| LbMsg::Speed(n))
            }
            div css=[card::CTRLS] {
                Slider name=(Name::new("clients", 46)) scale=(Scale::new(10, MAX_CLIENTS as u32, 10))
                    at=(clients_at) fmt=(fmt_count) moved=>(|n| LbMsg::Clients(n))
                Slider name=(Name::new("balancers", 58)) scale=(Scale::new(1, MAX_LBS as u32, 1))
                    at=(lbs_at) fmt=(fmt_count) moved=>(|n| LbMsg::Lbs(n))
                Slider name=(Name::new("servers", 46)) scale=(Scale::new(2, 10, 1))
                    at=(servers_at) fmt=(fmt_count) moved=>(|n| LbMsg::Servers(n))
            }
            div css=[card::CTRLS] {
                ToggleGroup items=(policy_items) knob=(policy_knob) picked=(picked_policy)
            }
            div css=[wstyles::TIER] measure=>(|e| e.rect().map(LbMsg::Stage)) {
                ClientStrip dots=(strip) count=(strip_count) measured=(dot_measured)
                div css=[fstyles::LABEL] { "balancers" }
                div css=[fstyles::FLEET] {
                    @for (at, name, clients, clients_zero, inflight, inflight_zero, idle) in (lb_chips) {
                        div css=[sbstyles::BOX, styles::CHIP, $idle => sbstyles::IDLE]
                            measure=>(move |e| e.rect().map(|r| LbMsg::Chip(at, r))) {
                            div css=[sbstyles::SH] { span css=[sbstyles::SN] { (name) } }
                            div css=[sbstyles::M] {
                                span css=[sbstyles::ML] { "clients" }
                                span css=[sbstyles::MV, $clients_zero => sbstyles::ZERO] { $clients }
                            }
                            div css=[sbstyles::M] {
                                span css=[sbstyles::ML] { "inflight" }
                                span css=[sbstyles::MV, $inflight_zero => sbstyles::ZERO] { $inflight }
                            }
                        }
                    }
                }
                div css=[fstyles::FLEET] {
                    @for (at, name, counts, idle) in (boxes) {
                        div measure=>(move |e| e.rect().map(|r| LbMsg::Server(at, r))) {
                            ServerBox name=(name) counts=(counts) idle=(idle)
                        }
                    }
                }
                canvas css=[wstyles::WIRES, wstyles::UNDER] painting=(picture) {}
            }
            div css=[fstyles::LABEL] { "experience · shared ms scale · last 30 s" }
            div css=[card::CTRLS] {
                ToggleGroup items=(metric_items) knob=(metric_knob) picked=(picked_metric)
            }
            Strips rows=(rows_sig) labels=(36) marks=(marks_sig)
        }
    }).await?;

    let mut credited = 0usize;
    // The trips a sample is cut from, and the samples the rows draw — both bounded windows,
    // both the loop's own values; the signals carry only what the view shows.
    let mut window: VecDeque<(f64, f64)> = VecDeque::new();
    let mut samples: VecDeque<Sample> = VecDeque::new();
    // How many samples have ever been taken — the timeline's own clock, which a mark's place
    // and a stored sample's slot are both measured against.
    let mut sampled = 0u64;
    let mut marked: VecDeque<u64> = VecDeque::new();
    let mut since_sample = 0.0;
    let mut axis = 1.0f64;
    let mut showing = 0usize;
    let mut playing = false;
    // The crowd's faces, mirrored locally so only changes touch signals, and the measured ends
    // every wire is drawn between.
    let mut faces: Vec<String> = vec![ink(false); MAX_CLIENTS];
    let mut stage_rect: Option<Rect> = None;
    let mut dot_rects: Vec<Option<Rect>> = vec![None; MAX_CLIENTS];
    let mut chip_rects: Vec<Option<Rect>> = vec![None; MAX_LBS];
    let mut box_rects: Vec<Option<Rect>> = vec![None; SERVERS];
    let mut lbs_now = OPENS_LBS;
    let mut servers_now = SERVERS;
    let mut speed_now = 1.0f64;
    // Where each client lives, as the wires under the traffic are drawn from it: a rebirth
    // moves one client's wire, and nothing else about a frame moves any of them.
    let mut homes = engine.homes();
    loop {
        let (msg, turn) = ctx.recv().await?;
        let mut relaid = false;
        match msg {
            LbMsg::Toggle => {
                playing = !playing;
                // Pausing stops departures, never travel already in the air — and the picture
                // is drawn by this loop or not at all, so the loop outlives the last pulse.
                running.set(&turn, playing || edge_tier.airborne() || hop_tier.airborne());
            }
            LbMsg::Qps(v) => {
                qps.set(&turn, v);
                engine.set_qps(v);
                note(&mut marked, sampled);
            }
            LbMsg::Clients(v) => {
                clients.set(&turn, v);
                engine.set_clients(v as usize);
                note(&mut marked, sampled);
            }
            LbMsg::Lbs(v) => {
                lbs.set(&turn, v);
                engine.set_lbs(v as usize);
                lbs_active.set(&turn, v as usize);
                lbs_now = v as usize;
                relaid = true;
                note(&mut marked, sampled);
            }
            LbMsg::Servers(v) => {
                servers.set(&turn, v);
                engine.set_servers(v as usize);
                active.set(&turn, v as usize);
                servers_now = v as usize;
                relaid = true;
                note(&mut marked, sampled);
            }
            LbMsg::Lifetime(v) => {
                lifetime.set(&turn, v);
                engine.set_lifetime(v);
                note(&mut marked, sampled);
            }
            LbMsg::Speed(raw) => {
                speed.set(&turn, raw);
                speed_now = speed_from_raw(raw);
            }
            LbMsg::Policy(i) => {
                policy.set(&turn, i);
                engine.set_policy(POLICIES[i].1);
                note(&mut marked, sampled);
            }
            LbMsg::Stage(rect) => {
                stage_rect = Some(rect);
                relaid = true;
            }
            LbMsg::ClientDot(c, rect) => {
                set_rect(&mut dot_rects, c, rect);
                relaid = true;
            }
            LbMsg::Chip(l, rect) => {
                set_rect(&mut chip_rects, l, rect);
                relaid = true;
            }
            LbMsg::Server(s, rect) => {
                set_rect(&mut box_rects, s, rect);
                relaid = true;
            }
            LbMsg::Metric(i) => {
                metric.set(&turn, i);
                showing = i;
                // The reader asked for the other reading of the same history: the axis and
                // the lines take it whole rather than easing from numbers of a different kind.
                axis = axis_for(&samples, showing);
                rows.set(&turn, strip_rows(&samples, showing, axis));
            }
            LbMsg::Tick(wall) => {
                // The reader's clock and the fleet's: a paused card advances no virtual time,
                // and goes on drawing until what was already sent has landed.
                let wall = wall.min(MAX_STEP_MS);
                let dt = match playing {
                    true => wall,
                    false => 0.0,
                };
                engine.tick(dt * speed_now);
                let fleet = engine.fleet();
                for (signal, server) in counts.iter().zip(&fleet) {
                    signal.set(&turn, server.counts);
                }
                for (signal, lb) in lb_counts.iter().zip(engine.balancers()) {
                    signal.set(&turn, (lb.clients, lb.inflight));
                }
                let answered = engine.answered();
                served.set(&turn, group(answered.trips));
                refused.set(&turn, group(answered.refusals));

                // The crowd's heartbeat: orange while owed.
                ink_frame(&turn, &mut faces, &ink_signals, &engine.outstanding());
                // The frame's departures become pulses on their own hop: the client's send
                // and its answer on the edge wires, the balancer's asking — refusals and
                // all — on the machine wires.
                let mut edge_launches = Vec::new();
                let mut hop_launches = Vec::new();
                for event in engine.wire_events() {
                    let ink = match (event.homeward, event.outcome) {
                        (false, _) => Ink::Sent,
                        (true, Some(outcome)) if outcome.refused() => Ink::Refusal,
                        (true, _) => Ink::Answer,
                    };
                    match event.leg {
                        Leg::Edge { client, lb } => {
                            edge_launches.push(((client, lb), event.homeward, ink));
                        }
                        Leg::Server { sender, server } => {
                            hop_launches.push(((sender, server), event.homeward, ink));
                        }
                    }
                }
                edge_tier.frame(edge_launches, wall, speed_now);
                hop_tier.frame(hop_launches, wall, speed_now);
                running.set(&turn, playing || edge_tier.airborne() || hop_tier.airborne());

                let living = engine.homes();
                if living != homes {
                    homes = living;
                    relaid = true;
                }

                let landed = answered.trips - credited;
                credited = answered.trips;
                engine.with_records(|records| {
                    for r in records.iter().rev().take(landed).rev() {
                        let wait: f64 =
                            QUEUE_TIME.iter().filter_map(|s| r.sections.get(s)).sum();
                        if window.len() >= WINDOW {
                            window.pop_front();
                        }
                        window.push_back((r.total_ms, wait));
                    }
                });
                since_sample += dt;
                if since_sample >= SAMPLE_MS && !window.is_empty() {
                    since_sample = 0.0;
                    if samples.len() >= POINTS {
                        samples.pop_front();
                    }
                    samples.push_back(cut_sample(&window));
                    sampled += 1;
                    while marked.front().is_some_and(|&m| m + POINTS as u64 <= sampled) {
                        marked.pop_front();
                    }
                    axis += (axis_for(&samples, showing) - axis) * 0.3;
                    rows.set(&turn, strip_rows(&samples, showing, axis));
                    marks.set(&turn, mark_path(&marked, sampled, samples.len()));
                }
            }
        }
        let at = Ends {
            stage: &stage_rect,
            dots: &dot_rects,
            chips: &chip_rects,
            boxes: &box_rects,
        };
        if relaid {
            hops.sync(
                &turn,
                hops_of(&at, &homes, Rotation { lbs: lbs_now, servers: servers_now }),
            );
        }
        traffic.sync(&turn, traffic_of(&at, &edge_tier, &hop_tier));
    }
}

/// Where the two hops' ends stand, as the layout has placed them.
struct Ends<'a> {
    stage: &'a Option<Rect>,
    dots: &'a [Option<Rect>],
    chips: &'a [Option<Rect>],
    boxes: &'a [Option<Rect>],
}

/// Who is in rotation: the balancers the crowd is spread over, and the machines they ask.
#[derive(Clone, Copy)]
struct Rotation {
    lbs: usize,
    servers: usize,
}

impl Ends<'_> {
    /// The bow a client's wire down to its balancer runs on — the one geometry both the
    /// standing wire and the traffic on it come off.
    fn edge(&self, (client, lb): (usize, usize)) -> Option<Bow> {
        let stage = self.stage.as_ref()?;
        Some(bow_down(stage, self.dots.get(client)?.as_ref()?, self.chips.get(lb)?.as_ref()?))
    }

    /// The same, one hop down: a balancer's wire to a machine.
    fn hop(&self, (lb, server): (usize, usize)) -> Option<Bow> {
        let stage = self.stage.as_ref()?;
        Some(bow_down(stage, self.chips.get(lb)?.as_ref()?, self.boxes.get(server)?.as_ref()?))
    }
}

/// The layer under the traffic: hop one as one wire per living client, down to the balancer
/// it lives on, and hop two as the rotation — every standing balancer to every machine still
/// in it.
fn hops_of(at: &Ends, homes: &[Option<usize>], rotation: Rotation) -> Vec<Shape> {
    let mut layer = Vec::new();
    for (client, home) in homes.iter().enumerate() {
        if let Some(bow) = home.and_then(|lb| at.edge((client, lb))) {
            layer.push(standing(bow, IDLE));
        }
    }
    wires_down(
        at.stage,
        at.chips,
        at.boxes,
        (0..rotation.lbs).flat_map(|l| (0..rotation.servers).map(move |s| (l, s))),
        IDLE,
        &mut layer,
    );
    layer
}

/// The layer over it: each hop's traffic, riding the wires it was launched on.
fn traffic_of(at: &Ends, edge_tier: &LaneTier, hop_tier: &LaneTier) -> Vec<Shape> {
    let mut layer = Vec::new();
    edge_tier.paint(|wire| at.edge(wire), &mut layer);
    hop_tier.paint(|wire| at.hop(wire), &mut layer);
    layer
}

/// The five cuts of the window, both metrics at once — one sorted pass each, nearest-rank
/// like every other percentile in the chapter.
fn cut_sample(window: &VecDeque<(f64, f64)>) -> Sample {
    let mut totals: Vec<f64> = window.iter().map(|&(t, _)| t).collect();
    let mut waits: Vec<f64> = window.iter().map(|&(_, w)| w).collect();
    totals.sort_by(f64::total_cmp);
    waits.sort_by(f64::total_cmp);
    let at = |xs: &[f64], pc: f64| xs[((xs.len() - 1) as f64 * pc / 100.0).round() as usize];
    let cut = |xs: &[f64]| CUTS.map(|(_, pc)| at(xs, pc));
    Sample { total: cut(&totals), wait: cut(&waits) }
}

/// The axis the rows share: the tallest reading in view — the newest p99 is usually it, but
/// a spike scrolling out decides until it is gone — with headroom.
fn axis_for(samples: &VecDeque<Sample>, metric: usize) -> f64 {
    samples.iter().map(|s| s.of(metric)[CUTS.len() - 1]).fold(1.0f64, f64::max) * AXIS_HEADROOM
}

/// The rows as drawn: each percentile's line over every stored sample, scaled to the strip,
/// wearing the colour its percentile wears on a dial.
fn strip_rows(samples: &VecDeque<Sample>, metric: usize, axis: f64) -> Vec<StripRow> {
    CUTS.iter()
        .enumerate()
        .map(|(row, &(label, pc))| {
            let points = samples
                .iter()
                .enumerate()
                .map(|(j, s)| {
                    let v = s.of(metric)[row];
                    format!("{:.1},{:.1}", slot_x(j), y_of(v, axis))
                })
                .collect::<Vec<_>>()
                .join(" ");
            let reading = match samples.back() {
                Some(s) => format!("{:.0} ms", s.of(metric)[row]),
                None => "– ms".to_string(),
            };
            StripRow { label, points, reading, ink: ramp(pc) }
        })
        .collect()
}

fn fmt_count(v: f64) -> String {
    format!("{v:.0}")
}

fn fmt_life(v: f64) -> String {
    format!("{:.1} s", v / 1000.0)
}

#[styles]
mod styles {
    use idyll_styles::Style;

    /// What a balancer box adds to the server box it copies: the reading sizes the fleet's
    /// rows wear, on a box with only two rows to wear them.
    pub const CHIP: Style = css! {{
        font_size: "10.5px",
        font_variant_numeric: "tabular-nums",
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    fn steady(ms: f64) -> VecDeque<(f64, f64)> {
        (0..100).map(|_| (ms, ms / 2.0)).collect()
    }

    /// One window, both readings, five ordered cuts — a sample is a partition of nothing:
    /// just the same trips read five ways, so the cuts cannot cross.
    #[test]
    fn a_sample_cuts_both_metrics_in_order() {
        let window: VecDeque<(f64, f64)> =
            (1..=200).map(|i| (i as f64, i as f64 / 2.0)).collect();
        let s = cut_sample(&window);
        for w in s.total.windows(2) {
            assert!(w[0] <= w[1], "cuts in order: {:?}", s.total);
        }
        assert!(s.wait[4] <= s.total[4], "waiting is part of the trip");
    }

    /// The rows draw what the metric pill says from the same stored history.
    #[test]
    fn switching_metric_rereads_history() {
        let mut samples = VecDeque::new();
        samples.push_back(cut_sample(&steady(100.0)));
        let total = strip_rows(&samples, 0, 200.0);
        let wait = strip_rows(&samples, 1, 200.0);
        assert_eq!(total[1].reading, "100 ms");
        assert_eq!(wait[1].reading, "50 ms");
        assert_ne!(total[1].points, wait[1].points, "the lines move with the reading");
    }

    /// A mark scrolls with the samples it was dropped between and leaves with them.
    #[test]
    fn a_mark_rides_the_timeline_and_falls_off_its_end() {
        let mut marked = VecDeque::new();
        note(&mut marked, 10);
        note(&mut marked, 10);
        assert_eq!(marked.len(), 1, "a dragged slider is one change");
        let on_stage = mark_path(&marked, 20, 15);
        assert!(on_stage.contains("M "), "still on stage: {on_stage}");
        let gone = mark_path(&marked, 200, POINTS.min(120));
        assert_eq!(gone, "", "scrolled out with its history");
    }
}
