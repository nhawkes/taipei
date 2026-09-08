//! The queue visualiser as a **normal DOM live** — the real taipei stack in the engine,
//! waypoint motion in `simview`, and this file: the message loop that drives them, the
//! controls around the picture, and the *to-draw* fold. The picture itself — the 960×520
//! stage, its readouts and the dot swarm — is [`MachineView`], a pure function of the
//! per-tick [`Frame`] this loop folds ([`build_frame`]) and publishes. The running system
//! is inspectable in devtools and replayable from the live's message log like any other
//! component.
//!
//! Update discipline: the live ticks at 30 Hz (`ctx.every`) and publishes one [`Frame`]
//! snapshot per tick; the controls' readouts are memos over the knobs
//! ([`Computed`](idyll::Computed)s bail on `PartialEq`), so a binding rewrites the DOM
//! only when its own value moves — the change-detection is the framework's, not
//! hand-rolled. What each snapshot means for the picture is [`MachineView`]'s to draw.

use idyll::{live_view, Ctx, Setup, Signal};
use std::rc::Rc;
use taipei::compose::{Layers, Stage};

use crate::atoms::button::{Button, ButtonKind};
use crate::atoms::code::{token_style, token_text};
use crate::atoms::controls::styles as cstyles;
use crate::atoms::invite::Invite;
use crate::atoms::slider::{raw_from_speed, speed_from_raw, Name, Scale, Slider};
use crate::atoms::toggle::{self, ToggleGroup};
use crate::engine::{Behavior, Gate, Outcome, SimEngine, DEFAULT_QPS};
use crate::simview::*;
use crate::SimMsg;

use crate::atoms::stage::Paint;
use crate::machine_view::{build_frame, Frame, MachineView, PaintState, GREEN, MUTED, RED};

/// The simulation's fixed timestep, in real ms ("Fix Your Timestep"): the engine
/// consumes time in constant 30 Hz steps regardless of display refresh rate, so the
/// sim quantizes time identically everywhere — determinism the replayable message
/// log can rely on. Virtual time per step is `ENGINE_STEP_MS × speed`.
///
/// Presentation is **snapshot interpolation**, the networked-game pattern: each tick
/// writes target transforms/fractions and the compositor tweens between them at native
/// refresh — 30 Hz of authoritative state, display-rate smoothness, zero per-frame JS.
/// The tweens themselves ride the picture ([`MachineView`]); this const only fixes the
/// cadence they interpolate across.
const ENGINE_STEP_MS: f64 = 1000.0 / 30.0;
/// Cap on accumulated catch-up steps per rendered frame (returning from a background
/// tab, a long GC): run a few steps and drop the rest of the debt — never spiral.
const MAX_CATCH_UP: f64 = 3.0;

/// The speed slider is **log-scaled**: raw `0..=100` maps to `0.0001×..=1×`, one decade
/// A memoized projection of a signal: derive `g` from `src` as a [`Computed`], which
/// bails on `PartialEq` — so the binding only touches the DOM when its value moves.
/// This is the whole update discipline; there is no manual change-check anywhere.
fn memo<S, T>(
    ctx: &Ctx<Setup, SimMsg>,
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
    ctx: &Ctx<Setup, SimMsg>,
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

// ─── control state (the knobs, applied lazily to the engine) ──────────────────

#[derive(Clone, PartialEq)]
struct Params {
    qps: f64,
    speed: f64,
    backpressure: bool,
    queue_timeout: bool,
    processing: bool,
    response_timeout_ms: f64,
    concurrency: f64,
    io_speed: f64,
    /// The selected leaf-server failure mode (the tab). Switching it rebuilds.
    behavior: Behavior,
    /// The selected admission-gate signal — `Some` only in a gate-tabbed sim.
    /// Switching it is LIVE (`SimEngine::set_gate`), never a rebuild.
    gate: Option<Gate>,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            qps: DEFAULT_QPS,
            speed: crate::engine::DEFAULT_SPEED,
            backpressure: true,
            queue_timeout: true,
            processing: false,
            response_timeout_ms: 1000.0,
            concurrency: 8.0,
            io_speed: 1.0,
            behavior: Behavior::Good,
            gate: None,
        }
    }
}

fn build_engine(p: &Params, stage: Stage, cpu_only: bool) -> SimEngine {
    let mut e = SimEngine::new(p.qps, stage, p.gate, cpu_only, p.behavior);
    e.set_speed(p.speed);
    e.set_backpressure(p.backpressure);
    e.set_queue_timeout(p.queue_timeout);
    e.set_processing_timeout(p.processing);
    e.set_response_timeout_ms(p.response_timeout_ms);
    e.set_concurrency_limit(p.concurrency);
    e.set_io_speed(p.io_speed);
    e
}

// ─── how each knob writes itself ──────────────────────────────────────────────
// A slider is handed the value and the pen: these are what its readout says.

