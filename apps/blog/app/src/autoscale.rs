//! **Autoscale** — the fleet that buys itself a machine, and the two readings it can buy on.
//!
//! Every other fleet card in the chapter has ten machines and asks what to do with them. This
//! one asks how many there should be. The crowd, the wires and the boxes are the flow card's
//! ([`MultiEngine::flowing`], ten real taipei stacks); what is new is that only some of them
//! are in rotation, and something moves that number while the reader watches.
//!
//! Two scalers run side by side on the same demand, and the pill says which one the reader is
//! looking at. Both are fed the same arrival stream from the same seed, so what separates the
//! two histories is the reading each steers by and nothing else — the whole comparison the
//! section's aside is making. Switching does not restart either: the fleet the reader turns
//! to has been running all along, which is the only way its lag is visible at all.
//!
//! What each reading is, and why one of them is late, is [`crate::scale`].

use std::collections::VecDeque;

use idyll::{live_view, Ctx, MutableVec, Rect, Setup, Shape, Signal};

use crate::atoms::figure::{count, slots};
use crate::atoms::client_strip::{ink, ink_frame, ClientStrip};
use crate::atoms::button::styles as bstyles;
use crate::atoms::lanes::{Ink, LaneTier, WIRES};
use crate::atoms::legend::{Key, Legend};
use crate::atoms::server_box::{server_name, ServerBox};
use crate::atoms::sim_card::{styles as card, SimHeader};
use crate::atoms::slider::{fmt_speed, raw_from_speed, speed_from_raw, Name, Scale, Slider};
use crate::atoms::stage::{Paint, Stage};
use crate::atoms::strips::{
    mark_path, note, slot_x, y_of, StripRow, Strips, POINTS, SAMPLE_MS,
};
use crate::atoms::toggle::{knob, Tone, ToggleGroup, ToggleItem};
use crate::atoms::wires::{bow_down, set_rect, styles as wstyles, wires_down, IDLE};
use crate::engine::{ServerCounts, ADMISSION_LIMIT, NET_MS};
use crate::flow::styles as fstyles;
use crate::multi::{sim_seed, Batch, Leg as WireLeg, MultiEngine, Policy, SERVERS};
use crate::scale::{self, Reading, Scaler, Signal as Steer};
use blog_core::SimKey;

/// The crowd: the same hundred clients the rest of the chapter loads its fleet with, so a
/// machine here means what a machine there meant.
const CLIENTS: usize = 100;

/// The rate the card opens at: about half of what the ten-machine fleet serves flat out, so
/// the fleet this demand deserves is about half the fleet — room to climb when the reader
/// pushes the slider, and room to give machines back when they pull it down. A card that
/// opened at the ceiling could only ever be watched scaling one way.
const OPENS_AT: f64 = 300.0;

/// The rotation the card opens with: fewer machines than that demand needs, so the first
/// thing Run shows is the loop closing rather than a fleet already at rest.
const OPENS_WITH: usize = 3;

/// The speed the card opens at.
///
/// Real time, where the single-server sims crawl to follow one request through a machine.
/// A scaler's cycle is a second and the CPU average is three, so at a hundredth of speed a
/// reader would wait five minutes to watch a machine arrive. The lag *is* the subject here,
/// and a subject has to fit inside the reader's attention.
const SPEED: f64 = 1.0;

/// The most virtual time one frame may advance — as everywhere: a tab returning from the
/// background hands over a delta of seconds, and a message arm runs to completion.
const MAX_STEP_MS: f64 = 32.0;

/// The two scalers, in pill order, and what the reader calls them.
const STEERS: [(&str, Steer); 2] = [("queue", Steer::Queue), ("CPU-utilisation", Steer::Cpu)];

