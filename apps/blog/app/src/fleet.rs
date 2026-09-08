//! **The cluster** — ten real servers, and the two ways to read one. Each server is a
//! [`SimEngine`](crate::engine) from [`Cluster`](crate::cluster), running live from cold at
//! `t=0`. The fleet view draws each as a [`ServerBox`] summary off its own state; drilling
//! into one draws that same engine in full through [`MachineView`](crate::machine_view) — the
//! summary and the picture are two readings of one running machine, which is the sim's lesson.
//!
//! Ten engines tick every frame whatever is on screen, so the fleet you zoom out to is the
//! fleet that was running while you watched one server. Only the focused server keeps the
//! motion state its picture is drawn from; the rest need only their counts.

use std::rc::Rc;

use idyll::{live_view, Ctx, Setup, Signal};
use idyll_styles::styles;

use crate::atoms::button::{Button, ButtonKind};
use crate::atoms::invite::Invite;
use crate::atoms::server_box::{server_name, ServerBox};
use crate::atoms::sim_card::SimHeader;
use crate::atoms::slider::{fmt_qps, Name, Scale, Slider};
use crate::cluster::{Cluster, HEALTHY_QPS, SERVERS};
use crate::engine::ServerCounts;
use crate::machine_view::{build_frame, MachineView, PaintState};
use crate::simview::{Layout, ViewState, STAGE_W};
use blog_core::SimKey;

/// The engine steps at 30 Hz — one quantum of virtual time per rendered frame, the pace the
/// motion layer and the single-server sim are tuned around.
const ENGINE_STEP_MS: f64 = 1000.0 / 30.0;
/// The most catch-up a slow frame may buy, so a stall doesn't fast-forward the machine.
const MAX_CATCH_UP: f64 = 3.0;

/// The cluster sim's messages: a frame tick (its ms delta), the stage's measured width, and
/// the zoom — drill into a server, climb back to the fleet, or move the focused server's load.
#[derive(Debug)]
pub enum FleetMsg {
    Tick(f64),
    Resize(f64),
    Focus(usize),
    Up,
    Qps(f64),
    /// Release the still. Clicking the machine only ever starts it — climbing out to the
    /// fleet is the [`Invite`]d button's job, so a stray click cannot lose the reader's place.
    Start,
}

/// The focused server's frame producer: which engine it draws, and the motion/paint state its
/// picture is folded from. Rebuilt when the reader drills into a different server, so the dots
/// start in the new picture rather than teleporting from the last one.
struct Focused {
    server: usize,
    vs: ViewState,
    ps: PaintState,
    accum: f64,
}

impl Focused {
    fn new(server: usize, layout: Layout) -> Focused {
        Focused { server, vs: ViewState::new(layout), ps: PaintState::new(), accum: 0.0 }
    }
}