/// Speed spans decades, so trailing zeros are trimmed off a fixed-4 format rather
/// than a precision pinned: 0.0001, 0.001, 0.01, 0.1 and 1 all read cleanly.
fn fmt_speed(raw: f64) -> String {
    let s = format!("{:.4}", speed_from_raw(raw));
    format!("{}×", s.trim_end_matches('0').trim_end_matches('.'))
}
fn fmt_qps(v: f64) -> String {
    format!("{v:.0} qps")
}
fn fmt_ms(v: f64) -> String {
    format!("{v:.0} ms")
}
fn fmt_int(v: f64) -> String {
    format!("{v:.0}")
}
fn fmt_io(raw: f64) -> String {
    format!("{:.2}×", raw / 100.0)
}

fn onoff(v: bool) -> &'static str {
    if v { "on" } else { "off" }
}

/// The manual sim's request/response state: click `GET /ping`, watch the one request
/// flow through the stage below, then read its reply. Tracks a single in-flight ping.
#[derive(Clone, PartialEq)]
enum Ping {
    Idle,
    Waiting,
    Pong(f64),
    Failed(Outcome),
}

fn ping_text(p: &Ping) -> String {
    match p {
        Ping::Idle => "click to send a request".to_string(),
        // The spinner element carries the waiting state; no text alongside it.
        Ping::Waiting => String::new(),
        Ping::Pong(lat) => format!("pong · {lat:.0} ms"),
        Ping::Failed(Outcome::Rejected) => "connection refused".to_string(),
        Ping::Failed(_) => "timed out".to_string(),
    }
}

fn ping_col(p: &Ping) -> Paint {
    match p {
        Ping::Idle | Ping::Waiting => MUTED,
        Ping::Pong(_) => GREEN,
        Ping::Failed(_) => RED,
    }
}

// ─── policies ────────────────────────────────────────────────────────────────

/// The queue's shed deadline, as a `Duration` — the manifest asks for one, and which
/// layers a stage has never depends on its value.
const TIMEOUT: std::time::Duration = std::time::Duration::from_millis(crate::engine::TIMEOUT_MS as u64);

/// One composition the reader can switch the sim to: what the pill calls it, the
/// stage that runs it, and the code that produces it (from the tab group the fence
/// claimed — empty when the fence named policies without one).
///
/// Every policy keeps its **own** machine for the life of the mount. Switching shows
/// a different one; it does not build one. That is what makes the pill a set of tabs
/// rather than a reset button — go back and the queue is as deep as you left it.
struct Policy {
    label: Rc<str>,
    stage: Stage,
}

/// The fence's stage vocabulary, in taipei's terms. Two names for one set, because
/// the content crate cannot depend on the engine — the same seam [`Gate::from_signal`]
/// crosses, and the reason a fence never carries a stage as a string.
/// What the visualiser runs for a fence that named no stage: the full stack, which is
/// where the book ends up. Named here rather than taken from `Stage::default()`, because
/// which composition an unconfigured fence shows is this sim's decision and not a
/// property of the vocabulary.
const UNNAMED: Stage = Stage::Queue;

fn stage_of(stage: blog_core::PolicyStage) -> Stage {
    match stage {
        blog_core::PolicyStage::App => Stage::App,
        blog_core::PolicyStage::Backpressure => Stage::Backpressure,
        blog_core::PolicyStage::Reject => Stage::Reject,
        blog_core::PolicyStage::Wait => Stage::Wait,
        blog_core::PolicyStage::Queue => Stage::Queue,
    }
}

impl Policy {
    /// The policies a fence names — each a label and the stage it runs. The listing
    /// under each pill is the captured source of that stage's real composition
    /// ([`crate::compose::src_for`]), so a policy carries no code of its own.
    fn of(key: &blog_core::SimKey) -> Vec<Policy> {
        if key.tabs.is_empty() {
            return vec![Policy { label: "".into(), stage: key.stage.map_or(UNNAMED, stage_of) }];
        }
        key.tabs
            .iter()
            .map(|tab| Policy { label: tab.display.as_str().into(), stage: stage_of(tab.stage) })
            .collect()
    }
}

/// One policy's running machine: the engine, the motion derived from it, and the
/// paint state that diffs its frames. A sim holds one per policy for the life of the
/// mount — switching policy changes which one is shown and ticked, never what exists.
struct Machine {
    engine: Option<SimEngine>,
    vs: ViewState,
    ps: PaintState,
    /// Left-over real time from the last rendered frame, held so the fixed timestep
    /// stays fixed across a frame boundary — and across being hidden and shown again.
    accum: f64,
}


/// Apply a live setter to every *built* engine. Unbuilt policies rebuild lazily, so only
/// `Some` engines take a mid-flight retune — the one place that "built engines only" guard
/// lives, rather than restated at every knob.
fn each_engine(machines: &mut [Machine], mut set: impl FnMut(&mut SimEngine)) {
    for m in machines.iter_mut() {
        if let Some(engine) = &mut m.engine {
            set(engine);
        }
    }
}