/// The most the reader can ask for, and what the arrivals row is drawn against — so the row
/// states the demand as a fraction of what can be demanded rather than of itself.
///
/// Just past what the ten machines serve flat out. A ceiling much above that is a fleet with
/// nowhere left to scale, refusing whatever it is given: past the top of this slider the card
/// would have nothing left to show.
const ARRIVALS_MAX: f64 = 700.0;

#[derive(Debug)]
pub enum AutoscaleMsg {
    Tick(f64),
    /// The run control. A card that ran on mount would have spent the reader's first second
    /// of attention before they arrived.
    Toggle,
    Qps(f64),
    /// Dilate the engine's clock. The scalers count in the fleet's time, not the reader's, so
    /// slowing the picture slows the loop with it.
    Speed(f64),
    /// Show the fleet this scaler has been running. Neither is restarted.
    Steer(usize),
    /// The stage and its wire ends, as layout places them.
    Stage(Rect),
    Dot(usize, Rect),
    Server(usize, Rect),
}

/// One sample slot: what the reader asked for, what the scaler bought, and the reading it
/// bought it on — which is a different number in each arm, and the reason the third row
/// changes when the pill does.
#[derive(Clone, Copy)]
struct Sample {
    arrivals: f64,
    machines: f64,
    signal: f64,
}

/// What one frame of an arm changed that the card has to answer for.
struct Moved {
    /// The rotation changed — and so did every wire on the stage.
    rotation: bool,
    /// A sample slot closed — the rows have a point they did not have.
    sample: bool,
}

/// One scaler and the fleet it is steering — everything that has to keep running whether the
/// reader is looking at it or not.
struct Arm {
    steer: Steer,
    engine: MultiEngine,
    scaler: Scaler,
    /// The rotation the engine is actually holding, so a cycle that decided nothing costs
    /// nothing.
    rotation: usize,
    tier: LaneTier,
    samples: VecDeque<Sample>,
    since_sample: f64,
}

impl Arm {
    fn new(key: &SimKey, steer: Steer) -> Arm {
        let batch = Batch { clients: CLIENTS, servers: SERVERS, reqs_per_client: 1 };
        // Both arms take the same seed: same clients, same arrival instants, same work. What
        // separates their histories is the scaler, and a second seed would put luck in the
        // comparison too.
        let mut engine =
            MultiEngine::flowing(sim_seed(key), batch, OPENS_AT, Policy::PowerOfTwo);
        engine.set_servers(OPENS_WITH);
        Arm {
            steer,
            engine,
            scaler: Scaler::new(steer, OPENS_WITH, SERVERS),
            rotation: OPENS_WITH,
            tier: LaneTier::new(WIRES, NET_MS),
            samples: VecDeque::new(),
            since_sample: 0.0,
        }
    }

    /// One frame of this arm's fleet: run it, let the scaler look, and take a sample if the
    /// quarter-second has come round.
    fn frame(&mut self, dt: f64, wall: f64, speed: f64, arrivals: f64) -> Moved {
        self.engine.tick(dt * speed);

        let fleet = self.engine.fleet();
        let busy: usize = fleet.iter().take(self.rotation).map(|s| s.counts.busy).sum();
        let believed = self.engine.believed();
        let want = self.scaler.frame(dt, Reading { believed: &believed, busy });
        let rotation = want != self.rotation;
        if rotation {
            self.engine.set_servers(want);
            self.rotation = want;
        }

        let launches = self.engine.wire_events().into_iter().filter_map(|event| {
            let WireLeg::Server { sender, server } = event.leg else { return None };
            let ink = match (event.homeward, event.outcome) {
                (false, _) => Ink::Sent,
                (true, Some(outcome)) if outcome.refused() => Ink::Refusal,
                (true, _) => Ink::Answer,
            };
            Some(((sender, server), event.homeward, ink))
        });
        self.tier.frame(launches, wall, speed);

        self.since_sample += dt;
        let sample = self.since_sample >= SAMPLE_MS;
        if sample {
            self.samples.push_back(Sample {
                arrivals,
                machines: self.rotation as f64,
                signal: self.signal(),
            });
            self.since_sample = 0.0;
            if self.samples.len() > POINTS {
                self.samples.pop_front();
            }
        }
        Moved { rotation, sample }
    }

