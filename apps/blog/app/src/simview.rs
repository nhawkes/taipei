//! LiveView state for the queue visualiser — **snapshot-derived motion that always travels
//! the pipes**. Each frame the engine hands over `obs.live` (every request and its
//! [`Station`]); this layer moves a dot per request along the drawn topology toward that
//! station. Deterministic legs (the network glides, an IO circuit, the queue's deadline
//! conveyor) are *placed* by the engine's progress and dilate with the sim-speed knob;
//! the un-modelled hops *between* stations (reaching a worker, the core, admission) are
//! *pursued* — at a base rate near the target, faster the further behind
//! ([`CATCH_UP_MS`]) — paced by virtual time within a clamped band so a hop tracks the
//! speed knob without ever crawling (trailing the engine) or blinking.
//!
//! The dot **commits to each leg and walks it to the end before adopting the next
//! station** — arrive-before-you-leave — so every structural waypoint is actually
//! traversed, never cut across. Position may lag the engine's timing; colour (keyed off
//! the station) is the instant truth and the dot catches up.
//!
//! Every dot records the exact polyline it walked this rendered frame — start, every
//! vertex crossed — as its [`Dot::frame_path`]. The paint layer emits it verbatim as
//! the frame's `offset-path`, so even *interpolated* positions lie on the walked
//! route: no corner is ever cut.
//!
//! The two waits at the worker boundary are **separate queues**, as they are in the
//! kernel and in tokio: the SYN pipe holds the kernel accept queue (teal — FIFO, nothing
//! can be inserted ahead of you, so motion is forward-only by construction), and the
//! return-pipe column holds the tokio run queue (amber — spawned handlers and IO wakes,
//! stacking downward and compressing under load). They share only the worker pool.
//!
//! Entrance and exit are the first and last legs of the same walk, and the same *kind* of
//! leg: a request arrives as [`Station::NetworkIn`] (network io, placed down the SYN pipe)
//! and leaves as [`Station::NetworkOut`] (network io, placed out its verdict's pipe — a
//! shed request *into the queue, then back out*). It is live for both, so its dot is an
//! ordinary dot the whole journey and despawns once the reply is off the stage. The one
//! exception is the request nobody is waiting for any more: it has no leg home, so it
//! drops where it stood and fades.

use crate::engine::{Exit, Hop, Obs, Outcome, Reply, Rng, Station};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt::Write as _;

// ─── geometry constants (the 960×520 stage) ───────────────────────────────────
pub const STAGE_W: f64 = 960.0;
pub const STAGE_H: f64 = 520.0;
pub const QY: f64 = 190.0;
/// The fork: where the queue meets the CPU's left wall. The gate stands here, so the
/// queue's head waits *at the wall* and what happens next is admission or shedding —
/// there is no third place for it to be.
pub const QFORK_X: f64 = CPU_X;
pub const QTAIL: f64 = 140.0;
/// The lane inside the CPU box an admitted request rides once through the gate — below
/// the cores, so it crosses the box without threading between them.
pub const CPU_ADM_Y: f64 = 254.0;
pub const CPU_X: f64 = 380.0;
pub const CPU_Y: f64 = 96.0;
pub const CPU_W: f64 = 200.0;
pub const CPU_H: f64 = 188.0;
pub const TMOY: f64 = 143.0;       // queue timeout (top)
pub const SUCCY: f64 = 190.0;      // success      (middle)
pub const PROC_TMO_Y: f64 = 237.0; // proc timeout (bottom)
pub const PIPE_IO: f64 = 540.0;
pub const PIPE_RET: f64 = 420.0;
pub const IOC_X: f64 = 380.0;
pub const IOC_Y: f64 = 340.0;
pub const IOC_W: f64 = 200.0;
pub const IOC_H: f64 = 140.0;
pub const CORRIDOR: f64 = 258.0;
/// The lane inside the CPU box every outbound request turns down before it leaves —
/// clear of the cores, short of the box's right wall, so a reply crosses the box on
/// one line rather than cutting from wherever it happened to be.
pub const TURN_OUT: f64 = 560.0;
/// The stage-width breakpoint below which labels wrap, the gate shrinks, the queue's leg
/// pulls in, and the least load-bearing readings drop — a *width*, unrelated to the
/// `TURN_OUT` x-lane that happens to share the value.
pub const NARROW_W: f64 = 560.0;
pub const SYN_Y: f64 = IOC_Y + IOC_H * 0.5;   // ~410, centre of IO chamber left wall
/// The queue-tx pipe: out of the CPU box through the opening in its top wall at
/// `QTX_X`, up to the top run at `QTX_TOP_Y`, left, and down into the queue tail.
/// Clear of the box's rounded top-left corner — a riser any further left carves a
/// notch out of it when the channel pass cuts the pipe's interior.
pub const QTX_X: f64 = 430.0;
pub const QTX_TOP_Y: f64 = 56.0;

// ── pursuit pacing ─────────────────────────────────────────────────────────────
// A station's *dwell* is a modelled duration — the engine knows how long it lasts, so
// the dot is `place`d by the engine's progress and dilates with the sim-speed knob (a
// 6 ms leg at 0.01× fills 600 real ms). The *hop between* stations has no modelled
// duration: it is the un-modelled travel the dot does to reach the next station's
// anchor. It is paced by virtual time so it tracks the speed knob — slow the sim and
// the hops slow too, which is what a reader watching frame-by-frame wants — but the
// per-frame virtual advance it consumes is **clamped to a band** ([`PURSUE_VIRT_FLOOR`],
// [`PURSUE_VIRT_CEIL`]): at a crawl the floor keeps hops moving instead of trailing the
// engine, and at full speed the ceiling keeps them a visible sweep instead of a blink.

/// Base pursuit rate, in stage-px per virtual ms — the speed of a dot near its target.
const RATE: f64 = 7.5;
/// Catch-up half-life, in virtual ms: a lagging dot closes its remaining distance on
/// this timescale (`speed = max(RATE, remaining / CATCH_UP_MS)`), so pursuit lag is
/// bounded in time however long the route — the longest route catches up as fast as a
/// short hop falls behind. One law, no per-leg pacing constants.
const CATCH_UP_MS: f64 = 15.0;
/// Ceiling on catch-up speed, stage-px per virtual ms: however far behind, the dot
/// still *moves* rather than blinks — a bounded fast sweep reads as continuous travel.
const RATE_MAX: f64 = 36.0;
/// The band the per-frame virtual advance is clamped to before it paces pursuit. The
/// floor stops hops crawling when the sim is slow (the reader still sees them move); the
/// ceiling stops them blinking when it is fast. Between the two, pursuit tracks the
/// speed knob. A frame that didn't advance the sim at all is exempt — it moves nothing.
///
/// The floor is tuned so a hop's real-world speed at **0.01×** matches a placed network
/// glide there (~0.44 stage-px/real-ms, measured): a placed leg scales linearly with the
/// knob while the floored hop is constant, so the two meet at one speed — put it at the
/// slow viewing speed, and the dot moves at one roughly-constant pace across the whole
/// journey (glide in → hop to the core → glide out).
const PURSUE_VIRT_FLOOR: f64 = 2.2;
const PURSUE_VIRT_CEIL: f64 = 7.0;

// ── the gate ───────────────────────────────────────────────────────────────────
// A discrete machine, stepped once per engine frame, and the single truth about which
// exit is open. The picture only interpolates it, and is allowed to lag.
//
// Each frame the gate covers exactly one of three thirds: the ENTRANCE (the closed
// resting state — nothing to admit), the TIMEOUT third (which leaves the ADMIT exit
// open) or the ADMIT third (which leaves the TIMEOUT exit open). It moves at most one
// third per frame.
//
// The head may cross only when both conditions hold: the entrance is clear AND the gate
// sits over the third that leaves *its* exit open. Each unmet condition costs a whole
// frame — gate still on the entrance, the head waits a frame OUTSIDE; gate open but on
// the wrong third, it waits a frame INSIDE the circle. Its deadline runs the whole time,
// which is what a reader sees when a held request sheds.
//
// Which exit the head is bound for comes from the engine and only from the engine: this
// layer never predicts a verdict, so the gate can say nothing the stack has not already
// done. That is the lag — state is instant truth, position trails it.

/// Which third the grey shutter covers this frame.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Third {
    /// The resting pose: nothing is being admitted or shed, so neither exit is claimed.
    Entrance,
    /// Covering the timeout exit ⇒ **admission** is the open one.
    Timeout,
    /// Covering the admit exit ⇒ the **timeout** is the open one.
    Admit,
}

impl Third {
    /// Degrees clockwise from the resting pose — the rotation the shutter wears.
    pub fn deg_of(self) -> f64 {
        match self {
            Third::Entrance => -120.0,
            Third::Timeout => 0.0,
            Third::Admit => 120.0,
        }
    }
}

/// What the engine decided about the head, and so which third has to be *covered* for
/// it to leave: an admitted request needs the timeout third covered, a shed one needs
/// the admit third covered.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Fate {
    Admit,
    Shed,
}

impl Fate {
    fn serving(self) -> Third {
        match self {
            Fate::Admit => Third::Timeout,
            Fate::Shed => Third::Admit,
        }
    }
}

/// Where the head stands in the handshake.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Phase {
    /// Outside the circle, waiting for the entrance to clear.
    Before,
    /// Inside the circle, waiting for its own exit to open.
    Inside,
}

/// The gate as the engine's frames leave it: which third is covered, the accumulated
/// rotation that got there (unbounded, so the shutter never spins the long way), and
/// the head it is working on.
pub struct Gate {
    covers: Third,
    deg: f64,
    head: Option<(u32, Fate, Phase)>,
}

impl Gate {
    fn new() -> Gate {
        Gate { covers: Third::Entrance, deg: Third::Entrance.deg_of(), head: None }
    }

