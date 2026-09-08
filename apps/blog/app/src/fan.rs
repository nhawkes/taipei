//! **Sim 2 · Fan-out** — ten clients, ten servers, one sticky-random batch of a hundred
//! requests, and what it costs. The servers are [`ServerBox`] atoms down the right; each
//! client is a dot on the left, coloured by how loaded the server it stuck to is (a
//! client that piled onto a hot server wears the hot colour). The boxplot below is the
//! real latency the batch paid, partitioned into phases — the tail is the imbalance.
//!
//! A wire layer connects each client to the server it stuck to, on the canvas its traffic
//! rides — aligned to the HTML boxes by runtime measurement (`measure=>`): the stage, every
//! client dot, and every server box report their laid-out rects, and the wires are a pure
//! function of those rects, so they re-bow whenever the layout reflows.

use idyll::{live_view, Ctx, MutableVec, Rect, Setup, Shape, Signal};
use idyll_styles::{styles, Style};

use crate::atoms::button::styles as bstyles;
use crate::atoms::lanes::{Ink, LaneTier};
use crate::atoms::latency_table::LatencyTable;
use crate::atoms::legend::{Key, Legend};
use crate::atoms::server_box::{group, server_name, ServerBox};
use crate::atoms::sim_card::{styles as card, SimHeader};
use crate::atoms::stage::Stage;
use crate::atoms::wires::{bow_across, set_rect, styles as wstyles, wires, REQUEST, RESPONSE, SHED};
use crate::engine::{ServerCounts, NET_MS};
use crate::multi::{axis_for, sim_seed, Leg, MultiEngine, SERVERS};
use blog_core::SimKey;

/// A DOM measurement of one measured element in the stage (keyed by what it is), or an
/// animation frame carrying its millisecond delta.
#[derive(Debug)]
pub enum FanMsg {
    Stage(Rect),
    Client(usize, Rect),
    Server(usize, Rect),
    Tick(f64),
    /// Send the batch. A second send is a fresh one: the fleet goes back to rest and the
    /// same routing takes the same wires again.
    Send,
}

const CLIENTS: usize = 10;

/// The most virtual time one animation frame may advance. A frame the browser was slow to
/// deliver would otherwise settle the batch in one step, and the picture exists to be watched.
const MAX_STEP_MS: f64 = 32.0;

