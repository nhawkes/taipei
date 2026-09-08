//! **Sim 3 · Percentile** — the same batch read at two percentiles at once.
//!
//! Sims 1 and 2 say what a batch cost overall. This one says who paid: the median client's
//! round trip beside an unlucky one's, cut into the same four phases and drawn against one
//! shared axis, so the gap between a p50 experience and a p90 experience is a length rather
//! than a claim. That gap is the chapter's point — a fleet can look healthy on the mean and
//! be bad for a tenth of the people using it.
//!
//! The batch is the real one ([`MultiEngine`]) and it settles **in the browser**, a frame of
//! virtual time per animation frame, so the two readings grow into place as requests come home.
//! Nothing runs until the reader sends: the card mounts as four phases at rest, and a scale's
//! fleet is not assembled until that scale is asked for. A settled batch is far too dear to be
//! a server's work — a mount gets milliseconds, and a client that keeps asking can spend
//! seconds of virtual time getting an answer.

use std::collections::VecDeque;
use std::rc::Rc;

use idyll::{live_view, Ctx, Setup, Signal};
use idyll_styles::styles;
use crate::atoms::figure::slots;
use crate::atoms::cut::styles as cut_styles;

use crate::atoms::button::styles as bstyles;
use crate::atoms::client_grid::{Client, ClientGrid};
use crate::atoms::sim_card::styles as card;
use crate::atoms::legend::{Key, Legend};
use crate::atoms::server_box::{server_name, ServerBox};
use crate::atoms::cut::{at_rest, ramp, Cut, CutColumn};
use crate::atoms::stage::{Paint, Stage};
use crate::atoms::toggle::{knob, Tone, ToggleGroup, ToggleItem};
use crate::atoms::waterfall::Leg;
use crate::atoms::wires::{set_rect, styles as wstyles, wire_d, wires_d};
use crate::engine::{Outcome, ServerCounts};
use crate::multi::{phase_ms, sim_seed, Batch, Journey, MultiEngine, Record, PHASES};
use idyll::Rect;
use idyll_styles::Keyframes;
use blog_core::SimKey;

/// One tab of the scale control: what it is called, and the batch it runs.
struct Preset {
    label: &'static str,
    batch: Batch,
}

/// The three scales, and the fleet stays the same size across all of them — which is the point.
/// Ten machines serve ten requests each at once, so forty requests is a fleet with room, a
/// hundred is a fleet exactly full, and a thousand is one ten times over. The reading changes
/// character as you climb: at the bottom the gap between the median and the unlucky client is
/// the work's own variance, and at the top it is routing — clients piled onto a machine that
/// refuses them, asking again somewhere else.
///
/// A client sends four requests at the smallest scale and one above it. Ten wires, each with a
/// single request on it, reads as a diagram rather than as a fan-out; a thousand clients sending
/// ten apiece is a retry storm, which is a later chapter's subject and not a picture of a spread.
const PRESETS: [Preset; 3] = [
    Preset { label: "10", batch: Batch { clients: 10, servers: 10, reqs_per_client: 4 } },
    Preset { label: "100", batch: Batch { clients: 100, servers: 10, reqs_per_client: 1 } },
    Preset { label: "1000", batch: Batch { clients: 1000, servers: 10, reqs_per_client: 1 } },
];

/// Which scale the card opens on — a hundred clients, where the fleet is exactly full and the
/// spread is already worth reading.
const OPENS_ON: usize = 1;

/// The two readings drawn side by side, and where each opens.
const CUTS: [u32; 2] = [50, 90];

/// A percentile is an index into a sorted batch, so the ends are not readings: p0 is the one
/// client who got lucky and p100 the one who did not.

/// Headroom past the slowest request, so the longest bar stops short of the edge rather than
/// against it.
const AXIS_HEADROOM: f64 = 1.05;

/// The most virtual time one frame may advance. A tab coming back from the background hands
/// over a delta of seconds, and a message arm runs to completion with nothing able to interrupt
/// it — so an unclamped delta is one long freeze rather than one long frame.
const MAX_STEP_MS: f64 = 50.0;

/// The caption before a scale has been sent. A count of nothing out of a thousand is a
/// measurement of a batch nobody asked for, so until there is a batch the card says so.
const AT_REST: &str = "fills in as clients respond";