    /// Move the shutter one third toward `target`, the short way round.
    fn step(&mut self, target: Third) {
        if self.covers == target {
            return;
        }
        let delta = (target.deg_of() - self.covers.deg_of() + 540.0).rem_euclid(360.0) - 180.0;
        self.deg += if delta > 0.0 { 120.0 } else { -120.0 };
        self.covers = match self.deg.rem_euclid(360.0) {
            a if a < 60.0 || a > 300.0 => Third::Timeout,
            a if a < 180.0 => Third::Admit,
            _ => Third::Entrance,
        };
    }

    /// One frame of the handshake, read against the gate's position **before** it steps.
    /// Returns the id to release, if this is the frame its exit came open.
    fn frame(&mut self, fated: Option<(u32, Fate)>) -> Option<u32> {
        // A new head (or none) replaces whatever the last one was doing.
        match (fated, self.head) {
            (Some((id, fate)), Some((held, _, phase))) if held == id => {
                // The same request, possibly with a changed fate: a request held at a
                // shut gate can burn its deadline and turn from admit to shed.
                self.head = Some((id, fate, phase));
            }
            (Some((id, fate)), _) => self.head = Some((id, fate, Phase::Before)),
            (None, _) => self.head = None,
        }
        // What this frame is serving, read *before* the handshake: a request crossing
        // does not free the gate until the next frame, so the third it went through
        // stays open for the frame it went through it. Idle — no fated head — rests
        // the shutter over the entrance.
        let serving = self.head.map_or(Third::Entrance, |(_, fate, _)| fate.serving());
        let mut release = None;
        if let Some((id, fate, phase)) = self.head {
            match phase {
                // The entrance is clear — step inside and spend the frame there.
                Phase::Before if self.covers != Third::Entrance => {
                    self.head = Some((id, fate, Phase::Inside));
                }
                // Inside, and this is the frame its exit came open.
                Phase::Inside if self.covers == fate.serving() => {
                    release = Some(id);
                    self.head = None;
                }
                _ => {}
            }
        }
        self.step(serving);
        release
    }

    /// Whether the head is standing inside the circle this frame — where its dot goes.
    fn head_inside(&self) -> Option<u32> {
        self.head.and_then(|(id, _, phase)| (phase == Phase::Inside).then_some(id))
    }

    /// The shutter's rotation, in degrees. Unbounded — it accumulates, so a step is
    /// always the short way round and the shutter never unwinds a full turn.
    pub fn deg(&self) -> f64 {
        self.deg
    }

    /// Which third is covered, and so which exit is open.
    pub fn covers(&self) -> Third {
        self.covers
    }
}

pub fn slot_pos(i: usize) -> (f64, f64) {
    (412.0 + (i % 4) as f64 * 45.0, 150.0 + (i / 4) as f64 * 80.0)
}

// ─── reflow ────────────────────────────────────────────────────────────────────

/// The x-range the machine itself occupies — the CPU box and the IO chamber below it.
/// Everything outside it is plumbing: pipes whose *length* carries no meaning.
const CORE_L: f64 = CPU_X;
const CORE_R: f64 = CPU_X + CPU_W;
/// The machine holds its size until the stage is narrower than [`CORE_HOLD`], then
/// compresses to no less than [`CORE_MIN`] of it — the last give available once the
/// plumbing has none left.
const CORE_MIN: f64 = 0.82;
const CORE_HOLD: f64 = 460.0;
const CORE_FLOOR: f64 = 348.0;
/// The sparkline column: the width its charts are *drawn* to, and where it starts down
/// the stage. The drawing stretches sideways to whatever width the column has; its
/// height is the drawn one at every width.
pub const CHART_W: f64 = 318.0;
pub const CHART_Y: f64 = 278.0;
/// The sparklines' drawn heights at their design width, and the height of the key
/// under each — the one part of the band the browser owns, so the stage is told it
/// rather than measuring it.
const CHART_HS: f64 = 108.0 + 68.0;
const KEY_H: f64 = 30.0;

/// How the stage maps its 960-wide drawing onto the width it was actually given.
///
/// The machine keeps its proportions and the **plumbing absorbs the difference**: a
/// narrow stage shows the same CPU, the same eight cores and the same chamber, reached
/// by shorter pipes. That is the honest compression, because a pipe's drawn length was
/// never the quantity — where a request *is* along it is.
///
/// The mapping is piecewise-linear, strictly increasing in x, and leaves y alone. Both
/// facts are load-bearing: a route computed in drawing coordinates maps to a route
/// through the same regions in the same order, so every waypoint in this file stays
/// correct at any width and only the *emitted* points are mapped.
#[derive(Clone, Copy, PartialEq)]
pub struct Layout {
    /// The stage's width in CSS pixels.
    pub w: f64,
    /// Left plumbing width — everything before the machine.
    l: f64,
    /// Right plumbing width — the exit pipes.
    r: f64,
    /// The machine's mapped width, and the factor that produced it.
    core_w: f64,
    core_scale: f64,
    /// Where the queue's descending leg lands — a knot in the ingress mapping, so the
    /// pipe, the dots queued along it and its labels all move together.
    leg: f64,
}

impl Layout {
    pub fn of(width: f64) -> Layout {
        let w = width.max(320.0);
        let core_scale = if w >= CORE_HOLD {
            1.0
        } else {
            (CORE_MIN + (1.0 - CORE_MIN) * (w - CORE_FLOOR) / (CORE_HOLD - CORE_FLOOR))
                .clamp(CORE_MIN, 1.0)
        };
        let core_w = (CORE_R - CORE_L) * core_scale;
        let plumbing = (w - core_w).max(0.0);
        let l = plumbing * 0.5;
        // On a narrow stage the gate's readings wrap and reach further left, so the leg
        // pulls in until its near wall clears them. Everything on the ingress side is
        // mapped through this, so nothing has to be told the leg moved.
        let nominal = QTAIL * (l / CORE_L);
        let leg = if w < NARROW_W {
            nominal.min(l - 6.0 - GATE_LABEL_W - HW_QUEUE - 3.0).max(24.0)
        } else {
            nominal
        };
        Layout { w, l, r: l, core_w, core_scale, leg }
    }

    /// Map a drawing x onto this width. Piecewise-linear and strictly increasing, with
    /// a knot at the queue's leg: ingress compresses, the machine holds, the exits
    /// compress.
    pub fn x(&self, x: f64) -> f64 {
        if x <= QTAIL {
            x * (self.leg / QTAIL)
        } else if x <= CORE_L {
            self.leg + (x - QTAIL) * ((self.l - self.leg) / (CORE_L - QTAIL))
        } else if x >= CORE_R {
            self.l + self.core_w + (x - CORE_R) * (self.r / (STAGE_W - CORE_R))
        } else {
            self.l + (x - CORE_L) * self.core_scale
        }
    }

    pub fn pt(&self, p: (f64, f64)) -> (f64, f64) {
        (self.x(p.0), p.1)
    }

    /// A stage too narrow to keep a label beside the thing it names — below this the
    /// pipe-side labels wrap, the gate shrinks, the queue's leg pulls in, and the
    /// least load-bearing readings are dropped.
    pub fn narrow(&self) -> bool {
        self.w < NARROW_W
    }

    /// A stage too narrow for an exit's reading to stay on one line. Wider than
    /// [`Layout::narrow`]: these rows are the longest text on the stage, so they run
    /// out of room first.
    pub fn stats_stacked(&self) -> bool {
        self.w < 640.0
    }

    /// An exit reading's baseline. Each hugs the pipe it describes and keeps a full
    /// line's gap to the pipe *above* it — the larger gap is what says which pipe a
    /// reading belongs to once the readings are stacked.
    pub fn readout_y(&self, row: usize) -> f64 {
        let ex = self.exits();
        if self.stats_stacked() {
            let pipe = [ex.timeout, ex.success, ex.processing];
            match row {
                // The top reading rises off its own pipe by its own height.
                0 => ex.timeout - ROW_LINES[0] * LN - HUG - DOT + 10.0,
                // The rest hang under the pipe above them.
                r => pipe[r - 1] + DOT + LN + 1.0 + 10.0,
            }
        } else {
            match row {
                0 => ex.timeout - 16.0,
                1 => ex.success - 16.0,
                2 => ex.processing - 16.0,
                _ => ex.processing + 31.0,
            }
        }
    }

    /// A stage too narrow for the sparklines to keep any shape beside the machine.
    /// They move below it instead, where they get the full width — a 5-second trace
    /// squeezed into a hundred pixels is a smear, not a chart.
    pub fn charts_below(&self) -> bool {
        self.w < 900.0
    }

    /// The stage's height. Charts moved below the machine need the room, and taking
    /// it here is what keeps them at full width instead of overlapping the picture.
    pub fn height(&self) -> f64 {
        if self.charts_below() { self.charts_top() + self.charts_h() } else { STAGE_H }
    }

    /// The reflowed chart band's height: both sparklines, a key under each, and the
    /// design's margins. A sparkline **stretches horizontally only** — it is a trace
    /// over time, so more width is more time on screen, and a chart that grew as tall
    /// as it is wide would be a landscape, not a reading.
    fn charts_h(&self) -> f64 {
        4.0 + CHART_HS + 2.0 * KEY_H + 16.0
    }

    /// Where the chart band starts — just below the picture.
    pub fn charts_top(&self) -> f64 {
        STAGE_H + 8.0
    }

    /// The exit pipes' ys. They spread apart on a narrow stage, where each pipe's
    /// reading wraps and needs the room; the extra pitch comes out of the box's spare
    /// margin, so the stage height never changes.
    ///
    /// The spread is **derived from the readings**, not a constant: a pipe sits far
    /// enough below the one above it to clear that pipe's dots, the reading that hangs
    /// under it — however many lines that reading takes — and a full line's gap. A
    /// fixed pitch is a pitch that is wrong for the one reading with three parts.
    pub fn exits(&self) -> Exits {
        if !self.narrow() {
            return Exits { timeout: TMOY, success: SUCCY, processing: PROC_TMO_Y };
        }
        let pitch = |row: usize| DOT + HUG + ROW_LINES[row] * LN + LN + 1.0 + DOT;
        let success = TMOY + pitch(1);
        Exits { timeout: TMOY, success, processing: success + pitch(2) }
    }
}

