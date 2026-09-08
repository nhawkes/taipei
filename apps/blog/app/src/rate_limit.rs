//! **Rate limiting** — three tenants, two servers, and one budget between them.
//!
//! The blame panel showed a server working out what each tenant cost it. This is what a
//! fleet does with that number. Both servers run the real
//! [`queue_rate_limited`](crate::compose::queue_rate_limited) stack: blame goes out through
//! the tenant reporter into a [`Store`] they share, a separate pass divides the budget by
//! weighted fair share once a virtual second, and the servers refuse traffic by what comes
//! back.
//!
//! What the picture is for is the **delay**. Write, calculate, read — each reads the epoch
//! before it, so a tenant that starts flooding is not refused by its new share for three
//! seconds. The table is laid out as those stages left to right, so the reader watches a
//! number walk across it one column per second rather than being told that it does.

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use idyll::{live_view, Ctx, Rect, Setup, Signal};
use idyll_styles::styles;

use crate::atoms::button::{Button, ButtonKind};
use crate::atoms::controls::styles as cstyles;
use crate::atoms::invite::Invite;
use crate::atoms::latency_table::{Bar, LatencyTable};
use crate::atoms::server_box::{server_name, TenantLine};
use crate::atoms::sim_card::SimHeader;
use crate::atoms::slider::{raw_from_speed, speed_from_raw, Name, Scale, Slider};
use crate::atoms::stage::Paint;
use crate::atoms::switch::Switch;
use crate::atoms::tenant_stage::{Measured, ServerRow, TenantDot, TenantStage, Wire};
use crate::atoms::toggle::{knob, Tone, ToggleGroup, ToggleItem};
use crate::engine::{
    core_ink_of, cpu_cost_ms, on_the_wire, queues_by_tenant, work_for_cpu_cost, ServerCounts,
    SimEngine, TenantQueues, HOMEWARD, SOLO, TENANTS,
};
use crate::limits::{PerTenant, Snapshot, Store, DEFAULT_BUDGET, EPOCH_MS};
use crate::multi::{
    axis_for, Latencies, Summary, HALF_LIFE_MS, PROCESSING_TIME, QUEUE_TIME, SUMMARY_MS,
};
use blog_core::SimKey;

/// The fleet the chapter draws. Two is the smallest number that makes the store worth
/// having: one server could divide its own budget alone and never need to agree with anyone.
const SERVERS: usize = 2;

/// The engine steps at 30 Hz, and a slow frame may buy at most this much catch-up — the
/// pacing every sim in the blog shares.
const ENGINE_STEP_MS: f64 = 1000.0 / 30.0;
const MAX_CATCH_UP: f64 = 3.0;

/// The speed the sim opens at.
///
/// Real time, where the single-server sims crawl at 0.01× to follow one request through a
/// machine. This one is about a control loop whose whole subject is a **three second** delay:
/// at a hundredth of speed an epoch would take a minute and a half, and the lesson would be
/// invisible to anyone not prepared to sit through five minutes of it.
const SPEED: f64 = 1.0;

/// The base seed the two servers fan out from — `base ^ index`, so they see different draws.
/// A real fleet is alike box to box, never identical.
const FLEET_SEED: u64 = 0x11_11_5eed;

/// What one tenant's three knobs are set to.
///
/// A record rather than three lists read at the same index: a rate and a cost that could
/// drift apart from the tenant they belong to is a bug waiting for a fourth tenant.
#[derive(Clone, Copy, PartialEq)]
struct Knobs {
    /// Requests per second this tenant asks the **fleet** for.
    qps: f64,
    /// The CPU one of its requests costs, in milliseconds.
    cost_ms: f64,
    /// Its priority — a weight, meaningful only against the other two.
    priority: f64,
}

/// A named set of positions for every knob: one click, one story.
struct Preset {
    label: &'static str,
    tenants: PerTenant<Knobs>,
}

/// The cost a request carries when nothing is being said about cost — the weight the blame
/// panel opens on, in the milliseconds this sim's knob reads.
fn even_cost() -> f64 {
    (cpu_cost_ms(crate::engine::TENANTS[0].work) / 10.0).round() * 10.0
}

/// The three stories the row tells. Each asks the fleet for the same **total** CPU, so what
/// separates them is only how it is distributed — which is exactly what the scheme is for.
fn presets() -> [Preset; 3] {
    let cost = even_cost();
    let level = |qps: f64, cost_ms: f64| Knobs { qps, cost_ms, priority: 33.0 };
    [
        Preset {
            label: "all equal",
            tenants: [level(160.0, cost), level(160.0, cost), level(160.0, cost)],
        },
        Preset {
            label: "different QPS",
            tenants: [level(320.0, cost), level(80.0, cost), level(80.0, cost)],
        },
        Preset {
            label: "different weight",
            tenants: [
                level(160.0, cost * 2.0),
                level(160.0, cost / 2.0),
                level(160.0, cost / 2.0),
            ],
        },
    ]
}