/// How long a leg of a round trip takes to travel its wire, in seconds: out to a machine, home
/// with a refusal, and home with the answer.
///
/// This is the one thing in the card that is not the engine's own number. A network leg is
/// [`NET_MS`](crate::engine::NET_MS) — six milliseconds, a third of one frame — so a leg drawn
/// at the speed it happens is not drawn at all, and here the leg *is* the picture. Stretching it
/// costs the honesty of the clock and nothing else: a pulse's whole path is the wire it rides, so
/// every position it is seen at is one the request was actually at. What the reader will notice
/// is that a client's ring goes green while its pulses are still coming home — the numbers are
/// the engine's, and the picture is running behind them.
const OUT_S: f64 = 0.85;
const REFUSED_HOME_S: f64 = 0.6;
const ANSWER_HOME_S: f64 = 0.85;

/// How large a crowd has every round trip drawn, and how many are drawn past that. Thousands of
/// simultaneous `offset-path` animations are a load on the compositor rather than a picture, so
/// above [`CROWD`] clients only every `ceil(clients / DRAWN)`-th of them travels. What a drawn
/// client travels is a whole trip — [`CALLS_DRAWN`] calls, each an out and a home, all mounted
/// at once and staggered by delay — so the cap is on trips and the nodes on the stage are six
/// times it: some eight hundred at the top scale, where drawing every client would be six
/// thousand. Nothing is hidden by it: what a client experienced is its ring's colour, which the
/// engine writes for every one of them either way.
///
/// How many requests a client has in flight at once is *not* capped: it is
/// [`Batch::reqs_per_client`], four at the smallest scale and one above it, and a client's
/// pulses are however many it really sent.
const CROWD: usize = 140;
const DRAWN: usize = 130;

fn stride(clients: usize) -> usize {
    match clients > CROWD {
        true => clients.div_ceil(DRAWN),
        false => 1,
    }
}

/// How many calls of one round trip are followed: the first ask and two more, which is the
/// retry the chapter's prose describes.
///
/// At a thousand clients the fleet turns some of them away sixteen times, and sixteen calls is
/// half a minute of travel for a trip the engine finished in forty milliseconds — the picture
/// would still be retelling the first second of the batch when the reader had lost interest.
/// Every leg that is drawn is one the client made, to the machine it made it to; what a
/// truncated chain does not say is how much longer it went on. The ring says that: it is the
/// client's whole experience, and it is the length of the tail this sim is about.
const CALLS_DRAWN: usize = 3;

/// The picture's vocabulary: the four phases a round trip is cut into, then the two things only
/// the stage says — a refusal on its way back, and an answer that landed.
///
/// The phases are [`PHASES`] itself, so a swatch cannot end up naming a colour something other
/// than what the waterfall under it calls the same colour.
fn keys() -> Vec<Key> {
    PHASES
        .iter()
        .map(|p| Key { ink: p.ink, means: p.label })
        .chain([
            Key { ink: Stage::amber.value(), means: "shed → retry" },
            Key { ink: Stage::green.value(), means: "response" },
        ])
        .collect()
}

#[derive(Debug)]
pub enum PercentileMsg {
    /// Send the batch. Sending a scale that has already run starts it over rather than
    /// adding to it, so what the readings describe is always one batch.
    Send,
    /// One animation frame: the delta since the previous, in milliseconds. The batch advances
    /// by the same amount of virtual time, so the settle runs at the speed it would.
    Tick(f64),
    /// Show this scale.
    Scale(usize),
    /// Move one of the two readings to a percentile.
    Cut(usize, f64),
    /// The stage's own laid-out box — the frame the other two are measured against.
    Stage(Rect),
    Client(usize, Rect),
    Server(usize, Rect),
}

/// The fleet every scale runs. It is the same size at all three, which is what makes the scales
/// comparable — so a server's box, and the rect the wires reach it by, survive a scale change.
const SERVERS: usize = PRESETS[0].batch.servers;

/// The largest crowd any scale runs — how many measured client rects there is ever room for.
const MOST_CLIENTS: usize = PRESETS[PRESETS.len() - 1].batch.clients;

/// One scale's own machine. A reader who has sent at a hundred, switched to a thousand and come
/// back should find the hundred as they left it — so a scale keeps its engine and its readings
/// rather than the card keeping one of each.
struct Machine {
    engine: Option<MultiEngine>,
    /// The clients whose requests are all back, slowest experience last — which is what makes
    /// an index into it a percentile.
    home: Vec<Record>,
    running: bool,
}

