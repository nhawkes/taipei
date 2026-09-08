//! The single-server **machine picture** — the 960×520 stage and its colour key,
//! lifted out of the queue visualiser so a second sim can render the identical view.
//!
//! The picture is a pure function of two reactive sources — a per-tick [`Frame`]
//! snapshot and the [`Layout`] the stage was measured to — plus the static `charts_on`
//! choice and the `armed` still flag. Every readout, style and the dot swarm is a memo
//! over one of those, so a binding rewrites the DOM only when its own value moves. The
//! frame fold ([`build_frame`]) and the [`PaintState`] it threads live here too: the
//! host live builds frames and hands them in.

use idyll::{live_view, Callback, Ctx, Event, Never, Setup, Signal};
use std::rc::Rc;

use crate::atoms::controls::styles as cstyles;
use crate::atoms::figure::slots;
use crate::atoms::stage::{Paint, Stage as Ink};
use crate::engine::{CpuTask, Obs, Outcome, SparkPoint, Station, CORES};
use crate::simview::*;

// ─── palette ──────────────────────────────────────────────────────────────────
// The names the drawing code uses; the values live in the stylesheet
// (`crate::atoms::stage`), so the diagram retints without recompiling.

pub(crate) const TEAL:   Paint = Ink::teal.value();
pub(crate) const GREEN:  Paint = Ink::green.value();
pub(crate) const AMBER:  Paint = Ink::amber.value();
pub(crate) const BLUE:   Paint = Ink::blue.value();
pub(crate) const ORANGE: Paint = Ink::orange.value();
pub(crate) const PURPLE: Paint = Ink::purple.value();
pub(crate) const MUTED:  Paint = Ink::muted.value();
pub(crate) const LINE:   Paint = Ink::wall.value();
pub(crate) const GREY:   Paint = Ink::grey.value();
pub(crate) const SALMON: Paint = Ink::salmon.value();
pub(crate) const RED:    Paint = Ink::red.value();
pub(crate) const CHANNEL: Paint = Ink::channel.value();
pub(crate) const CORE_EDGE: Paint = Ink::core.value();
// The two exits, washed: a gate third is the *possibility* of that outcome, so it
// carries the outcome's hue at a fraction of its strength. Full strength belongs to
// requests that actually took it.
pub(crate) const WASH_ADMIT: Paint = Ink::wash_admit.value();
pub(crate) const WASH_TIMEOUT: Paint = Ink::wash_timeout.value();

const SPARK_N: usize = 130;

/// The colour/transform transition, a hair over the ~33 ms tick so the windows overlap
/// and never gap; and the walk animation's length, one tick, so a dot reaches its path's
/// end just as the next write lands.
const TWEEN_MS: u32 = 45;
const WALK_MS: u32 = 34;
/// The at-rest sheen's period. Each dot runs [`cstyles::DotSheen`] on this cycle, its start
/// offset by a negative delay keyed to the dot's position, so the bright bump travels across
/// the field as one sweep rather than every dot flashing at once.
const SHEEN_PERIOD_S: f64 = 4.8;
/// How many dots the SYN and run-queue pipes draw before the tail elides into the `+N
/// more` readout — a slot past the drawn pipe is off the picture, and a backlog is a
/// number, not a solid bar of dots. The true depth always rides the readout.
const SYN_DRAWN_CAP: usize = 14;
const RUNQ_DRAWN_CAP: usize = 26;

// ─── static style helpers (computed once, at view build) ──────────────────────

/// A text label. Canvas `fill_text` y is the baseline; approximate with top = y − size.
/// Labels are the picture, so their *anchor* moves with the plumbing but their size
/// never does — a diagram whose text shrinks has stopped being readable, not reflowed.
fn lab(lay: &Layout, x: f64, y: f64, size: u32, col: impl std::fmt::Display) -> String {
    labelled(lay, x, y, size, WEIGHT, col, false)
}
/// A label that names a total rather than annotating a part — set heavier, because it
/// is the number the reader is meant to find.
fn lab_bold(lay: &Layout, x: f64, y: f64, size: u32, col: impl std::fmt::Display) -> String {
    labelled(lay, x, y, size, 600, col, false)
}
/// The gate's own readings: right-anchored beside it, and heavier, because which exit
/// is open is the one thing the fork is saying.
fn lab_gate(lay: &Layout, x: f64, y: f64, size: u32, col: impl std::fmt::Display) -> String {
    labelled(lay, x, y, size, 600, col, true)
}
fn lab_r(lay: &Layout, x: f64, y: f64, size: u32, col: impl std::fmt::Display) -> String {
    labelled(lay, x, y, size, WEIGHT, col, true)
}

/// The ordinary label weight. Stage text is small, so it is set a step heavier than
/// body copy would be to hold its colour against the channel behind it.
const WEIGHT: u32 = 500;

/// The closest a label sits to the stage's leading edge — clear of the card's own border.
const EDGE_INSET: f64 = 8.0;

fn labelled(
    lay: &Layout,
    x: f64,
    y: f64,
    size: u32,
    weight: u32,
    col: impl std::fmt::Display,
    right: bool,
) -> String {
    // `y` is the baseline, as it is in the drawing; a top-positioned box approximates
    // it by rising its own size.
    let place = match right {
        true => format!("right:{:.1}px;text-align:right", lay.w - lay.x(x)),
        // The ingress plumbing compresses to a hard floor, so the leftmost labels map to a
        // couple of pixels and read as printed on the card's edge. They hold an inset off it
        // instead: at every width where the mapping has room this is the mapping's own answer.
        false => format!("left:{:.1}px", lay.x(x).max(EDGE_INSET)),
    };
    format!("{place};top:{}px;font-size:{size}px;font-weight:{weight};color:{col}", y - size as f64)
}
/// A stat row over the exit pipes: it spans the whole right-hand plumbing, so it
/// centres over the pipes it describes at any width.
///
/// Once that plumbing is too narrow to hold the row on one line it is allowed to
/// wrap, and the exit pipes have already spread to make the room ([`Layout::exits`]).
/// The row is anchored by its **bottom** there, so it grows upward and the line
/// nearest its pipe stays nearest its pipe.
/// The in-flight reading. Named because [`row_c`] and its caller have to agree on which row
/// it is: pulled in over the machine it is the one that lands on the IO chamber's own label,
/// so it is the one that lifts.
const ROW_IN_FLIGHT: usize = 3;

/// How far it lifts — the overlap, and a line's breathing room on top of it.
const FLIGHT_LIFT: f64 = 10.0;

fn row_c(lay: &Layout, row: usize) -> String {
    let size = if lay.stats_stacked() { 10.0 } else { 11.0 };
    if lay.stats_stacked() {
        let lift = if row == ROW_IN_FLIGHT { FLIGHT_LIFT } else { 0.0 };
        // Centred, a reading longer than the plumbing it reports on hangs off *both* ends of
        // the stage and is clipped at each. Anchored to the stage's right edge instead, it
        // grows the one way there is room to grow — inward, over the machine, where
        // [`cstyles::STAT_SCRIM`] is what keeps it readable.
        return format!(
            "right:6px;top:{:.1}px;text-align:right;\
             font-size:{size}px;line-height:12px;color:{MUTED}",
            lay.readout_y(row) - size - lift,
        );
    }
    // Centred on the exit pipes' own anchor, not on the midpoint of whatever space is
    // left — the reading belongs to the pipe under it, so it sits where the pipe does.
    let (from, to) = (lay.x(CPU_X + CPU_W), lay.w);
    let w = (to - from).max(0.0);
    format!(
        "left:{:.1}px;top:{:.1}px;width:{w:.1}px;text-align:center;\
         font-size:{size}px;line-height:12px;color:{MUTED}",
        lay.x(READOUT_X) - w / 2.0,
        lay.readout_y(row) - size,
    )
}

/// Where an exit reading centres, in drawing coordinates — over the run of pipe it
/// describes, clear of the box it comes out of.
const READOUT_X: f64 = 760.0;

// ─── the fixed picture ─────────────────────────────────────────────────────────

/// Which parts of the machine this sim draws — the shape of the composition, fixed
/// for the life of the stage.
#[derive(Clone, Copy, PartialEq)]
struct Parts {
    /// The composition holds requests: it has a queue to draw, a fork to hold their
    /// head at, and a processing-timeout exit for work that finished too late.
    queue: bool,
    /// Something leaves by the shed pipe — a queue's deadline, or a rejection.
    shed: bool,
}

/// One drawn shape of the machine's silhouette: a path and the paint it wears.
#[derive(Clone, PartialEq)]
struct Shape {
    i: usize,
    d: String,
    style: String,
}

fn shape_d(s: &Shape) -> String {
    s.d.clone()
}
fn shape_style(s: &Shape) -> String {
    s.style.clone()
}