/// How much of the queue-time chart is always shown, in milliseconds. A floor, not a ceiling:
/// below it the axis holds still so a draining queue is a bar visibly shrinking rather than a
/// rescaling picture, and above it the axis grows — the overload the scheme exists for is not
/// something to clip off the right-hand edge.
const QUEUE_FLOOR_MS: f64 = 100.0;

const QPS_SCALE: Scale = Scale::new(0, 400, 10);
const COST_SCALE: Scale = Scale::new(0, 60, 1);
const PRIORITY_SCALE: Scale = Scale::new(0, 100, 1);
const BUDGET_SCALE: Scale = Scale::new(0, 200, 5);

fn fmt_qps(v: f64) -> String {
    format!("{v:.0}/s")
}

fn fmt_ms(v: f64) -> String {
    format!("{v:.0} ms")
}

fn fmt_weight(v: f64) -> String {
    format!("{v:.0}")
}

/// A duration as the tables read it — milliseconds to one place, because a budget of 50 ms
/// split three ways is a number with a decimal in it.
/// A reading in milliseconds, bare. The unit is the column's, named once in its heading —
/// a unit repeated down every row is the widest thing in a column that has no room for it.
fn ms(d: Duration) -> String {
    format!("{:.1}", d.as_secs_f64() * 1000.0)
}

fn pct(v: f64) -> String {
    format!("{:.0}%", v * 100.0)
}

fn fmt_speed(raw: f64) -> String {
    let s = speed_from_raw(raw);
    match s < 0.1 {
        true => format!("{s:.2}×"),
        false => format!("{s:.1}×"),
    }
}

#[derive(Debug)]
pub enum RateLimitMsg {
    Tick(f64),
    Toggle,
    Speed(f64),
    Reset,
    /// One tenant's arrival rate, in requests/second asked of the fleet.
    Qps(usize, f64),
    /// One tenant's request cost, in milliseconds of CPU.
    Cost(usize, f64),
    /// One tenant's priority — its weight in the budget split.
    Priority(usize, f64),
    /// The fleet's budget, in milliseconds of shut-time per epoch.
    Budget(f64),
    /// Turn the whole scheme on or off.
    Limiting,
    /// Put every knob where a named story wants it.
    Preset(usize),
    Measured(Measured),
}

/// One tenant's line in the table — the same reading at each stage of the loop, so the three
/// seconds are three columns rather than a claim.
#[derive(Clone, PartialEq)]
struct Line {
    tenant: String,
    tint: Paint,
    /// Its share of the budget, as the allocator normalises the priorities.
    priority: String,
    /// What the servers are billing it *right now* — the only number that moves between
    /// epochs, and what the write step will snapshot.
    accruing: String,
    /// What the last epoch wrote — the blame the servers actually measured.
    blame: String,
    /// What that implies it would have cost unrefused.
    demand: String,
    /// The refusal the budget split works out to.
    calculated: String,
    /// What the servers are refusing by right now — the calculation from an epoch ago.
    enforced: String,
}

/// A tenant's colour dot in its table row — the same tint it wears on its dot and its wires.
fn swatch(line: &Line) -> String {
    format!("background:{}", line.tint)
}

fn place(slots: &mut [Option<Rect>], i: usize, rect: Rect) {
    if let Some(slot) = slots.get_mut(i) {
        *slot = Some(rect);
    }
}

/// One server box's queue block: every tenant, where its requests are waiting, what it has
/// cost this machine, and what the fleet is refusing of it.
///
/// Every tenant is always a line, including one with nothing on the server. A block that grew
/// its rows as traffic arrived would shift the lines under them as the reader watched, and the
/// tenant with nothing getting through is the one being looked for.
fn tenant_lines(
    queues: &[TenantQueues],
    billed: &PerTenant<Duration>,
    enforced: &PerTenant<f64>,
) -> Vec<TenantLine> {
    TENANTS
        .iter()
        .enumerate()
        .map(|(k, t)| TenantLine {
            name: t.id.to_string(),
            tint: t.tint,
            queues: queues.get(k).copied().unwrap_or_default(),
            blame_ms: billed[k].as_secs_f64() * 1000.0,
            drop_pct: enforced[k],
        })
        .collect()
}

/// The block before anything has run: every tenant present, nothing measured. The box wears
/// its tenant layout from first paint — one that showed queue depths until the reader pressed
/// run and then swapped to tenant lines would read as two different boxes.
fn resting_lines() -> Vec<TenantLine> {
    tenant_lines(&[], &[Duration::ZERO; TENANTS.len()], &[0.0; TENANTS.len()])
}