    /// The reading this arm steers by, as a number: what is waiting per machine, or the
    /// cores' trailing average as a percentage.
    fn signal(&self) -> f64 {
        match self.steer {
            Steer::Queue => self.scaler.queued(),
            Steer::Cpu => self.scaler.util() * 100.0,
        }
    }

    /// The same reading as the card states it — the one thing the two arms cannot share.
    fn steering(&self) -> String {
        match self.steer {
            Steer::Queue => format!(
                "clear · {:.0}% of {:.0}%",
                self.scaler.clear() * 100.0,
                scale::CLEAR_TARGET * 100.0,
            ),
            Steer::Cpu => format!(
                "cpu · {:.0}% of {:.0}%",
                self.scaler.util() * 100.0,
                scale::TARGET * 100.0,
            ),
        }
    }
}

pub(crate) async fn run(
    ctx: Ctx<Setup, AutoscaleMsg>,
    _seed: crate::PageSeed,
    key: SimKey,
) -> idyll::Result {
    // Both arms live from mount and both tick every frame. A hidden arm that stopped would be
    // a fresh sim wearing an old fleet's history the moment the reader turned to it.
    let mut arms: Vec<Arm> = STEERS.iter().map(|&(_, steer)| Arm::new(&key, steer)).collect();

    let running = ctx.mutable_signal(false);
    ctx.frames(&running.read(), AutoscaleMsg::Tick);
    let run_label = {
        let running = running.read();
        ctx.computed(move |cx| match running.get(cx) {
            true => "❚❚ Pause".to_string(),
            false => "▶ Run".to_string(),
        })
        .read()
    };

    let qps = ctx.mutable_signal(OPENS_AT);
    let qps_at = qps.read();
    let speed = ctx.mutable_signal(raw_from_speed(SPEED));
    let speed_at = speed.read();
    let served = ctx.mutable_signal(count(0));
    let refused = ctx.mutable_signal(count(0));
    let aggs = vec![("served", served.read()), ("refused", refused.read())];

    let shown = ctx.mutable_signal(0usize);
    let items: Vec<ToggleItem> = STEERS
        .iter()
        .enumerate()
        .map(|(i, (label, _))| {
            let on = shown.read();
            (i, (*label).into(), ctx.computed(move |cx| on.get(cx) == i).read(), Tone::Normal)
        })
        .collect();
    let pill_knob = knob(&ctx, &items);
    let picked = ctx.callback(AutoscaleMsg::Steer);

    // The crowd, one circle each: orange while a client is owed an answer.
    let ink_signals: Vec<_> = (0..CLIENTS).map(|_| ctx.mutable_signal(ink(false))).collect();
    let strip: Vec<(usize, Signal<String>)> =
        ink_signals.iter().enumerate().map(|(c, s)| (c, s.read())).collect();
    let strip_count = ctx.constant(CLIENTS);
    let measured = ctx.callback(|(c, rect): (usize, Rect)| AutoscaleMsg::Dot(c, rect));

    // Ten boxes always: the fleet is built once and the scaler decides who is in rotation. A
    // box out of rotation greys rather than leaves — a machine given back is still a machine,
    // and the fleet's shape is what the reader is watching change.
    let rotation = ctx.mutable_signal(OPENS_WITH);
    let counts: Vec<_> =
        (0..SERVERS).map(|_| ctx.mutable_signal(ServerCounts::default())).collect();
    let boxes: Vec<(usize, String, Signal<ServerCounts>, Signal<bool>)> = (0..SERVERS)
        .map(|i| {
            let rotation = rotation.read();
            let idle = ctx.computed(move |cx| i >= rotation.get(cx)).read();
            (i, server_name(i), counts[i].read(), idle)
        })
        .collect();

    let lattice: MutableVec<Shape> = ctx.mutable_vec();
    let traffic: MutableVec<Shape> = ctx.mutable_vec();
    let picture = ctx.mutable_vec_of(vec![lattice.clone(), traffic.clone()]);
    let mut stage_rect: Option<Rect> = None;
    let mut dot_rects: Vec<Option<Rect>> = vec![None; CLIENTS];
    let mut box_rects: Vec<Option<Rect>> = vec![None; SERVERS];

    let steering = ctx.mutable_signal(arms[0].steering());
    let rows = ctx.mutable_signal(strip_rows(&arms[0].samples, arms[0].steer));
    let marks = ctx.mutable_signal(String::new());
    let keys = vec![
        Key { ink: Stage::teal.value(), means: "request" },
        Key { ink: Stage::amber.value(), means: "refused → retry" },
        Key { ink: Stage::green.value(), means: "response" },
    ];

    let title = "Autoscale · 100 clients → 10 servers";
    let mut ctx = ctx.render(live_view! {
        div css=[card::CARD] role=("group") aria_label=(title) {
            SimHeader title=(title) aggs=(aggs)
            div css=[card::CTRLS] {
                button css=[bstyles::BASE, bstyles::CTA]
                    onclick=>(|_| Some(AutoscaleMsg::Toggle)) { $run_label }
                Slider name=(Name::new("arrivals", 52)) scale=(Scale::new(50, ARRIVALS_MAX as u32, 10))
                    at=(qps_at) fmt=(fmt_qps) moved=>(|n| AutoscaleMsg::Qps(n))
                Slider name=(Name::new("speed", 38)) scale=(Scale::new(0, 100, 1))
                    at=(speed_at) fmt=(fmt_speed) moved=>(|n| AutoscaleMsg::Speed(n))
            }
            div css=[card::CTRLS] {
                ToggleGroup items=(items) knob=(pill_knob) picked=(picked)
            }
            div css=[wstyles::TIER] measure=>(|e| e.rect().map(AutoscaleMsg::Stage)) {
                ClientStrip dots=(strip) count=(strip_count) measured=(measured)
                div css=[fstyles::FLEET] {
                    @for (at, name, counts, idle) in (boxes) {
                        div measure=>(move |e| e.rect().map(|r| AutoscaleMsg::Server(at, r))) {
                            ServerBox name=(name) counts=(counts) idle=(idle)
                        }
                    }
                }
                canvas css=[wstyles::WIRES, wstyles::UNDER] painting=(picture) {}
            }
            div css=[fstyles::LABEL] { $steering }
            Strips rows=(rows) labels=(80) marks=(marks)
            Legend keys=(keys)
        }
    }).await?;

    let mut readouts = Readouts {
        counts,
        rotation,
        served,
        refused,
        steering,
        rows,
        marks,
        ink: ink_signals,
        faces: vec![ink(false); CLIENTS],
    };
    let mut playing = false;
    let mut arrivals = OPENS_AT;
    let mut speed_now = SPEED;
    let mut at = 0usize;
    let mut marked: VecDeque<u64> = VecDeque::new();
    let mut sampled = 0u64;
    loop {
        let (msg, turn) = ctx.recv().await?;
        let mut relaid = false;
        match msg {
            AutoscaleMsg::Toggle => {
                playing = !playing;
                // Pausing stops departures, never travel already in the air — and on a canvas
                // the picture is drawn by this loop or not at all.
                running.set(&turn, playing || arms[at].tier.airborne());
            }
            AutoscaleMsg::Qps(v) => {
                arrivals = v;
                qps.set(&turn, v);
                for arm in arms.iter_mut() {
                    arm.engine.set_qps(v);
                }
                note(&mut marked, sampled);
            }
            AutoscaleMsg::Speed(raw) => {
                speed.set(&turn, raw);
                speed_now = speed_from_raw(raw);
            }
            AutoscaleMsg::Steer(i) => {
                let Some(arm) = arms.get(i) else { continue };
                at = i;
                shown.set(&turn, i);
                // A card still showing the arm it was switched away from is the same fault as
                // a frame that never refreshed it, so the turn owes the reader all of it — the
                // rows included, which a frame writes only on the sample.
                readouts.fleet(arm, &turn);
                readouts.chart(arm, &marked, sampled, &turn);
                // The fleet under the reader changed; so did every wire on it.
                relaid = true;
            }
            AutoscaleMsg::Stage(rect) => {
                stage_rect = Some(rect);
                relaid = true;
            }
            AutoscaleMsg::Dot(c, rect) => {
                set_rect(&mut dot_rects, c, rect);
                relaid = true;
            }
            AutoscaleMsg::Server(s, rect) => {
                set_rect(&mut box_rects, s, rect);
                relaid = true;
            }
            AutoscaleMsg::Tick(wall) => {
                // The reader's clock and the fleet's: a paused card advances no virtual time,
                // and goes on drawing until what was already sent has landed.
                let wall = wall.min(MAX_STEP_MS);
                let dt = match playing {
                    true => wall,
                    false => 0.0,
                };
                let mut sample = false;
                for (i, arm) in arms.iter_mut().enumerate() {
                    let moved = arm.frame(dt, wall, speed_now, arrivals);
                    if i == at {
                        relaid |= moved.rotation;
                        sample = moved.sample;
                    }
                }

                let arm = &arms[at];
                readouts.fleet(arm, &turn);
                if sample {
                    sampled += 1;
                    while marked.front().is_some_and(|&m| m + POINTS as u64 <= sampled) {
                        marked.pop_front();
                    }
                    readouts.chart(arm, &marked, sampled, &turn);
                }
                running.set(&turn, playing || arm.tier.airborne());
            }
        }
        if relaid {
            lattice.sync(
                &turn,
                lattice_of(&stage_rect, &dot_rects, &box_rects, arms[at].rotation),
            );
        }
        traffic.sync(&turn, traffic_of(&stage_rect, &dot_rects, &box_rects, &arms[at].tier));
    }
}