/// A rounded rectangle as a path, so a box and a pipe are the same primitive and the
/// silhouette is one list of shapes in paint order.
fn rounded_rect(x: f64, y: f64, w: f64, h: f64, r: f64) -> String {
    let r = r.min(w / 2.0).min(h / 2.0);
    format!(
        "M{:.1} {y:.1} H{:.1} A{r:.1} {r:.1} 0 0 1 {:.1} {:.1} V{:.1} \
         A{r:.1} {r:.1} 0 0 1 {:.1} {:.1} H{:.1} A{r:.1} {r:.1} 0 0 1 {x:.1} {:.1} V{:.1} \
         A{r:.1} {r:.1} 0 0 1 {:.1} {y:.1} Z",
        x + r,
        x + w - r,
        x + w,
        y + r,
        y + h - r,
        x + w - r,
        y + h,
        x + r,
        y + h - r,
        y + r,
        x + r,
    )
}

/// The machine's silhouette, in **two passes**: everything in the wall colour first,
/// then everything again inset by the wall's thickness in the channel colour. The
/// second pass carves the interior out of the first, so a pipe running into a box
/// opens a mouth in its wall by construction — there is no cap to hide and no eraser
/// to keep aligned with the thing it erases.
fn silhouette(lay: &Layout, parts: Parts) -> Vec<Shape> {
    const WALL_T: f64 = 1.5;
    let boxes = [
        (CPU_X, CPU_Y, CPU_W, CPU_H),
        (IOC_X, IOC_Y, IOC_W, IOC_H),
    ];
    let pipes = crate::simview::pipes(lay, parts.queue, parts.shed);
    let mut out: Vec<Shape> = Vec::new();
    let mut push = |d: String, style: String| out.push(Shape { i: out.len(), d, style });

    for pass in 0..2 {
        let (paint, inset) = if pass == 0 { (LINE, 0.0) } else { (CHANNEL, WALL_T) };
        for &(x, y, w, h) in &boxes {
            let (x0, x1) = (lay.x(x), lay.x(x + w));
            push(
                rounded_rect(x0 + inset, y + inset, x1 - x0 - 2.0 * inset, h - 2.0 * inset, 16.0 - inset),
                format!("fill:{paint}"),
            );
        }
        for p in &pipes {
            push(
                crate::simview::rounded_path(&p.pts, CORNER),
                format!(
                    "fill:none;stroke:{paint};stroke-width:{:.1};stroke-linejoin:round;stroke-linecap:round",
                    p.hw * 2.0 + if pass == 0 { WALL_T * 2.0 } else { 0.0 }
                ),
            );
        }
    }
    out
}

/// A core's burst, as the arc it sweeps: the same 2px round-capped stroke every other
/// ring on the stage is drawn with, riding just outside the core's own edge so a busy
/// core reads as a disc with a ring around it rather than a disc with a ring in it.
///
/// Drawn in the core's own box, so it needs no layout — the machine it sits in is what
/// moves.
fn core_arc(ring: &SlotRing) -> String {
    let (a, b) = (ring.a - 90.0, ring.b - 90.0);
    if b - a < 2.0 {
        return String::new(); // pinched shut between bursts
    }
    crate::simview::arc_path(CORE_BOX, CORE_BOX, 17.0, a, b)
}

/// Half the box a core's ring is drawn in — the arc's centre in its own coordinates.
const CORE_BOX: f64 = 19.0;

/// A core's drawn radius. Fixed: the machine keeps its parts' size at every width and
/// only the plumbing around it compresses.
const CORE_R: f64 = 15.0;

fn core_arc_style(ring: &SlotRing) -> String {
    let base = format!(
        "fill:none;stroke:{BLUE};stroke-width:2;stroke-linecap:round;pointer-events:none"
    );
    match ring.pulse {
        Some(parity) => format!(
            "{base};animation:{} 240ms ease-out",
            if parity { cstyles::TurnA } else { cstyles::TurnB }
        ),
        None => base,
    }
}

/// The eight cores, as the ring each one wears when idle: a filled disc with a hairline
/// edge. The burst arc is drawn over it per frame.
///
/// A core is a fixed-size part of the machine: only where it sits moves with the width.
/// Its burst ring is drawn at a fixed radius in the machine's own box, so a disc that
/// shrank would slide out from under its ring.
fn core_discs(lay: &Layout) -> Vec<Shape> {
    let r = CORE_R;
    (0..CORES)
        .map(|i| {
            let (cx, cy) = slot_pos(i);
            Shape {
                i,
                d: crate::simview::circle_path(lay.x(cx), cy, r),
                style: format!("fill:{CHANNEL};stroke:{CORE_EDGE};stroke-width:1"),
            }
        })
        .collect()
}

/// The gate at the fork, as the preview draws it: a circle standing in the CPU's left
/// wall, split into three 120° thirds — the queue's entrance (west), admission
/// (down-right into the box) and the timeout (up-right, out). Two thirds are washed in
/// their outcome's colour; a grey shutter covers whichever one is shut this frame, so
/// the request routes through the third left uncovered.
fn fork_thirds(lay: &Layout, present: bool) -> Vec<Shape> {
    if !present {
        return Vec::new();
    }
    let (cx, cy) = (lay.x(QFORK_X), FORK_Y);
    let r = crate::simview::fork_r(lay);
    let third = |i: usize, a0: f64, a1: f64, col: Paint| Shape {
        i,
        d: crate::simview::arc_path(cx, cy, r, a0, a1),
        style: format!("fill:none;stroke:{col};stroke-width:2;stroke-linecap:round"),
    };
    vec![
        // the disc the thirds ride, masking the queue's corner inside the circle
        Shape {
            i: 0,
            d: crate::simview::circle_path(cx, cy, r),
            style: format!("fill:{CHANNEL};stroke:none"),
        },
        third(1, 0.0, 120.0, WASH_ADMIT),
        third(2, 240.0, 360.0, WASH_TIMEOUT),
    ]
}

/// The gate's shutter — the grey third, over whichever exit is shut. It is one element
/// whose rotation the frame writes, so the gate reads as a shutter that *moves*
/// between exits rather than two lamps that happen to swap.
///
/// The transition is one engine frame long, like every other target the tick writes:
/// the picture interpolates *between* the gate's discrete positions and has always
/// arrived by the time the next one lands. A longer one would leave the shutter
/// permanently mid-swing, saying nothing.
fn shutter(lay: &Layout, angle: f64) -> String {
    let (cx, cy) = (lay.x(QFORK_X), FORK_Y);
    format!(
        "transform-origin:{cx:.1}px {cy:.1}px;transform:rotate({angle:.1}deg);\
         fill:none;stroke:{GREY};stroke-width:2;stroke-linecap:round;\
         transition:transform {WALK_MS}ms linear"
    )
}

/// The two labels that name a part of the machine rather than report on it: they move
/// with the plumbing and say the same thing at every width.
fn await_at(lay: &Layout) -> String {
    lab(lay, PIPE_IO + 20.0, 320.0, 11, MUTED)
}
fn run_queue_at(lay: &Layout) -> String {
    lab_r(lay, PIPE_RET - 18.0, 306.0, 11, MUTED)
}

/// A gate reading's presence: full while its exit is the open one, faded back when it
/// is not. The transition is what makes a gate that opens every few frames read as
/// open rather than as a strobe.
fn wash(open: bool) -> String {
    let opacity = if open { 1.0 } else { 0.3 };
    format!("opacity:{opacity};transition:opacity 300ms")
}

/// The width a gate reading wraps within on a narrow stage — the same figure the
/// queue's leg pulls in to clear, so the two can never disagree about the room.
fn gate_label_wrap() -> String {
    ";white-space:normal;width:40px;line-height:11px".to_string()
}

/// Where the sparklines sit: right-anchored beside the machine, in the gap the
/// right-hand plumbing leaves; below it at full width once there is no gap. Only the
/// block is placed — the titles, charts and legends inside it are ordinary flow, which
/// is what keeps each legend under its own chart at either width.
fn charts_at(lay: &Layout) -> String {
    if lay.charts_below() {
        format!("left:16px;top:{:.1}px;width:{:.1}px", lay.charts_top() + 4.0, (lay.w - 32.0).max(0.0))
    } else {
        format!("right:18px;top:{CHART_Y}px;width:{CHART_W}px")
    }
}

/// A core's burst-ring box, in the machine wrapper's own coordinates. It is placed
/// through the same mapping as the core's disc — the machine compresses its cores
/// *toward each other* on a narrow stage without shrinking them, so a ring positioned
/// off the unmapped grid would slide out from under the disc it belongs to.
fn core_at(lay: &Layout, i: usize) -> String {
    let (cx, cy) = slot_pos(i);
    format!(
        "left:{:.1}px;top:{:.1}px",
        lay.x(cx) - lay.x(CPU_X) - CORE_BOX,
        cy - CORE_BOX,
    )
}