/// Two servers wired to one store, each taking its share of every tenant's traffic.
fn fleet(store: &Arc<Store>) -> Vec<SimEngine> {
    (0..SERVERS)
        .map(|s| {
            let mut engine =
                SimEngine::rate_limited(FLEET_SEED ^ s as u64, Arc::clone(store), 1.0 / SERVERS as f64);
            engine.set_speed(SPEED);
            engine
        })
        .collect()
}

/// Put the knobs into the machines: each server takes its share of the rate, the whole cost,
/// and the store takes the priorities.
fn apply(engines: &mut [SimEngine], store: &Store, knobs: &PerTenant<Knobs>) {
    for (k, tenant) in TENANTS.iter().enumerate() {
        for engine in engines.iter_mut() {
            engine.set_tenant_qps(tenant.id, knobs[k].qps / SERVERS as f64);
            engine.set_tenant_work(tenant.id, work_for_cpu_cost(knobs[k].cost_ms));
        }
    }
    store.set_weights(std::array::from_fn(|k| knobs[k].priority));
}

pub(crate) async fn run(
    ctx: Ctx<Setup, RateLimitMsg>,
    _seed: crate::PageSeed,
    _key: SimKey,
) -> idyll::Result {
    let store = Arc::new(Store::new());
    let mut engines = fleet(&store);
    apply(&mut engines, &store, &presets()[0].tenants);
    // One ledger per server. Request ids are an engine's own counter, so two engines sharing a
    // ledger would file each other's requests under the same id.
    let mut ledgers: Vec<Latencies> = (0..SERVERS).map(|_| Latencies::default()).collect();

    let running = ctx.mutable_signal(false);
    let run_lbl = {
        let running = running.read();
        ctx.computed(move |cx| match running.get(cx) {
            true => "pause".to_string(),
            false => "run".to_string(),
        })
        .read()
    };
    let reset_btn = ctx.constant("reset".to_string());
    let speed = ctx.mutable_signal(raw_from_speed(SPEED));
    ctx.frames(&running.read(), RateLimitMsg::Tick);

    // The stage's measurements, and the wires that are a pure function of them.
    let stage_rect = ctx.mutable_signal::<Option<Rect>>(None);
    let tenant_rects = ctx.mutable_signal::<Vec<Option<Rect>>>(vec![None; TENANTS.len()]);
    let server_rects = ctx.mutable_signal::<Vec<Option<Rect>>>(vec![None; SERVERS]);
    // One wire per tenant per server: on a fleet a tenant's traffic is several connections,
    // and each carries its own requests.
    let wires = ctx.mutable_signal(
        (0..SERVERS)
            .flat_map(|s| {
                (0..TENANTS.len()).map(move |k| Wire {
                    tenant: k,
                    server: s,
                    shed: false,
                    sent: Vec::new(),
                    home: vec![Vec::new(); HOMEWARD.len()],
                })
            })
            .collect::<Vec<_>>(),
    );

    let counts: Vec<_> = engines.iter().map(|e| ctx.mutable_signal(e.counts())).collect();
    let core_ink: Vec<_> = (0..SERVERS).map(|_| ctx.mutable_signal(Vec::<Paint>::new())).collect();
    // Each box's queue block, one line per tenant. The blame is that server's own; the share
    // being refused is the fleet's, so it reads the same in both — one machine's evidence,
    // everyone's decision.
    let box_tenants: Vec<_> =
        (0..SERVERS).map(|_| ctx.mutable_signal(resting_lines())).collect();
    let server_rows: Vec<ServerRow> = (0..SERVERS)
        .map(|s| {
            let live = counts[s].read();
            let idle = ctx
                .computed(move |cx| {
                    let c: ServerCounts = live.get(cx);
                    c.inflight == 0 && c.busy == 0
                })
                .read();
            ServerRow {
                name: server_name(s),
                counts: counts[s].read(),
                idle,
                ink: core_ink[s].read(),
                tenants: box_tenants[s].read(),
            }
        })
        .collect();
    let tenant_dots: Vec<TenantDot> =
        TENANTS.iter().map(|t| TenantDot { name: t.id.to_string(), tint: t.tint }).collect();

    // The two cuts of the round trip, per tenant, on an axis each. Refusing traffic drains the
    // queue and leaves the work that got through as it was, so the queue is where the scheme
    // shows — and against a processing time an order of magnitude larger it is a sliver.
    let queue_bars = ctx.mutable_signal(Vec::<Bar>::new());
    let work_bars = ctx.mutable_signal(Vec::<Bar>::new());
    let queue_axis = ctx.mutable_signal(QUEUE_FLOOR_MS);
    let work_axis = ctx.mutable_signal(1.0f64);

    // The table, and the clock it moves on.
    let lines = ctx.mutable_signal(table(&Snapshot::default(), &presets()[0].tenants));
    let epoch_no = ctx.mutable_signal(0u64);
    let epoch_text = {
        let epoch_no = epoch_no.read();
        ctx.computed(move |cx| format!("{}s", epoch_no.get(cx))).read()
    };
    let budget = ctx.mutable_signal(DEFAULT_BUDGET.as_secs_f64() * 1000.0);

    // Whether the fleet is enforcing at all. The sim opens with it off, so the reader first sees
    // three tenants overrunning a fleet that does nothing about it, and turns the scheme on
    // themselves.
    let limiting = ctx.mutable_signal(false);
    let limiting_at = limiting.read();
    // The invitation stands exactly while the thing it invites has not been done.
    let uninvited = {
        let limiting = limiting.read();
        ctx.computed(move |cx| !limiting.get(cx)).read()
    };

    // The knobs are the state. Everything else about them — the rails, the readouts, which
    // preset is lit — is read off these, so there is one place a value lives.
    let opening = presets()[0].tenants;
    let qps: Vec<_> = (0..TENANTS.len()).map(|k| ctx.mutable_signal(opening[k].qps)).collect();
    let cost: Vec<_> = (0..TENANTS.len()).map(|k| ctx.mutable_signal(opening[k].cost_ms)).collect();
    let priority: Vec<_> =
        (0..TENANTS.len()).map(|k| ctx.mutable_signal(opening[k].priority)).collect();

    // One tenant's three knobs, read as the record they are. Everything downstream — which
    // preset is lit, what the machines are set to — asks this rather than the three rails, so
    // a knob has exactly one reading however many things want it.
    let dials: Vec<Signal<Knobs>> = (0..TENANTS.len())
        .map(|k| {
            let (q, c, p) = (qps[k].read(), cost[k].read(), priority[k].read());
            ctx.computed(move |cx| Knobs {
                qps: q.get(cx),
                cost_ms: c.get(cx),
                priority: p.get(cx),
            })
            .read()
        })
        .collect();

    // A preset is lit exactly while the knobs are where it puts them. Derived, not
    // remembered: moving a knob away is not an event anything handles, it is a set of
    // positions that has stopped matching — and moving it back matches again for free.
    let preset_items: Vec<ToggleItem> = presets()
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let (want, dials) = (p.tenants, dials.clone());
            let on = ctx
                .computed(move |cx| dials.iter().zip(want).all(|(at, want)| at.get(cx) == want))
                .read();
            (i, Rc::from(p.label), on, Tone::Normal)
        })
        .collect();
    let preset_knob = knob(&ctx, &preset_items);

    // The knob rows, each carrying the tenant it belongs to.
    struct Rail {
        k: usize,
        name: Name,
        tint: Paint,
        qps: Signal<f64>,
        cost: Signal<f64>,
        priority: Signal<f64>,
    }
    let rails: Vec<Rail> = TENANTS
        .iter()
        .enumerate()
        .map(|(k, t)| Rail {
            k,
            name: Name::new(t.id, 56),
            tint: t.tint,
            qps: qps[k].read(),
            cost: cost[k].read(),
            priority: priority[k].read(),
        })
        .collect();
    let client_rails: Vec<_> = rails
        .iter()
        .map(|r| (r.k, r.name, r.tint, r.qps.clone(), r.cost.clone()))
        .collect();
    let budget_rails: Vec<_> =
        rails.iter().map(|r| (r.k, r.name, r.tint, r.priority.clone())).collect();

    let (wires_read, stage_read, dots_read, boxes_read, lines_read, budget_read, speed_read) = (
        wires.read(),
        stage_rect.read(),
        tenant_rects.read(),
        server_rects.read(),
        lines.read(),
        budget.read(),
        speed.read(),
    );

    let mut ctx = ctx.render(live_view! {
        div css=[cstyles::QV] {
            div css=[crate::atoms::sim_card::styles::CARD] {
                SimHeader title=("Rate limiting") aggs=(Vec::new())
                div css=[styles::BOARD] {
                    TenantStage tenants=(tenant_dots) servers=(server_rows) wires=(wires_read)
                        stage=(stage_read) tenant_rects=(dots_read) server_rects=(boxes_read)
                        measured=>(RateLimitMsg::Measured)
                    div css=[styles::LEDGER] {
                        div css=[styles::LEDGER_HEAD] {
                            span css=[styles::LEDGER_TITLE] { "the store" }
                            span css=[styles::EPOCH] { ($epoch_text) }
                        }
                        div css=[styles::TABLE] {
                            div css=[styles::HEAD_ROW] {
                                span css=[styles::WHO] { "tenant" }
                                span css=[styles::NUM, styles::LOW] { "share" }
                                span css=[styles::NUM] { "billing ms" }
                                span css=[styles::NUM, styles::LOW] { "blame ms" }
                                span css=[styles::NUM] { "demand ms" }
                                span css=[styles::NUM, styles::LOW] { "drop" }
                                span css=[styles::NUM] { "enforcing" }
                            }
                            @for l in $lines_read [key = l.tenant.clone()] {
                                div css=[styles::LINE] {
                                    span css=[styles::WHO] {
                                        span css=[styles::SWATCH] style=(swatch(&$l)) {}
                                        ($l.tenant)
                                    }
                                    span css=[styles::NUM] { ($l.priority) }
                                    span css=[styles::NUM] { ($l.accruing) }
                                    span css=[styles::NUM] { ($l.blame) }
                                    span css=[styles::NUM] { ($l.demand) }
                                    span css=[styles::NUM] { ($l.calculated) }
                                    span css=[styles::NUM, styles::LIVE] { ($l.enforced) }
                                }
                            }
                        }
                    }
                }
                div css=[styles::CUTS] {
                    div css=[styles::CUT] {
                        span css=[styles::CUT_TITLE] { "queue time" }
                        LatencyTable bars=(queue_bars) axis=(queue_axis)
                    }
                    div css=[styles::CUT] {
                        span css=[styles::CUT_TITLE] { "processing time" }
                        LatencyTable bars=(work_bars) axis=(work_axis)
                    }
                }
                div css=[cstyles::CONTROLS] {
                    div css=[cstyles::ROW] {
                        span css=[cstyles::GRP] { "simulation" }
                        div css=[cstyles::ROW_BODY] {
                            Button kind=(ButtonKind::Cta) label=(run_lbl) pressed=>(|_| RateLimitMsg::Toggle)
                            Slider name=(Name::new("speed", 44)) scale=(Scale::new(0, 100, 1))
                                at=(speed_read) fmt=(fmt_speed) moved=>(RateLimitMsg::Speed)
                            Button kind=(ButtonKind::Solid) label=(reset_btn) pressed=>(|_| RateLimitMsg::Reset)
                        }
                    }
                    div css=[cstyles::ROW] {
                        span css=[cstyles::GRP] { "clients" }
                        div css=[styles::RAILS] {
                            @for (k, name, tint, at_qps, at_cost) in (client_rails) {
                                div css=[styles::RAIL] {
                                    Slider name=(name) scale=(QPS_SCALE) at=(at_qps) fmt=(fmt_qps)
                                        moved=>(move |v| RateLimitMsg::Qps(k, v)) ?tint=(tint)
                                    Slider name=(Name::new("cost", 40)) scale=(COST_SCALE) at=(at_cost)
                                        fmt=(fmt_ms) moved=>(move |v| RateLimitMsg::Cost(k, v)) ?tint=(tint)
                                }
                            }
                        }
                    }
                    div css=[cstyles::ROW] {
                        span css=[cstyles::GRP] { "rate limiting" }
                        div css=[styles::RAILS] {
                            div css=[styles::RAIL] {
                                Invite when=(uninvited) hint=("Turning on rate-limit cuts the queue time by dropping requests according to their budget") {
                                    Switch off=("off") on=("enforcing") at=(limiting_at)
                                        flipped=>(|_| RateLimitMsg::Limiting)
                                }
                                Slider name=(Name::new("budget", 56)) scale=(BUDGET_SCALE)
                                    at=(budget_read) fmt=(fmt_ms) moved=>(RateLimitMsg::Budget)
                            }
                            @for (k, name, tint, at_priority) in (budget_rails) {
                                div css=[styles::RAIL] {
                                    Slider name=(name) scale=(PRIORITY_SCALE) at=(at_priority)
                                        fmt=(fmt_weight) moved=>(move |v| RateLimitMsg::Priority(k, v))
                                        ?tint=(tint)
                                }
                            }
                        }
                    }
                    div css=[cstyles::ROW] {
                        span css=[cstyles::GRP] { "presets" }
                        div css=[cstyles::ROW_BODY] {
                            ToggleGroup items=(preset_items) knob=(preset_knob)
                                picked=>(RateLimitMsg::Preset)
                        }
                    }
                }
            }
        }
    }).await?;

    let mut accum = 0.0f64;
    let mut next_epoch = EPOCH_MS;
    let mut summary_at = f64::NEG_INFINITY;
    // Blame per server per tenant, this epoch — the box's own reading, cleared when the epoch
    // turns so a line says what its machine is drawing now rather than since the page opened.
    let mut billed = vec![[Duration::ZERO; TENANTS.len()]; SERVERS];
    loop {
        let (msg, turn) = ctx.recv().await?;
        // Whether this message left a knob somewhere new, so the machines need retuning to it.
        // Which preset that lights is nobody's business here — it is read off the knobs.
        let mut moved = false;
        match msg {
            RateLimitMsg::Tick(dt) => {
                accum = (accum + dt).min(ENGINE_STEP_MS * MAX_CATCH_UP);
                while accum >= ENGINE_STEP_MS {
                    accum -= ENGINE_STEP_MS;
                    for (engine, ledger) in engines.iter_mut().zip(&mut ledgers) {
                        engine.tick(ENGINE_STEP_MS);
                        // Whose a request is is the engine's to answer, and answering borrows
                        // it — so the names are taken before the snapshot the ledger folds.
                        let named: HashMap<u32, &'static str> = engine
                            .obs()
                            .latencies
                            .iter()
                            .map(|&(id, _)| id)
                            .collect::<Vec<_>>()
                            .into_iter()
                            .map(|id| (id, engine.whose(id).unwrap_or(SOLO)))
                            .collect();
                        ledger.absorb(engine.obs(), &|id| named.get(&id).copied().unwrap_or(SOLO));
                    }
                }
                // The loop's beat, on the servers' own clock: they share a speed, so either
                // one's virtual time is the fleet's.
                let now = engines[0].obs().t;
                let mut turned = false;
                while now >= next_epoch {
                    store.epoch();
                    next_epoch += EPOCH_MS;
                    turned = true;
                }
                if turned {
                    epoch_no.update(&turn, |n| *n += 1);
                }

                for (s, engine) in engines.iter_mut().enumerate() {
                    counts[s].set(&turn, engine.counts());
                    let ink = core_ink_of(engine);
                    if core_ink[s].now(&turn) != ink {
                        core_ink[s].set(&turn, ink);
                    }
                }

                let seen = store.snapshot();
                // What each tenant has drawn on each machine this epoch. The same fares the
                // store is banking, kept per server as well — a box reports what it measured.
                if turned {
                    billed = vec![[Duration::ZERO; TENANTS.len()]; SERVERS];
                }
                for (s, engine) in engines.iter_mut().enumerate() {
                    for (tenant, fare) in engine.take_fares() {
                        if let Some(k) = TENANTS.iter().position(|t| t.id == tenant) {
                            billed[s][k] += fare;
                        }
                    }
                    let lines = tenant_lines(&queues_by_tenant(engine), &billed[s], &seen.enforced);
                    if box_tenants[s].now(&turn) != lines {
                        box_tenants[s].set(&turn, lines);
                    }
                }
                // A tenant's wire wears the shed colour while the fleet is actually refusing
                // it — so the three-second lag is on the wires too, not only in the table.
                let refusing: Vec<bool> = seen.enforced.iter().map(|&d| d > 0.0).collect();
                let mut next = Vec::with_capacity(SERVERS * TENANTS.len());
                for (s, engine) in engines.iter_mut().enumerate() {
                    let on_wire = on_the_wire(engine);
                    for (k, (out, home)) in
                        on_wire.out.into_iter().zip(on_wire.home).enumerate()
                    {
                        next.push(Wire {
                            tenant: k,
                            server: s,
                            shed: refusing.get(k).copied().unwrap_or(false),
                            sent: out,
                            home,
                        });
                    }
                }
                if wires.now(&turn) != next {
                    wires.set(&turn, next);
                }

                let rows = table(&seen, &std::array::from_fn(|k| dials[k].now(&turn)));
                if lines.now(&turn) != rows {
                    lines.set(&turn, rows);
                }

                // Both servers' records pool into one reading: a tenant's experience is of the
                // service, not of whichever machine happened to take the request.
                if now - summary_at >= SUMMARY_MS {
                    summary_at = now;
                    let pooled =
                        Summary::recent(
                            ledgers.iter().flat_map(|l| l.records().iter().cloned()).collect(),
                            HALF_LIFE_MS,
                        );
                    let queue = pooled.bars(pooled.by_tenant(QUEUE_TIME));
                    let work = pooled.bars(pooled.by_tenant(PROCESSING_TIME));
                    queue_axis.set(&turn, axis_for(&queue).max(QUEUE_FLOOR_MS));
                    work_axis.set(&turn, axis_for(&work));
                    queue_bars.set(&turn, queue);
                    work_bars.set(&turn, work);
                }
            }
            RateLimitMsg::Toggle => running.update(&turn, |r| *r = !*r),
            RateLimitMsg::Speed(raw) => {
                speed.set(&turn, raw);
                for engine in &mut engines {
                    engine.set_speed(speed_from_raw(raw));
                }
            }
            RateLimitMsg::Reset => {
                engines = fleet(&store);
                for engine in &mut engines {
                    engine.set_speed(speed_from_raw(speed.now(&turn)));
                }
                // Fresh machines, so the sample the boxplots read starts empty — otherwise the
                // graphs would go on reporting requests served by servers that no longer exist.
                ledgers = (0..SERVERS).map(|_| Latencies::default()).collect();
                // And the store forgets with them: its window is a reading of the servers that
                // have just been thrown away, and left alone it would go on refusing by it.
                store.clear();
                // The reader is put back where they came in: nothing enforcing, and the
                // invitation to turn it on standing again.
                limiting.set(&turn, false);
                enforce(&store, false, budget.now(&turn));
                billed = vec![[Duration::ZERO; TENANTS.len()]; SERVERS];
                for block in &box_tenants {
                    block.set(&turn, resting_lines());
                }
                next_epoch = EPOCH_MS;
                epoch_no.set(&turn, 0);
                moved = true;
            }
            RateLimitMsg::Qps(k, v) => {
                qps[k].set(&turn, v);
                moved = true;
            }
            RateLimitMsg::Cost(k, v) => {
                cost[k].set(&turn, v);
                moved = true;
            }
            RateLimitMsg::Priority(k, v) => {
                priority[k].set(&turn, v);
                moved = true;
            }
            RateLimitMsg::Budget(v) => {
                budget.set(&turn, v);
                enforce(&store, limiting.now(&turn), v);
            }
            RateLimitMsg::Limiting => {
                let on = !limiting.now(&turn);
                limiting.set(&turn, on);
                enforce(&store, on, budget.now(&turn));
            }
            // A preset only puts the knobs somewhere. What lights up is read back off them,
            // so there is nothing here that could disagree with what the rails show.
            RateLimitMsg::Preset(i) => {
                if let Some(preset) = presets().get(i) {
                    for (k, want) in preset.tenants.iter().enumerate() {
                        qps[k].set(&turn, want.qps);
                        cost[k].set(&turn, want.cost_ms);
                        priority[k].set(&turn, want.priority);
                    }
                    moved = true;
                }
            }
            RateLimitMsg::Measured(Measured::Stage(rect)) => stage_rect.set(&turn, Some(rect)),
            RateLimitMsg::Measured(Measured::Tenant(k, rect)) => {
                tenant_rects.update(&turn, |v| place(v, k, rect))
            }
            RateLimitMsg::Measured(Measured::Server(k, rect)) => {
                server_rects.update(&turn, |v| place(v, k, rect))
            }
        }
        // The machines follow the knobs. Whatever moved one, the dials are where the current
        // setting is read from — there is no second copy to keep in step with them.
        if moved {
            let at: PerTenant<Knobs> = std::array::from_fn(|k| dials[k].now(&turn));
            apply(&mut engines, &store, &at);
        }
    }
}