impl Machine {
    fn at_rest() -> Machine {
        Machine { engine: None, home: Vec::new(), running: false }
    }
}

pub(crate) async fn run(
    ctx: Ctx<Setup, PercentileMsg>,
    _seed: crate::PageSeed,
    key: SimKey,
) -> idyll::Result {
    let scale = ctx.mutable_signal(OPENS_ON);
    let scales: Vec<ToggleItem> = PRESETS
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let at = scale.read();
            let on = ctx.computed(move |cx| at.get(cx) == i).read();
            (i, Rc::from(p.label), on, Tone::Normal)
        })
        .collect();
    let scale_knob = knob(&ctx, &scales);

    // One axis for both columns, off the slowest request so far: two columns drawn to their own
    // maxima would show the p50 and the p90 as the same length, which is the one thing this sim
    // exists to deny.
    let axis = ctx.mutable_signal(1.0_f64);
    let cuts: Vec<Cut> = CUTS
        .iter()
        .map(|&pc| Cut {
            at: ctx.mutable_signal(pc as f64),
            legs: ctx.mutable_signal(at_rest()),
        })
        .collect();
    // Each column publishes its own place on the scale as a custom property, and the track
    // inside it reads that property for its fill, its knob and its readout. The colour is a
    // fact about the reading, so it is written where the reading is and inherited by whatever
    // stands for it — rather than handed separately to three controls that could disagree.
    let columns: Vec<(Signal<f64>, Signal<Vec<Leg>>, idyll::Callback<f64>)> = cuts
        .iter()
        .enumerate()
        .map(|(i, c)| (c.at.read(), c.legs.read(), ctx.callback(move |v| PercentileMsg::Cut(i, v))))
        .collect();
    let shared_axis = axis.read();

    let running = ctx.mutable_signal(false);
    ctx.frames(&running.read(), PercentileMsg::Tick);

    let counted = ctx.mutable_signal(PRESETS[OPENS_ON].batch);
    let count = {
        let counted = counted.read();
        ctx.computed(move |cx| {
            let b = counted.get(cx);
            format!("{} clients · {} servers · {} requests", slots(b.clients, 3), slots(b.servers, 3), slots(b.requests(), 4))
        })
        .read()
    };
    let landed = ctx.mutable_signal(String::from(AT_REST));

    // The picture is a pure function of three measurements — the stage's own box and every
    // client's and server's rect inside it — so a wire re-bows whenever the layout reflows and
    // nothing restates a layout rule the stylesheet already owns.
    let stage = ctx.mutable_signal::<Option<Rect>>(None);
    let client_rects = ctx.mutable_signal::<Vec<Option<Rect>>>(vec![None; MOST_CLIENTS]);
    let server_rects = ctx.mutable_signal::<Vec<Option<Rect>>>(vec![None; SERVERS]);
    // Which server each client stuck to. Behind an `Rc` because the detour set reads it every
    // frame and a thousand clients is a thousand words to copy for a test the answer to which
    // changed only when the batch was sent.
    let routing = ctx.mutable_signal::<Rc<Vec<usize>>>(Rc::new(Vec::new()));
    // The crowd is the still: every client is on the page from the start, waiting, and sending
    // colours them in. A field that arrived with the first frame of the settle would have the
    // card change shape under the reader at the moment they asked it to do something.
    //
    // One signal per place, so a frame restyles the rings whose reading moved and leaves the rest
    // of the field alone. Every place any scale can fill is minted here, as the measured rects
    // are: the grid draws the batch's own count of them and the rest stand empty.
    let painted = vec![waiting(); MOST_CLIENTS];
    let crowd: Vec<idyll::MutableSignal<Client>> =
        painted.iter().map(|c| ctx.mutable_signal(c.clone())).collect();
    let places: Vec<(usize, Signal<Client>)> =
        crowd.iter().enumerate().map(|(at, ring)| (at, ring.read())).collect();
    let crowd_size = {
        let counted = counted.read();
        ctx.computed(move |cx| counted.get(cx).clients).read()
    };
    let fleet: Vec<idyll::MutableSignal<ServerCounts>> =
        (0..SERVERS).map(|_| ctx.mutable_signal(ServerCounts::default())).collect();
    let idle: Vec<idyll::MutableSignal<bool>> =
        (0..SERVERS).map(|_| ctx.mutable_signal(true)).collect();
    let boxes: Vec<(usize, String, Signal<ServerCounts>, Signal<bool>)> = (0..SERVERS)
        .map(|i| (i, server_name(i), fleet[i].read(), idle[i].read()))
        .collect();
    let wires = {
        let (stage, clients, servers, routing) =
            (stage.read(), client_rects.read(), server_rects.read(), routing.read());
        ctx.computed(move |cx| {
            let routing = routing.get(cx);
            wires_d(
                &stage.get(cx),
                &clients.get(cx),
                &servers.get(cx),
                routing.iter().copied().enumerate(),
            )
        })
        .read()
    };
    // The legs in the air. A leg is mounted once with the whole of its travel written into it —
    // the wire as its `offset-path` and one sweep along it — so a pulse is never rewritten, only
    // added when its round trip closed and dropped when it has arrived.
    let pulses = ctx.mutable_signal::<Vec<Pulse>>(Vec::new());
    let travelling = pulses.read();
    // A retry lands on a machine the still drew no wire to, and nothing on this stage travels
    // where there is no wire. This layer is the detours the legs in the air are taking: it
    // appears with the retry that needed it and leaves with it.
    //
    // Which detours are being taken changes with the traffic; where a wire *runs* changes only
    // when the page reflows. Reading both in one place would redraw a thousand chords sixty
    // times a second to move a handful of pulses, so the two rates meet at this seam: the set
    // is derived per frame, and — being a value the memo compares — a frame that takes the same
    // detours as the last one leaves the geometry below untouched.
    let detours = {
        let (routing, pulses) = (routing.read(), pulses.read());
        ctx.computed(move |cx| {
            let routing = routing.get(cx);
            let mut detours: Vec<(usize, usize)> = pulses
                .get(cx)
                .iter()
                .map(|p| p.pair)
                .filter(|&(client, server)| routing.get(client) != Some(&server))
                .collect();
            detours.sort_unstable();
            detours.dedup();
            detours
        })
        .read()
    };
    let retry_wires = {
        let (stage, clients, servers, detours) =
            (stage.read(), client_rects.read(), server_rects.read(), detours);
        ctx.computed(move |cx| {
            wires_d(&stage.get(cx), &clients.get(cx), &servers.get(cx), detours.get(cx))
        })
        .read()
    };
    let measured = ctx.callback(|(at, rect)| PercentileMsg::Client(at, rect));
    let keys = keys();

    let mut ctx = ctx.render(live_view! {
        div css=[crate::atoms::sim_card::styles::CARD] {
            div css=[card::CTRLS] {
                ToggleGroup items=(scales) knob=(scale_knob) picked=>(PercentileMsg::Scale)
                button css=[bstyles::BASE, bstyles::CTA]
                    onclick=>(|_| Some(PercentileMsg::Send)) { "▶ send" }
                span css=[card::COUNT] { $count }
            }
            div css=[styles::LAB] {
                "experience breakdown · " span css=[styles::RECV] { $landed } " · shared ms scale"
            }
            div css=[styles::SPLIT] {
                div css=[styles::COLH] { span { "clients" } span { "servers" } }
                div css=[wstyles::STAGE, styles::FIELD]
                    measure=>(|e| e.rect().map(PercentileMsg::Stage)) {
                    ClientGrid clients=(places) count=(crowd_size) measured=(measured)
                    svg css=[wstyles::WIRES] {
                        path css=[wstyles::WIRE_IDLE] d=($wires) {}
                        path css=[wstyles::WIRE_SHED] d=($retry_wires) {}
                    }
                    div css=[wstyles::SERVERS] {
                        @for (i, name, counts, quiet) in (boxes) {
                            div measure=>(move |e| e.rect().map(|r| PercentileMsg::Server(i, r))) {
                                ServerBox name=(name) counts=(counts) idle=(quiet)
                            }
                        }
                    }
                    div css=[styles::PULSES] {
                        @for p in $travelling [key = p.key] {
                            div css=[styles::PULSE] style=($p.style) {}
                        }
                    }
                }
            }
            Legend keys=(keys)
            div css=[cut_styles::CHARTS] {
                @for (at, legs, moved) in (columns) {
                    CutColumn at=(at) legs=(legs) axis=(shared_axis.clone()) moved=(moved)
                }
            }
        }
    }).await?;

    let mut readouts = Readouts { cuts, fleet, idle, axis, landed, crowd, painted };
    let mut machines: Vec<Machine> = PRESETS.iter().map(|_| Machine::at_rest()).collect();
    let mut shown = OPENS_ON;
    // Real milliseconds since the card mounted — the clock the legs in the air are retired on.
    // The engine's virtual time will not do: a leg is a CSS animation and runs at the speed the
    // reader's browser paints, not at the speed the batch settles.
    let mut clock = 0.0_f64;
    loop {
        let (msg, turn) = ctx.recv().await?;
        match msg {
            PercentileMsg::Scale(i) => {
                let (Some(preset), Some(machine)) = (PRESETS.get(i), machines.get(i)) else {
                    continue;
                };
                shown = i;
                scale.set(&turn, i);
                counted.set(&turn, preset.batch);
                running.set(&turn, machine.running);
                routing.set(
                    &turn,
                    Rc::new(machine.engine.iter().flat_map(|e| e.assignment()).copied().collect()),
                );
                // A picture the reader has turned away from is not travelling, and a leg's path
                // was written against a layout this scale has replaced.
                pulses.set(&turn, Vec::new());
                readouts.show(machine, preset.batch, &turn);
            }
            PercentileMsg::Send => {
                let batch = PRESETS[shown].batch;
                let machine = &mut machines[shown];
                // Sending is starting over: a settled scale sent again is a fresh fleet and an
                // empty ranking, not a second batch onto machines the first one left warm.
                *machine = Machine::at_rest();
                machine.engine.insert(MultiEngine::new(sim_seed(&key), batch)).fire();
                machine.running = true;
                running.set(&turn, true);
                // Which server each client stuck to. It is settled the instant the batch is
                // sent and never moves again, so the wires are drawn once and the retries that
                // wander off them are the picture's business, not the routing's.
                routing.set(
                    &turn,
                    Rc::new(machine.engine.iter().flat_map(|e| e.assignment()).copied().collect()),
                );
                pulses.set(&turn, Vec::new());
                readouts.show(machine, batch, &turn);
            }
            PercentileMsg::Tick(dt) => {
                clock += dt;
                let batch = PRESETS[shown].batch;
                let machine = &mut machines[shown];
                let Some(engine) = machine.engine.as_mut() else { continue };
                engine.tick(dt.min(MAX_STEP_MS));
                let closed = engine.journeys();
                // Only while the batch is still coming in: a settled one has nothing left to
                // re-rank, and ranking a thousand records a frame while the last legs land would
                // be the most expensive thing on the page.
                if machine.running {
                    machine.home = engine.with_records(|rs| home_by_experience(rs, batch));
                    machine.running = !engine.settled();
                    readouts.show(machine, batch, &turn);
                }

                let mut legs = pulses.now(&turn);
                legs.retain(|p| p.ends_at > clock);
                let drawn: Vec<Journey> =
                    closed.into_iter().filter(|j| j.client % stride(batch.clients) == 0).collect();
                if !drawn.is_empty() {
                    let (stage, clients, servers) =
                        (stage.now(&turn), client_rects.now(&turn), server_rects.now(&turn));
                    for journey in &drawn {
                        legs.extend(pulses_of(journey, &stage, &clients, &servers, clock));
                    }
                }
                // The frame loop outlives the batch: stopping it the instant the last request
                // lands would leave the last legs stranded halfway home.
                running.set(&turn, machine.running || !legs.is_empty());
                pulses.set(&turn, legs);
            }
            PercentileMsg::Stage(rect) => stage.set(&turn, Some(rect)),
            PercentileMsg::Client(i, rect) => {
                client_rects.update(&turn, |v| set_rect(v, i, rect))
            }
            PercentileMsg::Server(i, rect) => {
                server_rects.update(&turn, |v| set_rect(v, i, rect))
            }
            PercentileMsg::Cut(i, pc) => {
                if let Some(cut) = readouts.cuts.get(i) {
                    cut.at.set(&turn, pc);
                    cut.legs.set(&turn, phases_of(at(&machines[shown].home, pc)));
                }
            }
        }
    }
}