// ─── per-frame style builders ──────────────────────────────────────────────────

/// One moving dot. `key` is the request id — stable across every station transition
/// *and* the live→dropping handoff (ids never reuse, and a request is never live and
/// dropping at once), so state changes restyle the same node and the verdict's colour
/// tweens in place. `path` is the **exact polyline the dot walked this frame** —
/// emitted as the node's `offset-path` with a shared 0→100% walk animation, so every
/// rendered interpolated position lies on the walked route: no corner is ever cut.
#[derive(Clone, PartialEq)]
struct DotVm {
    key: u32,
    x: f64,
    y: f64,
    path: String,
    /// Alternates per frame so the walk animation re-triggers every write.
    parity: bool,
    col: Paint,
    r: f64,
    /// Always 1 — a dot travels, it does not fade. Only the request that fell out of
    /// the picture unanswered ([`ViewState::leaving`](crate::simview)) carries anything
    /// else.
    opacity: f64,
    /// This dot's place in the at-rest sheen sweep, `0..1` by screen position — a negative
    /// [`SHEEN_PERIOD_S`] delay, so the bright bump reaches it in field order (see
    /// [`sheen_phase`]).
    sheen_phase: f64,
}

/// A request's queue-deadline arc, riding its dot. A ring is in this list **only while
/// it should be drawn** — the request is in the app queue, the shed deadline is live,
/// and the dot has actually reached the queue pipe — so presence is the whole
/// visibility rule and no invisible node is carried for the requests that have none.
#[derive(Clone, PartialEq)]
struct RingVm {
    key: u32,
    path: String,
    parity: bool,
    ring: Ring,

}

/// A countdown arc as **two forward-only angles** (degrees, unbounded): the arc
/// spans `a → b` of a rotated conic, so its length is `b − a` and wrap is free.
/// The lens: over a burst's life both edges travel one lap, `b` on an ease-out
/// curve, `a` on an ease-in — the arc opens fast, peaks mid-burst, and pinches
/// closed exactly as the burst ends.
#[derive(Clone, Copy, PartialEq)]
struct Ring {
    a: f64,
    b: f64,
    col: Paint,
}

/// One core slot's arc — a persistent element whose angles accumulate a lap per
/// burst. `pulse` fires the turnover blink on a frame where one burst ended and the
/// next began before the snapshot (the parity alternates to re-trigger).
#[derive(Clone, Copy, PartialEq, Default)]
struct SlotRing {
    a: f64,
    b: f64,
    pulse: Option<bool>,
}

/// The lens curves (quadratic): gap = out − in = 2p(1−p), peaking at half a lap.
fn ease_in(p: f64) -> f64 {
    p * p
}
fn ease_out(p: f64) -> f64 {
    p * (2.0 - p)
}

/// The dot's colour/transform transition ([`TWEEN_MS`], a hair over the tick).
fn dot_trans() -> String {
    format!("transform {TWEEN_MS}ms linear, background {TWEEN_MS}ms linear, opacity {TWEEN_MS}ms linear")
}
/// The ring's compositor transition, named by the handles.
fn ring_trans() -> String {
    format!("{} {TWEEN_MS}ms linear, {} {TWEEN_MS}ms linear", cstyles::Motion::a, cstyles::Motion::b)
}
/// The shared walk animation: `offset-distance` 0→100% along this frame's path,
/// restarting every write via the alternating name. A tick's length, so the dot is at
/// the path's end when the next write lands.
fn walk_anim(parity: bool) -> ::idyll_styles::Keyframes {
    if parity { cstyles::WalkA } else { cstyles::WalkB }
}

/// The dot's whole inline style: this frame's walked polyline as its `offset-path`
/// (base `offset-distance:100%` holds the end after the walk), scaled by radius,
/// painted, colour tweened.
///
/// A dot is solid. It enters the picture already travelling, from off-stage down the
/// SYN pipe, and leaves past the right edge — so it never appears or disappears where
/// the reader is looking, and its opacity is 1 for its whole journey. The exception is
/// the request nobody is waiting for any more ([`ViewState::leaving`](crate::simview)).
/// The machine ordered into one continuous sheen route, so the at-rest highlight flows the
/// way the dots travel — no gap between regions. Each stage is a segment; a dot's phase is its
/// segment index plus its rank within the segment, over the segment count. The per-segment key
/// sorts by the segment's own flow direction (see [`assign_sheen_phases`]).
const SEG_SYN: u8 = 0; // arrive: left → right
const SEG_RUNQ: u8 = 1; // run queue: up (IO → CPU)
const SEG_CPU: u8 = 2; // CPU: bottom → top
const SEG_IO: u8 = 3; // IO circuit: down the await leg, then up the return
const SEG_EXIT: u8 = 4; // exits: left → right
const SEG_QUEUE: u8 = 5; // app queue loop: right → left
const SHEEN_SEGMENTS: usize = 6;

/// Which sheen segment a live station belongs to. The dropping dot is placed in [`SEG_EXIT`]
/// directly (it carries no station).
fn sheen_segment(station: &Station) -> u8 {
    match station {
        Station::NetworkIn { .. } | Station::SynBacklog { .. } | Station::Accept { .. } => SEG_SYN,
        Station::RunQueue { .. } => SEG_RUNQ,
        Station::Cpu { .. } => SEG_CPU,
        Station::Io { .. } => SEG_IO,
        Station::AppQueue { .. } => SEG_QUEUE,
        Station::NetworkOut { .. } => SEG_EXIT,
    }
}

/// Each dot's place in the sheen sweep, `0..1`, from its `(segment, x, y)`. Within a segment
/// the dots sort by that segment's flow direction, and the phase is
/// `(segment + rank_fraction) / SHEEN_SEGMENTS` — continuous across the whole machine, so the
/// highlight travels through it as one sweep rather than every dot flashing at once. Returned
/// parallel to `route` (and so to `dots`).
fn sheen_phases(route: &[(u8, f64, f64)]) -> Vec<f64> {
    // The IO circuit's midline splits its down-leg (await, right) from its up-leg (return,
    // left), so the sweep runs down one and up the other rather than jumping between them.
    let (mut io_lo, mut io_hi) = (f64::MAX, f64::MIN);
    for (_, x, _) in route.iter().filter(|(s, _, _)| *s == SEG_IO) {
        io_lo = io_lo.min(*x);
        io_hi = io_hi.max(*x);
    }
    let io_mid = if io_lo <= io_hi { (io_lo + io_hi) / 2.0 } else { 0.0 };
    let key = |seg: u8, x: f64, y: f64| match seg {
        SEG_SYN | SEG_EXIT => x,
        SEG_RUNQ | SEG_CPU => -y,
        SEG_IO => if x > io_mid { y } else { 1e6 - y },
        _ => -x, // SEG_QUEUE
    };
    let mut phases = vec![0.5; route.len()];
    for seg in 0..SHEEN_SEGMENTS as u8 {
        let mut members: Vec<usize> = (0..route.len()).filter(|&i| route[i].0 == seg).collect();
        members.sort_by(|&a, &b| {
            key(seg, route[a].1, route[a].2).total_cmp(&key(seg, route[b].1, route[b].2))
        });
        let n = members.len();
        for (rank, &i) in members.iter().enumerate() {
            let frac = if n > 1 { rank as f64 / (n - 1) as f64 } else { 0.5 };
            phases[i] = (seg as f64 + frac) / SHEEN_SEGMENTS as f64;
        }
    }
    phases
}

/// The dot's whole inline style. Two animations ride it: the walk (this frame's path) and the
/// at-rest sheen, their play-states toggled opposite — the walk runs while the stage does, the
/// sheen runs while it is `paused`. The sheen's negative delay places this dot in the sweep by
/// its [`sheen_phase`], so a held stage glimmers a highlight across the field.
fn dot_style(d: &DotVm, paused: bool) -> String {
    let sheen_delay = -(SHEEN_PERIOD_S * (1.0 - d.sheen_phase));
    let play = if paused { "paused,running" } else { "running,paused" };
    format!(
        "offset-path:path('{}');offset-distance:100%;transform:scale({:.2});background:{};opacity:{:.2};transition:{};animation:{} {WALK_MS}ms linear,{} {SHEEN_PERIOD_S}s linear {sheen_delay:.3}s infinite;animation-play-state:{play}",
        d.path, d.r / 5.0, d.col, d.opacity, dot_trans(), walk_anim(d.parity), cstyles::DotSheen,
    )
}