/// Everything the shown arm is read into: the fleet's boxes and how many of them are in
/// rotation, the tallies, the scaler's own reading, and the rows under it.
struct Readouts {
    counts: Vec<idyll::MutableSignal<ServerCounts>>,
    rotation: idyll::MutableSignal<usize>,
    served: idyll::MutableSignal<String>,
    refused: idyll::MutableSignal<String>,
    steering: idyll::MutableSignal<String>,
    rows: idyll::MutableSignal<Vec<StripRow>>,
    marks: idyll::MutableSignal<String>,
    ink: Vec<idyll::MutableSignal<String>>,
    /// What each circle is showing — the card's own copy, so a frame can tell which clients
    /// changed hands without asking the page.
    faces: Vec<String>,
}

impl Readouts {
    /// The fleet as it stands: what each box holds, how many are in rotation, the tallies, who
    /// is owed an answer, and what the scaler is looking at. Every frame.
    fn fleet(&mut self, arm: &Arm, turn: &idyll::Reducer<AutoscaleMsg>) {
        for (signal, server) in self.counts.iter().zip(arm.engine.fleet()) {
            signal.set(turn, server.counts);
        }
        self.rotation.set(turn, arm.rotation);
        let answered = arm.engine.answered();
        self.served.set(turn, count(answered.trips));
        self.refused.set(turn, count(answered.refusals));
        ink_frame(turn, &mut self.faces, &self.ink, &arm.engine.outstanding());
        self.steering.set(turn, arm.steering());
    }