/// The centreline corner radius. At or below a pipe's half-width, so a dot riding the
/// centreline stays exactly half-width from each wall through a corner too —
/// dot-in-channel holds by construction rather than by a clamp.
pub const CORNER: f64 = 10.0;

/// A polyline as an SVG path with rounded corners: each corner is a quadratic through
/// the vertex, inset by `r` along both legs (or half the shorter leg, whichever is
/// less, so short segments can't overshoot).
///
/// The walls are this same centreline stroked to either side, so the wall geometry and
/// the dot's route are one description rather than two that have to agree.
pub fn rounded_path(pts: &[(f64, f64)], r: f64) -> String {
    use std::fmt::Write as _;
    let Some(&first) = pts.first() else { return String::new() };
    let mut d = format!("M{:.1} {:.1}", first.0, first.1);
    if pts.len() < 2 {
        return d;
    }
    for i in 1..pts.len() - 1 {
        let (p0, p1, p2) = (pts[i - 1], pts[i], pts[i + 1]);
        let rr = r.min(seg_len(p0, p1) / 2.0).min(seg_len(p1, p2) / 2.0);
        let a = along(p1, p0, rr);
        let b = along(p1, p2, rr);
        let _ = write!(d, " L{:.1} {:.1} Q{:.1} {:.1} {:.1} {:.1}", a.0, a.1, p1.0, p1.1, b.0, b.1);
    }
    let last = pts[pts.len() - 1];
    let _ = write!(d, " L{:.1} {:.1}", last.0, last.1);
    d
}

fn seg_len(a: (f64, f64), b: (f64, f64)) -> f64 {
    (b.0 - a.0).hypot(b.1 - a.1)
}

fn along(from: (f64, f64), to: (f64, f64), r: f64) -> (f64, f64) {
    let (dx, dy) = (to.0 - from.0, to.1 - from.1);
    let l = dx.hypot(dy).max(1e-9);
    (from.0 + dx / l * r, from.1 + dy / l * r)
}

/// An arc of a circle as an SVG path — the shape every ring on the stage is drawn
/// with, from a core's burst to a request's deadline to the gate's thirds.
pub fn arc_path(cx: f64, cy: f64, r: f64, a0: f64, a1: f64) -> String {
    let large = if (a1 - a0).rem_euclid(360.0) > 180.0 { 1 } else { 0 };
    let p = |deg: f64| {
        let a = deg.to_radians();
        (cx + r * a.cos(), cy + r * a.sin())
    };
    let (s, e) = (p(a0), p(a1));
    format!(
        "M{:.1} {:.1} A{r:.1} {r:.1} 0 {large} 1 {:.1} {:.1}",
        s.0, s.1, e.0, e.1
    )
}

/// A full circle as a path — two half arcs, because a single arc whose ends meet is a
/// zero-length one and draws nothing.
pub fn circle_path(cx: f64, cy: f64, r: f64) -> String {
    format!(
        "M{:.1} {cy:.1} A{r:.1} {r:.1} 0 1 1 {:.1} {cy:.1} A{r:.1} {r:.1} 0 1 1 {:.1} {cy:.1} Z",
        cx - r,
        cx + r,
        cx - r
    )
}

/// How many lines each exit reading takes once it is stacked, in pipe order — offered,
/// goodput, processing, in-flight. The goodput row is the long one: it reports the
/// served rate, the success count *and* the clients who gave up waiting, so it takes a
/// line more than the rest and the pipes below it move down to give it one.
const ROW_LINES: [f64; 4] = [2.0, 3.0, 2.0, 2.0];
/// A stacked reading's line height, the gap it keeps to the pipe it belongs to, and a
/// pipe's own drawn radius.
const LN: f64 = 12.0;
const HUG: f64 = 6.0;
const DOT: f64 = 7.0;

/// The three exit pipes a request can leave by, at this width.
#[derive(Clone, Copy, PartialEq)]
pub struct Exits {
    pub timeout: f64,
    pub success: f64,
    pub processing: f64,
}

impl Exits {
    /// The y of the pipe a verdict rides home — the engine names the exit, this says where
    /// it runs.
    fn y(&self, exit: Exit) -> f64 {
        match exit {
            Exit::Timeout => self.timeout,
            Exit::Success => self.success,
            Exit::Processing => self.processing,
        }
    }
}

// ─── the pipe network ──────────────────────────────────────────────────────────

/// Every pipe's **centreline**, in drawing coordinates. One description: the walls are
/// this stroked to a half-width either side, and the dots ride the line itself.
///
/// Each end runs off-stage or a few pixels *into* a box, so no cap is ever visible and
/// every mouth is open by construction — there is nothing to erase.
pub struct Pipe {
    pub pts: Vec<(f64, f64)>,
    pub hw: f64,
}

/// The pipe half-width. A pipe is a bore, so this is the same at every stage width.
pub const HW: f64 = 13.0;
/// The queue is a shade wider — it is the one pipe requests *queue in* rather than
/// travel through, and the deadline rings need the room.
pub const HW_QUEUE: f64 = 14.0;

/// Where the queue meets the CPU's left wall: the fork, and the gate that stands in it.
pub const FORK_Y: f64 = QY;
/// How much room the gate's own readings take to the left of it — what the queue's
/// leg has to clear once they wrap.
const GATE_LABEL_W: f64 = 40.0;
pub fn fork_r(lay: &Layout) -> f64 {
    if lay.narrow() { 12.0 } else { 15.0 }
}

/// A request leaves the gate **through one of its thirds**, never through the wall the
/// gate stands in: shedding emerges up-and-right at 300°, admission down-and-right at
/// 60°, just outside the ring the thirds are drawn on. Both points are in drawing
/// coordinates like every other waypoint, so they take the desktop radius — on a narrow
/// stage the ring is smaller and the dot simply clears it by a little more.
fn fork_exit(deg: f64) -> (f64, f64) {
    const R: f64 = 18.0;
    let a = deg.to_radians();
    (QFORK_X + R * a.cos(), FORK_Y + R * a.sin())
}
/// Out of the gate by the timeout third — up into the box, bound for the shed pipe.
fn gate_shed() -> (f64, f64) {
    fork_exit(300.0)
}
/// Out of the gate by the admit third — down into the box, bound for a core.
fn gate_admit() -> (f64, f64) {
    fork_exit(60.0)
}

/// Where the head waits *outside* the gate — clear of the thirds by 3 px, so its
/// deadline ring stands beside the circle rather than on top of it. Drawing
/// coordinates, so it takes the desktop ring; a narrower stage draws a smaller one and
/// the head simply clears it by more.
pub const GATE_WAIT_X: f64 = QFORK_X - 28.0;

/// The network at this width, already mapped. `parts` decides which routes this
/// composition actually has.
pub fn pipes(lay: &Layout, queue: bool, shed: bool) -> Vec<Pipe> {
    let ex = lay.exits();
    let m = |pts: Vec<(f64, f64)>| pts.into_iter().map(|p| lay.pt(p)).collect::<Vec<_>>();
    let mut out = vec![
        // ingress, off-stage into the chamber's left wall
        Pipe { pts: m(vec![(-16.0, SYN_Y), (IOC_X + 4.0, SYN_Y)]), hw: HW },
        // the run queue's column and the IO descent, each ending inside a box
        Pipe { pts: m(vec![(PIPE_RET, CPU_Y + CPU_H - 4.0), (PIPE_RET, IOC_Y + 4.0)]), hw: HW },
        Pipe { pts: m(vec![(PIPE_IO, CPU_Y + CPU_H - 4.0), (PIPE_IO, IOC_Y + 4.0)]), hw: HW },
        // the reply home
        Pipe { pts: m(vec![(CPU_X + CPU_W - 4.0, ex.success), (STAGE_W + 14.0, ex.success)]), hw: HW },
    ];
    if shed {
        out.push(Pipe {
            pts: m(vec![(CPU_X + CPU_W - 4.0, ex.timeout), (STAGE_W + 14.0, ex.timeout)]),
            hw: HW,
        });
    }
    // Work can only finish *too late* if it was held first, so this exit belongs to
    // the compositions that hold.
    if queue {
        out.push(Pipe {
            pts: m(vec![(CPU_X + CPU_W - 4.0, ex.processing), (STAGE_W + 14.0, ex.processing)]),
            hw: HW,
        });
    }
    if queue {
        // ONE continuous channel: out of the CPU's top, along the top run, down the
        // leg, and back into the CPU's left wall at the fork. Both ends finish inside
        // the box, so the corners are real joins and no cap shows.
        out.push(Pipe {
            pts: m(vec![
                (QTX_X, CPU_Y + 6.0),
                (QTX_X, QTX_TOP_Y),
                (QTAIL, QTX_TOP_Y),
                (QTAIL, QY),
                (CPU_X + 4.0, QY),
            ]),
            hw: HW_QUEUE,
        });
    }
    out
}

// ─── the two queues at the worker boundary ─────────────────────────────────────

/// The kernel accept queue's head: just short of the chamber's left wall, where the
/// SYN pipe meets it. Everything teal lives left of here; `accept()` pops from here.
const SYN_HEAD_X: f64 = IOC_X - 14.0;
/// Where the SYN pipe begins — **off-stage**. A request comes from somewhere else on
/// the network, so it enters the picture already travelling rather than appearing in
/// it; the same reason its reply leaves past the right edge instead of stopping at it.
const SYN_TAIL_X: f64 = -16.0;
/// The tokio run queue's head: the top of the return-pipe column, at the CPU box's
/// floor mouth, where a worker takes the next task. The stack grows downward, through
/// the chamber's own vertical, compressing once it outgrows the span.
const RUNQ_HEAD_Y: f64 = CPU_Y + CPU_H + 6.0; // 290
const RUNQ_END_Y: f64 = IOC_Y + IOC_H - 20.0; // 460
/// Nominal gap between queued dots in either queue; compresses when the queue outgrows
/// its span.
const QUEUE_SPACING: f64 = 12.0;
/// Minimum gap on the app queue's deadline conveyor — dots stack this close behind the
/// neighbour ahead rather than overlapping.
const CONVEYOR_SPACING: f64 = 9.0;

