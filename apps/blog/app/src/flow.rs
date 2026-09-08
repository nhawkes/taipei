//! **Sims 4 and 5 · Flow** — demand that does not stop, and the choice each client makes in it.
//!
//! The spike sims ask what one batch cost. Nothing can be balanced there: every client sends
//! at once, knowing nothing, so the draw *is* the routing. Under continuous demand a client
//! sends again and again and gets answers back between sends, and that is the whole of what a
//! policy has to work with.
//!
//! Both cards run the one fleet ([`MultiEngine::flowing`]) — ten real taipei stacks admitting
//! on CPU backpressure — and differ only in what they let the reader do with it. The first
//! shows that the loop exists: ten machines filling unevenly under one arrival rate. The
//! second puts the three policies on a pill and lets the reader change which one the crowd is
//! using *while it runs*, because what a policy is worth is a comparison and a comparison the
//! reader has to reload for is not one.
//!
//! Switching does not reset: what is already in the air was sent under the old policy and is
//! still owed an answer under it. The fleet the reader is looking at is the fleet they have
//! been loading all along.

use std::collections::VecDeque;

use idyll::{live_view, Ctx, MutableVec, Rect, Setup, Shape, Signal};
use idyll_styles::styles;

use crate::atoms::button::styles as bstyles;
use crate::atoms::client_strip::{ink, ink_frame, ClientStrip};
use crate::atoms::lanes::{Ink, LaneTier, WIRES};
use crate::atoms::legend::{Key, Legend};
use crate::atoms::server_box::{group, server_name, ServerBox};
use crate::atoms::sim_card::{styles as card, SimHeader};
use crate::atoms::cut::{at_rest, cut_label, ink_of, styles as cut_styles, CUT_SCALE};
use crate::atoms::slider::{fmt_speed, raw_from_speed, speed_from_raw, Name, Scale, Slider};
use crate::atoms::toggle::{knob, Tone, ToggleGroup, ToggleItem};
use crate::atoms::waterfall::{Against, Leg, Waterfall};
use crate::atoms::wires::{bow_down, set_rect, styles as wstyles, wires_down, IDLE};
use crate::engine::{ServerCounts, NET_MS};
use crate::multi::{
    phase_ms, remember, sim_seed, Batch, Leg as WireLeg, MultiEngine, Phase, Policy, Record,
    PHASES, SERVERS,
};
use blog_core::SimKey;

/// Which of the three cards this is.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Shows {
    /// The loop itself: one rate, ten machines, and the spread between them.
    Loop,
    /// The same fleet with the choice on a pill and the experience underneath.
    Policies,
    /// The pill again, pick-2 no longer idealised: counters are what the machines said, a
    /// pool decides who a client can ask, and the crowd is small enough that its counters
    /// mean something — twenty clients, because at a hundred no pool size keeps a counter
    /// fresh at six answers a second each.
    Pool,
}

#[derive(Debug)]
pub enum FlowMsg {
    Tick(f64),
    /// The run control. A card that ran on mount would have spent the reader's first second
    /// of attention before they arrived.
    Toggle,
    Qps(f64),
    /// Dilate the engine's clock — slow motion. Fronts stretch with it; afterglow does not.
    Speed(f64),
    /// Show the policy at this index. The crowd changes its mind between requests.
    Policy(usize),
    /// Move one of the two percentile readings.
    Cut(usize, f64),
    /// Resize every client's pool. A parameter change, so the pick-2 column starts over.
    Pool(f64),
    /// The stage and its wire ends, as layout places them: the tier, a strip circle, a
    /// server box. The wires are a pure function of these.
    Stage(Rect),
    Dot(usize, Rect),
    Server(usize, Rect),
}

/// The crowd: a hundred clients against the ten-machine fleet, one request at a time each.
/// A hundred is enough that no single client's luck is the reading, and the fleet is the same
/// ten machines every other card in the chapter draws.
const CLIENTS: usize = 100;

/// The pool card's crowd. Smaller on purpose: a counter is only as fresh as the answers the
/// client itself receives, and at a hundred clients each hears six a second — every counter
/// a whole queue-deadline stale before its next send. Twenty clients hear thirty a second,
/// which is the regime where what a machine said last is still worth acting on.
const POOL_CLIENTS: usize = 20;

/// Where the pool slider opens: the whole fleet, so the card opens on the real pick-2 at its
/// best and shrinking the pool is the experiment rather than the starting handicap.
const OPENS_AT_POOL: usize = SERVERS;

/// The rate both cards open at. The fleet's gate admits on cores, and what it lets through is
/// near 680 a second — so this is the loaded-but-serving load the chapter's prose describes,
/// where a machine's queue is worth something and no machine has given up.
const OPENS_AT: f64 = 600.0;

/// The most virtual time one frame may advance. A tab returning from the background hands over
/// a delta of seconds, and a message arm runs to completion — an unclamped delta is one long
/// freeze rather than one long frame.
const MAX_STEP_MS: f64 = 32.0;

/// Headroom past the slowest reading, so the longest bar stops short of the edge.
const AXIS_HEADROOM: f64 = 1.05;

/// Where the three columns are read, until the reader moves it. One dial over all three:
/// the columns exist to be compared, and three dials standing at three different places would
/// be three readings of nothing in particular.
const OPENS_AT_PC: f64 = 90.0;

/// How far either side of the dial a reading is drawn from. Five percentiles either way of a
/// full column is a band of a few hundred trips — enough that no one client's luck is the
/// reading, and narrow enough that p90 is still the late tail rather than a smear of the middle.
const BAND_PC: f64 = 5.0;

/// How many of its policy's answered trips a column keeps — the sample its readings are cut
/// from, and so what a column left standing *means*. Queue noise comes in bursts a few hundred
/// ms wide, so readings a moment apart do not average it away; a window has to span many bursts
/// before no single one of them is the reading. At the rate the card opens on this is about
/// seven seconds of trips, which steadies the p90 band to a few percent — small next to the
/// gaps between the policies, which are what the columns exist to show.
const COLUMN_SAMPLE: usize = 4000;