pub(crate) async fn run(ctx: Ctx<Setup, FanMsg>, _seed: crate::PageSeed, key: SimKey) -> idyll::Result {
    // Fired, not settled: routing is a draw per client and costs nothing, so the still knows
    // where every client went — while the batch it has yet to pay for settles in the browser,
    // a frame of virtual time per animation frame. Settling here would put a hundred requests
    // through ten real towers before the page could be sent.
    let mut engine = MultiEngine::with_clients(sim_seed(&key), CLIENTS);
    engine.fire();
    let assignment: Vec<usize> = engine.assignment().to_vec();

    // Each server's client-count colours the clients that chose it. Routing is settled, so
    // this is the one reading the still can make.
    let mut per_server_clients = vec![0usize; SERVERS];
    for &s in &assignment {
        per_server_clients[s] += 1;
    }
    let clients: Vec<Style> = assignment.iter().map(|&s| load_colour(per_server_clients[s])).collect();
    // A dot goes orange while its client is owed answers, over the load tint it rests in —
    // the same word the strip cards' circles speak.
    let busy: Vec<_> = (0..CLIENTS).map(|_| ctx.mutable_signal(String::new())).collect();
    let dots: Vec<(usize, Style, Signal<String>)> =
        clients.into_iter().enumerate().map(|(i, col)| (i, col, busy[i].read())).collect();
    let batch_count = format!(
        "{CLIENTS} clients · {SERVERS} servers · {} requests",
        CLIENTS * crate::multi::REQS_PER_CLIENT
    );

    let bars = ctx.mutable_signal(Vec::new());
    let axis = ctx.mutable_signal(1.0_f64);
    let (latency_rows, latency_axis) = (bars.read(), axis.read());
    let shed_tally = ctx.mutable_signal(group(0));
    let aggs = vec![("shed", shed_tally.read())];
    let keys = vec![
        Key { ink: Stage::teal.value(), means: "request" },
        Key { ink: Stage::amber.value(), means: "shed → retry" },
        Key { ink: Stage::green.value(), means: "response" },
    ];

    // The stage's own picture, in two layers: the sticky wires underneath and the traffic on
    // them. The wire geometry is a pure function of three measurements — the stage's origin
    // and every client/server rect — so the wires re-bow as the layout reflows. The outbound
    // request is split by outcome (a shed server's wire wears its own colour); the response
    // is under every wire.
    let sticky_wires: MutableVec<Shape> = ctx.mutable_vec();
    let traffic: MutableVec<Shape> = ctx.mutable_vec();
    let picture = ctx.mutable_vec_of(vec![sticky_wires.clone(), traffic.clone()]);
    let mut stage: Option<Rect> = None;
    let mut client_rects: Vec<Option<Rect>> = vec![None; CLIENTS];
    let mut server_rects: Vec<Option<Rect>> = vec![None; SERVERS];
    // A client's request is shed → retried when the server it stuck to sheds any: its wire
    // takes the amber "shed → retry", the rest the teal "request". The one imbalance the
    // chapter is about, read straight off the wire — and it is earned as the batch settles,
    // so a wire changes colour at the moment its server refuses something.
    let mut client_shed = vec![false; CLIENTS];

    // Traffic is the batch's own requests, each a pulse at the fraction of its wire the
    // engine's clock says it has covered — riding out to the server and home again. A
    // message-driven tick (Elm's `Sub`), so the animation replays from the log. The wires
    // hold still until the reader asks for them: sending is what puts the batch down them,
    // and a settled batch's wires stand empty.
    let running = ctx.mutable_signal(false);
    ctx.frames(&running.read(), FanMsg::Tick);
    // Ten sticky wires, so the tier is exactly that size — and per-wire merging is idle
    // here, since a batch's requests land on distinct wires.
    let mut tier = LaneTier::new(CLIENTS, NET_MS);

    // Each box reads its own machine as the batch settles — the same live shape the cluster
    // grid passes to this atom. A server nobody stuck to stays idle for the whole batch, which
    // routing already knows, so that much is fixed from the still.
    let counts: Vec<_> =
        (0..SERVERS).map(|_| ctx.mutable_signal(ServerCounts::default())).collect();
    let boxes: Vec<(usize, String, Signal<ServerCounts>, Signal<bool>)> = (0..SERVERS)
        .map(|i| {
            (i, server_name(i), counts[i].read(), ctx.constant(per_server_clients[i] == 0))
        })
        .collect();

    let mut ctx = ctx.render(live_view! {
        div css=[crate::atoms::sim_card::styles::CARD] {
            SimHeader title=("Fan-out · 10 clients → 10 servers") aggs=(aggs)
            div css=[card::CTRLS] {
                button css=[bstyles::BASE, bstyles::CTA]
                    onclick=>(|_| Some(FanMsg::Send)) { "▶ send" }
                span css=[card::COUNT] { (batch_count) }
            }
            div css=[wstyles::STAGE] measure=>(|e| e.rect().map(FanMsg::Stage)) {
                div css=[wstyles::CLIENTS] {
                    @for (i, col, waiting) in (dots) {
                        div css=[wstyles::CNODE] {
                            span css=[wstyles::DOT, col] style=($waiting)
                                measure=>(move |e| e.rect().map(|r| FanMsg::Client(i, r))) {}
                            span css=[wstyles::CL] { (format!("c{}", i + 1)) }
                        }
                    }
                }
                div css=[wstyles::SERVERS] {
                    @for (i, name, counts, idle) in (boxes) {
                        div measure=>(move |e| e.rect().map(|r| FanMsg::Server(i, r))) {
                            ServerBox name=(name) counts=(counts) idle=(idle)
                        }
                    }
                }
                canvas css=[wstyles::WIRES, wstyles::UNDER] painting=(picture) {}
            }
            div css=[styles::LABEL] { "latency · box p25–p95 · line p50 · from this batch" }
            LatencyTable bars=(latency_rows) axis=(latency_axis)
            Legend keys=(keys)
        }
    }).await?;
    // The dots' local mirror, so only a change of state touches the DOM.
    let mut waiting: Vec<String> = vec![String::new(); CLIENTS];
    loop {
        let (msg, turn) = ctx.recv().await?;
        let mut relaid = false;
        match msg {
            FanMsg::Stage(rect) => {
                stage = Some(rect);
                relaid = true;
            }
            FanMsg::Client(i, rect) => {
                set_rect(&mut client_rects, i, rect);
                relaid = true;
            }
            FanMsg::Server(i, rect) => {
                set_rect(&mut server_rects, i, rect);
                relaid = true;
            }
            FanMsg::Tick(dt) => {
                let dt = dt.min(MAX_STEP_MS);
                engine.tick(dt);

                let fleet = engine.fleet();
                let shed: Vec<bool> =
                    assignment.iter().map(|&s| fleet[s].counts.retried > 0).collect();
                if shed != client_shed {
                    client_shed = shed;
                    relaid = true;
                }
                for (signal, server) in counts.iter().zip(&fleet) {
                    signal.set(&turn, server.counts);
                }
                let outstanding = engine.outstanding();
                for (c, (signal, was)) in busy.iter().zip(waiting.iter_mut()).enumerate() {
                    let now = match outstanding.get(c).copied().unwrap_or(0) > 0 {
                        true => format!("background:{}", Stage::orange.value()),
                        false => String::new(),
                    };
                    if *was != now {
                        *was = now.clone();
                        signal.set(&turn, now);
                    }
                }

                // The batch's departures become pulses: sends out in teal, answers home in
                // green, refusals home in amber — real requests, so a settled batch's wires
                // go still and stay still.
                let launches = engine.wire_events().into_iter().filter_map(|event| {
                    let Leg::Server { sender, server } = event.leg else { return None };
                    let ink = match (event.homeward, event.outcome) {
                        (false, _) => Ink::Sent,
                        (true, Some(outcome)) if outcome.refused() => Ink::Refusal,
                        (true, _) => Ink::Answer,
                    };
                    Some(((sender, server), event.homeward, ink))
                });
                tier.frame(launches, dt, 1.0);

                let summary = engine.so_far();
                let rows = summary.bars(summary.phases());
                axis.set(&turn, axis_for(&rows));
                bars.set(&turn, rows);
                shed_tally.set(&turn, group(summary.shed()));

                // A settled batch has nothing left to pay, so the frames stop and the picture
                // holds at the reading it earned — once the last afterglow of it has decayed,
                // since a canvas is redrawn by the loop or not at all.
                if engine.settled() && !tier.airborne() {
                    running.set(&turn, false);
                }
            }
            FanMsg::Send => {
                // A second send is a fresh batch, not more load on machines the first one left
                // warm. Routing is a function of the seed, so the wires the still drew are the
                // wires this batch takes — the picture holds and the numbers start over.
                engine = MultiEngine::with_clients(sim_seed(&key), CLIENTS);
                engine.fire();
                client_shed = vec![false; CLIENTS];
                relaid = true;
                for signal in &counts {
                    signal.set(&turn, ServerCounts::default());
                }
                bars.set(&turn, Vec::new());
                axis.set(&turn, 1.0);
                shed_tally.set(&turn, group(0));
                running.set(&turn, true);
            }
        }
        if relaid {
            sticky_wires.sync(
                &turn,
                sticky_of(&stage, &client_rects, &server_rects, &assignment, &client_shed),
            );
        }
        traffic.sync(&turn, traffic_of(&stage, &client_rects, &server_rects, &tier));
    }
}