impl Machine {
    fn new(lay: Layout) -> Machine {
        Machine { engine: None, vs: ViewState::new(lay), ps: PaintState::new(), accum: 0.0 }
    }

    /// Build this machine and its opening still: a single request in flight toward TCP SYN
    /// (`SimEngine::open_incoming` places it mid-network, deterministically), about to be set
    /// moving — not a queue already under load and not an empty diagram. A sim with no
    /// arrivals of its own (the manual `GET /ping` stages) has nothing to inject into — its
    /// whole point is the one request the reader sends — so it opens empty, as it should.
    fn settle(&mut self, p: &Params, stage: Stage, cpu_only: bool) -> Frame {
        let eng = self.engine.insert(build_engine(p, stage, cpu_only));
        if p.qps > 0.0 {
            eng.open_incoming();
        } else {
            eng.tick(0.0);
        }
        let obs = eng.obs();
        self.vs.absorb_subtick(obs);
        self.vs.step(obs);
        build_frame(eng.obs(), &self.vs, &mut self.ps)
    }

    /// Rebuild at the current knobs, discarding whatever was running. The reset button
    /// and a server switch; never a policy switch.
    fn reset(&mut self, lay: Layout) {
        *self = Machine::new(lay);
    }
}

// ─── the live component ─────────────────────────────────────────────────────