/// Slot `i` of the kernel accept queue, head at the chamber mouth, backing up along
/// the SYN pipe toward ingress.
fn syn_slot(i: usize, n: usize) -> (f64, f64) {
    let sp = QUEUE_SPACING
        .min((SYN_HEAD_X - SYN_TAIL_X - 4.0) / (n.max(2) - 1) as f64)
        .max(7.0);
    ((SYN_HEAD_X - i as f64 * sp).max(SYN_TAIL_X + 2.0), SYN_Y)
}

/// Slot `i` of the tokio run queue, head at the box's floor mouth, stacking downward.
fn runq_slot(i: usize, n: usize) -> (f64, f64) {
    let sp = QUEUE_SPACING
        .min((RUNQ_END_Y - RUNQ_HEAD_Y) / (n.max(2) - 1) as f64)
        .max(6.0);
    (PIPE_RET, (RUNQ_HEAD_Y + i as f64 * sp).min(RUNQ_END_Y))
}


/// The queue conveyor's slot xs this frame, in queue order. A queue that outgrows its
/// pipe pushes slots past the tail — the fold elides those and its `+N more` label
/// carries the count, like the run queue's deep tail.
pub fn queue_xs(obs: &Obs, lay: Layout) -> Vec<f64> {
    Slots::of(obs, lay).queue_x
}

/// Per-frame layout the anchors derive from: queue lengths, and the app queue's
/// **deadline conveyor** — each queued dot placed by its shed-deadline progress
/// (`age 0` at the tail, `age 1` exactly at the fork, so a shed request stands at the
/// timeout gate the instant it sheds), capped a minimum spacing behind the neighbour
/// ahead. FIFO means deadline order is queue order, and both terms only ever move
/// fork-ward, so the conveyor is forward-only. With the timeout off, age saturates and
/// the queue stacks compactly at the fork, waiting on admission.
struct Slots {
    syn_n: usize,
    run_n: usize,
    queue_x: Vec<f64>,
    /// The width the stage is laid out to. The exit pipes' ys come off it, so a reply's leg
    /// home ends on the pipe that is actually drawn for it.
    lay: Layout,
}

impl Slots {
    fn of(obs: &Obs, lay: Layout) -> Slots {
        let mut queue_x = Vec::new();
        for &(_, station) in &obs.live {
            if let Station::AppQueue { idx, age } = station {
                let placed = QTAIL + age * (GATE_WAIT_X - QTAIL);
                let capped = match idx.checked_sub(1).and_then(|i| queue_x.get(i)) {
                    Some(ahead) => placed.min(ahead - CONVEYOR_SPACING),
                    None => placed,
                };
                debug_assert_eq!(queue_x.len(), idx, "queue_stubs ride live in idx order");
                queue_x.push(capped);
            }
        }
        Slots {
            syn_n: obs.syn_backlog.len(),
            run_n: obs.ready.len(),
            queue_x,
            lay,
        }
    }

    /// Floored at the tail: slots the conveyor pushed past the pipe belong to elided
    /// dots — anything still shown (or walking in) stands at the tail, on the pipe.
    fn queue_slot(&self, idx: usize) -> (f64, f64) {
        (self.queue_x.get(idx).copied().unwrap_or(QTAIL).clamp(QTAIL, GATE_WAIT_X), QY)
    }

    /// Where a reply's leg home ends: off-stage right, on its own exit pipe — the mirror of
    /// the inbound leg's off-stage start, so a reply leaves the picture rather than stopping
    /// at its edge.
    fn exit_end(&self, reply: Reply) -> (f64, f64) {
        (STAGE_W + 20.0, self.lay.exits().y(reply.exit()))
    }
}

fn in_io_pipe(x: f64, y: f64) -> bool {
    (x - PIPE_IO).abs() < 9.0 && y > CPU_Y + CPU_H - 2.0 && y < IOC_Y + 13.0
}

/// The route out to an exit pipe at `exit_y`: through the drawn openings back to the
/// corridor, then out. Shared by success and the timeouts. A dot can be anywhere when
/// its verdict lands (position lags the engine), so every origin region has a legal
/// route: the SYN pipe enters the chamber through its mouth, the chamber unwinds up the
/// return pipe, and the queue side rides the admission diagonal into the box.
fn exit_wp(from: (f64, f64), exit_y: f64) -> Vec<(f64, f64)> {
    let (lx, ly) = from;
    let mut wp = Vec::new();
    if ly > CPU_Y + CPU_H - 2.0 {
        if lx < IOC_X && (ly - SYN_Y).abs() < 15.0 {
            wp.push((IOC_X, SYN_Y)); // along the SYN pipe, in through the chamber mouth
        }
        if in_io_pipe(lx, ly) {
            wp.push((PIPE_IO, IOC_Y + 10.0));
        }
        if ly > IOC_Y || !wp.is_empty() {
            wp.push((PIPE_RET, IOC_Y + 10.0));
        }
        wp.push((PIPE_RET, CORRIDOR));
        wp.push((TURN_OUT, CORRIDOR));
    } else if ly < CPU_Y || lx < CPU_X {
        // Queue side of the box (the pipe, or anywhere along the queue-tx approach):
        // forward to the fork and in through the admission opening — never straight
        // through a wall.
        wp.extend(enter_via_admission((lx, ly)));
        wp.push((TURN_OUT, CPU_ADM_Y));
    } else {
        wp.push((TURN_OUT, ly));
    }
    wp.push((TURN_OUT, exit_y));
    wp.push((STAGE_W + 20.0, exit_y));
    wp
}

// ─── arc-length parameterised polyline ─────────────────────────────────────────

/// A polyline the dot walks by arc length. Immutable once built: a new leg is a **fresh**
/// path rooted at the dot's current position (see [`Dot::steer`]), so the walked distance
/// is always within the path and the emitted point can never desync from it.
pub struct ArcPath {
    pts: Vec<(f64, f64)>,
    cum: Vec<f64>,
    total: f64,
}

impl ArcPath {
    pub fn new(pts: Vec<(f64, f64)>) -> Self {
        let mut cum = vec![0.0f64];
        let mut l = 0.0;
        for i in 1..pts.len() {
            let dx = pts[i].0 - pts[i - 1].0;
            let dy = pts[i].1 - pts[i - 1].1;
            l += (dx * dx + dy * dy).sqrt();
            cum.push(l);
        }
        Self { pts, cum, total: l }
    }

    fn at_dist(&self, d: f64) -> (f64, f64) {
        if self.pts.len() < 2 || self.total <= 1e-9 {
            return self.pts.first().copied().unwrap_or((0.0, 0.0));
        }
        let d = d.clamp(0.0, self.total);
        let mut i = 1;
        while i < self.cum.len() - 1 && self.cum[i] < d {
            i += 1;
        }
        let a = self.pts[i - 1];
        let b = self.pts[i];
        let seg = (self.cum[i] - self.cum[i - 1]).max(1e-9);
        let f = (d - self.cum[i - 1]) / seg;
        (a.0 + (b.0 - a.0) * f, a.1 + (b.1 - a.1) * f)
    }

    fn total(&self) -> f64 {
        self.total
    }

    fn end(&self) -> (f64, f64) {
        self.pts.last().copied().unwrap_or((0.0, 0.0))
    }
}

// ─── one dot: a position that walks the pipes leg by leg ────────────────────────

/// A single request's dot. It walks the current `path` by arc-length `dist`, and only when
/// it **arrives** does it adopt the next station — arrive-before-you-leave, so every leg is
/// fully travelled and its waypoints are hit. Each new leg re-roots a fresh path at the
/// current position (`dist` back to 0), so the emitted point is always on the path and
/// never jumps. A same-pipe slide (queue drain) re-aims along the one pipe — but only
/// while the dot is actually *on* that pipe; mid-leg it keeps walking.
pub struct Dot {
    path: ArcPath,
    dist: f64,
    /// The current leg's destination (its route's end) — a route is rebuilt only when the
    /// target actually moves.
    dest: (f64, f64),
    /// Station-kind of the current leg — distinguishes a genuine transition from a slide.
    kind: u8,
    /// The current leg is the IO circuit, paced by the engine's IO progress.
    io: bool,
    /// What the dot is doing besides walking its station.
    life: Life,
    /// The exact polyline walked this rendered frame — start point, every vertex
    /// crossed — reset by [`Dot::begin_frame`]. Emitted verbatim as the frame's
    /// `offset-path`, so rendered motion *is* the walked route.
    trace: Vec<(f64, f64)>,
    /// Stations the request entered (engine hops) that the dot has not yet visually
    /// reached — the route plan. While non-empty the dot is in **transit**, walking
    /// each hop in order (one leg per arrival); the live station's own behaviour
    /// (placed glides, the conveyor, slides) engages only once the itinerary drains.
    /// This is what makes sub-frame stations renderable: the engine reports every
    /// entry, so nothing it does between snapshots can be skipped.
    itinerary: VecDeque<Station>,
}

/// A dot's life beyond the station it is walking. While the engine has the request it is
/// [`Life::Live`] and every leg comes off the snapshot; a departure hands the dot the rest of
/// the story to finish on its own.
///
/// A reply that reached its client walks out whatever is left of its leg home
/// ([`Life::Received`]) — the engine can run a 6 ms leg inside one 16 ms frame, and a dot is
/// never seen to vanish mid-stage. The request nobody is waiting for any more drops where it
/// stood: [`Life::GaveUp`] until its current leg is walked out, then [`Life::Falling`] once the
/// drop is the path it is on. So "falling but still live" and "gave up twice" are not states.
#[derive(Clone, Copy, PartialEq)]
enum Life {
    Live,
    Received(Reply),
    GaveUp,
    Falling,
}