/// Everything a machine is read into: the two columns, the fleet's boxes, the axis the columns
/// share, the crowd, and the caption over them.
struct Readouts {
    cuts: Vec<Cut>,
    fleet: Vec<idyll::MutableSignal<ServerCounts>>,
    idle: Vec<idyll::MutableSignal<bool>>,
    axis: idyll::MutableSignal<f64>,
    landed: idyll::MutableSignal<String>,
    /// One ring per client at the largest scale; a smaller scale draws a prefix of them.
    crowd: Vec<idyll::MutableSignal<Client>>,
    /// What each ring is showing — the card's own copy of the field, so a frame can tell which
    /// rings moved without asking the page.
    painted: Vec<Client>,
}

impl Readouts {
    /// Write a machine's state into the card. One function, because a switch between scales and
    /// a frame of a settle owe the reader exactly the same update — a scale showing the fleet it
    /// was switched away from is the same fault as a frame that never refreshed it.
    ///
    /// The crowd is written ring by ring against what each ring already shows. Below the median
    /// [`ramp`] is flat, so a frame late in a settle writes the tail, the handful of rings a
    /// decile label has just moved between, and nothing else.
    fn show(&mut self, machine: &Machine, batch: Batch, turn: &idyll::Reducer<PercentileMsg>) {
        let home = &machine.home;
        self.axis.set(turn, home.last().map(|r| r.total_ms * AXIS_HEADROOM).unwrap_or(1.0));
        for cut in &self.cuts {
            cut.legs.set(turn, phases_of(at(home, cut.at.now(turn))));
        }
        self.landed.set(
            turn,
            match &machine.engine {
                None => String::from(AT_REST),
                Some(_) => format!("{} / {} clients", slots(home.len(), 3), batch.clients),
            },
        );
        for ((ring, painted), next) in
            self.crowd.iter().zip(&mut self.painted).zip(field(home, batch))
        {
            if *painted != next {
                *painted = next.clone();
                ring.set(turn, next);
            }
        }

        let seen = machine.engine.as_ref().map(MultiEngine::fleet).unwrap_or_default();
        for (i, (counts, quiet)) in self.fleet.iter().zip(&self.idle).enumerate() {
            let server = seen.get(i).copied();
            counts.set(turn, server.map(|s| s.counts).unwrap_or_default());
            // Idle is the routed load, which is what the fan-out means by it too — and a scale
            // nobody has sent is ten idle machines rather than the last scale's ten.
            quiet.set(turn, server.is_none_or(|s| s.load == 0));
        }
    }
}