/// Tell the store what the fleet will spend, or that it is not rate limiting. Off is the
/// absence of a budget rather than a budget of nothing — a fleet spending zero would refuse
/// everyone, which is the opposite of what the switch says.
fn enforce(store: &Store, limiting: bool, budget_ms: f64) {
    store.set_budget(limiting.then(|| Duration::from_secs_f64(budget_ms / 1000.0)));
}

/// The store's four columns, per tenant — one row of the loop, mid-flight.
fn table(seen: &Snapshot, knobs: &PerTenant<Knobs>) -> Vec<Line> {
    let (accruing, written, calculated, enforced) =
        (&seen.accruing, &seen.written, &seen.calculated, &seen.enforced);
    let total: f64 = knobs.iter().map(|k| k.priority.max(0.0)).sum();
    TENANTS
        .iter()
        .enumerate()
        .map(|(k, tenant)| Line {
            tenant: tenant.id.to_string(),
            tint: tenant.tint,
            priority: match total > 0.0 {
                true => pct(knobs[k].priority.max(0.0) / total),
                false => pct(1.0 / TENANTS.len() as f64),
            },
            accruing: ms(accruing[k]),
            blame: ms(written[k]),
            demand: ms(calculated[k].estimated),
            calculated: pct(calculated[k].drop_pct),
            enforced: pct(enforced[k]),
        })
        .collect()
}