/// The deadline ring walks the same path as its dot; its conic angles are the
/// registered `Motion::a`/`Motion::b`.
fn ring_style(r: &RingVm) -> String {
    format!(
        "offset-path:path('{}');offset-distance:100%;{}:{};{}:{};{}:{};transition:{};animation:{} {WALK_MS}ms linear",
        r.path,
        cstyles::Motion::a, deg(r.ring.a),
        cstyles::Motion::b, deg(r.ring.b),
        cstyles::Motion::c, r.ring.col,
        ring_trans(), walk_anim(r.parity),
    )
}


fn deg(v: f64) -> String {
    format!("{v:.1}deg")
}

/// The colour of a request at a station — the engine's truth, shown instantly.
fn station_col(station: &Station) -> Paint {
    match station {
        // Kernel-side of accept(): in flight or established in the accept queue — teal.
        Station::NetworkIn { .. } | Station::SynBacklog { .. } => TEAL,
        // Runtime-side of spawn(): a task awaiting a worker — amber.
        Station::RunQueue { .. } => AMBER,
        // A hung handler zombie-holding its core is dead, not working: red.
        Station::Cpu { hung: true, .. } => RED,
        Station::Accept { .. } | Station::Cpu { .. } => BLUE,
        Station::AppQueue { .. } => GREY,
        Station::Io { .. } => PURPLE,
        // On the way home a request wears its verdict — the station carries it.
        Station::NetworkOut { reply, .. } => outcome_col(reply.outcome()),
    }
}

/// The colour a finished request wears — the one mapping from verdict to hue, so a mark riding
/// home on a wire and a dot leaving down an exit pipe cannot disagree about what happened.
pub(crate) fn outcome_col(outcome: Outcome) -> Paint {
    match outcome {
        Outcome::Success => GREEN,
        Outcome::QueueTimeout => ORANGE,
        Outcome::Rejected | Outcome::ResponseTimeout => RED,
        // Refused for being over a share, not because the server had nothing to give — the
        // rate-limiting chapter turns on the difference, so it does not wear the shed reds.
        Outcome::RateLimited => AMBER,
        Outcome::ProcessingTimeout => SALMON,
    }
}

// ─── the per-tick snapshot the view derives from ────────────────────────────────

/// One tick's worth of everything the picture shows. The message loop publishes a
/// fresh `Frame` after each engine step; the view's memos slice it. Held behind an
/// `Rc` so reading it — in ~50 memos, every tick — clones a pointer, not the swarm.
pub(crate) struct Frame {
    arrived: u32,
    queue_len: usize,
    head_wait: f64,
    more: usize,
    busy: usize,
    util_pct: u32,
    /// `Some(parity)` on a frame where core occupancy dipped below its end value and
    /// came back — drives the readout's one-shot dip pulse.
    cpu_dip: Option<bool>,
    offered: f64,
    shed_n: u32,
    gput: f64,
    succ_n: u32,
    rtmo_n: u32,
    ptmo_n: u32,
    inflight: usize,
    last_lat: Option<f64>,
    ready_n: usize,
    io_sleeping: usize,
    syn_pending: usize,
    has_bp: bool,
    queue_timeout_on: bool,
    limit: Option<usize>,
    /// The OS-CPU gate's trailing average, percent — `Some` only under that gate.
    os_cpu_pct: Option<u32>,
    reject: bool,
    queue: bool,
    /// The gate's accumulated rotation, in degrees — the shutter's position, straight
    /// from the machine that owns it. Unbounded, so it never spins the long way round.
    gate_deg: f64,
    /// Which third the shutter covers, and so which exit is open.
    gate_covers: Third,
    dots: Vec<DotVm>,
    rings: Vec<RingVm>,
    slot_rings: Vec<SlotRing>,
    charts: Charts,
}

impl Frame {
    /// The admission limit the running stack reports, if any — the host live's
    /// concurrency dial reads it back to show the controller-driven ceiling.
    pub(crate) fn limit(&self) -> Option<usize> {
        self.limit
    }

    /// The composition before it has run: the layers its stage assembles, and no
    /// traffic. What the server paints, and what the browser holds for the moment
    /// between hydrating and measuring itself.
    pub(crate) fn at_rest(has_bp: bool, queue: bool, timeout: bool, reject: bool, limit: Option<usize>) -> Frame {
        Frame {
            arrived: 0, queue_len: 0, head_wait: 0.0, more: 0, busy: 0, util_pct: 0,
            cpu_dip: None,
            offered: 0.0, shed_n: 0, gput: 0.0, succ_n: 0, rtmo_n: 0, ptmo_n: 0,
            inflight: 0, last_lat: None, ready_n: 0, io_sleeping: 0, syn_pending: 0,
            has_bp,
            queue_timeout_on: timeout,
            limit,
            os_cpu_pct: None,
            reject,
            queue,
            gate_deg: Third::Entrance.deg_of(),
            gate_covers: Third::Entrance,
            dots: Vec::new(),
            rings: Vec::new(),
            slot_rings: vec![SlotRing::default(); CORES],
            charts: Charts::default(),
        }
    }
}

/// The two sparkline series pairs, as SVG `points` strings — rebuilt only on the
/// engine's ~100 ms sample cadence, so between samples the strings compare equal and
/// the memo leaves the polylines untouched.
#[derive(Clone, Default, PartialEq)]
struct Charts {
    offered: String,
    goodput: String,
    inflight: String,
    queue: String,
}

/// A memoized projection of a signal, over the picture's [`Never`] context: derive
/// `g` from `src` as a [`Computed`](idyll::Computed), which bails on `PartialEq` — so
/// the binding only touches the DOM when its value moves.
fn memo<S, T>(
    ctx: &Ctx<Setup, Never>,
    src: &Signal<S>,
    g: impl Fn(&S) -> T + 'static,
) -> Signal<T>
where
    S: Clone + 'static,
    T: Clone + PartialEq + 'static,
{
    let src = src.clone();
    ctx.computed(move |cx| g(&src.get(cx))).read()
}

/// The same, of two sources — what anything positioned by the layout *and* reported
/// from the frame needs (a label's colour is this frame's, its place is this width's).
fn memo2<A, B, T>(
    ctx: &Ctx<Setup, Never>,
    a: &Signal<A>,
    b: &Signal<B>,
    g: impl Fn(&A, &B) -> T + 'static,
) -> Signal<T>
where
    A: Clone + 'static,
    B: Clone + 'static,
    T: Clone + PartialEq + 'static,
{
    let (a, b) = (a.clone(), b.clone());
    ctx.computed(move |cx| g(&a.get(cx), &b.get(cx))).read()
}

// ─── the frame fold: Obs + ViewState → Frame ───────────────────────────────────

pub(crate) struct PaintState {
    /// Chart redraw clock (the engine's ~100 ms virtual sample cadence).
    chart_t: f64,
    /// Per-slot arc machine: which burst holds the lap, and where the lap began.
    slots: Vec<SlotArc>,
    /// Last-built chart strings; reused between sample points so the memos idle.
    charts: Charts,
    /// Frame counter — its parity alternates the gate-pulse animation name so a run of
    /// flipped frames each re-triggers the one-shot.
    frame_n: u64,
}

#[derive(Clone, Copy, Default)]
struct SlotArc {
    burst: Option<(u32, u32)>,
    /// Where the current lap starts; advances one lap per burst, never resets.
    base: f64,
}

impl SlotArc {
    /// Step this slot by one frame against the core task occupying it (if any), and read
    /// off the lens: both edges of the arc as it fills, plus a `pulse` on the frame a new
    /// burst takes over. The base only ever advances — one lap per burst, so a turnover
    /// blinks rather than rewinds — which is what makes this a machine rather than a
    /// per-frame recompute (the same shape as [`Gate`](crate::simview) and `Dot`).
    fn step(&mut self, task: Option<&CpuTask>, parity: bool) -> SlotRing {
        match task {
            Some(c) => {
                let key = (c.id, c.phase as u32);
                let mut pulse = None;
                if self.burst != Some(key) {
                    if self.burst.is_some() {
                        self.base += 360.0;
                        pulse = Some(parity);
                    }
                    self.burst = Some(key);
                }
                let p = (1.0 - c.remaining_ms / c.dur_ms.max(1.0)).clamp(0.0, 1.0);
                SlotRing { a: self.base + 360.0 * ease_in(p), b: self.base + 360.0 * ease_out(p), pulse }
            }
            None => {
                if self.burst.take().is_some() {
                    self.base += 360.0; // the lap the lens closes into
                }
                SlotRing { a: self.base, b: self.base, pulse: None }
            }
        }
    }
}

impl PaintState {
    pub(crate) fn new() -> Self {
        PaintState {
            chart_t: -1.0,
            slots: vec![SlotArc::default(); CORES],
            charts: Charts::default(),
            frame_n: 0,
        }
    }
}

/// One entry in the picture's colour key. A caller that recolours the swarm supplies its own
/// set, because a key that still named stations while the dots wore something else would be the
/// picture lying about itself.
#[derive(Clone, PartialEq)]
pub(crate) struct Chip {
    pub col: Paint,
    pub label: String,
}