/// The clients whose every request is back, slowest experience last — each as the request that
/// *is* that experience.
///
/// A client is not served until its last request is, so its experience is its slowest request —
/// not their mean, and not whichever came home first. One still waiting has no experience yet
/// and is not in this list.
///
/// One ordering for the whole card. The columns read a percentile out of it and the rings are
/// coloured by their place in it, so a ring labelled `p90` and the p90 column are the same
/// client by construction rather than by the scales happening to send one request each.
fn home_by_experience(records: &VecDeque<Record>, batch: Batch) -> Vec<Record> {
    /// What one client is waiting on and what it has felt so far: a client is its *slowest*
    /// round trip, because the one that kept you waiting is the one you experienced.
    #[derive(Clone, Copy)]
    struct Felt<'a> {
        back: usize,
        worst: Option<&'a Record>,
    }

    let mut by_client = vec![Felt { back: 0, worst: None }; batch.clients];
    for r in records {
        let Some(felt) = by_client.get_mut(r.client) else { continue };
        felt.back += 1;
        if felt.worst.is_none_or(|w| r.total_ms > w.total_ms) {
            felt.worst = Some(r);
        }
    }
    // Only clients with nothing still out: a client half of whose requests are back has not
    // finished waiting, and ranking it on what has landed would flatter it.
    let mut home: Vec<Record> = by_client
        .into_iter()
        .filter(|felt| felt.back == batch.reqs_per_client)
        .filter_map(|felt| felt.worst.cloned())
        .collect();
    home.sort_by(|a, b| a.total_ms.total_cmp(&b.total_ms));
    home
}