#[styles]
pub mod styles {
    use idyll_styles::Style;

    use crate::atoms::stage::Stage;
    use crate::atoms::tokens::{Face, Radius};
    use crate::styles::Palette;

    /// The traffic and the thing deciding what happens to it. The store's column is sized to
    /// hold seven equal columns at their widest reading — a number that wraps, or a heading
    /// that will not sit over its own column, costs more than the room does.
    ///
    /// Below the width that holds both, the store goes under the picture rather than beside
    /// it: side by side, the narrower the page the less the traffic and the table each get,
    /// and the table is the one that stops being readable first.
    pub const BOARD: Style = css! {{
        display: "grid",
        grid_template_columns: "1fr 466px",
        align_items: "stretch",
        gap: "14px",
        padding: "14px",
        max_width(900px): { grid_template_columns: "minmax(0,1fr)" },
    }};

    /// Sized by its rows, not by the stage beside it: the table is three tenants long whatever
    /// the picture next to it is doing.
    pub const LEDGER: Style = css! {{
        display: "flex",
        flex_direction: "column",
        align_self: "flex-start",
        gap: "8px",
        min_width: "0",
        padding: "12px 14px",
        background: Stage::ground,
        border_width: "1px",
        border_style: "solid",
        border_color: Palette::line,
        border_radius: Radius::card,
    }};