pub(crate) async fn run(ctx: Ctx<Setup, FleetMsg>, _seed: crate::PageSeed, _key: SimKey) -> idyll::Result {
    let mut cluster = Cluster::new();
    // Each server's arrival rate, so drilling in shows the load it is actually running and a
    // reader who moves one knob leaves the others where they were.
    let mut qps = vec![HEALTHY_QPS; SERVERS];

    // One live counts signal per server for the fleet grid; each box tracks its own engine.
    let counts: Vec<_> = cluster.fleet_counts().into_iter().map(|c| ctx.mutable_signal(c)).collect();
    let boxes: Vec<(usize, String, Signal<ServerCounts>, Signal<bool>)> = (0..SERVERS)
        .map(|k| {
            let live = counts[k].read();
            let idle = {
                let live = live.clone();
                ctx.computed(move |cx| {
                    let c = live.get(cx);
                    c.inflight == 0 && c.busy == 0
                })
                .read()
            };
            (k, server_name(k), live, idle)
        })
        .collect();

    // The zoom: `Some(k)` draws server k in full, `None` is the fleet grid. The reader opens on
    // web-01, watches it fill, then climbs up to meet the rest.
    let focus = ctx.mutable_signal(Some(0usize));
    // The invitation to climb out of the one server and meet the rest — it glows the Up
    // button until the reader first takes it, then never again.
    let invited = ctx.mutable_signal(true);
    let up_when = invited.read();
    let zoomed = {
        let focus = focus.read();
        ctx.computed(move |cx| focus.get(cx).is_some()).read()
    };
    let focused_name = {
        let focus = focus.read();
        ctx.computed(move |cx| focus.get(cx).map(server_name).unwrap_or_default()).read()
    };

    // The focused picture's reactive sources: the per-tick frame folded from the focused
    // engine, the width it is drawn to, and the load its knob shows.
    let layout = ctx.mutable_signal(Layout::of(STAGE_W));
    let focused_qps = ctx.mutable_signal(HEALTHY_QPS);
    let qps_at = focused_qps.read();

    // The focused frame producer, seeded on web-01, and its opening frame off the cold engine.
    let mut focused: Option<Focused> = Some(Focused::new(0, Layout::of(STAGE_W)));
    let opening = {
        let f = focused.as_mut().unwrap();
        build_frame(cluster.server(0).obs(), &f.vs, &mut f.ps)
    };
    let frame = ctx.mutable_signal(Rc::new(opening));

    // The cluster holds a still until the reader releases it: ten engines stepping from t=0
    // is ten engines stepping whether or not anyone has reached this far down the page.
    let running = ctx.mutable_signal(false);
    ctx.frames(&running.read(), FleetMsg::Tick);
    ctx.resizes(FleetMsg::Resize);
    let armed = {
        let running = running.read();
        ctx.computed(move |cx| !running.get(cx)).read()
    };

    let up_lbl = ctx.constant("↑ the fleet".to_string());

    let mut ctx = ctx.render(live_view! {
        div css=[crate::atoms::sim_card::styles::CARD] {
            @if ($zoomed) {
                div css=[styles::BAR] {
                    Invite when=(up_when) hint=("Click to see summary view") {
                        Button kind=(ButtonKind::Ghost) label=(up_lbl) pressed=>(|_| FleetMsg::Up)
                    }
                    span css=[styles::WHO] { $focused_name }
                }
                MachineView frame=(frame) layout=(layout) charts_on=(false) armed=(armed) on_click=>(|_| FleetMsg::Start)
                div css=[styles::CTRL] {
                    Slider name=(Name::new("arrivals", 52)) scale=(Scale::new(1, 400, 1)) at=(qps_at) fmt=(fmt_qps) moved=>(|n| FleetMsg::Qps(n))
                }
            } else {
                SimHeader title=("Fleet · 10 workers") aggs=(Vec::new())
                div css=[styles::FLEET] {
                    @for (k, name, live, idle) in (boxes) {
                        div css=[styles::CELL] onclick=>(move |_| Some(FleetMsg::Focus(k))) {
                            ServerBox name=(name) counts=(live) idle=(idle)
                        }
                    }
                }
            }
        }
    }).await?;

    // The width the picture is drawn to, updated as the stage is measured; the focused
    // producer's `ViewState` is built against it.
    let mut lay = Layout::of(STAGE_W);
    loop {
        let (msg, turn) = ctx.recv().await?;
        match msg {
            FleetMsg::Tick(dt) => {
                // Every engine advances, focused or not, in whole ENGINE_STEP_MS quanta out of
                // an accumulator — fixed-timestep motion, one frame publish per rendered frame.
                if let Some(f) = &mut focused {
                    let s = f.server;
                    f.accum = (f.accum + dt.max(0.0)).min(ENGINE_STEP_MS * MAX_CATCH_UP);
                    while f.accum >= ENGINE_STEP_MS {
                        f.accum -= ENGINE_STEP_MS;
                        cluster.tick(ENGINE_STEP_MS);
                        f.vs.absorb_subtick(cluster.server(s).obs());
                    }
                    f.vs.step(cluster.server(s).obs());
                    let fresh = build_frame(cluster.server(s).obs(), &f.vs, &mut f.ps);
                    frame.set(&turn, Rc::new(fresh));
                } else {
                    cluster.tick(dt);
                }
                for (slot, fresh) in counts.iter().zip(cluster.fleet_counts()) {
                    slot.set(&turn, fresh);
                }
            }
            FleetMsg::Resize(width) => {
                let next = Layout::of(width);
                if next != lay {
                    lay = next;
                    layout.set(&turn, next);
                    // A new geometry restarts the focused picture's motion — every dot's route
                    // was walked in the old width.
                    if let Some(f) = &mut focused {
                        *f = Focused::new(f.server, next);
                    }
                }
            }
            FleetMsg::Focus(k) => {
                focus.set(&turn, Some(k));
                focused_qps.set(&turn, qps[k]);
                focused = Some(Focused::new(k, lay));
            }
            FleetMsg::Up => {
                focus.set(&turn, None);
                invited.set(&turn, false);
                focused = None;
            }
            FleetMsg::Start => running.set(&turn, true),
            FleetMsg::Qps(v) => {
                if let Some(f) = &focused {
                    cluster.set_qps(f.server, v);
                    qps[f.server] = v;
                    focused_qps.set(&turn, v);
                }
            }
        }
    }
}

#[styles]
pub mod styles {
    use idyll_styles::Style;

    use crate::atoms::tokens::Face;
    use crate::styles::Palette;

    /// The tiling grid: equal tracks that wrap at a content threshold — `auto-fill` +
    /// `minmax` behaviour flexbox can only approximate.
    pub const FLEET: Style = css! {{
        display: "grid",
        grid_template_columns: "repeat(auto-fill, minmax(200px, 1fr))",
        gap: "10px",
    }};

    /// One grid cell: the summary is a door into the machine, so it reads as pressable.
    pub const CELL: Style = css! {{
        cursor: "pointer",
        border_radius: "12px",
        transition: "transform 120ms, box-shadow 120ms",
        ":hover": {
            transform: "translateY(-2px)",
            box_shadow: "0 6px 18px #28301a1a",
        },
    }};

    /// The focused view's top bar: the way back up, and which server is drawn.
    pub const BAR: Style = css! {{
        display: "flex",
        align_items: "center",
        gap: "12px",
        margin: "2px 4px 12px",
    }};
    pub const WHO: Style = css! {{
        font_family: Face::mono,
        font_size: "13px",
        font_weight: 600,
        color: Palette::control_ink,
    }};
    pub const CTRL: Style = css! {{
        display: "flex",
        margin: "12px 4px 2px",
    }};
}