/// Every client as a ring: the colour its place in the spread earns it, and — for one client in
/// ten — the reading that place *is*.
///
/// The labels sit at the deciles of what has come home, so a reader can find the p90 client among
/// a thousand without a legend. One client per label: a batch too small to give every decile a
/// client of its own carries fewer than ten labels rather than two names for one ring, and the
/// label a ring wears is always the reading the column of that name is drawing. p100 is left
/// unwritten — the slowest client of a batch is a fact about that batch, not a percentile anyone
/// plans for.
fn field(home: &[Record], batch: Batch) -> Vec<Client> {
    let mut field = vec![waiting(); batch.clients];
    let last = home.len().saturating_sub(1);
    for (rank, record) in home.iter().enumerate() {
        let pc = match last {
            0 => 0.0,
            last => rank as f64 / last as f64 * 100.0,
        };
        let Some(ring) = field.get_mut(record.client) else { continue };
        ring.ink = ramp(pc);
    }
    let mut labelled = None;
    for decile in 0..10 {
        let Some(record) = at(home, decile as f64 * 10.0) else { break };
        if labelled == Some(record.client) {
            continue;
        }
        labelled = Some(record.client);
        let Some(ring) = field.get_mut(record.client) else { continue };
        ring.label = format!("p{}", decile * 10);
    }
    field
}