    pub const LEDGER_HEAD: Style = css! {{
        display: "flex",
        align_items: "baseline",
        justify_content: "space-between",
        gap: "10px",
    }};

    pub const LEDGER_TITLE: Style = css! {{
        font_family: Face::sans,
        font_size: "12px",
        font_weight: 600,
        letter_spacing: "0.06em",
        text_transform: "uppercase",
        color: Palette::gutter,
    }};

    /// The clock. It is the only thing on the panel that moves on the loop's beat, so it is
    /// what tells the reader a second has passed — and it reads in seconds, an epoch being one.
    pub const EPOCH: Style = css! {{
        font_family: Face::mono,
        font_size: "12px",
        color: Palette::ink_muted,
    }};

    pub const TABLE: Style = css! {{
        display: "flex",
        flex_direction: "column",
        gap: "2px",
    }};

    /// The headings are the widest thing in most of these columns, so they set how wide a
    /// column has to be — and where seven of them will not sit side by side, they interleave
    /// on two lines rather than run together.
    pub const HEAD_ROW: Style = css! {{
        display: "flex",
        align_items: "flex-end",
        gap: "4px",
        max_width(900px): { align_items: "flex-start", height: "26px" },
        padding_bottom: "4px",
        border_bottom_width: "1px",
        border_bottom_style: "solid",
        border_bottom_color: Palette::line,
        font_family: Face::sans,
        font_size: "9px",
        letter_spacing: "0.02em",
        text_transform: "uppercase",
        color: Palette::gutter,
    }};