/// Virtual ms between recuts of the columns' readings. Sorting three full windows is the cost
/// [`SUMMARY_MS`](crate::multi::SUMMARY_MS) names — far too dear for every frame — and the bars
/// ease over [`EASE_MS`] anyway, so a faster feed would be motion the browser flattens.
const RECUT_MS: f64 = 250.0;

/// How long a bar takes to close two thirds of the gap to the reading it is heading for, in
/// virtual ms. A segment carries a 420 ms ease of its own, so a constant much shorter than that
/// is smoothing the browser flattens anyway; at a quarter second a bar is within two percent of
/// a new reading one second later, which is soon enough that a policy switch reads as a switch.
const EASE_MS: f64 = 250.0;

/// The policies, in the order the chapter introduces them.
const POLICIES: [(&str, Policy); 3] = [
    ("Always Random", Policy::AlwaysRandom),
    ("Repick on Queue Timeout", Policy::RepickOnQueueTimeout),
    ("Power of Two", Policy::PowerOfTwoVirtual),
];

/// The card's three policies. One label, two power-of-twos: the policy card reads the
/// machines for nothing, the pool card pays for its counters — the same idea either side of
/// the cheat the chapter calls out.
fn cast(shows: Shows) -> [(&'static str, Policy); 3] {
    let mut cast = POLICIES;
    if shows == Shows::Pool {
        cast[2].1 = Policy::PowerOfTwo;
    }
    cast
}

/// The column the other two are read against: picking blindly, every time. It is the policy a
/// client has before it has an idea, so what an idea is worth is the difference from it.
const BASELINE: usize = 0;

/// The pick-2 column — the one the pool belongs to, and the only one a pool change resets.
const PICK2: usize = 2;

pub(crate) async fn run(
    ctx: Ctx<Setup, FlowMsg>,
    _seed: crate::PageSeed,
    key: SimKey,
    shows: Shows,
) -> idyll::Result {
    let cast = cast(shows);
    let clients = match shows {
        Shows::Pool => POOL_CLIENTS,
        _ => CLIENTS,
    };
    let batch = Batch { clients, servers: SERVERS, reqs_per_client: 1 };
    // Built at rest and not ticked until the reader plays: a fleet settled during mount would
    // spend a page's whole budget before the page was sent.
    let mut engine = MultiEngine::flowing(sim_seed(&key), batch, OPENS_AT, Policy::AlwaysRandom);

    let running = ctx.mutable_signal(false);
    ctx.frames(&running.read(), FlowMsg::Tick);
    let run_label = {
        let running = running.read();
        ctx.computed(move |cx| match running.get(cx) {
            true => "❚❚ Pause".to_string(),
            false => "▶ Run".to_string(),
        })
        .read()
    };

    let qps = ctx.mutable_signal(OPENS_AT);
    let served = ctx.mutable_signal(group(0));
    let refused = ctx.mutable_signal(group(0));
    let aggs = vec![("served", served.read()), ("retried", refused.read())];

    // The crowd itself, one circle each: orange while a client is owed an answer. Above the
    // fleet, in the order a request meets them.
    let ink_signals: Vec<_> = (0..clients).map(|_| ctx.mutable_signal(ink(false))).collect();
    let strip: Vec<(usize, Signal<String>)> =
        ink_signals.iter().enumerate().map(|(c, s)| (c, s.read())).collect();
    let strip_count = ctx.constant(clients);
    let measured = ctx.callback(|(c, rect): (usize, Rect)| FlowMsg::Dot(c, rect));

    // The stage's picture, in two layers: underneath, the warm wires — every connection a
    // client may use, each a bow from its circle to a machine, the whole fleet here and the
    // client's own pool on the pool card — and over them, what travels.
    let lattice: MutableVec<Shape> = ctx.mutable_vec();
    let traffic: MutableVec<Shape> = ctx.mutable_vec();
    let picture = ctx.mutable_vec_of(vec![lattice.clone(), traffic.clone()]);
    let mut stage_rect: Option<Rect> = None;
    let mut dot_rects: Vec<Option<Rect>> = vec![None; clients];
    let mut box_rects: Vec<Option<Rect>> = vec![None; SERVERS];
    let mut warm = engine.pools();
    // What travels the lattice: two-edge pulses on pooled lanes, launched per departure —
    // the fronts on the engine's clock, the afterglow on the reader's.
    let mut tier = LaneTier::new(WIRES, NET_MS);
    let speed = ctx.mutable_signal(raw_from_speed(1.0));
    let speed_at = speed.read();

    // Each box reads its own machine. The spread between them is the chapter's subject, so the
    // ten are drawn together and none of them is summarised away.
    let counts: Vec<_> =
        (0..SERVERS).map(|_| ctx.mutable_signal(ServerCounts::default())).collect();
    let boxes: Vec<(usize, String, Signal<ServerCounts>, Signal<bool>)> =
        (0..SERVERS).map(|i| (i, server_name(i), counts[i].read(), ctx.constant(false))).collect();

    // The gap between the busiest machine and the emptiest — what picking blindly costs,
    // before any of it has become a latency. This is the reading the loop card is for.
    let spread = ctx.mutable_signal("—".to_string());

    let policy = ctx.mutable_signal(0usize);
    let items: Vec<ToggleItem> = cast
        .iter()
        .enumerate()
        .map(|(i, (label, _))| {
            let chosen = policy.read();
            (i, (*label).into(), ctx.computed(move |cx| chosen.get(cx) == i).read(), Tone::Normal)
        })
        .collect();
    let pill_knob = knob(&ctx, &items);

    // One column per policy, each holding the reading that policy last earned. Only the one the
    // crowd is using moves; the other two keep what they were left with, which is the whole
    // point — a comparison the reader can look at rather than remember.
    let legs: Vec<_> = POLICIES.iter().map(|_| ctx.mutable_signal(at_rest())).collect();
    let opening = notes_for(&POLICIES.map(|_| VecDeque::new()), 0, false, &cast);
    let notes: Vec<_> =
        opening.into_iter().map(|note| ctx.mutable_signal(note)).collect();
    // Whether this column has a reading at all. Separate from its note, because the leftmost
    // rule silences the notes of columns that are just as empty as the one that speaks.
    let ready: Vec<_> = POLICIES.iter().map(|_| ctx.mutable_signal(false)).collect();
    let axis = ctx.mutable_signal(1.0_f64);
    let dial = ctx.mutable_signal(OPENS_AT_PC);
    let cut_ink = {
        let at = dial.read();
        ctx.computed(move |cx| ink_of(at.get(cx))).read()
    };
    let cut_at = dial.read();
    #[allow(clippy::type_complexity)]
    let columns: Vec<(
        &'static str,
        Signal<Vec<Leg>>,
        Signal<f64>,
        Signal<String>,
        Signal<String>,
        Signal<String>,
        Signal<String>,
    )> =
        cast.iter()
            .enumerate()
            .map(|(i, (label, _))| {
                let note = notes[i].read();
                // Bars if this column has earned a reading; the note if it has something to say.
                // Carried as declarations rather than branches: the rebuild closure a `@for`
                // gives its body cannot hand its signals to a nested `@if`.
                let hide_unless = |yes: Signal<bool>| {
                    ctx.computed(move |cx| match yes.get(cx) {
                        true => String::new(),
                        false => "display:none".to_string(),
                    })
                    .read()
                };
                let bars = hide_unless(ready[i].read());
                let has_note = {
                    let note = note.clone();
                    ctx.computed(move |cx| !note.get(cx).is_empty()).read()
                };
                let said = hide_unless(has_note);
                (*label, legs[i].read(), axis.read(), note, bars, said, cut_ink.clone())
            })
            .collect();
    let pool_columns = columns.clone();

    // The phone's one column: a mirror of whichever column the pill has chosen, because three
    // of them side by side is a reading nobody can take on a phone. Its own signals, since a
    // template cannot re-point at another column's.
    let phone_label = ctx.mutable_signal(cast[0].0.to_string());
    let phone_legs = ctx.mutable_signal(at_rest());
    let phone_note = ctx.mutable_signal(phone_note_for(false, false));
    let phone_on = ctx.mutable_signal(false);
    let (phone_bars, phone_said) = {
        let on = phone_on.read();
        let bars = ctx
            .computed(move |cx| match on.get(cx) {
                true => String::new(),
                false => "display:none".to_string(),
            })
            .read();
        let note = phone_note.read();
        let said = ctx
            .computed(move |cx| match note.get(cx).is_empty() {
                true => "display:none".to_string(),
                false => String::new(),
            })
            .read();
        (bars, said)
    };

    let pool = ctx.mutable_signal(OPENS_AT_POOL as f64);
    let pool_at = pool.read();
    let phone_legs_sig = phone_legs.read();
    let phone_axis = axis.read();

    let picked = ctx.callback(FlowMsg::Policy);
    let keys: Vec<Key> = PHASES.iter().map(|p| Key { ink: p.ink, means: p.label }).collect();
    let qps_at = qps.read();
    let title = match shows {
        Shows::Loop => "Continuous demand · 100 clients → 10 servers",
        Shows::Policies => "Picking a server · 100 clients → 10 servers",
        Shows::Pool => "Picking a server · 20 clients → 10 servers",
    };
    let is_policies = shows == Shows::Policies;
    let is_pool = shows == Shows::Pool;
    let has_pill = is_policies || is_pool;
    let spread_text = spread.read();

    let mut ctx = ctx.render(live_view! {
        div css=[card::CARD] role=("group") aria_label=(title) {
            SimHeader title=(title) aggs=(aggs)
            div css=[card::CTRLS] {
                button css=[bstyles::BASE, bstyles::CTA]
                    onclick=>(|_| Some(FlowMsg::Toggle)) { $run_label }
                Slider name=(Name::new("arrivals", 52)) scale=(Scale::new(50, 900, 10))
                    at=(qps_at) fmt=(fmt_qps) moved=>(|n| FlowMsg::Qps(n))
                Slider name=(Name::new("speed", 38)) scale=(Scale::new(0, 100, 1))
                    at=(speed_at) fmt=(fmt_speed) moved=>(|n| FlowMsg::Speed(n))
            }
            @if (has_pill) {
                div css=[card::CTRLS] {
                    ToggleGroup items=(items) knob=(pill_knob) picked=(picked)
                }
            }
            div css=[wstyles::TIER] measure=>(|e| e.rect().map(FlowMsg::Stage)) {
                ClientStrip dots=(strip) count=(strip_count) measured=(measured)
                div css=[styles::FLEET] {
                    @for (at, name, counts, idle) in (boxes) {
                        div measure=>(move |e| e.rect().map(|r| FlowMsg::Server(at, r))) {
                            ServerBox name=(name) counts=(counts) idle=(idle)
                        }
                    }
                }
                canvas css=[wstyles::WIRES, wstyles::UNDER] painting=(picture) {}
            }
            div css=[styles::LABEL] { "busiest machine over emptiest · " span { $spread_text } }
            @if (has_pill) {
                div css=[styles::LABEL] { "experience · shared ms scale · recent requests" }
                div css=[styles::DIAL] style=($cut_ink) {
                    Slider name=(Name::new("", 0)) scale=(CUT_SCALE) at=(cut_at)
                        fmt=(cut_label) moved=>(|n| FlowMsg::Cut(0, n))
                        ?tint=(cut_styles::Cut::ink.value())
                        ?readout_ink=(cut_styles::Cut::ink.value())
                }
            }
            @if (is_policies) {
                div css=[cut_styles::CHARTS] {
                    @for (label, legs, axis, note, bars, said, ink) in (columns) {
                        div css=[cut_styles::COL] style=($ink) {
                            div css=[styles::COLH] { (label) }
                            div style=($bars) { Waterfall legs=(legs) axis=(axis) }
                            div css=[styles::NULL] style=($said) { $note }
                        }
                    }
                }
            }
            @if (is_pool) {
                div css=[styles::WIDE] {
                    @for (label, legs, axis, note, bars, said, ink) in (pool_columns) {
                        div css=[cut_styles::COL] style=($ink) {
                            div css=[styles::COLH] { (label) }
                            div style=($bars) { Waterfall legs=(legs) axis=(axis) }
                            div css=[styles::NULL] style=($said) { $note }
                        }
                    }
                }
                div css=[styles::PHONE] style=($cut_ink) {
                    div css=[styles::COLH] { $phone_label }
                    div style=($phone_bars) { Waterfall legs=(phone_legs_sig) axis=(phone_axis) }
                    div css=[styles::NULL] style=($phone_said) { $phone_note }
                }
                div css=[styles::POOLROW] {
                    div {}
                    div {}
                    div {
                        Slider name=(Name::new("pool", 34)) scale=(Scale::new(2, 10, 1))
                            at=(pool_at) fmt=(fmt_pool) moved=>(|n| FlowMsg::Pool(n))
                    }
                }
            }
            @if (has_pill) {
                Legend keys=(keys)
            }
        }
    }).await?;

    // Where the dial stands and what each column has earned, as the loop's own values. A frame
    // needs the numbers themselves; the signals are what the view reads.
    let mut stands_at = OPENS_AT_PC;
    // What each policy has to show for itself: its last [`COLUMN_SAMPLE`] answered trips,
    // credited as they land. The readings are re-cut from these, so moving the dial moves all
    // three columns and not only the one still running.
    let mut earned: Vec<VecDeque<Felt>> = POLICIES.iter().map(|_| VecDeque::new()).collect();
    // How many trips have been credited — the fleet's own tally names the frame's landings.
    let mut credited = 0;
    // The reading each column is heading for, recut every [`RECUT_MS`].
    let mut heading: Vec<Option<Band>> = POLICIES.iter().map(|_| None).collect();
    let mut since_recut = RECUT_MS;
    // What the reader is looking at, which is not the same thing: the bars follow the reading
    // over [`EASE_MS`] rather than taking it, so a band that moves between two frames moves them
    // by a fraction of the distance and the card settles instead of flickering.
    let mut shown: Vec<Option<Band>> = POLICIES.iter().map(|_| None).collect();
    let mut shown_axis: Option<f64> = None;
    let mut wants_axis: Option<f64> = None;
    let mut chosen = BASELINE;
    let mut playing = false;
    // The strip's local mirror, so only changed faces touch their signals — and the loop's
    // own clock dilation.
    let mut faces: Vec<String> = vec![ink(false); clients];
    let mut speed_now = 1.0f64;
    loop {
        let (msg, turn) = ctx.recv().await?;
        let mut relaid = false;
        match msg {
            FlowMsg::Toggle => {
                playing = !playing;
                // Pausing stops departures, never travel already in the air — and the picture
                // is drawn by this loop or not at all, so the loop outlives the last pulse.
                running.set(&turn, playing || tier.airborne());
                for (note, text) in notes.iter().zip(notes_for(&earned, chosen, playing, &cast)) {
                    note.set(&turn, text);
                }
                if is_pool {
                    phone_note.set(&turn, phone_note_for(!earned[chosen].is_empty(), playing));
                }
            }
            FlowMsg::Qps(v) => {
                qps.set(&turn, v);
                engine.set_qps(v);
                // The load is the comparison's ground: a reading earned at one rate beside one
                // earned at another compares the loads, not the policies. The columns start
                // over at the new rate.
                for mine in earned.iter_mut() {
                    mine.clear();
                }
                heading = POLICIES.iter().map(|_| None).collect();
                shown = POLICIES.iter().map(|_| None).collect();
                shown_axis = None;
                wants_axis = None;
                for flag in ready.iter() {
                    flag.set(&turn, false);
                }
                for (note, text) in notes.iter().zip(notes_for(&earned, chosen, playing, &cast)) {
                    note.set(&turn, text);
                }
                if is_pool {
                    phone_on.set(&turn, false);
                    phone_note.set(&turn, phone_note_for(false, playing));
                }
            }
            FlowMsg::Policy(i) => {
                policy.set(&turn, i);
                engine.set_policy(cast[i].1);
                chosen = i;
                for (note, text) in notes.iter().zip(notes_for(&earned, chosen, playing, &cast)) {
                    note.set(&turn, text);
                }
                if is_pool {
                    phone_label.set(&turn, cast[chosen].0.to_string());
                    phone_on.set(&turn, !earned[chosen].is_empty());
                    phone_note.set(&turn, phone_note_for(!earned[chosen].is_empty(), playing));
                    if let Some(column) = columns_of(&shown).swap_remove(chosen) {
                        phone_legs.set(&turn, column);
                    }
                }
            }
            FlowMsg::Pool(v) => {
                pool.set(&turn, v);
                engine.set_pool(v as usize);
                warm = engine.pools();
                relaid = true;
                // The pool is the pick-2 column's own parameter: a reading earned against one
                // pool size says nothing about another, so that column starts over — and only
                // that one, because the other two policies never look at a pool.
                earned[PICK2].clear();
                heading[PICK2] = None;
                shown[PICK2] = None;
                ready[PICK2].set(&turn, false);
                for (note, text) in notes.iter().zip(notes_for(&earned, chosen, playing, &cast)) {
                    note.set(&turn, text);
                }
                if is_pool && chosen == PICK2 {
                    phone_on.set(&turn, false);
                    phone_note.set(&turn, phone_note_for(false, playing));
                }
            }
            FlowMsg::Speed(raw) => {
                speed.set(&turn, raw);
                speed_now = speed_from_raw(raw);
            }
            FlowMsg::Stage(rect) => {
                stage_rect = Some(rect);
                relaid = true;
            }
            FlowMsg::Dot(c, rect) => {
                set_rect(&mut dot_rects, c, rect);
                relaid = true;
            }
            FlowMsg::Server(s, rect) => {
                set_rect(&mut box_rects, s, rect);
                relaid = true;
            }
            FlowMsg::Cut(_, pc) => {
                stands_at = pc;
                dial.set(&turn, pc);
                // The reader asked for a different reading, so the bars take it whole. Easing
                // toward it would answer the dial with a second of travel from a number the
                // reader has just stopped asking about.
                heading = bands_at(&earned, pc);
                since_recut = 0.0;
                shown = heading.clone();
                for (signal, column) in legs.iter().zip(columns_of(&shown)) {
                    if let Some(column) = column {
                        signal.set(&turn, column);
                    }
                }
                if is_pool {
                    if let Some(column) = columns_of(&shown).swap_remove(chosen) {
                        phone_legs.set(&turn, column);
                    }
                }
            }
            FlowMsg::Tick(wall) => {
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
                let loads: Vec<usize> = fleet.iter().map(|s| s.counts.inflight).collect();
                spread.set(&turn, spread_text_of(&loads));

                let answered = engine.answered();
                served.set(&turn, group(answered.trips));
                refused.set(&turn, group(answered.refusals));
                ink_frame(&turn, &mut faces, &ink_signals, &engine.outstanding());

                // The frame's departures become pulses: a send in teal, an answer home in
                // green, a refusal home in amber — and the amber ride followed by the same
                // client's fresh teal send is the retry chain, live.
                let launches = engine.wire_events().into_iter().filter_map(|event| {
                    let WireLeg::Server { sender, server } = event.leg else { return None };
                    let ink = match (event.homeward, event.outcome) {
                        (false, _) => Ink::Sent,
                        (true, Some(outcome)) if outcome.refused() => Ink::Refusal,
                        (true, _) => Ink::Answer,
                    };
                    Some(((sender, server), event.homeward, ink))
                });
                tier.frame(launches, wall, speed_now);
                running.set(&turn, playing || tier.airborne());

                let landed = answered.trips - credited;
                credited = answered.trips;
                if engine.with_records(|records| credit(&mut earned, records, landed, &cast)) {
                    for (flag, mine) in ready.iter().zip(&earned) {
                        flag.set(&turn, !mine.is_empty());
                    }
                    for (note, text) in notes.iter().zip(notes_for(&earned, chosen, true, &cast)) {
                        note.set(&turn, text);
                    }
                    if is_pool {
                        phone_on.set(&turn, !earned[chosen].is_empty());
                        phone_note.set(&turn, phone_note_for(!earned[chosen].is_empty(), true));
                    }
                }
                since_recut += dt;
                if since_recut >= RECUT_MS {
                    since_recut = 0.0;
                    heading = bands_at(&earned, stands_at);
                    // One axis over all three, so a column's bars mean the same length as its
                    // neighbour's — which is the only way three of them are a comparison.
                    wants_axis = earned
                        .iter()
                        .flatten()
                        .map(|f| f.total_ms)
                        .reduce(f64::max)
                        .map(|slowest| (slowest * AXIS_HEADROOM).max(1.0));
                }
                // Every column, not only the one running: the other two are read against the
                // baseline, so a reading it earns changes what they say about themselves — and
                // all three are still travelling toward the reading they were last given.
                for (shown, fresh) in shown.iter_mut().zip(&heading) {
                    if let Some(fresh) = fresh {
                        follow(shown, fresh.clone(), dt);
                    }
                }
                for (signal, column) in legs.iter().zip(columns_of(&shown)) {
                    if let Some(column) = column {
                        signal.set(&turn, column);
                    }
                }
                if is_pool {
                    if let Some(column) = columns_of(&shown).swap_remove(chosen) {
                        phone_legs.set(&turn, column);
                    }
                }
                // The axis follows the same ease the bars do: a long trip leaving the sample
                // changes what every bar is drawn against, and taken whole that is three
                // columns jumping at once.
                if let Some(want) = wants_axis {
                    let now = shown_axis.map_or(want, |was| toward(was, want, dt));
                    shown_axis = Some(now);
                    axis.set(&turn, now);
                }
            }
        }
        if relaid {
            lattice.sync(&turn, lattice_of(&stage_rect, &dot_rects, &box_rects, &warm));
        }
        traffic.sync(&turn, traffic_of(&stage_rect, &dot_rects, &box_rects, &tier));
    }
}

/// The layer under the traffic: every warm connection as a bow from a client's circle to a
/// machine. Empty until the stage has been measured, and each wire waits on both of its own
/// endpoints — so this is a function of the layout and the pool, and of nothing that ticks.
fn lattice_of(
    stage: &Option<Rect>,
    dots: &[Option<Rect>],
    boxes: &[Option<Rect>],
    warm: &[Vec<usize>],
) -> Vec<Shape> {
    let mut layer = Vec::new();
    wires_down(
        stage,
        dots,
        boxes,
        warm.iter().enumerate().flat_map(|(c, pool)| pool.iter().map(move |&s| (c, s))),
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

/// A round trip as a column reads it: what it cost and where the time went. A column keeps
/// what its policy earned long after the fleet's ledger has forgotten it — the ledger holds the
/// last [`SAMPLE`](crate::multi::SAMPLE) requests of the whole crowd — so what it keeps is the
/// reading rather than the record.
///
/// The mean of a [`band`] is one of these too, and so is a bar on its way to one: averaging and
/// easing are done over the same partition, which is what keeps the phases summing to the trip.
#[derive(Clone)]
struct Felt {
    total_ms: f64,
    phases: Vec<(&'static Phase, f64)>,
}

impl Felt {
    fn of(record: &Record) -> Felt {
        Felt { total_ms: record.total_ms, phases: phase_ms(record) }
    }

    /// What this trip spent waiting — the row a policy decides, and the one the columns are
    /// compared on.
    fn queue_wait_ms(&self) -> f64 {
        self.phases.iter().filter(|(phase, _)| phase.is_queue_wait()).map(|(_, ms)| ms).sum()
    }
}

/// A column's reading with its own noise attached: the band's mean, and — per phase — how far
/// that mean might sit from where an endless run would put it.
#[derive(Clone)]
struct Band {
    mean: Felt,
    give: Vec<f64>,
}

/// How many runs a window is split into to read its own noise. Queue trouble comes in bursts,
/// so neighbouring trips agree with each other and a spread taken trip-by-trip would flatter
/// the reading; the spread that means anything is between stretches of time. Eight runs of a
/// full column is a run of nearly a second — several bursts wide — and the give on a reading is
/// the spread of the same cut across the runs, shrunk by √runs.
const GIVE_RUNS: usize = 8;

/// The reading at `pc` of what a column earned: the [`cut`] of the whole window, and its give —
/// the same cut taken over each run of the window, spread into a standard error.
fn band<'a>(readings: impl IntoIterator<Item = &'a Felt>, pc: f64) -> Option<Band> {
    let window: Vec<&Felt> = readings.into_iter().collect();
    let mean = cut(window.clone(), pc)?;
    let run = window.len().div_ceil(GIVE_RUNS).max(1);
    let cuts: Vec<Felt> = window.chunks(run).filter_map(|run| cut(run.to_vec(), pc)).collect();
    let give = give_of(&cuts, mean.phases.len());
    Some(Band { mean, give })
}

/// The standard error of each phase's reading, from the spread of the same cut across the
/// window's runs — batch means. Fewer than two runs is a spread of nothing, which reads as no
/// give rather than as none needed.
fn give_of(cuts: &[Felt], phases: usize) -> Vec<f64> {
    if cuts.len() < 2 {
        return vec![0.0; phases];
    }
    let k = cuts.len() as f64;
    (0..phases)
        .map(|i| {
            let mean = cuts.iter().map(|c| c.phases[i].1).sum::<f64>() / k;
            let var = cuts.iter().map(|c| (c.phases[i].1 - mean).powi(2)).sum::<f64>() / (k - 1.0);
            (var / k).sqrt()
        })
        .collect()
}

/// The mean trip in the band a dial names: every reading from `pc - BAND_PC` to `pc + BAND_PC`
/// of what a column earned, averaged phase by phase. One request at the rank would be a reading
/// of one client's luck, and which client sits at the rank changes as the sample rolls.
///
/// The edges are clamped to the ends of the scale rather than slid back inside them, so a dial
/// at p99 reads p94 to p100 — the worst trips there are, rather than a band centred where the
/// reader did not put it.
///
/// One set of trips for all four phases: averaged apart they would each be a mean over a
/// different set of requests, and four such means do not add up to a trip anyone took.
fn cut(mut sorted: Vec<&Felt>, pc: f64) -> Option<Felt> {
    sorted.sort_by(|a, b| a.total_ms.total_cmp(&b.total_ms));
    let last = sorted.len().checked_sub(1)?;
    let rank = |pc: f64| (pc.clamp(0.0, 100.0) / 100.0 * last as f64).round() as usize;
    let (first, rest) = sorted[rank(pc - BAND_PC)..=rank(pc + BAND_PC)].split_first()?;
    let mut mean = Felt { total_ms: first.total_ms, phases: first.phases.clone() };
    for felt in rest {
        mean.total_ms += felt.total_ms;
        for ((_, sum), &(_, ms)) in mean.phases.iter_mut().zip(&felt.phases) {
            *sum += ms;
        }
    }
    let trips = (1 + rest.len()) as f64;
    mean.total_ms /= trips;
    for (_, ms) in &mut mean.phases {
        *ms /= trips;
    }
    Some(mean)
}

/// Where every column's band stands at `pc`. A column that has earned nothing has no reading.
fn bands_at(earned: &[VecDeque<Felt>], pc: f64) -> Vec<Option<Band>> {
    earned.iter().map(|mine| band(mine, pc)).collect()
}

/// Fold the newest `landed` records into the columns, each credited to the policy that sent
/// it. By the stamp, not by the pill: a trip in the air when the crowd changed its mind lands
/// in the column that chose its machine, so a switch drops nothing and credits nothing wrongly.
/// True when a column got its first reading — the caller's cue that a note has become bars.
fn credit(
    earned: &mut [VecDeque<Felt>],
    records: &VecDeque<Record>,
    landed: usize,
    cast: &[(&str, Policy); 3],
) -> bool {
    let mut filled = false;
    for r in records.iter().rev().take(landed).rev() {
        let Some(column) = cast.iter().position(|&(_, p)| Some(p) == r.policy) else {
            continue;
        };
        filled |= earned[column].is_empty();
        remember(&mut earned[column], Some(COLUMN_SAMPLE), Felt::of(r));
    }
    filled
}

/// One reading's step toward the one it is heading for, over `dt_ms` of virtual time. Framed on
/// the elapsed time rather than on the frame, so a slow frame carries a slow frame's distance.
fn toward(shown: f64, fresh: f64, dt_ms: f64) -> f64 {
    shown + (fresh - shown) * (1.0 - (-dt_ms / EASE_MS).exp())
}

/// Move a column's bars toward what its policy has just earned. A column with nothing shown
/// takes the reading whole — a first bar growing out of zero would draw a fleet that was never
/// that fast — and one that has a reading follows from where it stands, which is what makes a
/// policy switch a move rather than a jump.
fn follow(shown: &mut Option<Band>, fresh: Band, dt_ms: f64) {
    match shown {
        Some(shown) => {
            shown.mean.total_ms = toward(shown.mean.total_ms, fresh.mean.total_ms, dt_ms);
            for ((_, ms), &(_, now)) in shown.mean.phases.iter_mut().zip(&fresh.mean.phases) {
                *ms = toward(*ms, now, dt_ms);
            }
            for (give, &now) in shown.give.iter_mut().zip(&fresh.give) {
                *give = toward(*give, now, dt_ms);
            }
        }
        None => *shown = Some(fresh),
    }
}

/// What the columns draw: each reading cut into its phases — and on the waiting, where the
/// column stands against the one they are all read against. Read off what the columns are
/// *showing* rather than off the fresh band, so the percentage under a bar is a percentage of
/// the bars either side of it. Where the *baseline* shows nothing, nothing carries a difference,
/// because a difference from no reading is not zero.
fn columns_of(shown: &[Option<Band>]) -> Vec<Option<Vec<Leg>>> {
    let baseline = shown[BASELINE].as_ref().map(|b| b.mean.queue_wait_ms());
    shown
        .iter()
        .enumerate()
        .map(|(column, band)| {
            let band = band.as_ref()?;
            Some(
                band.mean
                    .phases
                    .iter()
                    .zip(&band.give)
                    .map(|(&(phase, ms), &give)| Leg {
                        label: phase.label.to_string(),
                        ms: Some(ms),
                        give: (give > 0.0).then_some(give),
                        against: phase
                            .is_queue_wait()
                            .then(|| against(column, ms, baseline))
                            .flatten(),
                        ink: phase.ink,
                    })
                    .collect(),
            )
        })
        .collect()
}

/// How a column's waiting stands against the column they are all read against: the baseline says
/// that it is one, and the others say how far from it they are.
fn against(column: usize, ms: f64, baseline: Option<f64>) -> Option<Against> {
    let baseline = baseline?;
    match column {
        BASELINE => Some(Against::Baseline),
        _ if baseline > 0.0 => Some(Against::By((ms - baseline) / baseline * 100.0)),
        _ => None,
    }
}

/// The imbalance, as the two numbers it is made of. A ratio would hide the case the chapter is
/// about — a machine with nothing on it, where the ratio is not a number at all.
fn spread_text_of(loads: &[usize]) -> String {
    match (loads.iter().max(), loads.iter().min()) {
        (Some(&most), Some(&least)) => format!("{most} over {least}"),
        _ => "—".to_string(),
    }
}

fn fmt_qps(v: f64) -> String {
    format!("{v:.0}/s")
}

fn fmt_pool(v: f64) -> String {
    format!("{v:.0} of {SERVERS}")
}

/// What the phone's column says with nothing to show. It mirrors whichever policy the pill has
/// chosen, so the only thing it can ever be waiting for is the fleet itself.
fn phone_note_for(has_reading: bool, running: bool) -> String {
    match has_reading || running {
        true => String::new(),
        false => "Press \"Run\"".to_string(),
    }
}

#[styles]
pub mod styles {
    use idyll_styles::Style;

    pub const FLEET: Style = css! {{
        display: "grid",
        grid_template_columns: "repeat(5, 1fr)",
        gap: "8px",
        margin: "12px 0 0",
    }};

    pub const LABEL: Style = css! {{
        font_size: "10px",
        letter_spacing: ".08em",
        text_transform: "uppercase",
        color: "#aab09c",
        font_weight: 600,
        margin: "16px 2px 8px",
    }};

    pub const DIAL: Style = css! {{
        max_width: "260px",
        margin: "0 0 12px",
    }};

    /// A column's heading — the policy the reading under it belongs to.
    pub const COLH: Style = css! {{
        font_size: "10px",
        letter_spacing: ".08em",
        text_transform: "uppercase",
        font_weight: 600,
        color: "#aab09c",
        margin: "0 0 6px",
    }};

    /// What a column says instead of bars it has not earned.
    pub const NULL: Style = css! {{
        display: "flex",
        align_items: "center",
        justify_content: "center",
        min_height: "96px",
        font_size: "13px",
        color: "#aab09c",
        text_align: "center",
    }};

    /// The three-column comparison, on screens wide enough to compare three of anything — the
    /// same row [`cut::styles::CHARTS`](crate::atoms::cut::styles::CHARTS) lays out, except that
    /// on a phone it is gone rather than stacked: the chosen policy's column stands in for it,
    /// because a tower of three charts is a scroll, not a comparison. One const owns `display`,
    /// since two atoms contesting one property leaves the winner to stylesheet order.
    pub const WIDE: Style = css! {{
        display: "flex",
        gap: "28px",
        margin_top: "16px",
        mobile: { display: "none" },
    }};

    /// The phone's single column — the chosen policy mirrored, where three abreast would be
    /// bars too narrow to read.
    pub const PHONE: Style = css! {{
        display: "none",
        mobile: { display: "block" },
    }};

    /// The pool slider's row: the comparison's own grid again, so the slider sits under the
    /// pick-2 column it belongs to — and the whole width once that column is the only one.
    pub const POOLROW: Style = css! {{
        display: "grid",
        grid_template_columns: "1fr 1fr 1fr",
        gap: "16px",
        margin: "8px 0 0",
        mobile: { grid_template_columns: "1fr" },
    }};
}

/// What each column says when it has no reading to show.
///
/// A column is empty until its policy has run, and what it needs to hear depends on why: a
/// policy nobody has chosen wants choosing, and the chosen one wants the fleet started. Only
/// the leftmost empty column says so — three columns repeating the same instruction is the
/// instruction three times, not three instructions.
fn notes_for(
    earned: &[VecDeque<Felt>],
    chosen: usize,
    running: bool,
    cast: &[(&str, Policy); 3],
) -> Vec<String> {
    let mut said = false;
    cast.iter()
        .enumerate()
        .map(|(i, (label, _))| {
            if earned.get(i).is_some_and(|mine| !mine.is_empty()) || said {
                return String::new();
            }
            said = true;
            match i == chosen {
                true if running => String::new(),
                true => "Press \"Run\"".to_string(),
                false => format!("Select \"{label}\""),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One frame of virtual time, as the card is driven.
    const FRAME_MS: f64 = 16.0;

    /// A trip of `ms`, spread over the phases so that no two of them hold the same number — a
    /// mean that mixed one phase into another would read as a different trip rather than as the
    /// same one.
    fn trip(ms: f64) -> Felt {
        let phases: Vec<(&'static Phase, f64)> =
            PHASES.iter().enumerate().map(|(i, phase)| (phase, ms * (i + 1) as f64)).collect();
        Felt { total_ms: phases.iter().map(|(_, ms)| ms).sum(), phases }
    }

    /// A hundred and one trips, one at each percentile of a scale that runs 0 to 100 — so the
    /// rank a dial names is the number it names.
    fn ranked() -> Vec<Felt> {
        (0..=100).map(|ms| trip(ms as f64)).collect()
    }

    #[test]
    fn a_reading_is_the_mean_of_the_band_around_the_dial() {
        let reading = band(&ranked(), 50.0).expect("a hundred trips is a reading");
        assert_eq!(
            reading.mean.total_ms.round(),
            trip(50.0).total_ms.round(),
            "p45 to p55 of a straight ranking averages to the trip at p50"
        );
    }

    #[test]
    fn a_band_at_the_end_of_the_scale_is_clamped_and_not_slid() {
        let top = band(&ranked(), 99.0).expect("a hundred trips is a reading");
        assert_eq!(top.mean.total_ms.round(), trip(97.0).total_ms.round(), "p99 reads p94 to p100");
        let bottom = band(&ranked(), 1.0).expect("a hundred trips is a reading");
        assert_eq!(bottom.mean.total_ms.round(), trip(3.0).total_ms.round(), "and p1 reads p0 to p6");
    }

    #[test]
    fn the_phases_of_a_band_still_add_up_to_its_trip() {
        let reading = band(&ranked(), 90.0).expect("a hundred trips is a reading");
        let phases: f64 = reading.mean.phases.iter().map(|(_, ms)| ms).sum();
        assert!(
            (phases - reading.mean.total_ms).abs() < 1e-9,
            "one set of trips, so the parts are still the whole: {phases} against {}",
            reading.mean.total_ms
        );
    }

    /// Runs of a steady window agree with each other, so its reading carries no give — a
    /// whisker on a number that is not in doubt would be noise about noise.
    #[test]
    fn a_steady_window_has_no_give() {
        let steady: Vec<Felt> = (0..800).map(|_| trip(50.0)).collect();
        let reading = band(&steady, 90.0).expect("a full window is a reading");
        assert!(reading.give.iter().all(|&g| g == 0.0), "no spread between runs: {:?}", reading.give);
    }

    /// A window still drifting disagrees with itself run to run, and the give says so — the
    /// honest sign that the number on the row is not yet a number.
    #[test]
    fn a_drifting_window_carries_give() {
        let drifting: Vec<Felt> = (0..800).map(|i| trip(i as f64)).collect();
        let reading = band(&drifting, 50.0).expect("a full window is a reading");
        assert!(reading.give.iter().any(|&g| g > 0.0), "runs of a ramp disagree");
    }

    /// A window too short to split into two runs has no spread to read: the reading stands,
    /// with no give rather than a false certainty of zero drawn as a dot.
    #[test]
    fn a_window_of_one_run_carries_no_give() {
        let brief: Vec<Felt> = vec![trip(10.0)];
        let reading = band(&brief, 50.0).expect("one trip is still a reading");
        assert!(reading.give.iter().all(|&g| g == 0.0));
    }

    /// A trip is credited to the column of the policy that *sent* it. Switching leaves trips
    /// in the air, and each lands in the column that chose its machine — never the one the
    /// pill happens to be showing when it comes home.
    #[test]
    fn a_trip_lands_in_the_column_of_the_policy_that_sent_it() {
        let mut earned: Vec<VecDeque<Felt>> = POLICIES.iter().map(|_| VecDeque::new()).collect();
        let records: VecDeque<Record> =
            [record(Policy::AlwaysRandom), record(Policy::PowerOfTwoVirtual)].into();
        assert!(credit(&mut earned, &records, 2, &POLICIES), "first readings turn notes into bars");
        assert_eq!(
            earned.iter().map(VecDeque::len).collect::<Vec<_>>(),
            vec![1, 0, 1],
            "each lands in its own column",
        );
        assert!(!credit(&mut earned, &records, 0, &POLICIES), "a frame with no landings fills nothing");
    }

    /// The two cards share labels and differ in one policy: the pool card's pick-2 is the paid
    /// one, so a record it stamps lands in the pool card's third column and not the policy
    /// card's.
    #[test]
    fn the_pool_cast_swaps_only_the_paid_pick_two() {
        let pool = cast(Shows::Pool);
        assert_eq!(pool[PICK2].1, Policy::PowerOfTwo);
        assert_eq!(cast(Shows::Policies)[PICK2].1, Policy::PowerOfTwoVirtual);
        assert_eq!(pool[BASELINE].1, Policy::AlwaysRandom);

        let mut earned: Vec<VecDeque<Felt>> = POLICIES.iter().map(|_| VecDeque::new()).collect();
        let records: VecDeque<Record> = [record(Policy::PowerOfTwo)].into();
        assert!(credit(&mut earned, &records, 1, &pool), "the paid pick-2 fills its column");
        assert_eq!(earned[PICK2].len(), 1);
    }

    /// A column's window is a window: full to [`COLUMN_SAMPLE`], it forgets its oldest trip
    /// for each new one, so a column left running forever costs what a full one does.
    #[test]
    fn a_full_column_forgets_its_oldest_trip() {
        let mut earned: Vec<VecDeque<Felt>> = POLICIES.iter().map(|_| VecDeque::new()).collect();
        let records: VecDeque<Record> = (0..COLUMN_SAMPLE + 3)
            .map(|_| record(Policy::AlwaysRandom))
            .collect();
        credit(&mut earned, &records, records.len(), &POLICIES);
        assert_eq!(earned[0].len(), COLUMN_SAMPLE);
    }

    fn record(policy: Policy) -> Record {
        Record {
            tenant: crate::engine::SOLO,
            client: 0,
            policy: Some(policy),
            outcome: crate::engine::Outcome::Success,
            total_ms: 10.0,
            sections: Default::default(),
            refusals: 0,
            at: 0.0,
        }
    }

    /// A trip as a reading, certain of itself — what the ease tests move around.
    fn steady(felt: Felt) -> Band {
        Band { give: vec![0.0; felt.phases.len()], mean: felt }
    }

    #[test]
    fn a_column_with_nothing_shown_takes_its_reading_whole() {
        let mut shown = None;
        follow(&mut shown, steady(trip(40.0)), FRAME_MS);
        assert_eq!(
            shown.map(|b| b.mean.total_ms),
            Some(trip(40.0).total_ms),
            "a first bar has nowhere to ease from"
        );
    }

    #[test]
    fn a_second_of_frames_settles_a_bar_on_its_reading() {
        let mut shown = Some(steady(trip(0.0)));
        let fresh = trip(100.0);
        for _ in 0..(1000.0 / FRAME_MS) as usize {
            follow(&mut shown, steady(trip(100.0)), FRAME_MS);
        }
        let settled = shown.expect("a bar that was shown is shown still").mean.total_ms;
        let left = 1.0 - settled / fresh.total_ms;
        assert!(left < 0.02, "a second of frames leaves {:.1}% of the gap", left * 100.0);
    }
}