/// A client nobody has heard back from: the wall's own colour, and nothing written inside.
fn waiting() -> Client {
    Client { ink: Stage::wall.value().to_string(), label: String::new() }
}

/// The client at a percentile of a batch already ordered by experience, as the request that was
/// their experience.
fn at(home: &[Record], pc: f64) -> Option<&Record> {
    match home.len() {
        0 => None,
        n => home.get(((pc / 100.0) * (n - 1) as f64).round() as usize),
    }
}

/// One leg of a round trip, travelling. Everything about it is written once: the wire it rides
/// as its `offset-path`, the colour of what it is carrying, and one sweep along it delayed to
/// its place in the chain. Nothing rewrites a pulse, so a leg cannot be interrupted by a frame.
#[derive(Clone, PartialEq)]
struct Pulse {
    key: (u32, usize),
    /// The two boxes it runs between, which is what says whether it is on a drawn wire.
    pair: (usize, usize),
    style: String,
    /// Real ms at which it has arrived and is no longer anything.
    ends_at: f64,
}

/// A closed round trip as the legs that draw it: out to the machine the client asked and back
/// with its answer, once for every call the client had to make.
///
/// Nothing here is a choice. Which machines, how many, in what order, and what each said are the
/// journey's; the wire is the one the two boxes are actually laid out at. A trip whose endpoints
/// have not been measured yet draws no legs at all rather than half of one.
fn pulses_of(
    journey: &Journey,
    stage: &Option<Rect>,
    clients: &[Option<Rect>],
    servers: &[Option<Rect>],
    clock: f64,
) -> Vec<Pulse> {
    let (Some(stage), Some(Some(client))) = (stage, clients.get(journey.client)) else {
        return Vec::new();
    };
    let followed = &journey.calls[..journey.calls.len().min(CALLS_DRAWN)];
    let answered = followed.len() == journey.calls.len();
    let Some(route) = followed
        .iter()
        .map(|call| {
            let server = servers.get(call.server)?.as_ref()?;
            Some((call, wire_d(stage, client, server)))
        })
        .collect::<Option<Vec<_>>>()
    else {
        return Vec::new();
    };

    let last = route.len() - 1;
    let mut legs = Vec::with_capacity(route.len() * 2);
    let mut delay = 0.0;
    for (i, (call, d)) in route.iter().enumerate() {
        // Every call but the last was refused — that is what made the client call again — and a
        // chain that stops before the client did ends on the refusal that made it call again.
        let (ink, home_s) = match (i == last && answered).then_some(call.outcome) {
            None => (Stage::amber.value(), REFUSED_HOME_S),
            Some(Outcome::Success) => (Stage::green.value(), ANSWER_HOME_S),
            Some(_) => (Stage::amber.value(), ANSWER_HOME_S),
        };
        legs.push(Pulse {
            key: (journey.trip, i * 2),
            pair: (journey.client, call.server),
            style: pulse_style(d, Stage::teal.value(), styles::Flow, OUT_S, delay),
            ends_at: clock + (delay + OUT_S) * 1000.0,
        });
        delay += OUT_S;
        legs.push(Pulse {
            key: (journey.trip, i * 2 + 1),
            pair: (journey.client, call.server),
            style: pulse_style(d, ink, styles::Back, home_s, delay),
            ends_at: clock + (delay + home_s) * 1000.0,
        });
        delay += home_s;
    }
    legs
}