    /// The rows, on the sample rather than on the frame: a hundred and twenty points rewritten
    /// sixty times a second would be the same picture four times over, at the frame's expense.
    fn chart(
        &mut self,
        arm: &Arm,
        marked: &VecDeque<u64>,
        sampled: u64,
        turn: &idyll::Reducer<AutoscaleMsg>,
    ) {
        self.rows.set(turn, strip_rows(&arm.samples, arm.steer));
        self.marks.set(turn, mark_path(marked, sampled, arm.samples.len()));
    }
}

/// The layer under the traffic: a bow from every client to every machine in rotation. A
/// machine given back keeps its box and loses its wires, which is what says nothing is being
/// sent there any more.
fn lattice_of(
    stage: &Option<Rect>,
    dots: &[Option<Rect>],
    boxes: &[Option<Rect>],
    rotation: usize,
) -> Vec<Shape> {
    let mut layer = Vec::new();
    wires_down(
        stage,
        dots,
        boxes,
        (0..dots.len()).flat_map(|c| (0..rotation).map(move |s| (c, s))),
        IDLE,
        &mut layer,
    );
    layer
}

/// The layer over it: what travels, along the same bows.
fn traffic_of(
    stage: &Option<Rect>,
    dots: &[Option<Rect>],
    boxes: &[Option<Rect>],
    tier: &LaneTier,
) -> Vec<Shape> {
    let mut layer = Vec::new();
    tier.paint(
        |(c, s)| {
            let stage = stage.as_ref()?;
            Some(bow_down(stage, dots.get(c)?.as_ref()?, boxes.get(s)?.as_ref()?))
        },
        &mut layer,
    );
    layer
}