fn chip_style(c: &Chip) -> String {
    format!("background:{}", c.col)
}

fn chip_label(c: &Chip) -> String {
    c.label.clone()
}

impl Frame {
    /// Recolour the swarm by the caller's own scheme, keyed by request id. This is how the
    /// blame panel's tenant breakdown trades station colour for tenant identity — a mode the
    /// reader chooses, never the default, so law 1 still governs the picture it left.
    /// `None` leaves a dot the colour its station gave it.
    pub(crate) fn tint(&mut self, of: impl Fn(u32) -> Option<Paint>) {
        for dot in &mut self.dots {
            if let Some(col) = of(dot.key) {
                dot.col = col;
            }
        }
    }
}

pub(crate) fn build_frame(obs: &Obs, vs: &ViewState, ps: &mut PaintState) -> Frame {
    ps.frame_n = ps.frame_n.wrapping_add(1);
    let parity = ps.frame_n % 2 == 0;
    // ── the dot swarm: one dot per live request, coloured by its station (the engine's
    // truth), positioned where the motion layer has walked it (allowed to lag) ──
    let mut dots: Vec<DotVm> = Vec::new();
    // `(segment, x, y)` per dot, parallel to `dots` — the at-rest sheen route, resolved to
    // per-dot phases once every dot is placed (see `assign_sheen_phases`).
    let mut route: Vec<(u8, f64, f64)> = Vec::new();
    let mut rings: Vec<RingVm> = Vec::new();
    let qn = obs.queue_stubs.len();
    // The conveyor's own slots decide visibility: a slot pushed past the tail is off
    // the pipe, so its dot elides and the `+N more` label carries it.
    let vis = queue_xs(obs, vs.layout).iter().take_while(|&&x| x >= QTAIL - 4.0).count();
    let tmo_on = obs.queue_timeout_ms.is_some();
    for &(id, station) in &obs.live {
        let Some(((x, y), path)) = vs.dot_frame(id) else { continue };
        let col = station_col(&station);
        // The app queue is the only station with a radial — and it shows only once the
        // dot has actually reached the queue pipe, so a shed deadline never trails across
        // the cpu box or io chamber on a still-arriving request.
        let r = match station {
            // Both boundary queues abstract away their deep tails — the readout labels
            // carry the true counts. The accept queue's tail is drawn short: a backlog
            // is a number, and a pipe packed end to end with dots reads as a solid bar
            // rather than as a queue with a length. A slot past the drawn pipe is off
            // the picture, the same as one that has left it.
            Station::SynBacklog { idx } if idx >= SYN_DRAWN_CAP => continue,
            Station::RunQueue { idx } if idx >= RUNQ_DRAWN_CAP => continue,
            Station::AppQueue { idx, age: _ } if idx >= vis => continue,
            Station::AppQueue { age, .. } => {
                // The shed deadline runs for the whole queue wait, so the radial rides the
                // dot along its queue-tx approach and in the pipe — but never while still
                // on a core or down in the IO/await zone (where it would read wrong).
                let in_queue_zone = (x < CPU_X || y < CPU_Y) && y < CORRIDOR;
                if tmo_on && in_queue_zone {
                    rings.push(RingVm {
                        key: id,
                        path: path.clone(),
                        parity,
                        ring: Ring {
                            a: 360.0 * ease_in(age),
                            b: 360.0 * ease_out(age),
                            col: if age < 0.65 { AMBER } else { RED },
                        },
                    });
                }
                4.5
            }
            _ => 5.0,
        };
        dots.push(DotVm { key: id, x, y, path, parity, col, r, opacity: 1.0, sheen_phase: 0.0 });
        route.push((sheen_segment(&station), x, y));
    }
    // Dots the engine has finished with, walking out the last leg it gave them — the same key
    // as their live life, so the handoff restyles the node in place.
    for (id, (x, y), path, outcome, opacity) in vs.leaving() {
        dots.push(DotVm { key: id, x, y, path, parity, col: outcome_col(outcome), r: 4.5, opacity, sheen_phase: 0.0 });
        route.push((SEG_EXIT, x, y));
    }
    // Resolve the sheen route to per-dot phases before the DOM sort reorders `dots`.
    for (dot, phase) in dots.iter_mut().zip(sheen_phases(&route)) {
        dot.sheen_phase = phase;
    }
    // Stable order = stable DOM: the keyed `@for` then only restyles surviving nodes.
    dots.sort_unstable_by_key(|d| d.key);
    rings.sort_unstable_by_key(|r| r.key);

    // ── core arcs: persistent per-slot elements, angles only advance ──
    // The lens sampled at each burst's virtual progress: per-tick targets, one
    // 45 ms window from the truth — so pause freezes within a window, and burst
    // end (progress snapping to 1) pinches the arc closed in the same window.
    // A burst that turned over within the frame (one ended, the next began before
    // the snapshot) blinks the ring — the sub-frame churn a saturated CPU would
    // otherwise hide behind an eternally-full arc.
    let mut slot_rings = Vec::with_capacity(CORES);
    for slot in 0..CORES {
        let task = obs.cpu.iter().find(|c| c.slot == Some(slot));
        slot_rings.push(ps.slots[slot].step(task, parity));
    }

    // ── sparkline charts, on the engine's ~100 ms sample cadence ──
    if obs.t - ps.chart_t >= 100.0 || obs.t < ps.chart_t {
        ps.chart_t = obs.t;
        let max_rate = obs
            .spark
            .iter()
            .map(|sp| sp.offered_throughput)
            .fold(0.0_f32, f32::max)
            .max(1.0)
            * 1.15;
        let max_depth = obs
            .spark
            .iter()
            .map(|sp| sp.in_flight.max(sp.queue))
            .fold(0.0_f32, f32::max)
            .max(8.0)
            * 1.1;
        ps.charts = Charts {
            offered: points(obs, 102.0, 96.0, |sp| sp.offered_throughput / max_rate),
            goodput: points(obs, 102.0, 96.0, |sp| sp.goodput / max_rate),
            inflight: points(obs, 64.0, 58.0, |sp| sp.in_flight / max_depth),
            queue: points(obs, 64.0, 58.0, |sp| sp.queue / max_depth),
        };
    }

    // ── scalars (formatting and colour thresholds live in the view's memos) ──
    // The utilisation readout is the end-of-frame truth; a dip that reversed within
    // the frame is replayed as a one-shot pulse, never shown as a number.
    let busy = obs.busy();
    let head_wait = if obs.queue_stubs.is_empty() { 0.0 } else { obs.t - obs.queue_stubs[0].1 };
    let has_queue = obs.layers.queue;
    Frame {
        arrived: obs.stats.arrived,
        queue_len: qn,
        head_wait,
        more: qn.saturating_sub(vis),
        busy,
        util_pct: ((busy as f64 / CORES as f64) * 100.0) as u32,
        cpu_dip: (obs.busy_min < busy).then_some(parity),
        offered: obs.offered_throughput_ps(),
        shed_n: if has_queue { obs.stats.queue_timeout } else { obs.stats.rejected },
        gput: obs.goodput_ps(),
        succ_n: obs.stats.success,
        rtmo_n: obs.stats.response_timeout,
        ptmo_n: obs.stats.processing_timeout,
        inflight: busy + obs.io_sleeping + obs.ready.len(),
        last_lat: obs.last_latency_ms,
        ready_n: obs.ready.len(),
        io_sleeping: obs.io_sleeping,
        syn_pending: obs.syn_backlog.len(),
        has_bp: obs.layers.backpressure,
        queue_timeout_on: obs.queue_timeout_ms.is_some(),
        limit: obs.layers.limit,
        os_cpu_pct: obs.os_cpu_pct,
        reject: obs.layers.reject,
        queue: has_queue,
        gate_deg: vs.gate.deg(),
        gate_covers: vs.gate.covers(),
        dots,
        rings,
        slot_rings,
        charts: ps.charts.clone(),
    }
}