/// The `queue-viz` sim: `stage` (from the fence's `stage=` param) selects the taipei
/// composition; `workload="cpu"` selects the single-burst request shape. The stage
/// geometry is fixed at 960×520 — the fence's width/height are advisory here.
pub(crate) async fn run(
    ctx: Ctx<Setup, SimMsg>,
    _seed: crate::PageSeed,
    key: blog_core::SimKey,
) -> idyll::Result {
    let (charts_on, manual) = (key.charts, key.manual);
    let (servers, gate_signals, try_control) = (&key.servers, &key.gates, key.try_control);
    // The policies the fence offers, each a real composition. With none named, the
    // sim is the one stage the fence asked for — the pill simply has nothing to show.
    let policies: Vec<Policy> = Policy::of(&key);
    let policy = ctx.mutable_signal(0usize);
    let changes = ctx.mutable_signal(0usize);
    let policy_change = changes.read();
    let policy_at = policy.read();
    let stage = policies[0].stage;
    let cpu_only = key.workload == "cpu";
    // The selectable server behaviors (tabs) and their initial pick.
    let behaviors: Vec<Behavior> = servers.iter().map(|b| Behavior::from_name(b.as_str())).collect();
    let tab_labels: Vec<&'static str> = servers.iter().map(|b| b.label()).collect();
    let show_tabs = servers.len() > 1;
    // The selectable admission-gate signals (their own tab row), likewise.
    let gates: Vec<Gate> = gate_signals.iter().map(|&g| Gate::from_signal(g)).collect();
    let gate_labels: Vec<&'static str> = gate_signals.iter().map(|g| g.label()).collect();
    let has_gates = gates.len() > 1;
    let gate0 = gates.first().copied();
    let layers0 = stage.manifest(std::time::Duration::from_millis(100));
    // Which parts of the machine the picture draws. These follow the *selected*
    // policy, because each policy is a different composition — a reject stage has no
    // queue to draw, a bare app has no admission gate. Derived once here, as memos, so
    // every `@if` in the view asks the same question.
    let stages: Vec<Stage> = policies.iter().map(|p| p.stage).collect();
    // The opening position's own layers — what the *defaults* and the at-rest frame are
    // chosen against. The memos below follow the reader's choice; these do not move.
    let (q0, t0, r0) = (layers0.queue, layers0.queue_timeout.is_some(), layers0.reject);
    // Each policy's manifest, resolved once. The picture asks these rather than
    // re-deriving a composition from its name, so a stage's layers are read from one
    // place — and the per-frame answers below come from the machine itself.
    let manifests: Vec<Layers> = stages.iter().map(|s| s.manifest(TIMEOUT)).collect();
    let controls = |f: Box<dyn Fn(&Layers) -> bool>| {
        let manifests = manifests.clone();
        memo(&ctx, &policy_at, move |&i| f(&manifests[i]))
    };

    // Which knobs a composition *can* offer is a property of the stage, not of this
    // frame — a backpressure toggle exists whether or not it is currently on.
    let show_bp = {
        let gated = has_gates;
        controls(Box::new(move |l| l.backpressure && !gated))
    };
    let show_tmo = controls(Box::new(|l| l.queue_timeout.is_some()));
    let show_server = {
        let gated = has_gates;
        controls(Box::new(move |l| {
            l.backpressure || l.queue || l.limit.is_some() || gated
        }))
    };

    // 30 Hz tick, gated on `running` (Elm's on-tick): between ticks the compositor
    // interpolates, so the live has no per-frame work.
    let running = ctx.mutable_signal(false);
    ctx.every(&running.read(), std::time::Duration::from_millis(33), SimMsg::Tick);
    // The stage lays itself out to the width it is given; the first measurement lands
    // on hydration, behind the click-to-run scrim.
    ctx.resizes(SimMsg::Resize);

    // The two reactive sources the whole picture derives from: the control knobs and
    // the per-tick sim snapshot (seeded with the stage's at-rest picture). Defaults are
    // the story's opening position:
    //
    // - **Speed.** A manual sim crawls — the reader follows one request with their
    //   eyes. The bare-app sims run slow enough to read the lifecycle; the protection
    //   stages run faster, because by then the parts are known and the broad behaviour
    //   is the point.
    // - **Arrivals.** The bare app opens comfortably underloaded (cpu-only capacity is
    //   ~360/s, mixed ~100/s), so the reader pushes it into overload themselves — the
    //   `try` highlight marks that slider when the prose asks. The protection stages
    //   open already under the load they exist for: the queue absorbing a heavy spike,
    //   the limiter steadily rejecting the overflow while serving within its limit.
    // - **Client patience.** A flat 1 s. A healthy server's honest round trip is well
    //   under that (~0.5 s worst case), so it times nobody out; under overload latency
    //   explodes past any deadline, so the exact value barely matters — 1 s reads as a
    //   plausibly impatient client without dragging the bad-server "gives up" beat.
    // Paced against the network model: an in-region leg is 6 virtual ms, so a frame has
    // to buy little enough virtual time that the short legs still read as travel. The
    // manual sims go slowest — they show ONE request's lifecycle end to end, and a ping
    // is over in ~35 virtual ms, so anything faster is a flash rather than a journey.
    // Every sim opens at the slow viewing speed — the one the motion is tuned around
    // (see [`crate::simview`]'s pursuit floor), and the middle of the speed slider's
    // travel, so the reader has as much room to speed the machine up as to slow it
    // down. A sim that opens fast has already skipped the part worth watching.
    let speed_default = 0.01;
    let qps_default = if manual {
        0.0
    } else if has_gates {
        // The gate comparison opens *sustainable*: every gate rests open, and the
        // reader creates the incident themselves by slowing IO down.
        88.0
    } else if q0 {
        375.0
    } else if r0 {
        75.0
    } else if cpu_only {
        150.0
    } else {
        62.0
    };
    let rtmo_default = 1000.0;
    let cc_default = crate::engine::default_limit(stage) as f64;
    let params0 = Params {
        qps: qps_default,
        speed: speed_default,
        response_timeout_ms: rtmo_default,
        behavior: behaviors.first().copied().unwrap_or(Behavior::Good),
        gate: gate0,
        concurrency: cc_default,
        ..Params::default()
    };
    let params = ctx.mutable_signal(params0.clone());
    // The manual request/response affordance: the reply to the last `GET /ping`.
    let ping = ctx.mutable_signal(Ping::Idle);
    // Which server tab is active (index into `behaviors`), for highlighting. Named
    // for what it selects: this sim has three independent selections and a shared
    // name for "the current index" is how one silently becomes another.
    let active_tab = ctx.mutable_signal(0usize);
    let server_at = active_tab.read();
    // Each tab: its (static) index and label, plus a reactive "is this one active?"
    // signal — so a tab switch restyles only the tabs whose highlight changed.
    // A leaf server that never accepts, or accepts and hangs, is a failure mode — the
    // bar says so before the reader has to infer it from the dots.
    let tabs: Vec<toggle::ToggleItem> = tab_labels
        .iter()
        .copied()
        .zip(servers.iter().copied())
        .enumerate()
        .map(|(i, (label, behavior))| {
            let tone = match behavior {
                blog_core::ServerBehavior::Good => toggle::Tone::Normal,
                _ => toggle::Tone::Alarm,
            };
            (i, label.into(), memo(&ctx, &server_at, move |&active| active == i), tone)
        })
        .collect();
    // The gate tabs, the same shape.
    let active_gate = ctx.mutable_signal(0usize);
    let ag = active_gate.read();
    let gate_tabs: Vec<toggle::ToggleItem> = gate_labels
        .iter()
        .copied()
        .enumerate()
        .map(|(i, label)| {
            (i, label.into(), memo(&ctx, &ag, move |&active| active == i), toggle::Tone::Normal)
        })
        .collect();
    // The policy pill, when the fence named more than one.
    let has_policies = policies.len() > 1;
    let policy_items: Vec<toggle::ToggleItem> = policies
        .iter()
        .enumerate()
        .map(|(i, p)| {
            // A policy wears its own outcome: one that turns requests away and one
            // that holds them are not the same choice, and the switch should say so.
            let tone = match p.stage.manifest(TIMEOUT).reject {
                true => toggle::Tone::Shed,
                false => toggle::Tone::Serve,
            };
            (i, p.label.clone(), memo(&ctx, &policy_at, move |&picked| picked == i), tone)
        })
        .collect();
    let policy_knob = toggle::knob(&ctx, &policy_items);
    // The listing under a pill is the **captured source of the function the engine runs**
    // for that stage — `#[shown]`, not a hand-copied fence — so the code cannot drift
    // from the machine below it. Policy tabs carry no gate; the gate sims show theirs in
    // the panel instead.
    let policy_code = {
        let coloured: Vec<Vec<crate::atoms::code::Token>> = policies
            .iter()
            .map(|p| crate::atoms::code::highlight(crate::compose::src_for(p.stage, None)))
            .collect();
        let picked = policy_at.clone();
        ctx.synced(
            move |cx| coloured.get(picked.get(cx)).cloned().unwrap_or_default(),
            |t: &crate::atoms::code::Token| t.i,
        )
    };
    let tab_knob = toggle::knob(&ctx, &tabs);
    let gate_knob = toggle::knob(&ctx, &gate_tabs);
    // The picture before the machine has run: the composition's own layers, and nothing
    // in it yet. The browser replaces it with the still as it measures the stage.
    let (bp0, limit0) = match gate0 {
        None | Some(Gate::RuntimeCpu) => (layers0.backpressure, layers0.limit),
        Some(Gate::ConcurrencyLimit) => (false, Some(cc_default as usize)),
        Some(Gate::OsCpu) => (false, layers0.limit),
    };
    let frame = ctx.mutable_signal(Rc::new(Frame::at_rest(bp0, q0, t0, r0, limit0)));

    let fr = frame.read();

    let has_limit = memo(&ctx, &fr, |f| f.limit().is_some());
    // The width the picture is drawn to. The server paints at the drawing width; the
    // browser measures the stage on hydration and this becomes the real one.
    let layout = ctx.mutable_signal(Layout::of(STAGE_W));
    let pr = params.read();

    // Any admission gate at all — backpressure, a limit, or the OS-CPU average.
    let cc_gate_on = memo(&ctx, &pr, |p| p.gate == Some(Gate::ConcurrencyLimit));
    // What each slider is set to, in its own raw units. A slider derives its readout
    // and its filled rail from this one signal, so nothing here formats them.
    let speed_at = memo(&ctx, &pr, |p| raw_from_speed(p.speed));
    let qps_at = memo(&ctx, &pr, |p| p.qps);
    let rtmo_at = memo(&ctx, &pr, |p| p.response_timeout_ms);
    // Under the OS-CPU signal the ceiling is the controller's, so the dial reads the
    // live one; under the limit signal it reads what the reader set.
    let cc_at = memo2(&ctx, &fr, &pr, |f, p| match p.gate {
        Some(Gate::OsCpu) => f.limit().unwrap_or(p.concurrency as usize) as f64,
        _ => p.concurrency,
    });
    let cc_driven = memo(&ctx, &pr, |p| p.gate == Some(Gate::OsCpu));
    let io_at = memo(&ctx, &pr, |p| p.io_speed * 100.0);
    let bp_btn = memo(&ctx, &pr, |p| format!("CPU backpressure: {}", onoff(p.backpressure)));
    let tmo_btn = memo(&ctx, &pr, |p| format!("queue timeout 100 ms: {}", onoff(p.queue_timeout)));
    let ptmo_btn = memo(&ctx, &pr, |p| format!("processing timeout: {}", onoff(p.processing)));
    let run_lbl = {
        let running = running.read();
        ctx.computed(move |cx| if running.get(cx) { "pause" } else { "run" }.to_string()).read()
    };
    // The buttons whose word is fixed. `Button` reads a signal either way, so the
    // difference between a label that moves and one that does not stays here.
    let ping_btn = ctx.constant("GET /ping".to_string());
    let reset_btn = ctx.constant("reset".to_string());
    let inject_btn = ctx.constant("send a request".to_string());
    // The stage is armed while it holds the still.
    let armed = memo(&ctx, &running.read(), |&r| !r);

    let pg = ping.read();
    let ping_lbl = memo(&ctx, &pg, ping_text);
    let ping_sty = memo(&ctx, &pg, |p| format!("color:{}", ping_col(p)));
    let ping_waiting = memo(&ctx, &pg, |p| matches!(p, Ping::Waiting));

    // The invitation the prose makes, and whether it still stands: one control at a
    // time, and moving it answers it, so the glow stops the moment the reader takes
    // the hint. Every slider asks the same question — *am I the invited one?* — which
    // is how the controls the prose never invites answer it without a stand-in signal.
    let invitation = ctx.mutable_signal(try_control);
    let open = invitation.read();
    let invite_arrivals =
        memo(&ctx, &open, |o| *o == Some(blog_core::TryControl::Arrivals));
    let invite_io = memo(&ctx, &open, |o| *o == Some(blog_core::TryControl::IoSpeed));
    let limit_row = memo2(&ctx, &has_limit, &cc_gate_on, |&l, &g| l || g);
    let server_knobs = {
        let gated = has_gates;
        memo(&ctx, &has_limit, move |&l| l || gated)
    };
    // The listing fades in on each change. Alternating names is what re-triggers it:
    // re-setting one name on an element already wearing it is not a new animation.
    let policy_fade = memo(&ctx, &policy_change, |&n| {
        format!("animation:{} 220ms ease", if n % 2 == 0 { cstyles::FadeA } else { cstyles::FadeB })
    });
    // The panel's listing follows the selected gate, because each gate is its own
    // composition and the reader must see the one that is running.
    let panel_code = {
        let gate = memo(&ctx, &pr, |p| p.gate);
        ctx.synced(
            move |cx| crate::atoms::code::highlight(crate::compose::src_for(stage, gate.get(cx))),
            |t: &crate::atoms::code::Token| t.i,
        )
    };

    let mut ctx = ctx.render(live_view! {
        div css=[cstyles::QV] {
            // ── the policy pill: which composition the machine below runs, and the
            //    code that produces it. Each policy keeps its own machine. ──
            @if (has_policies) {
                div css=[cstyles::POLICY] {
                    ToggleGroup items=(policy_items) knob=(policy_knob) picked=>(|i| SimMsg::SetPolicy(i))
                    pre css=[cstyles::POLICY_CODE] style=($policy_fade) {
                        code {
                            @for (_, tok) in $policy_code {
                                span style=(token_style(&$tok)) { (token_text(&$tok)) }
                            }
                        }
                    }
                }
            }

            // ── server tabs: pick the leaf-server behavior to compare ──
            @if (show_tabs) {
                ToggleGroup items=(tabs.clone()) knob=(tab_knob.clone()) picked=>(|i| SimMsg::SetBehavior(i))
            }

            // ── the running composition, verbatim (`#[shown]` capture). A policy pill
            //    already shows the composition it names, so this is for the sims that
            //    have no pill — two listings of one stack would be one too many. ──
            @if (!has_policies) {
                details css=[crate::atoms::code::styles::DETAILS] open=("") {
                    summary { "taipei setup" }
                    pre { code {
                        @for (_, tok) in $panel_code {
                            span style=(token_style(&$tok)) { (token_text(&$tok)) }
                        }
                    } }
                }
            }

            // ── manual request/response: send one GET /ping, watch it flow, read pong ──
            @if (manual) {
                div css=[cstyles::PING] {
                    Button kind=(ButtonKind::Primary) label=(ping_btn) pressed=>(|_| SimMsg::Ping)
                    span css=[cstyles::PING_ARROW] { "→" }
                    @if ($ping_waiting) {
                        span css=[cstyles::SPINNER] {}
                    }
                    span css=[cstyles::PING_REPLY] style=($ping_sty) { $ping_lbl }
                }
            }

            MachineView frame=(frame) layout=(layout) charts_on=(charts_on) armed=(armed) on_click=>(|_| SimMsg::Start)

            // ── controls ──
            div css=[cstyles::CONTROLS] {
                div css=[cstyles::ROW] {
                    span css=[cstyles::GRP] { "simulation" }
                    div css=[cstyles::ROW_BODY] {
                        Button kind=(ButtonKind::Cta) label=(run_lbl) pressed=>(|_| SimMsg::Toggle)
                        Slider name=(Name::new("speed", 44)) scale=(Scale::new(0, 100, 1)) at=(speed_at) fmt=(fmt_speed) moved=>(|n| SimMsg::Speed(n))
                        Button kind=(ButtonKind::Solid) label=(reset_btn) pressed=>(|_| SimMsg::Reset)
                    }
                }
                // A manual sim is driven by the `GET /ping` button above the stage; its
                // auto-load knobs (arrival rate, response deadline) don't apply.
                @if (!manual) {
                    div css=[cstyles::ROW] {
                        span css=[cstyles::GRP] { "client" }
                        div css=[cstyles::ROW_BODY] {
                            Button kind=(ButtonKind::Primary) label=(inject_btn) pressed=>(|_| SimMsg::Inject)
                            Invite when=(invite_arrivals) hint=("watch the queue fill") {
                                Slider name=(Name::new("arrivals", 52)) scale=(Scale::new(1, 400, 1)) at=(qps_at) fmt=(fmt_qps) moved=>(|n| SimMsg::Qps(n))
                            }
                            Slider name=(Name::new("response timeout", 118)) scale=(Scale::new(100, 5000, 100)) at=(rtmo_at) fmt=(fmt_ms) moved=>(|n| SimMsg::RespTimeoutMs(n))
                        }
                    }
                }
                @if ($show_server) {
                    div css=[cstyles::ROW] {
                        span css=[cstyles::GRP] { "server" }
                        div css=[cstyles::ROW_BODY] {
                        // The admission signal, as a live toggle-button group: switching
                        // retunes the running machine (never a rebuild), so the reader
                        // flips policies mid-incident and watches the takeover.
                        @if (has_gates) {
                            ToggleGroup items=(gate_tabs.clone()) knob=(gate_knob.clone()) picked=>(|i| SimMsg::SetGate(i))
                        }
                        @if ($show_bp) {
                            Button kind=(ButtonKind::Ghost) label=(bp_btn) pressed=>(|_| SimMsg::ToggleBackpressure)
                        }
                        @if ($show_tmo) {
                            Button kind=(ButtonKind::Ghost) label=(tmo_btn) pressed=>(|_| SimMsg::ToggleQueueTimeout)
                            Button kind=(ButtonKind::Ghost) label=(ptmo_btn) pressed=>(|_| SimMsg::ToggleProcessingTimeout)
                        }
                        @if ($limit_row) {
                            Slider name=(Name::new("concurrency limit", 118)) scale=(Scale::new(1, 48, 1)) at=(cc_at) fmt=(fmt_int) moved=>(|n| SimMsg::Concurrency(n)) ?driven=(cc_driven)
                        }
                        @if ($server_knobs) {
                            Invite when=(invite_io) hint=("watch the IO chamber") {
                                Slider name=(Name::new("IO speed", 60)) scale=(Scale::new(10, 200, 5)) at=(io_at) fmt=(fmt_io) moved=>(|n| SimMsg::IoSpeed(n))
                            }
                        }
                        }
                    }
                }
            }
        }
    }).await?;

    // Until the browser measures the stage, the picture is laid out at its drawing
    // width — which is what the server paints, and what a stage wide enough to hold
    // the whole diagram gets anyway.
    let mut lay = Layout::of(STAGE_W);
    // One machine per policy, each with its own motion. Only the shown one ticks; the
    // rest hold exactly where they were, so switching away and back returns the reader
    // to the queue they left rather than to an empty stage.
    let mut machines: Vec<Machine> = stages.iter().map(|_| Machine::new(lay)).collect();
    let mut shown = 0usize;
    // Whether the stage has been measured yet. The first measurement is also when the
    // still is built — see [`SETTLE_MS`].
    let mut measured = false;
    // The in-flight `GET /ping`, if one is outstanding. The reply text resolves on the
    // engine's departure, which *is* the response reaching the client: a served or shed
    // verdict rides its leg home first, so the pong and the dot land together. The client
    // that gave up departs at the give-up, having nothing to wait for.
    let mut ping_id: Option<u32> = None;

    loop {
        let (msg, reducer) = ctx.recv().await?;
        let turn: idyll::Turn = (&reducer).into();
        // Set by the arms that change the machine itself (reset, a server switch —
        // a gate switch is live): the engine is rebuilt fresh at the current knobs
        // after the match.
        let mut rebuild = false;
        match msg {
            SimMsg::Tick(dt) => {
                let cur = params.now(&turn);
                let policy = stages[shown];
                let Machine { engine, vs, ps, accum } = &mut machines[shown];
                let eng = engine.get_or_insert_with(|| build_engine(&cur, policy, cpu_only));
                // Fixed-timestep simulation, per-frame render: the engine ticks in
                // whole ENGINE_STEP_MS quanta out of the accumulator; motion and the
                // frame publish run once per rendered frame with the real frame delta.
                *accum = (*accum + dt.max(0.0)).min(ENGINE_STEP_MS * MAX_CATCH_UP);
                while *accum >= ENGINE_STEP_MS {
                    *accum -= ENGINE_STEP_MS;
                    eng.tick(ENGINE_STEP_MS);
                    // Hand this tick's give-ups to the motion layer (with the ids still
                    // live after it, so a zombie handler isn't peeled off its core) before
                    // the clock races ahead and only the final snapshot survives.
                    let obs = eng.obs();
                    let last_lat = obs.last_latency_ms;
                    let departures = vs.absorb_subtick(obs);
                    // The tracked ping is answered when its verdict reaches the client.
                    if let Some(pid) = ping_id {
                        if let Some(&(_, outcome)) = departures.iter().find(|(id, _)| *id == pid) {
                            ping.set(&turn, match outcome {
                                Outcome::Success => Ping::Pong(last_lat.unwrap_or(0.0)),
                                other => Ping::Failed(other),
                            });
                            ping_id = None;
                        }
                    }
                }
                vs.step(eng.obs());
                frame.set(&turn, Rc::new(build_frame(eng.obs(), vs, ps)));
            }
            SimMsg::Toggle => running.update(&turn, |r| *r = !*r),
            SimMsg::Start => running.set(&turn, true),
            SimMsg::Qps(v) => {
                invitation.set(&turn, None);
                params.update(&turn, |p| p.qps = v);
                each_engine(&mut machines, |e| e.set_lambda(v));
            }
            SimMsg::Speed(raw) => {
                let speed = speed_from_raw(raw);
                params.update(&turn, |p| p.speed = speed);
                each_engine(&mut machines, |e| e.set_speed(speed));
            }
            SimMsg::RespTimeoutMs(v) => {
                params.update(&turn, |p| p.response_timeout_ms = v);
                each_engine(&mut machines, |e| e.set_response_timeout_ms(v));
            }
            SimMsg::Concurrency(v) => {
                params.update(&turn, |p| p.concurrency = v);
                each_engine(&mut machines, |e| e.set_concurrency_limit(v));
            }
            SimMsg::IoSpeed(raw) => {
                invitation.set(&turn, None);
                params.update(&turn, |p| p.io_speed = raw / 100.0);
                each_engine(&mut machines, |e| e.set_io_speed(raw / 100.0));
            }
            SimMsg::ToggleBackpressure => {
                params.update(&turn, |p| p.backpressure = !p.backpressure);
                { let on = params.now(&turn).backpressure; each_engine(&mut machines, |e| e.set_backpressure(on)); }
            }
            SimMsg::ToggleQueueTimeout => {
                params.update(&turn, |p| p.queue_timeout = !p.queue_timeout);
                { let on = params.now(&turn).queue_timeout; each_engine(&mut machines, |e| e.set_queue_timeout(on)); }
            }
            SimMsg::Inject => {
                if let Some(e) = &mut machines[shown].engine { e.inject(); }
            }
            SimMsg::Ping => {
                // Start the sim if idle, send one request, and watch for its reply.
                running.update(&turn, |r| *r = true);
                let cur = params.now(&turn);
                let eng = machines[shown]
                    .engine
                    .get_or_insert_with(|| build_engine(&cur, stages[shown], cpu_only));
                ping_id = Some(eng.inject());
                ping.set(&turn, Ping::Waiting);
            }
            SimMsg::SetBehavior(i) => {
                // Switching to a different server is a fresh machine.
                if let Some(&b) = behaviors.get(i) {
                    active_tab.update(&turn, |a| *a = i);
                    params.update(&turn, |p| p.behavior = b);
                    rebuild = true;
                }
            }
            SimMsg::SetGate(i) => {
                // Live: the same machine keeps running under the new signal.
                if let Some(&g) = gates.get(i) {
                    active_gate.update(&turn, |a| *a = i);
                    params.update(&turn, |p| p.gate = Some(g));
                    each_engine(&mut machines, |e| e.set_gate(g));
                }
            }
            SimMsg::ToggleProcessingTimeout => {
                params.update(&turn, |p| p.processing = !p.processing);
                { let on = params.now(&turn).processing; each_engine(&mut machines, |e| e.set_processing_timeout(on)); }
            }
            SimMsg::Reset => rebuild = true,
            SimMsg::SetPolicy(i) => {
                // Show a different machine. The one being left keeps everything it had,
                // so coming back is a return, not a restart.
                if i < machines.len() {
                    shown = i;
                    policy.set(&turn, i);
                    changes.update(&turn, |n| *n += 1);
                    let cur = params.now(&turn);
                    let machine = &mut machines[shown];
                    // A policy shown for the first time has never run, so it gets the
                    // same still the stage opened with — switching compares two
                    // machines under load, not one running against one empty.
                    let next = match &mut machine.engine {
                        Some(eng) => build_frame(eng.obs(), &machine.vs, &mut machine.ps),
                        None => machine.settle(&cur, stages[shown], cpu_only),
                    };
                    frame.set(&turn, Rc::new(next));
                }
            }
            SimMsg::Resize(width) => {
                // A new width is a new picture. The engine's state is width-independent,
                // but every dot's route was walked in the old geometry, so motion starts
                // again rather than teleporting mid-leg.
                let next = Layout::of(width);
                if next != lay {
                    lay = next;
                    layout.set(&turn, lay);
                    rebuild = true;
                }
                // The stage has a width for the first time — build the still it holds
                // until the reader releases it.
                rebuild |= !std::mem::replace(&mut measured, true);
            }
        }
        if rebuild {
            // Every policy starts over: the knobs or the geometry moved under all of
            // them, so none of their held state describes the machine any more.
            let cur = params.now(&turn);
            for machine in &mut machines {
                machine.reset(lay);
            }
            ping.set(&turn, Ping::Idle);
            ping_id = None;
            let machine = &mut machines[shown];
            // A machine the reader is watching run refills itself within the second;
            // one that is holding the still has to be handed a still to hold.
            let next = match running.now(&turn) {
                true => {
                    let eng = machine.engine.insert(build_engine(&cur, stages[shown], cpu_only));
                    build_frame(eng.obs(), &machine.vs, &mut machine.ps)
                }
                false => machine.settle(&cur, stages[shown], cpu_only),
            };
            frame.set(&turn, Rc::new(next));
        }
    }
}