/// The three rows: what was asked of the fleet, how big the fleet was, and the reading the
/// scaler bought it on. Each on an axis of its own, because they are different quantities —
/// the demand against what the slider can ask for, the fleet and the machines worth sending to
/// against the ten that exist, a percentage against a hundred.
///
/// The third row is the arm's, so it changes with the pill: the two scalers cannot be laid
/// over one axis, and a row that tried would be two lines about nothing.
fn strip_rows(samples: &VecDeque<Sample>, steer: Steer) -> Vec<StripRow> {
    let read = |of: fn(&Sample) -> f64| samples.iter().map(of).collect::<Vec<f64>>();
    let (label, axis, unit) = match steer {
        Steer::Queue => ("queue", queue_axis(samples), ""),
        Steer::Cpu => ("cpu", 100.0, "%"),
    };
    vec![
        row("arrivals", ARRIVALS_MAX, Stage::teal.value(), read(|s| s.arrivals), "/s"),
        row("machines", SERVERS as f64, Stage::blue.value(), read(|s| s.machines), ""),
        row(label, axis, Stage::purple.value(), read(|s| s.signal), unit),
    ]
}

/// One row from its readings: the line scaled to the row's own axis, and the latest of them
/// stated in the row's own unit.
/// What the queue row is drawn against: a machine's worth of waiting, or the worst moment
/// still on stage when the fleet has been further behind than that. Fixed while the fleet is
/// keeping up, so the reader can see how far from empty it is; growing when it is not, so a
/// storm is a shape rather than a line pinned to the ceiling.
fn queue_axis(samples: &VecDeque<Sample>) -> f64 {
    samples
        .iter()
        .map(|s| s.signal)
        .fold(ADMISSION_LIMIT as f64, f64::max)
}

fn row(label: &'static str, axis: f64, ink: Paint, values: Vec<f64>, unit: &str) -> StripRow {
    let points = values
        .iter()
        .enumerate()
        .map(|(j, &v)| format!("{:.1},{:.1}", slot_x(j), y_of(v, axis)))
        .collect::<Vec<_>>()
        .join(" ");
    let reading = match values.last() {
        Some(v) => format!("{}{unit}", slots(format!("{v:.0}"), 4)),
        None => format!("{}{unit}", slots("–", 4)),
    };
    StripRow { label, points, reading, ink: ink.to_string() }
}

fn fmt_qps(v: f64) -> String {
    format!("{}/s", slots(format!("{v:.0}"), 4))
}