const KIND_NONE: u8 = 255;
/// The station-kind tag of the leg home — the one kind named outside [`station_kind`], because
/// a received dot lays its own leg when the engine outran the frame.
const KIND_NETWORK_OUT: u8 = 7;

impl Dot {
    fn at(pos: (f64, f64)) -> Self {
        Dot {
            path: ArcPath::new(vec![pos]),
            dist: 0.0,
            dest: pos,
            kind: KIND_NONE,
            io: false,
            life: Life::Live,
            trace: vec![pos],
            itinerary: VecDeque::new(),
        }
    }

    /// Bound a lagging dot's route plan: when the itinerary grows past a few legs,
    /// drop the earliest hop whose station-kind recurs later — repeated compute cycles
    /// (cpu→io→cpu…) collapse to the latest lap, while anything visited once (the
    /// queue, the handoff, the gate) is always walked.
    fn compact_itinerary(&mut self) {
        while self.itinerary.len() > 6 {
            // Never drop the front: it is the leg being walked right now, and pulling
            // it out from under the dot would strand it mid-route.
            let dup = self.itinerary.iter().enumerate().skip(1).find_map(|(i, a)| {
                let k = station_kind(*a);
                self.itinerary
                    .iter()
                    .skip(i + 1)
                    .any(|b| station_kind(*b) == k)
                    .then_some(i)
            });
            match dup {
                Some(i) => {
                    self.itinerary.remove(i);
                }
                None => break,
            }
        }
    }

    pub fn pos(&self) -> (f64, f64) {
        self.path.at_dist(self.dist)
    }

    fn begin_frame(&mut self) {
        let pos = self.pos();
        self.trace.clear();
        self.trace.push(pos);
    }

    /// The frame's walked polyline as a CSS `path()` string, ending at the current
    /// position, mapped to the stage's width. Consecutive duplicate points are
    /// dropped; an unmoved dot degenerates to a single `M`.
    ///
    /// Mapping vertex-by-vertex is exact rather than approximate: the walk's vertices
    /// are its only corners, and the mapping is linear between them.
    fn frame_path(&self, lay: &Layout) -> String {
        let mut out = String::new();
        let mut last: Option<(f64, f64)> = None;
        for (x, y) in self.trace.iter().copied().chain([self.pos()]).map(|p| lay.pt(p)) {
            if last.is_some_and(|(lx, ly)| (lx - x).abs() < 0.05 && (ly - y).abs() < 0.05) {
                continue;
            }
            let _ = if out.is_empty() {
                write!(out, "M{x:.1} {y:.1}")
            } else {
                write!(out, " L{x:.1} {y:.1}")
            };
            last = Some((x, y));
        }
        out
    }

    /// Record the vertices strictly between two arc lengths into the trace, in walking
    /// order (either direction).
    fn record_span(&mut self, d0: f64, d1: f64) {
        let n = self.path.pts.len();
        if d1 >= d0 {
            for k in 1..n.saturating_sub(1) {
                let c = self.path.cum[k];
                if d0 + 0.05 < c && c < d1 - 0.05 {
                    self.trace.push(self.path.pts[k]);
                }
            }
        } else {
            for k in (1..n.saturating_sub(1)).rev() {
                let c = self.path.cum[k];
                if d1 + 0.05 < c && c < d0 - 0.05 {
                    self.trace.push(self.path.pts[k]);
                }
            }
        }
    }

    /// Re-root the walked path at the current position, then follow the route to its end.
    /// The trace continues through the junction — position never jumps at a steer.
    fn steer(&mut self, wp: Vec<(f64, f64)>) {
        let pos = self.pos();
        let mut pts = Vec::with_capacity(wp.len() + 1);
        pts.push(pos);
        pts.extend(wp);
        self.dest = *pts.last().unwrap();
        self.path = ArcPath::new(pts);
        self.dist = 0.0;
    }

    /// Swap in a re-laid path for a placed leg (same pipe, moved endpoint), keeping the
    /// walked distance within it.
    fn relay(&mut self, path: ArcPath) {
        self.path = path;
        self.dist = self.dist.min(self.path.total());
        self.dest = self.path.end();
    }

    /// Walk to arc length `to`, recording the vertices crossed.
    fn walk_to(&mut self, to: f64) {
        self.record_span(self.dist, to);
        self.dist = to;
    }

    /// Pursue: walk toward the leg's end at the base rate, faster the further behind —
    /// the remaining distance closes on the [`CATCH_UP_MS`] timescale, so lag is
    /// bounded in time, not proportional to route length — capped at [`RATE_MAX`] so
    /// even a huge catch-up is a visible sweep, never a blink. `virt` is the (clamped)
    /// virtual ms this frame — see the pursuit-pacing note above the constants.
    fn pursue(&mut self, virt: f64) {
        let remaining = self.path.total() - self.dist;
        let speed = RATE.max(remaining / CATCH_UP_MS).min(RATE_MAX);
        self.walk_to((self.dist + speed * virt).min(self.path.total()));
    }

    /// Walk toward the gate seat `at` and pursue it: a request the gate holds finishes
    /// coming down the queue — along the pipes, never straight to the point — since its
    /// verdict may have landed while it was still on the queue-tx approach. Steers only
    /// when not already aimed there, so a re-held dot keeps its route.
    fn walk_held_to_gate(&mut self, at: (f64, f64), pursue_dt: f64) {
        if dist2(self.dest, at) > 1.0 {
            let route = route_to(self.pos(), Station::AppQueue { idx: 0, age: 0.0 }, at);
            self.steer(route);
        }
        self.pursue(pursue_dt);
    }

    /// Place: set the walked distance outright (a deterministic leg paced by the
    /// engine's progress).
    fn place(&mut self, d: f64) {
        self.walk_to(d.clamp(0.0, self.path.total()));
    }

    fn arrived(&self) -> bool {
        self.dist >= self.path.total() - 0.5
    }

    /// How much of this dot is left, as it drops. Every other request leaves the
    /// picture by travelling out of it; a client that gave up is owed no reply, so
    /// there is no journey home to draw — its request drops where it stood and goes
    /// out with the waiting. The only dot on the stage that fades.
    fn fade(&self) -> f64 {
        1.0 - (self.dist / self.path.total().max(1e-9)).clamp(0.0, 1.0)
    }
}

// ─── view state ────────────────────────────────────────────────────────────────

pub struct ViewState {
    dots: HashMap<u32, Dot>,
    /// Whose request each dot is drawing, for the sims that colour by tenant. Kept exactly as
    /// long as the dot is: the engine forgets a request when it departs, but the dot walks its
    /// last leg out after that, and it must not change colour on the way.
    names: HashMap<u32, usize>,
    /// Hops delivered by the engine but not yet handed to their dots — accumulated
    /// across engine sub-ticks (a rendered frame can contain several), drained by
    /// [`ViewState::step`].
    pending_hops: HashMap<u32, Vec<Station>>,
    /// The gate: a discrete third per engine frame, and the head's handshake through it.
    pub gate: Gate,
    /// The head's fate as the engine's sub-ticks reported it, waiting for the next
    /// gate frame. `None` until the engine has actually admitted or shed it.
    fated_head: Option<(u32, Fate)>,
    /// The request the gate has hold of, and the point the handshake puts it at this
    /// frame — outside the circle while the entrance is shut, inside it while waiting
    /// for its own exit. It walks there and waits; the gate decides when it goes on.
    held: Option<(u32, (f64, f64))>,
    /// The head as of the last engine sub-tick, so the frame it leaves the queue is the
    /// frame its fate is known.
    last_head: Option<u32>,
    /// The engine time motion has been advanced to ([`ViewState::step`]'s pacing).
    last_t: f64,
    /// The width the picture is currently laid out to. Motion is computed in drawing
    /// coordinates throughout and mapped only on the way out, so this is read by the
    /// emitters and by nothing else.
    pub layout: Layout,
    rng: Rng,
}

impl ViewState {
    pub fn new(layout: Layout) -> Self {
        Self {
            dots: HashMap::new(),
            names: HashMap::new(),
            pending_hops: HashMap::new(),
            gate: Gate::new(),
            fated_head: None,
            held: None,
            last_head: None,
            last_t: 0.0,
            layout,
            rng: Rng::new(0x51ce_57a7e),
        }
    }

    /// Absorb one engine sub-tick's hops (called once per `tick`, like
    /// [`ViewState::depart`], so no transition is lost when several engine steps fit
    /// in one rendered frame).
    pub fn ingest_hops(&mut self, hops: &[Hop]) {
        for hop in hops {
            self.pending_hops.entry(hop.id).or_default().push(hop.station);
        }
    }

    /// Say whose a request is, while the engine still knows. Held until the dot is dropped.
    pub fn name(&mut self, id: u32, tenant: Option<usize>) {
        if let Some(tenant) = tenant {
            self.names.insert(id, tenant);
        }
    }

    /// Whose the request being drawn as this dot is — the answer the engine gave while it
    /// still had it.
    pub fn named(&self, id: u32) -> Option<usize> {
        self.names.get(&id).copied()
    }

    /// A live request's dot this frame: its position and the exact path it walked
    /// (this frame's `offset-path`), both mapped to the current width.
    pub fn dot_frame(&self, id: u32) -> Option<((f64, f64), String)> {
        let lay = &self.layout;
        self.dots.get(&id).map(|d| (lay.pt(d.pos()), d.frame_path(lay)))
    }

    /// The dots the engine no longer has, still finishing the leg it started them on:
    /// `(id, pos, frame path, the verdict they carry, opacity)` for the fold.
    ///
    /// A reply walks out solid, in the colour it wore on its leg home, so the handoff off
    /// `live` changes nothing the reader can see. The request nobody is waiting for any more
    /// drops where it stood and fades — [`Outcome::ResponseTimeout`] by construction, being
    /// the one verdict that sends no reply.
    pub fn leaving(&self) -> Vec<(u32, (f64, f64), String, Outcome, f64)> {
        let lay = &self.layout;
        self.dots
            .iter()
            .filter_map(|(&id, d)| {
                let (outcome, opacity) = match d.life {
                    Life::Live => return None,
                    Life::Received(reply) => (reply.outcome(), 1.0),
                    Life::GaveUp | Life::Falling => (Outcome::ResponseTimeout, d.fade()),
                };
                Some((id, lay.pt(d.pos()), d.frame_path(lay), outcome, opacity))
            })
            .collect()
    }