/// A leg's whole inline style. It fills both ways because a round trip is mounted all at once:
/// a leg still waiting its turn has to wear its own first keyframe — which is nowhere — rather
/// than sit lit at the end of the wire it has not travelled yet.
fn pulse_style(d: &str, ink: Paint, sweep: Keyframes, secs: f64, delay: f64) -> String {
    format!(
        "offset-path:path('{d}');background:linear-gradient(90deg, rgba(255,255,255,0), {ink} 50%, rgba(255,255,255,0));animation:{sweep} {secs}s cubic-bezier(.45,.05,.55,.95) {delay:.2}s 1 both"
    )
}



/// One request's phases, as the waterfall's legs. A phase the request spent no time in is still
/// a leg: the four rows are the same four in both columns and at every moment of the settle, and
/// a row that came and went would make two readings different shapes rather than different
/// lengths.
fn phases_of(record: Option<&Record>) -> Vec<Leg> {
    let Some(record) = record else { return at_rest() };
    phase_ms(record)
        .into_iter()
        .map(|(phase, ms)| Leg {
            label: phase.label.to_string(),
            ms: Some(ms),
            give: None,
            against: None,
            ink: phase.ink,
        })
        .collect()
}


#[styles]
pub mod styles {
    use idyll_styles::Style;

    use crate::styles::Palette;

    keyframes! {
        /// A request travelling out to a machine, and an answer coming back down the same wire.
        /// Each fades in off the box it left and out at the one it reaches, so a leg reads as
        /// travel rather than as a bar appearing on a server.
        pub Flow {
            "0%" { offset_distance: "0%", opacity: 0 }
            "15%" { opacity: 1 }
            "85%" { opacity: 1 }
            "100%" { offset_distance: "100%", opacity: 0 }
        }
        pub Back {
            "0%" { offset_distance: "100%", opacity: 0 }
            "15%" { opacity: 1 }
            "85%" { opacity: 1 }
            "100%" { offset_distance: "0%", opacity: 0 }
        }
    }

    /// The layer the legs travel in: over the wires and the boxes both, and never in the way of
    /// a reader dragging a track underneath it.
    pub const PULSES: Style = css! {{
        position: "absolute",
        inset: "0",
        z_index: 3,
        pointer_events: "none",
    }};

    /// One travelling leg. Its wire and its colour are its own, written inline, so what lives
    /// here is only the shape every leg shares — a short bar, long enough to read as a thing
    /// moving along a line and short enough that the line is still the picture.
    pub const PULSE: Style = css! {{
        position: "absolute",
        left: "0",
        top: "0",
        width: "22px",
        height: "4px",
        border_radius: "2px",
        will_change: "offset-distance",
        pointer_events: "none",
    }};

    /// What the batch comes to, pushed to the trailing edge so the controls keep the leading one.
    /// The picture, in the room it is given. A thousand rings is taller than any card should
    /// be, so the stage scrolls inside itself rather than pushing the reading off the page.
    pub const SPLIT: Style = css! {{
        position: "relative",
        overflow_y: "auto",
        max_height: "440px",
        border_radius: "14px",
    }};

    /// Which side is which, pinned so the answer survives scrolling past the first rings.
    pub const COLH: Style = css! {{
        position: "sticky",
        top: "0",
        z_index: 5,
        display: "flex",
        justify_content: "space-between",
        padding: "6px 4px",
        font_size: "10px",
        letter_spacing: "0.09em",
        text_transform: "uppercase",
        font_weight: 600,
        color: Palette::ink_faint,
        background: Palette::ground,
        border_bottom: "1px solid transparent",
        border_color: Palette::line,
    }};

    /// The stage inside the card: it gives the surface back, because a picture that is one part
    /// of a card is not a thing in its own right the way a sim on the page is.
    pub const FIELD: Style = css! {{
        background: "transparent",
        border_color: "transparent",
        box_shadow: "none",
        align_items: "stretch",
        padding_top: "6px",
    }};

    /// What the two columns are, above them.
    pub const LAB: Style = css! {{
        font_size: "10px",
        letter_spacing: "0.08em",
        text_transform: "uppercase",
        color: Palette::ink_faint,
        font_weight: 600,
        margin: "0 2px 8px",
    }};

    /// How much of the batch is home, inside the caption. Tabular so a count that climbs every
    /// frame does not shuffle the words after it.
    pub const RECV: Style = css! {{
        font_variant_numeric: "tabular-nums",
    }};
}