/// The layer under the traffic: a bowed cubic from each client dot's centre to the left edge
/// of the server it stuck to, in stage-local pixels (the measured rects are root-relative, so
/// each is offset by the stage's own origin). Empty until the stage has been measured; each
/// wire waits on both its endpoints.
fn sticky_of(
    stage: &Option<Rect>,
    clients: &[Option<Rect>],
    servers: &[Option<Rect>],
    assignment: &[usize],
    client_shed: &[bool],
) -> Vec<Shape> {
    let mut layer = Vec::new();
    // The response is home on every wire; the outbound splits by outcome, and each client is on
    // exactly one server.
    let sticky = || assignment.iter().enumerate().map(|(c, &s)| (c, s));
    let shed_by = |c: usize| client_shed.get(c).copied().unwrap_or(false);
    let outcome = |shed: bool| sticky().filter(move |&(c, _)| shed_by(c) == shed);
    wires(stage, clients, servers, sticky(), RESPONSE, &mut layer);
    wires(stage, clients, servers, outcome(false), REQUEST, &mut layer);
    wires(stage, clients, servers, outcome(true), SHED, &mut layer);
    layer
}

/// The layer over it: the batch's traffic, along the same bows.
fn traffic_of(
    stage: &Option<Rect>,
    clients: &[Option<Rect>],
    servers: &[Option<Rect>],
    tier: &LaneTier,
) -> Vec<Shape> {
    let mut layer = Vec::new();
    tier.paint(
        |(c, s)| {
            let stage = stage.as_ref()?;
            Some(bow_across(stage, clients.get(c)?.as_ref()?, servers.get(s)?.as_ref()?))
        },
        &mut layer,
    );
    layer
}

/// A client's colour, by how many clients its server carries — grey when alone, warming
/// to the shed-orange as the pile grows (the preview's `loadCol`).
fn load_colour(clients_on_server: usize) -> Style {
    match clients_on_server {
        0 | 1 => styles::LOAD0,
        2 => styles::LOAD1,
        3 => styles::LOAD2,
        _ => styles::LOAD3,
    }
}

#[styles]
pub mod styles {
    use idyll_styles::Style;
    use crate::atoms::stage::Stage;


    pub const LABEL: Style = css! {{
        font_size: "10px",
        letter_spacing: ".08em",
        text_transform: "uppercase",
        color: "#aab09c",
        font_weight: 600,
        margin: "16px 2px 8px",
    }};
    // Client dot ring by their server's load — the same amber→red ramp the shed wires and
    // the stage draw in, read straight off the palette so a retint is one edit in `Stage`.
    // The resting ring alone is a plain neutral (no palette token names it).
    pub const LOAD0: Style = css! {{ border_color: "#c2c8b6" }};
    pub const LOAD1: Style = css! {{ border_color: Stage::amber }};
    pub const LOAD2: Style = css! {{ border_color: Stage::orange }};
    pub const LOAD3: Style = css! {{ border_color: Stage::red }};
}