    /// Hand each departed request's dot the rest of its story: the reply it has to finish
    /// carrying home, or the drop of a client that stopped waiting. Called once per engine
    /// sub-tick with the ids still live *after* it, so a zombie (accept-hang, still on its
    /// core) is left alone — its client gave up but the handler did not.
    pub fn depart(&mut self, departures: &[(u32, Outcome)], live: &HashSet<u32>) {
        for &(id, outcome) in departures {
            if live.contains(&id) {
                continue; // a leaked/zombie handler stays put; it did not really leave
            }
            let Some(dot) = self.dots.get_mut(&id) else { continue };
            if dot.life != Life::Live {
                continue;
            }
            // Whichever it is, the route plan is moot — what is left is one leg, or none.
            dot.itinerary.clear();
            dot.life = match outcome.reply() {
                Some(reply) => Life::Received(reply),
                None => Life::GaveUp,
            };
        }
    }

    /// Absorb one engine sub-tick into the motion layer, in the order the protocol
    /// requires — hops ingested, give-ups peeled (with the ids still live so a zombie
    /// isn't torn off its core), one gate frame — and hand back the departures the caller
    /// still needs (ping resolution, or nothing). One engine frame is one gate
    /// frame, so this runs exactly once per `tick`; the settle loop and the live tick share
    /// this single definition rather than each spelling the sequence out.
    pub fn absorb_subtick(&mut self, obs: &Obs) -> Vec<(u32, Outcome)> {
        let live = obs.live_ids();
        let departures = obs.departures.clone();
        self.ingest_hops(&obs.hops);
        self.depart(&departures, &live);
        self.gate_frame(obs);
        departures
    }

    /// Advance all motion by one rendered frame, paced by the engine's own time — the
    /// distance `obs.t` moved since the previous frame. One clock for truth and
    /// motion, so dots can never outrun or lag the simulation's time itself; a frame
    /// the engine didn't advance moves nothing.
    pub fn step(&mut self, obs: &Obs) {
        let virt = (obs.t - self.last_t).max(0.0);
        self.last_t = obs.t;

        // The un-modelled hops track the speed knob but within a watchable band: clamp
        // the per-frame virtual advance pursuit consumes to [FLOOR, CEIL]. A frame that
        // didn't advance the sim (paused, or no fixed-step quantum fired) is exempt — it
        // moves nothing. Placed/timed legs keep using the raw `virt`, so they dilate.
        let pursue_dt = if virt > 0.0 { virt.clamp(PURSUE_VIRT_FLOOR, PURSUE_VIRT_CEIL) } else { 0.0 };

        let layout = Slots::of(obs, self.layout);

        let ViewState { dots, rng, pending_hops, held, .. } = self;
        let held = *held;
        for d in dots.values_mut() {
            d.begin_frame();
        }
        for &(id, station) in &obs.live {
            let hops = pending_hops.remove(&id);
            let dot = dots.entry(id).or_insert_with(|| {
                // A brand-new dot is born where its journey began — its first hop
                // (network ingress for an ordinary arrival) — and walks from there,
                // even if the engine has already raced it several stations ahead.
                let first = hops.as_ref().and_then(|h| h.first().copied());
                Dot::at(spawn_pos(first.unwrap_or(station), &layout))
            });
            if let Some(hops) = hops {
                dot.itinerary.extend(hops);
            }
            dot.compact_itinerary();
            // The gate has hold of this one: it walks to where the handshake put it —
            // the entrance, then inside the circle — and waits there for the frame its
            // exit opens. Whatever the engine has already made of it can wait; the
            // request has not been *seen* to leave yet.
            if let Some(at) = held.filter(|&(h, _)| h == id).map(|(_, at)| at) {
                dot.walk_held_to_gate(at, pursue_dt);
                continue;
            }
            let k = station_kind(station);
            // The live station's own arm owns the final approach and dwell, so the
            // itinerary's last entry (which always agrees with the live station) is
            // handed over rather than walked as a transit leg.
            if dot.itinerary.len() == 1 && station_kind(dot.itinerary[0]) == k {
                dot.itinerary.pop_front();
            }
            // Transit: the engine visited stations the snapshot never showed. Walk
            // them in order — one leg per arrival, catch-up paced — so every hop is
            // traversed on the drawn topology, however briefly the engine held it.
            // Forward-only on the accept queue: when the queue was empty the timed leg
            // already carried the dot up to the run queue, so a sub-frame SYN-backlog hop
            // is now behind it — drop it rather than walk the dot back down the pipe.
            while let Some(&Station::SynBacklog { .. }) = dot.itinerary.front() {
                if dot.pos().0 <= SYN_HEAD_X + 4.0 {
                    break;
                }
                dot.itinerary.pop_front();
            }
            if let Some(&next) = dot.itinerary.front() {
                let nk = station_kind(next);
                if dot.kind != nk {
                    let pos = dot.pos();
                    let route = match next {
                        Station::Io { .. } => io_circuit(pos, rng),
                        _ => route_to(pos, next, anchor(next, &layout)),
                    };
                    dot.steer(route);
                    dot.kind = nk;
                    dot.io = false;
                }
                dot.pursue(pursue_dt);
                if dot.arrived() {
                    dot.itinerary.pop_front();
                }
                continue;
            }
            match station {
                // The timed inbound leg — placed by `p`, touching down at the arrival
                // instant. When the accept queue is backing up, it lands at the assigned
                // slot on the SYN pipe (FIFO, forward-only — the slot only advances toward
                // the head). When the accept queue is *empty*, nothing waits to be accepted,
                // so the timed leg carries the dot on past the head to the run-queue head —
                // the start of the run queue, a fixed point — where a worker takes it. That
                // delivers the dot *placed* at the run queue, so the accept's core burst
                // rises from there instead of the ring lighting while the dot lags.
                Station::NetworkIn { p, slot } => {
                    let path = if layout.syn_n == 0 {
                        vec![(SYN_TAIL_X, SYN_Y), (PIPE_RET, SYN_Y), runq_slot(0, layout.run_n.max(1))]
                    } else {
                        let (sx, _) = syn_slot(slot, layout.syn_n.max(slot + 1));
                        vec![(SYN_TAIL_X, SYN_Y), (sx, SYN_Y)]
                    };
                    dot.relay(ArcPath::new(path));
                    dot.io = false;
                    dot.kind = k;
                    let d = p.clamp(0.0, 1.0) * dot.path.total();
                    dot.place(d);
                }
                // The leg home, and the mirror of the one above: deterministic, so it is
                // *placed* by the engine's progress. Its route is laid once, from wherever the
                // dot stands when the verdict lands (arrive-before-you-leave first, as the IO
                // circuit does) — so the reply covers whatever it has to cover in exactly the
                // leg's own time, and reaches the edge as the client receives it.
                Station::NetworkOut { p, reply } => {
                    if dot.kind != k {
                        if !dot.arrived() {
                            dot.pursue(pursue_dt); // finish the leg it is on
                            continue;
                        }
                        dot.steer(route_to(dot.pos(), station, layout.exit_end(reply)));
                        dot.io = false;
                        dot.kind = k;
                    }
                    let d = p.clamp(0.0, 1.0) * dot.path.total();
                    dot.place(d);
                }
                // The app queue is the **deadline conveyor**: once on the pipe the dot
                // is *placed* at its layout position (deterministic — the shed deadline
                // is known), drifting fork-ward to stand at the timeout gate the instant
                // it sheds. The approach (the queue-tx pipe) is pursued; catch-up
                // pursuit keeps its lag time-bounded however long the route.
                Station::AppQueue { idx, .. } => {
                    dot.io = false;
                    let target = layout.queue_slot(idx);
                    let pos = dot.pos();
                    let on_pipe = (pos.1 - QY).abs() < 8.0 && pos.0 <= QFORK_X + 6.0;
                    if k == dot.kind && on_pipe {
                        dot.steer(vec![target]);
                        let d = dot.path.total();
                        dot.place(d);
                    } else if k != dot.kind && dot.arrived() {
                        dot.steer(route_to(pos, station, target));
                        dot.kind = k;
                        dot.pursue(pursue_dt);
                    } else {
                        dot.pursue(pursue_dt);
                    }
                }
                // IO is deterministic too — its duration is fixed the moment it starts — so
                // once the dot has finished travelling to the core (arrive-before-you-leave)
                // the circuit is laid and then **placed** by `p`: its speed is solved to walk
                // the whole circuit in exactly the IO's duration.
                Station::Io { p } => {
                    if !dot.io {
                        if !dot.arrived() {
                            dot.pursue(pursue_dt); // still reaching the core
                            continue;
                        }
                        dot.steer(io_circuit(dot.pos(), rng));
                        dot.io = true;
                        dot.kind = k;
                    }
                    let d = p.clamp(0.0, 1.0) * dot.path.total();
                    dot.place(d);
                }
                _ => {
                    dot.io = false;
                    let target = anchor(station, &layout);
                    if k == dot.kind && is_sliding(station) {
                        // A same-pipe slide (a queue draining toward its head): re-aim at
                        // the moved anchor — but only while actually on the station's
                        // pipe (both queues are straight, so the re-aim *is* the pipe).
                        // Mid-leg the dot keeps walking its committed route.
                        if on_station_pipe(station, dot.pos()) && dist2(target, dot.dest) > 1.0
                        {
                            dot.steer(vec![target]);
                        }
                    } else if dist2(target, dot.dest) > 1.0 && dot.arrived() {
                        // A genuine transition: only once the current leg is fully walked
                        // does the dot steer along the pipes to the new station.
                        dot.steer(route_to(dot.pos(), station, target));
                        dot.kind = k;
                    }
                    dot.pursue(pursue_dt);
                }
            }
        }

        // Dots the engine no longer has, finishing what it started. Each walks out the one leg
        // it has left — the reply's route home, or the drop of a client that stopped waiting —
        // and despawns at the end of it, so nothing disappears where the reader is looking.
        let live: HashSet<u32> = obs.live.iter().map(|(id, _)| *id).collect();
        self.dots.retain(|id, dot| {
            if live.contains(id) {
                return true;
            }
            // Shed at the gate: the engine is done with it, but it has not been *seen* to
            // leave until the gate lets it out. It waits at the entrance, steps inside, and
            // only then takes its pipe.
            if let Some(at) = held.filter(|&(h, _)| h == *id).map(|(_, at)| at) {
                dot.walk_held_to_gate(at, pursue_dt);
                return true;
            }
            match dot.life {
                // A verdict the engine dropped: no reply, no give-up, nothing to finish.
                Life::Live => false,
                // The reply is the client's. Whatever of its leg the dot has not walked, it
                // walks now — laying the route first if the engine ran the whole leg inside
                // one frame and the live arm never saw it.
                Life::Received(reply) => {
                    if dot.kind != KIND_NETWORK_OUT {
                        dot.steer(leg_home(dot.pos(), reply, layout.exit_end(reply)));
                        dot.kind = KIND_NETWORK_OUT;
                        dot.io = false;
                    }
                    dot.pursue(pursue_dt);
                    !dot.arrived()
                }
                Life::GaveUp => {
                    if !dot.arrived() {
                        dot.pursue(pursue_dt); // finish the leg it is on before dropping out of it
                        return true;
                    }
                    let (x, y) = dot.pos();
                    dot.steer(vec![(x, y + 20.0)]);
                    dot.life = Life::Falling;
                    true
                }
                Life::Falling => {
                    dot.pursue(pursue_dt);
                    !dot.arrived()
                }
            }
        });
        // A name is the dot's, so it goes when the dot does.
        self.names.retain(|id, _| self.dots.contains_key(id));
        // Hops for requests that are gone have no dot to walk them.
        self.pending_hops.clear();
    }