/// An SVG `points` string for one series — samples at fixed x slots (the canvas
/// original's spacing), y = `y0 − k·value` in the chart's local coordinates.
fn points(obs: &Obs, y0: f64, k: f64, series: impl Fn(&SparkPoint) -> f32) -> String {
    obs.spark
        .iter()
        .enumerate()
        .map(|(i, sp)| {
            let x = 4.0 + i as f64 / (SPARK_N - 1) as f64 * (CHART_W - 8.0);
            let y = y0 - (series(sp).min(1.0) as f64) * k;
            format!("{x:.1},{y:.1}")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

// ─── the machine picture component ─────────────────────────────────────────────

/// The 960×520 machine picture and its colour key — the stage as a pure function of
/// the running machine's `frame` and the `layout` it was measured to. `charts_on` is
/// the fence's static choice; `armed` holds the still until the first click; `on_click`
/// releases it (the host live turns a click into its own start message).
#[idyll::component]
pub async fn MachineView(
    ctx: Ctx<Setup, Never>,
    frame: Signal<Rc<Frame>>,
    layout: Signal<Layout>,
    charts_on: bool,
    armed: Signal<bool>,
    on_click: Callback<Event>,
    /// Drawn inside another card: the picture gives up its own elevation, since the surface
    /// belongs to whatever it is a section of. Left off, the stage is the page's own object and
    /// raised, which is what every sim that *is* the page wants.
    #[opt] nested: bool,
    /// A caller's own colour key, replacing the station one while it is non-empty. It travels
    /// with [`Frame::tint`]: a picture recoloured by something other than its stations needs the
    /// key that names *that*, and the two must switch together or the key is a lie.
    #[opt] key: Signal<Vec<Chip>>,
) -> idyll::Result {
    // A second reader for the dot swarm's sheen play-state (the stage class consumes the first).
    let armed_dot = armed.clone();
    // The key a caller supplied, and whether there is one. Empty is the ordinary case — the
    // station key below is the picture's own.
    let key = key.unwrap_or_else(|| ctx.constant(Vec::new()));
    let flat = ctx.constant(nested.unwrap_or(false));
    let own_key = {
        let key = key.clone();
        ctx.computed(move |cx| !key.get(cx).is_empty()).read()
    };

    // What the picture draws is what the *running machine* reports it has — the frame
    // carries the engine's own assembled layers, so the diagram cannot claim a stage
    // the stack isn't running.
    let has_queue = memo(&ctx, &frame, |f| f.queue);
    let has_reject = memo(&ctx, &frame, |f| f.reject);
    // A queue only sheds if something put a clock on it; an unbounded one never does.
    let has_shed = memo(&ctx, &frame, |f| f.queue_timeout_on || f.reject);
    // Which parts the silhouette draws, from the same frame the flags above read, so
    // the picture and the machine can never describe different compositions.
    let part_of =
        memo(&ctx, &frame, |f| Parts { queue: f.queue, shed: f.queue_timeout_on || f.reject });
    let await_sty = memo(&ctx, &layout, await_at);
    let run_queue_sty = memo(&ctx, &layout, run_queue_at);
    let shell = memo2(&ctx, &layout, &part_of, |l, parts| silhouette(l, *parts));
    let cores = memo(&ctx, &layout, core_discs);
    let thirds = memo2(&ctx, &layout, &part_of, |l, parts| fork_thirds(l, parts.queue));
    // The stage is as tall as what it draws: `Layout::height` reserves the reflowed chart
    // band, which only exists when the charts do.
    let box_h = move |l: &Layout| if charts_on { l.height() } else { STAGE_H };
    let svg_sty = memo(&ctx, &layout, move |l| {
        format!("left:0px;top:0px;width:{:.1}px;height:{:.1}px;overflow:visible", l.w, box_h(l))
    });
    let stage_sty = memo(&ctx, &layout, move |l| format!("height:{}px", box_h(l)));
    // Whether an exit's reading has to break onto a second line. Each row reads the
    // same condition twice — once to drop the middle dot, once to start the new line —
    // so each site takes its own handle on the one memo.
    let stacked = memo(&ctx, &layout, |l| l.stats_stacked());
    let wide = memo(&ctx, &layout, |l| !l.stats_stacked());
    let (tmo_sep, tmo_tail) = (stacked.clone(), stacked.clone());
    let (succ_sep, succ_tail) = (stacked.clone(), stacked.clone());
    // The one reading with three segments: stacked, each takes its own line, or the
    // last runs off the plumbing it is meant to sit over.
    let (rtmo_sep, rtmo_tail) = (stacked.clone(), stacked.clone());
    let proc_tail = stacked.clone();
    let (flight_sep, flight_tail) = (stacked.clone(), stacked);

    // Readouts, styles and the swarm — each a memo over the frame, so it rewrites the
    // DOM only when its own value moves.
    // One reading, not a name and a number that happen to sit beside each other.
    let arrived = memo(&ctx, &frame, |f| format!("{} arrived", slots(f.arrived, 4)));
    let queue_n = memo(&ctx, &frame, |f| f.queue_len.to_string());
    let queue_line_sty = memo2(&ctx, &frame, &layout, |f, l| {
        lab_bold(l, QTAIL + 18.0, QTX_TOP_Y - 20.0, 13, if f.head_wait > 200.0 { RED } else { AMBER })
    });
    let head_wait = memo(&ctx, &frame, |f| format!("head wait {} ms", slots(format!("{:.0}", f.head_wait), 4)));
    // The queue's own gauge already says how long the head has waited by where its dot
    // stands on the conveyor, so on a narrow stage this reading gives up its space.
    let head_wait_sty = memo2(&ctx, &frame, &layout, |f, l| {
        let base = lab(l, QTAIL + 24.0, QTX_TOP_Y + 30.0, 11, if f.head_wait > 900.0 { RED } else { MUTED });
        if l.narrow() { format!("{base};display:none") } else { base }
    });
    let more = memo(&ctx, &frame, |f| if f.more > 0 { format!("… +{} more", slots(f.more, 3)) } else { String::new() });
    // The elided tail's count. On a narrow stage the gate's own readings have wrapped
    // into this space and the queue's total is already stated above the pipe, so this
    // one gives up its room rather than sit on top of them.
    let more_sty = memo(&ctx, &layout, |l| {
        let base = lab(l, QTAIL + 24.0, QY - 24.0, 11, MUTED);
        match l.narrow() {
            true => format!("{base};display:none"),
            false => base,
        }
    });
    // Stacked upward on a narrow stage: the reading's last line holds the y it had, so
    // the block grows into the space above rather than onto the pipe below.
    let arrived_sty = memo(&ctx, &layout, |l| {
        let y = SYN_Y - 20.0 - if l.narrow() { 12.0 } else { 0.0 };
        lab(l, 26.0, y, 11, MUTED)
    });
    let narrow = memo(&ctx, &layout, |l| l.narrow());
    let (syn_sep, syn_tail) = (narrow.clone(), narrow);
    let syn_pending_sty = memo(&ctx, &layout, |l| lab_bold(l, 26.0, SYN_Y + 24.0, 12, TEAL));
    let io_sleeping_sty = memo(&ctx, &layout, |l| lab(l, IOC_X, IOC_Y + IOC_H + 16.0, 11, PURPLE));
    let machine_sty = memo(&ctx, &layout, |l| {
        format!(
            "left:{:.1}px;top:0px;width:{:.1}px;height:{STAGE_H}px",
            l.x(CPU_X),
            l.x(CPU_X + CPU_W) - l.x(CPU_X),
        )
    });
    // Whether the readings have been pulled in over the machine, and so need a surface. One
    // per row, since each wears it.
    let stacked = memo(&ctx, &layout, |l| l.stats_stacked());
    let (stacked_tmo, stacked_succ, stacked_proc, stacked_flight) =
        (stacked.clone(), stacked.clone(), stacked.clone(), stacked);
    let row_tmo = memo(&ctx, &layout, |l| row_c(l, 0));
    let row_succ = memo(&ctx, &layout, |l| row_c(l, 1));
    let row_proc = memo(&ctx, &layout, |l| row_c(l, 2));
    // The one row that reports rather than labels an exit: it sits below the last pipe
    // instead of above it, which is its own slot.
    let row_flight = memo(&ctx, &layout, |l| row_c(l, ROW_IN_FLIGHT));
    let cpu_lbl = memo(&ctx, &frame, |f| format!("CPU · busy {}/{CORES} ({}%)", f.busy, slots(f.util_pct, 3)));
    let cpu_lbl_sty = memo2(&ctx, &frame, &layout, |f, l| {
        let col = if f.util_pct > 100 { RED } else if f.util_pct >= 50 { AMBER } else { BLUE };
        let base = lab_bold(l, CPU_X + 78.0, CPU_Y - 8.0, 12, col);
        match f.cpu_dip {
            Some(parity) => format!(
                "{base};animation:{} 240ms ease-in-out",
                if parity { cstyles::CpuDipA } else { cstyles::CpuDipB }
            ),
            None => base,
        }
    });
    let offered = memo(&ctx, &frame, |f| format!("{}/s", slots(format!("{:.1}", f.offered), 5)));
    let shed_n = memo(&ctx, &frame, |f| slots(f.shed_n, 4));
    let gput = memo(&ctx, &frame, |f| format!("{}/s", slots(format!("{:.1}", f.gput), 5)));
    let succ_n = memo(&ctx, &frame, |f| slots(f.succ_n, 4));
    let rtmo_n = memo(&ctx, &frame, |f| slots(f.rtmo_n, 4));
    let ptmo_n = memo(&ctx, &frame, |f| slots(f.ptmo_n, 4));
    let inflight = memo(&ctx, &frame, |f| slots(f.inflight, 4));
    let last_lat = memo(&ctx, &frame, |f| match f.last_lat {
        Some(l) => format!("{} ms", slots(format!("{l:.0}"), 4)),
        None => format!("{} ms", slots("—", 4)),
    });
    let last_lat_sty = memo(&ctx, &frame, |f| match f.last_lat {
        Some(l) => format!("color:{}", if l > 2000.0 { RED } else if l > 700.0 { AMBER } else { MUTED }),
        None => format!("color:{MUTED}"),
    });
    let ready_n = memo(&ctx, &frame, |f| format!("{} ready", slots(f.ready_n, 4)));
    let ready_n_sty = memo2(&ctx, &frame, &layout, |f, l| {
        let col = if f.ready_n == 0 { GREEN } else if f.ready_n < 10 { ORANGE } else { RED };
        lab_r(l, PIPE_RET - 18.0, 320.0, 11, col)
    });
    let io_sleeping = memo(&ctx, &frame, |f| format!("{} sleeping", slots(f.io_sleeping, 4)));
    let syn_pending = memo(&ctx, &frame, |f| format!("{} pending", slots(f.syn_pending, 4)));

    // The shutter covers the exit that is shut. Resting over the *entrance* is the
    // idle pose — nothing is being admitted or shed, so neither exit is claimed.
    // No fork, no shutter — an empty path draws nothing, which is what a composition
    // with nothing to hold a request at should show.
    let shutter_d = memo2(&ctx, &frame, &layout, |f, l| match f.queue {
        false => String::new(),
        true => {
            let (cx, cy) = (l.x(QFORK_X), FORK_Y);
            crate::simview::arc_path(cx, cy, crate::simview::fork_r(l), 240.0, 360.0)
        }
    });
    // The shutter wears the gate's own rotation — no threshold, no smoothing, nothing
    // between the machine and the picture.
    let shutter_sty = memo2(&ctx, &frame, &layout, |f, l| shutter(l, f.gate_deg));

    let gate_adm_lbl = memo(&ctx, &frame, |f| match (f.os_cpu_pct, f.limit) {
        // The delayed signal, shown live beside its threshold — watching this number
        // trail the instantaneous CPU readout is the OS-CPU tab's whole lesson.
        (Some(pct), _) => format!("cpu avg {}% (<{:.0}%)", slots(pct, 3), crate::engine::OS_CPU_MAX * 100.0),
        (None, Some(n)) => format!("limit {}", slots(n, 3)),
        (None, None) => "< 50%".to_string(),
    });
    // Each gate reading washes to full only while its own exit is the open one, and
    // fades back otherwise — so the two labels and the shutter always say the same
    // thing, and the reader's eye is drawn to whichever exit traffic is taking.
    // Admission is open exactly when the shutter covers the timeout third — the same
    // fact the shutter is showing, said in words, so the two can never disagree.
    let gate_adm_lbl_sty = memo2(&ctx, &frame, &layout, |f, l| {
        let open = f.gate_covers == Third::Timeout;
        format!(
            "{};{}",
            lab_gate(l, QFORK_X - 6.0, 220.0, 10, if open { GREEN } else { RED }),
            wash(open)
        )
    });
    // These readings hang off the fork, so they exist only where one does — which is
    // where there is a queue to hold a request at.
    let gate_tmo_lbl = memo(&ctx, &frame, |f| {
        match f.queue_timeout_on {
            true => "queue timeout".to_string(),
            false => "timeout off".to_string(),
        }
    });
    let gate_tmo_lbl_sty = memo2(&ctx, &frame, &layout, |f, l| {
        let open = f.gate_covers == Third::Admit;
        let col = if open { ORANGE } else { MUTED };
        // The timeout reading is the wordy one, so it is the one that wraps. It grows
        // *upward* — the last line keeps the y that put it beside the gate.
        let (y, wrap) = if l.narrow() { (168.0 - 11.0, gate_label_wrap()) } else { (168.0, String::new()) };
        format!("{};{}{}", lab_gate(l, QFORK_X - 6.0, y, 10, col), wash(open), wrap)
    });
    let has_bp = memo(&ctx, &frame, |f| f.has_bp);
    let charts_at = memo(&ctx, &layout, charts_at);
    let line_offered = memo(&ctx, &frame, |f| f.charts.offered.clone());
    let line_goodput = memo(&ctx, &frame, |f| f.charts.goodput.clone());
    let line_inflight = memo(&ctx, &frame, |f| f.charts.inflight.clone());
    let line_queue = memo(&ctx, &frame, |f| f.charts.queue.clone());
    let dots = memo(&ctx, &frame, |f| f.dots.clone());
    let rings = memo(&ctx, &frame, |f| f.rings.clone());
    // Each core: the arc it is sweeping this frame, and where the machine's current
    // width puts it. Two memos, so a resize restyles the box and a burst the arc.
    let cores_vm: Vec<(Signal<SlotRing>, Signal<String>)> = (0..CORES)
        .map(|i| {
            (
                memo(&ctx, &frame, move |f| f.slot_rings[i]),
                memo(&ctx, &layout, move |l| core_at(l, i)),
            )
        })
        .collect();

    // The queue reads differently when there is no queue: a request WAITING for
    // admission is not queued, and what sheds it is a rejection, not a deadline.
    let shed_row_lbl = memo(&ctx, &has_queue, |&q| if q { " · queue timeout " } else { " · rejected (HTTP 529) " }.to_string());
    let shed_n_sty = memo(&ctx, &has_queue, |&q| format!("color:{}", if q { ORANGE } else { RED }));
    // The same label without its leading separator — the second line of a stacked
    // reading, where the dot it replaces is gone.
    let shed_row_tail = memo(&ctx, &has_queue, |&q| {
        if q { "queue timeout " } else { "rejected (HTTP 529) " }.to_string()
    });
    let waiting_not_queued = memo(&ctx, &has_queue, |&q| !q);

    Ok(ctx.render(live_view! {
        // ── the 960×520 stage. Armed until the first click releases the still ──
        div css=[cstyles::STAGE, $flat => cstyles::STAGE_NESTED, $armed => cstyles::ARMED] style=($stage_sty)
            onclick=(on_click) {
            // ── the machine's silhouette: every wall in one pass, every channel
            //    carved out of it in the next, so each mouth opens by construction ──
            svg style=($svg_sty) {
                @for s in $shell [key = s.i] {
                    path d=(shape_d(&$s)) style=(shape_style(&$s)) {}
                }
                @for c in $cores [key = c.i] {
                    path d=(shape_d(&$c)) style=(shape_style(&$c)) {}
                }
                @for th in $thirds [key = th.i] {
                    path d=(shape_d(&$th)) style=(shape_style(&$th)) {}
                }
                path d=($shutter_d) style=($shutter_sty) {}
            }

            // ── the labels that name the machine ──
            div css=[cstyles::LABEL] style=($await_sty) { "await" }
            div css=[cstyles::LABEL] style=($run_queue_sty) { "run queue" }

            @if ($has_queue) {
                div css=[cstyles::LABEL] style=($queue_line_sty) {
                    "QUEUE · "
                    span css=[cstyles::PLOTTED] style=(format!("color:{AMBER}")) { $queue_n }
                    " · unbounded"
                }
                div css=[cstyles::LABEL] style=($head_wait_sty) { $head_wait }
                div css=[cstyles::LABEL] style=($more_sty) { $more }
            }

            // The gate's two readings sit beside the fork, so they exist only where
            // a fork does. With no queue there is nothing holding a request and
            // nothing to label — the verdict is the colour the dot leaves in.
            @if ($has_queue) {
                div css=[cstyles::LABEL] style=($gate_tmo_lbl_sty) { $gate_tmo_lbl }
                div css=[cstyles::LABEL] style=($gate_adm_lbl_sty) { $gate_adm_lbl }
            }

            div css=[cstyles::LABEL] style=($cpu_lbl_sty) { $cpu_lbl }

            // stat rows over the exit pipes — each centred on the plumbing it reports on
            @if ($has_shed) {
                div css=[cstyles::LABEL, $stacked_tmo => cstyles::STAT_SCRIM] style=($row_tmo) {
                    "offered "
                    span css=[cstyles::PLOTTED] style=(format!("color:{ORANGE}")) { $offered }
                    span css=[$tmo_sep => cstyles::HIDDEN] { $shed_row_lbl }
                    span css=[$tmo_tail => cstyles::STAT_TAIL] {
                        span css=[$wide => cstyles::HIDDEN] { $shed_row_tail }
                        span style=($shed_n_sty) { $shed_n }
                    }
                }
            }
            div css=[cstyles::LABEL, $stacked_succ => cstyles::STAT_SCRIM] style=($row_succ) {
                "goodput "
                span css=[cstyles::PLOTTED] style=(format!("color:{GREEN}")) { $gput }
                span css=[$succ_sep => cstyles::HIDDEN] { " · " }
                span css=[$succ_tail => cstyles::STAT_TAIL] {
                    "success "
                    span style=(format!("color:{GREEN}")) { $succ_n }
                }
                span css=[$rtmo_sep => cstyles::HIDDEN] { " · " }
                span css=[$rtmo_tail => cstyles::STAT_TAIL] {
                    "response timeout "
                    span style=(format!("color:{RED}")) { $rtmo_n }
                }
            }
            @if ($has_queue) {
                div css=[cstyles::LABEL, $stacked_proc => cstyles::STAT_SCRIM] style=($row_proc) {
                    "processing "
                    span css=[$proc_tail => cstyles::STAT_TAIL] {
                        "timeout "
                        span style=(format!("color:{SALMON}")) { $ptmo_n }
                    }
                }
            }
            div css=[cstyles::LABEL, $stacked_flight => cstyles::STAT_SCRIM] style=($row_flight) {
                "in-flight "
                span css=[cstyles::PLOTTED] style=(format!("color:{BLUE}")) { $inflight }
                span css=[$flight_sep => cstyles::HIDDEN] { " · " }
                span css=[$flight_tail => cstyles::STAT_TAIL] {
                    "last latency "
                    span style=($last_lat_sty) { $last_lat }
                }
            }

            div css=[cstyles::LABEL] style=($ready_n_sty) { $ready_n }
            div css=[cstyles::LABEL] style=($io_sleeping_sty) { $io_sleeping }

            // ── the machine: the cores' burst rings, in the machine's own box ──
            div css=[cstyles::MACHINE] style=($machine_sty) {
                @for (ring, at) in (cores_vm) {
                    svg css=[cstyles::CORE_RING] style=($at) {
                        path d=(core_arc(&$ring)) style=(core_arc_style(&$ring)) {}
                    }
                }
            }

            div css=[cstyles::LABEL] style=($arrived_sty) {
                "TCP SYN"
                span css=[$syn_sep => cstyles::HIDDEN] { " · " }
                span css=[$syn_tail => cstyles::STAT_TAIL] { $arrived }
            }
            div css=[cstyles::LABEL] style=($syn_pending_sty) { $syn_pending }

            // ── the dot swarm ──
            @for d in $dots [key = d.key] {
                div css=[cstyles::DOT] style=(dot_style(&$d, $armed_dot)) {}
            }
            @for r in $rings [key = r.key] {
                div css=[cstyles::RING] style=(ring_style(&$r)) {}
            }

            // ── charts: real stroked polylines, like the prototype (a sim can hide
            // them for a simplified picture) ──
            @if (charts_on) {
                div css=[cstyles::CHARTS] style=($charts_at) {
                    svg css=[cstyles::CHART] viewBox=("0 0 318 108") height=("108")
                        preserveAspectRatio=("none") {
                        polyline css=[cstyles::LINE, cstyles::LINE_OFFERED] points=($line_offered) {}
                        polyline css=[cstyles::LINE, cstyles::LINE_GOODPUT] points=($line_goodput) {}
                    }
                    div css=[cstyles::LEGROW] {
                        span { span css=[cstyles::CHIP] style=(format!("background:{ORANGE}")) {} "offered throughput" }
                        span { span css=[cstyles::CHIP] style=(format!("background:{GREEN}")) {} "goodput" }
                        span css=[cstyles::UNITS] { "req/s, 5 s" }
                    }
                    svg css=[cstyles::CHART] viewBox=("0 0 318 68") height=("68")
                        preserveAspectRatio=("none") {
                        polyline css=[cstyles::LINE, cstyles::LINE_INFLIGHT] points=($line_inflight) {}
                        polyline css=[cstyles::LINE, cstyles::LINE_QUEUE] points=($line_queue) {}
                    }
                    div css=[cstyles::LEGROW] {
                        span { span css=[cstyles::CHIP] style=(format!("background:{BLUE}")) {} "in-flight" }
                        span { span css=[cstyles::CHIP] style=(format!("background:{AMBER}")) {} "queue depth" }
                    }
                }
            }

        }

            // ── the key to the dot colours: a caption under the picture, not a band
        //    taken out of it ──
        div css=[cstyles::LEGEND] {
          @if ($own_key) {
            @for c in $key [key = c.label.clone()] {
                span { span css=[cstyles::CHIP] style=(chip_style(&$c)) {} (chip_label(&$c)) }
            }
          } else {
            span { span css=[cstyles::CHIP] style=(format!("background:{TEAL}")) {} "tcp syn" }
            @if ($has_queue) {
                span { span css=[cstyles::CHIP] style=(format!("background:{GREY}")) {} "queued" }
            }
            @if ($waiting_not_queued && $has_bp) {
                span { span css=[cstyles::CHIP] style=(format!("background:{GREY}")) {} "waiting" }
            }
            span { span css=[cstyles::CHIP] style=(format!("background:{AMBER}")) {} "ready" }
            span { span css=[cstyles::CHIP] style=(format!("background:{BLUE}")) {} "on cpu" }
            span { span css=[cstyles::CHIP] style=(format!("background:{PURPLE}")) {} "in io" }
            span { span css=[cstyles::CHIP] style=(format!("background:{GREEN}")) {} "success" }
            @if ($has_queue) {
                span { span css=[cstyles::CHIP] style=(format!("background:{ORANGE}")) {} "queue timeout" }
                span { span css=[cstyles::CHIP] style=(format!("background:{SALMON}")) {} "proc timeout" }
            }
            @if ($has_reject) {
                span { span css=[cstyles::CHIP] style=(format!("background:{RED}")) {} "rejected (HTTP 529)" }
            }
          }
        }
    }).await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sheen sweep is one continuous route through the machine: segments run in order and,
    /// within a segment, dots are phased by that segment's own flow direction.
    #[test]
    fn sheen_route_flows_through_the_machine_in_order() {
        // Two dots in each of three segments, placed against their flow directions.
        let route = vec![
            (SEG_SYN, 100.0, 50.0),  // arrive: right one → later in its segment
            (SEG_SYN, 20.0, 50.0),   // arrive: left one → earlier
            (SEG_CPU, 400.0, 30.0),  // CPU: top (small y) → later (key is -y)
            (SEG_CPU, 400.0, 200.0), // CPU: bottom → earlier
            (SEG_QUEUE, 30.0, 90.0), // queue: left → later (key is -x)
            (SEG_QUEUE, 300.0, 90.0),// queue: right → earlier
        ];
        let p = sheen_phases(&route);

        // Segment order holds: every SYN phase precedes every CPU phase precedes every QUEUE.
        let syn = p[0].max(p[1]);
        let cpu = p[2].min(p[3]);
        let queue = p[4].min(p[5]);
        assert!(syn < cpu, "SYN sweeps before CPU: {syn} vs {cpu}");
        assert!(cpu < queue, "CPU sweeps before the app queue: {cpu} vs {queue}");

        // Within SYN the left dot leads the right; within CPU the bottom leads the top; within
        // the queue the right leads the left — each its segment's flow direction.
        assert!(p[1] < p[0], "SYN flows left → right");
        assert!(p[3] < p[2], "CPU flows bottom → top");
        assert!(p[5] < p[4], "app queue flows right → left");

        // Phases stay in [0,1] — a valid negative-delay offset for the shared period (the
        // last dot reaches 1.0, delay 0, which closes the loop against the first at 0.0).
        assert!(p.iter().all(|&v| (0.0..=1.0).contains(&v)), "phases in [0,1]: {p:?}");
    }

    /// A lone dot in a segment sits mid-segment (no rank to spread), and the IO circuit splits
    /// its down-leg from its up-leg so the sweep runs down one side and up the other.
    #[test]
    fn sheen_io_circuit_runs_down_then_up() {
        // IO dots: two on the right (down/await leg) and two on the left (up/return leg).
        let route = vec![
            (SEG_IO, 500.0, 40.0),  // right leg, top
            (SEG_IO, 500.0, 300.0), // right leg, bottom
            (SEG_IO, 300.0, 300.0), // left leg, bottom
            (SEG_IO, 300.0, 40.0),  // left leg, top
        ];
        let p = sheen_phases(&route);
        // Down-leg (right) sweeps top → bottom, entirely before the up-leg (left) sweeps
        // bottom → top.
        assert!(p[0] < p[1], "down-leg: top before bottom");
        assert!(p[1] < p[2], "down-leg finishes before the up-leg begins");
        assert!(p[2] < p[3], "up-leg: bottom before top");
    }
}