    pub const LINE: Style = css! {{
        display: "flex",
        align_items: "center",
        gap: "4px",
        font_family: Face::mono,
        font_size: "11px",
        color: Palette::ink_muted,
    }};

    /// The tenant's name and its colour, wide enough that the numbers beside it line up.
    pub const WHO: Style = css! {{
        display: "flex",
        align_items: "center",
        gap: "5px",
        width: "58px",
        flex_shrink: 0,
    }};

    pub const SWATCH: Style = css! {{
        width: "8px",
        height: "8px",
        border_radius: Radius::pill,
        flex_shrink: 0,
    }};

    /// One reading, right-aligned so a column is a column. Every column takes an equal share of
    /// the row, which is what makes the headings sit over their numbers: the heading row and the
    /// tenant rows are separate flex boxes, so sizing each to its own content lines a column up
    /// with nothing.
    pub const NUM: Style = css! {{
        flex_grow: 1,
        flex_basis: "0",
        min_width: "0",
        text_align: "right",
    }};

    /// A heading on the lower of the two lines. Alternating ones drop, so each still sits
    /// against its own column while its neighbours clear it.
    pub const LOW: Style = css! {{
        max_width(900px): { margin_top: "12px" },
    }};

    /// The number the servers are actually refusing by — the end of the loop, and the only
    /// column that is not a working.
    pub const LIVE: Style = css! {{
        font_weight: 600,
        color: Stage::amber,
    }};

    /// The two cuts of the round trip, side by side. They wrap rather than narrow: a boxplot
    /// squeezed to half a column stops being readable before it stops fitting.
    pub const CUTS: Style = css! {{
        display: "flex",
        gap: "18px",
        flex_wrap: "wrap",
        padding: "0 14px 14px",
    }};

    pub const CUT: Style = css! {{
        flex_grow: 1,
        flex_basis: "360px",
        min_width: "0",
    }};

    pub const CUT_TITLE: Style = css! {{
        display: "block",
        margin_bottom: "6px",
        font_family: Face::sans,
        font_size: "12px",
        font_weight: 600,
        letter_spacing: "0.06em",
        text_transform: "uppercase",
        color: Palette::gutter,
    }};

    /// A stack of knob rows under one gutter label — the tenants, one line each.
    pub const RAILS: Style = css! {{
        display: "flex",
        flex_direction: "column",
        gap: "8px",
        flex_grow: 1,
        flex_basis: "0",
    }};

    pub const RAIL: Style = css! {{
        display: "flex",
        align_items: "center",
        gap: "16px",
        flex_wrap: "wrap",
    }};
}