    /// **One gate frame**, called once per engine sub-tick — the gate advances on the
    /// engine's clock, not the display's.
    ///
    /// First the head's fate, and only from what the stack actually did: **the station the
    /// head went to when it left `queue_stubs`**. Straight onto its leg home is the shed
    /// deadline; anywhere else is admission into the machine. The itinerary reports every
    /// entry, so this reads the same however little of it fitted inside one frame — and a
    /// verdict dropped without one leaves no hop and so no fate. Then the handshake, then the
    /// shutter's single step.
    pub fn gate_frame(&mut self, obs: &Obs) {
        let head = obs.queue_stubs.first().map(|&(id, _)| id);
        if let Some(prev) = self.last_head.filter(|p| head != Some(*p)) {
            let fate = obs.hops.iter().find(|h| h.id == prev).map(|h| match h.station {
                Station::NetworkOut { .. } => Fate::Shed,
                _ => Fate::Admit,
            });
            if let Some(fate) = fate {
                self.fated_head = Some((prev, fate));
            }
        }
        self.last_head = head;

        if let Some(released) = self.gate.frame(self.fated_head) {
            if self.fated_head.is_some_and(|(id, _)| id == released) {
                self.fated_head = None;
            }
        }
        // Where the handshake leaves the request it has hold of: at the entrance until
        // it is clear, then inside the circle until its own exit opens.
        self.held = self.gate.head.map(|(id, _, _)| {
            match self.gate.head_inside() == Some(id) {
                true => (id, (QFORK_X, QY)),
                false => (id, (GATE_WAIT_X, QY)),
            }
        });
    }
}

fn dist2(a: (f64, f64), b: (f64, f64)) -> f64 {
    let (dx, dy) = (a.0 - b.0, a.1 - b.1);
    dx * dx + dy * dy
}

/// The station's kind tag — a change of kind is a genuine transition; a same-kind update is
/// a slide along one pipe.
fn station_kind(station: Station) -> u8 {
    match station {
        Station::NetworkIn { .. } => 0,
        Station::SynBacklog { .. } => 1,
        Station::Accept { .. } => 2,
        Station::AppQueue { .. } => 3,
        Station::RunQueue { .. } => 4,
        Station::Cpu { .. } => 5,
        Station::Io { .. } => 6,
        Station::NetworkOut { .. } => KIND_NETWORK_OUT,
    }
}

/// Stations whose anchor slides along a single straight pipe frame to frame — re-aim,
/// don't re-route. (The app queue is not here: it is the placed deadline conveyor; the
/// inbound leg is placed by `p`.)
fn is_sliding(station: Station) -> bool {
    matches!(station, Station::SynBacklog { .. } | Station::RunQueue { .. })
}

/// Whether the dot is actually on a sliding station's pipe — the only place a slide
/// re-aim is a move along one pipe rather than a wall-cutting shortcut.
fn on_station_pipe(station: Station, pos: (f64, f64)) -> bool {
    match station {
        Station::SynBacklog { .. } => {
            (pos.1 - SYN_Y).abs() < 12.0 && pos.0 <= SYN_HEAD_X + 4.0
        }
        Station::RunQueue { .. } => {
            (pos.0 - PIPE_RET).abs() < 12.0
                && pos.1 > CPU_Y + CPU_H - 6.0
                && pos.1 < RUNQ_END_Y + 8.0
        }
        _ => false,
    }
}

/// Where a brand-new dot for `station` first appears (then it walks its journey). IO spawns
/// at the chamber mouth; every other station at its anchor.
fn spawn_pos(station: Station, layout: &Slots) -> (f64, f64) {
    match station {
        Station::Io { .. } => (PIPE_IO, IOC_Y + IOC_H * 0.5),
        // The inbound leg lays its own path the same frame and places the dot by `p`.
        Station::NetworkIn { .. } => (SYN_TAIL_X, SYN_Y),
        other => anchor(other, layout),
    }
}

/// The target point for a station (the inbound leg and IO follow their laid paths instead).
fn anchor(station: Station, layout: &Slots) -> (f64, f64) {
    match station {
        Station::NetworkIn { slot, .. } => syn_slot(slot, layout.syn_n.max(slot + 1)),
        Station::SynBacklog { idx } => syn_slot(idx, layout.syn_n),
        // The accept runs *after* the run queue, so its 1 ms burst is on a core (the head
        // of the handler's first CPU for the bare app). The dot rises there from the run
        // queue — where its ring is — rather than lagging behind it.
        Station::Accept { slot, .. } => slot_pos(slot),
        Station::AppQueue { idx, .. } => layout.queue_slot(idx),
        Station::RunQueue { idx } => runq_slot(idx, layout.run_n),
        Station::Cpu { slot, .. } => slot_pos(slot),
        Station::Io { .. } => (PIPE_IO, IOC_Y + IOC_H * 0.5),
        Station::NetworkOut { reply, .. } => layout.exit_end(reply),
    }
}

/// The queue-tx pipe into the app queue: up through the CPU box (the worker that
/// accepted it), out the opening in the box's top-left, along the top run and down
/// into the queue tail — the remaining corners from wherever `from` stands along the
/// course, so a dot interrupted mid-handoff (a shed verdict) re-routes through the
/// remainder rather than cutting across. On the queue pipe itself there is nothing
/// left to walk.
fn queue_tx_corners(from: (f64, f64)) -> Vec<(f64, f64)> {
    const CORNERS: [(f64, f64); 5] = [
        (PIPE_RET, CPU_Y + 40.0),
        (QTX_X, CPU_Y + 40.0),
        (QTX_X, QTX_TOP_Y),
        (QTAIL, QTX_TOP_Y),
        (QTAIL, QY),
    ];
    let (x, y) = from;
    let mut wp = Vec::new();
    let stage: usize = if y > CPU_Y + CPU_H {
        wp.push((PIPE_RET, y)); // to the return column, then up into the box
        0
    } else if x < QFORK_X + 4.0 && y > QY - 16.0 {
        return wp; // already on the queue pipe
    } else if x > CPU_X && y > CPU_Y + 40.0 + 1.0 {
        1 // in the box, below the crossing
    } else if x > QTX_X + 1.0 {
        2 // at crossing height, riding left to the rise
    } else if y > QTX_TOP_Y + 1.0 {
        if x > QTAIL + 1.0 { 3 } else { 5 } // on the rise / on the drop
    } else {
        4 // on the top run
    };
    wp.extend(&CORNERS[stage.saturating_sub(1)..]);
    wp
}

/// The admission approach: down the queue-tx corners to the fork, then in through the
/// gate's admit opening — the diagonal every request rides into the box rather than
/// cutting through a wall. Callers append whatever the route does past the gate.
fn enter_via_admission(from: (f64, f64)) -> Vec<(f64, f64)> {
    let mut wp = queue_tx_corners(from);
    wp.push((QFORK_X, QY));
    wp.push(gate_admit());
    wp
}

/// The pipe-following waypoints from `from` to a station's `target` (not including `from`).
/// A pure function of position and destination — no history — chosen by where the dot is.
fn route_to(from: (f64, f64), station: Station, target: (f64, f64)) -> Vec<(f64, f64)> {
    match station {
        // The inbound leg is placed by `p` (handled in `step`), not routed.
        Station::NetworkIn { .. } => vec![target],
        // Landing in the kernel accept queue: a slide along the one pipe.
        Station::SynBacklog { .. } => vec![target],
        // Into the run queue. A freshly accepted connection (bare app) walks off the SYN
        // pipe through the chamber mouth and up the column — the spawned handler joining
        // the runtime's queue. Admitted out of the app queue (or interrupted anywhere
        // along the queue-tx approach), it rides the admission diagonal and drops through
        // the return-pipe mouth in the CPU box's floor. Coming back from IO it is already
        // at the column, so it just slides to its slot.
        Station::RunQueue { .. } => {
            if (from.1 - SYN_Y).abs() < 15.0 && from.0 < IOC_X {
                vec![(PIPE_RET, SYN_Y), target]
            } else if from.1 < CPU_Y || (from.1 < CORRIDOR - 20.0 && from.0 < CPU_X) {
                let mut wp = enter_via_admission(from);
                wp.extend([(PIPE_RET, CPU_Y + CPU_H), target]);
                wp
            } else if in_io_pipe(from.0, from.1) {
                // Mid-descent on the io pipe: down into the chamber and across through
                // its mouths — never straight across the void between the pipes.
                vec![(PIPE_IO, IOC_Y + 10.0), (PIPE_RET, IOC_Y + 10.0), target]
            } else if (from.0 - PIPE_RET).abs() < 12.0 {
                vec![target]
            } else {
                vec![(PIPE_RET, from.1.max(CPU_Y + CPU_H)), target]
            }
        }
        // To a core: from the app queue (or its approach), down the admission diagonal;
        // from the run queue / IO region below, along to the return pipe then up it
        // (through the pipe mouths), never diagonally across a wall. The accept burst
        // rises to its core the same way (it runs on a core after the run queue).
        Station::Cpu { .. } | Station::Accept { .. } => {
            if from.1 < CPU_Y || (from.1 < CORRIDOR - 20.0 && from.0 < CPU_X) {
                let mut wp = enter_via_admission(from);
                wp.push(target);
                wp
            } else if in_io_pipe(from.0, from.1) {
                vec![
                    (PIPE_IO, IOC_Y + 10.0),
                    (PIPE_RET, IOC_Y + 10.0),
                    (PIPE_RET, CORRIDOR),
                    target,
                ]
            } else {
                vec![(PIPE_RET, from.1.max(CORRIDOR)), (PIPE_RET, CORRIDOR), target]
            }
        }
        // To the app queue: if already on the queue pipe, slide along it as it drains.
        // Otherwise this is the accept→queue handoff — the queue-tx pipe.
        Station::AppQueue { .. } => {
            if (from.1 - QY).abs() < 16.0 && from.0 < QFORK_X {
                vec![target]
            } else {
                let mut wp = queue_tx_corners(from);
                wp.push(target);
                wp
            }
        }
        Station::Io { .. } => vec![target],
        Station::NetworkOut { reply, .. } => leg_home(from, reply, target),
    }
}

/// The reply's route home (not including `from`) — the pipe it rides out to `target`, the end
/// of its own exit.
///
/// A shed request is taken **into the queue and then back out**: if it is not already on the
/// queue pipe it is routed into the tail (via the queue-tx pipe, when it comes from the accept
/// side), then it traverses the queue to the fork and up the shed diagonal — so it is always
/// seen travelling the queue before it leaves. A rejection is refused at the gate the accept
/// burst reaches, so it turns around there instead: it never entered a queue — there isn't
/// one — so it must not be shown travelling through one. Every other reply unwinds out its
/// pipe.
fn leg_home(from: (f64, f64), reply: Reply, target: (f64, f64)) -> Vec<(f64, f64)> {
    match reply.outcome() {
        Outcome::QueueTimeout => {
            let on_queue = (from.1 - QY).abs() < 16.0 && from.0 <= QFORK_X + 4.0;
            let mut wp = match on_queue {
                true => Vec::new(),
                false => route_to(from, Station::AppQueue { idx: 0, age: 0.0 }, (QTAIL, QY)),
            };
            wp.push((QFORK_X, QY));
            wp.push(gate_shed());
            wp.push((TURN_OUT, target.1));
            wp.push(target);
            wp
        }
        _ => exit_wp(from, target.1),
    }
}

/// The IO circuit out through the await chamber and back to the return-pipe mouth — a
/// polyline (not including the start) the dot walks at the engine's IO pace. Normally
/// it starts at a core, but the dot can be anywhere when its request hits IO (position
/// lags the engine), so every origin enters the chamber through a drawn opening: the
/// queue side (the pipe and its queue-tx approach) rides the admission diagonal into
/// the box; the lane enters via the chamber's own mouths.
fn io_circuit(from: (f64, f64), rng: &mut Rng) -> Vec<(f64, f64)> {
    let depth = IOC_Y + 34.0 + rng.f64() * (IOC_H - 56.0);
    let mut wp = Vec::new();
    if from.1 < CPU_Y + CPU_H - 2.0 {
        if from.1 < CPU_Y || from.0 < CPU_X {
            wp.extend(queue_tx_corners(from));
            wp.push((QFORK_X, QY));
            wp.push(gate_admit());
        }
        wp.push((PIPE_IO, CORRIDOR));
        wp.push((PIPE_IO, depth));
        wp.push((PIPE_RET, depth));
    } else {
        if from.0 < IOC_X && (from.1 - SYN_Y).abs() < 16.0 {
            wp.push((IOC_X, SYN_Y));
        } else if from.1 < IOC_Y {
            wp.push((from.0.clamp(PIPE_RET, PIPE_IO), IOC_Y + 10.0));
        }
        wp.push((PIPE_RET, depth));
    }
    wp.push((PIPE_RET, IOC_Y));
    wp
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stacked, every exit reading has to clear the pipe it hangs under *and* the pipe
    /// below it — including the goodput row, which is a line taller than the others.
    /// A fixed pitch put its last line on the success pipe.
    #[test]
    fn stacked_readouts_clear_their_pipes() {
        let lay = Layout::of(420.0);
        assert!(lay.narrow() && lay.stats_stacked(), "420px stacks");
        let ex = lay.exits();
        let pipes = [ex.timeout, ex.success, ex.processing];
        for row in 0..4 {
            let top = lay.readout_y(row) - 10.0;
            let bottom = top + ROW_LINES[row] * LN;
            for (i, &p) in pipes.iter().enumerate() {
                let (near, far) = (p - DOT, p + DOT);
                assert!(
                    bottom <= near || top >= far,
                    "row {row} ({top}..{bottom}) runs into pipe {i} at {p}",
                );
            }
        }
        // And the pipes still fit inside the box they come out of.
        assert!(ex.processing + DOT < CPU_Y + CPU_H, "the exits stay in the box");
    }

    /// The shutter moves **at most one third per frame**, and always the short way —
    /// so it can never cross the exit it is on its way to open.
    #[test]
    fn shutter_steps_one_third_the_short_way() {
        for &(from, to) in &[
            (Third::Entrance, Third::Timeout),
            (Third::Entrance, Third::Admit),
            (Third::Timeout, Third::Admit),
            (Third::Admit, Third::Timeout),
            (Third::Timeout, Third::Entrance),
            (Third::Admit, Third::Entrance),
        ] {
            let mut gate = Gate::new();
            while gate.covers != from {
                gate.step(from);
            }
            let before = gate.deg;
            gate.step(to);
            assert_eq!(
                (gate.deg - before).abs(),
                120.0,
                "one third per frame, {from:?} -> {to:?}"
            );
            assert_eq!(gate.covers, to, "one step reaches any third, {from:?} -> {to:?}");
        }
    }

    /// The handshake: each unmet condition costs a whole frame. From rest the gate is
    /// over the entrance, so a fated head waits a frame OUTSIDE for it to clear, a
    /// frame INSIDE for its own exit, and crosses on the third.
    #[test]
    fn crossing_costs_a_frame_per_condition() {
        for fate in [Fate::Admit, Fate::Shed] {
            let mut gate = Gate::new();
            assert_eq!(gate.covers, Third::Entrance, "at rest the entrance is covered");

            // Frame 1: the entrance is still covered — the head waits outside.
            assert_eq!(gate.frame(Some((7, fate))), None, "{fate:?}: no crossing on frame 1");
            assert_eq!(gate.head_inside(), None, "{fate:?}: still outside after frame 1");

            // Frame 2: the entrance is clear, so it steps inside — but the gate is not
            // yet over the third that opens its exit.
            assert_eq!(gate.frame(Some((7, fate))), None, "{fate:?}: no crossing on frame 2");
            assert_eq!(gate.head_inside(), Some(7), "{fate:?}: inside after frame 2");

            // Frame 3: the gate has arrived, and it goes.
            assert_eq!(gate.covers, fate.serving(), "{fate:?}: its exit is open");
            assert_eq!(gate.frame(Some((7, fate))), Some(7), "{fate:?}: crosses on frame 3");
            assert_eq!(gate.head, None, "{fate:?}: the gate is free again");
        }
    }

    /// With nothing fated to cross, the gate returns to its resting pose — the
    /// entrance — rather than sitting over an exit it is not serving.
    #[test]
    fn idle_gate_rests_over_the_entrance() {
        let mut gate = Gate::new();
        for _ in 0..3 {
            gate.frame(Some((1, Fate::Admit)));
        }
        assert_ne!(gate.covers, Third::Entrance, "it left the entrance to serve");
        for _ in 0..3 {
            assert_eq!(gate.frame(None), None, "nothing to release");
        }
        assert_eq!(gate.covers, Third::Entrance, "and comes back to rest");
    }

    /// A head held at a shut gate can burn its deadline: the engine turns its fate from
    /// admit to shed under it, and the gate re-aims at the other exit.
    #[test]
    fn a_held_head_can_turn_from_admit_to_shed() {
        let mut gate = Gate::new();
        gate.frame(Some((3, Fate::Admit)));
        gate.frame(Some((3, Fate::Admit)));
        assert_eq!(gate.head_inside(), Some(3), "inside, waiting on the admit exit");
        assert_eq!(gate.covers, Third::Timeout, "which is the third it aimed at");

        // The deadline elapsed while it waited.
        assert_eq!(gate.frame(Some((3, Fate::Shed))), None, "the open exit is the wrong one now");
        assert_eq!(gate.covers, Third::Admit, "so the gate swings to the other exit");
        assert_eq!(gate.frame(Some((3, Fate::Shed))), Some(3), "and it sheds");
    }
}
