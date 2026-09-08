//! **The blame panel** — where a shut gate's cost goes. One [`SimEngine::tenanted`] server with
//! three tenants on it, its [`TenantReporter`](taipei::tenant::TenantReporter) reading the gate
//! from below the queue, drawn twice: the accounting on top, and the machine the accounting is
//! about ([`MachineView`]) underneath, so every microsecond in the strip belongs to a request the
//! reader can watch move.
//!
//! The strip **is** the bill. `World::pump` is a discrete-event stepper, so between two of its
//! events nothing changed: one interval becomes one cell per occupier, and the meter's wind across
//! that interval is what each of them accrued ([`BlameSpan`]). Summing cells is therefore summing
//! the library's own draws — the picture cannot disagree with `attributed()`, because it is not a
//! second calculation of it.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::rc::Rc;
use std::time::Duration;

use idyll::{live_view, Ctx, Rect, Setup, Signal};
use idyll_styles::styles;

use crate::atoms::figure::slots;
use crate::atoms::controls::styles as cstyles;
use crate::atoms::framed::Framed;
use crate::atoms::invite::Invite;
use crate::atoms::latency_table::{Bar, LatencyTable};
use crate::atoms::slider::{fmt_qps, fmt_speed, raw_from_speed, speed_from_raw, Name, Scale, Slider};
use crate::atoms::stage::{Paint, Stage};
use crate::atoms::server_box::server_name;
use crate::atoms::switch::Switch;
use crate::atoms::tenant_stage::{Measured, ServerRow, TenantDot, TenantStage, Wire};
use crate::atoms::toggle::{knob, Tone, ToggleGroup, ToggleItem};
use crate::engine::{
    core_ink_of, on_the_wire, tenant_of, BlameSpan, Obs, Outcome, SimEngine, Station, HOMEWARD,
    TENANTS,
};
use crate::atoms::button::{Button, ButtonKind};
use crate::machine_view::{build_frame, Chip, MachineView, PaintState};
use crate::multi::{
    axis_for, Latencies, Record, Summary, HALF_LIFE_MS, PROCESSING_TIME, QUEUE_TIME, SUMMARY_MS,
};
use crate::simview::{Layout, ViewState, STAGE_W};
use blog_core::SimKey;

/// The engine steps at 30 Hz — one quantum of virtual time per rendered frame, the pace the
/// motion layer below the panel is tuned around.
const ENGINE_STEP_MS: f64 = 1000.0 / 30.0;
/// The most catch-up a slow frame may buy, so a stall doesn't fast-forward the machine.
const MAX_CATCH_UP: f64 = 3.0;

/// Virtual ms bought by one real ms, as the panel opens. This is the speed control's starting
/// position, not the engine's default ([`DEFAULT_SPEED`](crate::engine::DEFAULT_SPEED), which
/// three other pages are tuned against) — the reader moves it from here.
const SPEED: f64 = 0.01;
/// The short window: how much virtual time the four fine rows show.
const WINDOW_MS: f64 = 800.0;
/// `now` sits here, leaving a gutter to its right so labels don't clip.
const NOW_X: f64 = 97.5;
/// How many windows the strip's track spans.
///
/// A settled cell never changes: it was minted at a fixed span of virtual time and, by the
/// strip's own invariant, is never redrawn. Only the *window* moves. So the cells are laid out
/// in virtual-time coordinates on a track that does not move, and the window slides across it as
/// a single `translateX` — one write a frame instead of one per cell. This is law 4's shape
/// applied to the strip: the swarm emits one path per frame swept by one shared animation,
/// and CSS stays stateless per window. Expressing the invariant in the geometry is what makes
/// it cheap; recomputing every cell to say "everything moved left one pixel" was fighting it.
///
/// The track is finite, so the epoch it is measured from is re-based when the window nears its
/// end — the one frame that does rewrite every cell, once per seven windows.
const TRACK_WINDOWS: f64 = 8.0;
/// The track's width, as a multiple of the plot's: one window is [`NOW_X`] of the plot.
const TRACK_W: f64 = TRACK_WINDOWS * NOW_X;
/// The queueing strip: a binary signal needs no more room than this.
const QUEUE_H: f64 = 22.0;
/// The blame strip's height. A cell's is this over the occupancy that minted it, so a lone
/// occupier fills it.
const LANES_H: f64 = 28.0;
/// The conveyor: this many rows of this height, clipped. Deep enough that a bar clears the
/// now-rule by the width its reading needs before it drifts off the top.
const REQ_ROW: f64 = 15.0;
const REQ_ROWS: f64 = 18.0;
/// How fast a conveyor row eases toward its slot, per frame.
const DRIFT: f64 = 0.16;
/// How far a ghost's echo reaches along the strip — the reference panel's share of its window.
/// This is *where* it is drawn, so it stays in virtual time with the cells around it.
const GHOST_MS: f64 = WINDOW_MS * 2600.0 / 30_000.0;
/// How long the panel lets a reader orient before it will offer them the speed knob, in **real**
/// milliseconds — one half of that offer's condition, the other being that the breakdown switch's
/// invitation has been answered.
///
/// [`SPEED`] is not a compromise: at `0.01×` a reader can follow one request through the stations
/// and work out what each row is saying, which is what the opening is for. The speed invitation is
/// the *graduation* out of that — into the regime where the aggregates fill and the fairness
/// argument becomes visible. So it waits on both halves, and they answer different things. The
/// beat is why it does not fire at all on load: an invitation glowing there would pull the reader
/// straight past the thing slow exists for. Waiting on the switch is why it never fires *beside*
/// another — the switch is a feature to discover, this is the next step to take, and a panel that
/// asks for two things at once is asking for neither. Together they mean at most one invitation
/// glows here at any moment.
///
/// The cost is deliberate and Nathan's: a reader who never takes the switch is never offered the
/// speed, and so never sees the aggregates fill.
const ORIENT_MS: f64 = 20_000.0;
/// How long that echo takes to fade, in **real** milliseconds.
///
/// The fade carries no engine truth: the departure is the fact, and the ghost is the eye being
/// given a moment to see the survivors thicken underneath. So it is paced by the wall clock and
/// reads the same at every speed — at `0.01×` a virtual-time fade would take minutes, because
/// virtual time is barely moving, and that is not a slower truth, just an unreadable one.
const GHOST_FADE_MS: f64 = 400.0;
/// Where the taximeter bar reads full. Above what an occupier of this server racks up, so the
/// bar stays a comparison between lanes rather than a row of full ones.
const METER_FULL_US: f64 = 60_000.0;
/// Meter rows, always drawn. The block is a fixed height ([`styles::METERS`]): lanes fill and
/// free continuously, and a block that resized with them would move everything below it on every
/// admission. Sized above the occupancy this load reaches, so there is always a lane to take.
const LANES: usize = 14;

/// The tenant rate knobs' scale. Each is that tenant's arrivals in requests per second \u2014 the load
/// is whatever the three come to, not a constant they divide. They open at [`TENANTS`]' own rates.
const QPS_SCALE: Scale = Scale::new(0, 200, 5);

/// The long chart: one sample per this much virtual time, this many kept, drawn into this box.
const LONG_SAMPLE_MS: f64 = 40.0;
const LONG_BARS: usize = 900;
const LONG_W: f64 = 900.0;
const LONG_H: f64 = 96.0;
/// The EWMA the samples are smoothed with.
const LONG_SMOOTH: f64 = 0.8;

/// A tenant's colour, by its index in [`TENANTS`] — read off the tenant, which is where it is
/// declared. Everything the panel draws a tenant with comes through here.
fn tenant_paint(k: usize) -> Paint {
    TENANTS[k].tint
}

/// Record one measured element's rect in a slot list, ignoring an index the current layout has
/// no element for.
fn place(slots: &mut [Option<Rect>], i: usize, rect: Rect) {
    if let Some(slot) = slots.get_mut(i) {
        *slot = Some(rect);
    }
}

/// Virtual milliseconds, as the reporter counts them.
fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn us(d: Duration) -> f64 {
    d.as_secs_f64() * 1e6
}

/// A whole number with thousands separators — every µs reading in the panel wears them.
fn grouped(v: f64) -> String {
    let digits = v.round().max(0.0).to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.char_indices() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

// ── the window's state ────────────────────────────────────────────────────────

/// One request inside the window: when it arrived, when it was admitted, when it left, and the
/// cells it minted while it was an occupier.
struct Track {
    tenant: Option<usize>,
    at: f64,
    admit: f64,
    out: Option<f64>,
    cells: Vec<BlameSpan>,
    /// Σ of the cells' accruals — what the meter wound for this request, which is what its
    /// `Blame` handle drew.
    owed: Duration,
    /// Where its conveyor row stands, easing toward its slot.
    y: Option<f64>,
}


/// A finished occupier's last cell, fading where it stood.
struct Ghost {
    tenant: Option<usize>,
    pos: usize,
    n: usize,
    /// Where it sits on the strip — the virtual instant its request stopped occupying.
    out: f64,
    /// When the fade started, on the wall clock that paces it.
    born: f64,
}

/// One long-chart sample: each tenant's smoothed rate, and the total the black line draws.
struct LongPoint {
    r: [f64; TENANTS.len()],
    tot: f64,
}

/// Everything the panel remembers between frames. The engine reports one frame at a time; the
/// window, the lanes and the long chart are the reading of them.
struct Panel {
    t: f64,
    /// Real milliseconds since the panel opened. Presentation that only has to stay legible is
    /// paced by this; everything that says what the engine did stays in virtual time above.
    real_t: f64,
    /// The instant the track measures from. Fixed for as many windows as the track is long, so a
    /// cell own coordinates never move; re-based when the window nears the end.
    epoch: f64,
    /// Requests that have landed but not yet been admitted: their arrival instant and tenant,
    /// waiting for the admission that turns them into a [`Track`]. A request the queue sheds
    /// never occupied the server, so it never appears in the panel at all.
    landed: BTreeMap<u32, (f64, Option<usize>)>,
    tracks: BTreeMap<u32, Track>,
    shut: Vec<(f64, f64)>,
    ghosts: VecDeque<Ghost>,
    /// Which request holds each lane, and the lanes in recycling order — oldest first.
    lanes: Vec<Option<u32>>,
    order: Vec<usize>,
    long: VecDeque<LongPoint>,
    long_acc: [Duration; TENANTS.len()],
    long_n: f64,
    /// Per tenant, every fare the reporter's completion callback has banked — the ledger, so the
    /// totals beside the chart are what was billed rather than a second sum of the same spans.
    fares: [Duration; TENANTS.len()],
    /// The reporter's readings at the last frame's end.
    meter: Duration,
    unattributed: Duration,
    shut_now: bool,
    inflight: u64,
    occupiers: Vec<u32>,
    /// What the meter wound over the last frame — the share each occupier took for it.
    charge: Duration,
}

impl Panel {
    fn new() -> Panel {
        Panel {
            t: 0.0,
            real_t: 0.0,
            epoch: 0.0,
            landed: BTreeMap::new(),
            tracks: BTreeMap::new(),
            shut: Vec::new(),
            ghosts: VecDeque::new(),
            lanes: vec![None; LANES],
            order: (0..LANES).collect(),
            long: VecDeque::new(),
            long_acc: [Duration::ZERO; TENANTS.len()],
            long_n: 0.0,
            fares: [Duration::ZERO; TENANTS.len()],
            meter: Duration::ZERO,
            unattributed: Duration::ZERO,
            shut_now: false,
            inflight: 0,
            occupiers: Vec::new(),
            charge: Duration::ZERO,
        }
    }

    /// A request lands on the server. Its tenant is asked of the engine here, which is why this
    /// is its own step: the engine cannot be asked while its snapshot is borrowed.
    fn land(&mut self, id: u32, at: f64, tenant: Option<usize>) {
        self.landed.entry(id).or_insert((at, tenant));
    }

    /// Advance the wall clock the panel's presentation is paced by. Once per *rendered* frame,
    /// never per engine step: a frame is what a reader sees, whatever the speed knob bought.
    fn tick_real(&mut self, dt: f64) {
        self.real_t += dt.max(0.0);
    }

    /// Bank the fares the reporter reported since the last frame.
    fn bank(&mut self, fares: Vec<(&'static str, Duration)>) {
        for (tenant, fare) in fares {
            if let Some(k) = tenant_of(Some(tenant)) {
                self.fares[k] += fare;
            }
        }
    }

    /// Fold one engine frame in.
    fn absorb(&mut self, obs: &Obs) {
        let dt = (obs.t - self.t).max(0.0);
        self.t = obs.t;

        for &(a, b) in &obs.blame.shut {
            match self.shut.last_mut() {
                Some(last) if last.1 == a => last.1 = b,
                _ => self.shut.push((a, b)),
            }
        }

        for span in &obs.blame.spans {
            // The first cell an id mints is its admission — the moment it became an occupier,
            // and the moment it enters the panel.
            let track = match self.tracks.entry(span.id) {
                std::collections::btree_map::Entry::Occupied(track) => track.into_mut(),
                std::collections::btree_map::Entry::Vacant(slot) => {
                    let Some((at, tenant)) = self.landed.remove(&span.id) else { continue };
                    slot.insert(Track {
                        tenant,
                        at,
                        admit: span.t_a,
                        out: None,
                        cells: Vec::new(),
                        owed: Duration::ZERO,
                        y: None,
                    })
                }
            };
            track.owed += span.accrual;
            if !track.cells.last_mut().is_some_and(|last| last.extend(span)) {
                track.cells.push(*span);
            }
            if let Some(k) = track.tenant {
                self.long_acc[k] += span.accrual;
            }
        }

        // A row ends when its request stops *occupying* the server, which the reporter's own
        // membership says — not when the client gets its answer, a leg home later.
        for (id, track) in &mut self.tracks {
            if track.out.is_none() && !obs.blame.occupiers.contains(id) {
                track.out = Some(obs.t);
            }
        }

        self.charge = obs.blame.meter.saturating_sub(self.meter);
        self.meter = obs.blame.meter;
        self.unattributed = obs.blame.unattributed;
        self.shut_now = obs.blame.shut_now;
        self.inflight = obs.blame.inflight;
        self.occupiers.clone_from(&obs.blame.occupiers);
        self.recycle_lanes();
        self.sample_long(dt);
        self.trim();
    }

    /// Seat the occupiers. A lane freed goes to the back of the recycling order and a new occupier
    /// takes the one freed most recently, so a lane's place in `order` — and so its row in the
    /// meters — only moves when it is actually reused.
    fn recycle_lanes(&mut self) {
        for lane in 0..self.lanes.len() {
            let Some(id) = self.lanes[lane] else { continue };
            if self.occupiers.contains(&id) {
                continue;
            }
            self.lanes[lane] = None;
            self.order.retain(|&l| l != lane);
            self.order.push(lane);
            if let Some(track) = self.tracks.get(&id) {
                if let Some(last) = track.cells.last() {
                    self.ghosts.push_back(Ghost {
                        tenant: track.tenant,
                        pos: last.pos,
                        n: last.n,
                        out: self.t,
                        born: self.real_t,
                    });
                }
            }
        }
        for i in 0..self.occupiers.len() {
            let id = self.occupiers[i];
            if self.lanes.contains(&Some(id)) {
                continue;
            }
            // Every lane taken: this occupier has no meter row until one frees. The strip still
            // draws its cells, so the accounting is whole — it is the meters that are a fixed
            // set of seats.
            let Some(lane) = self.order.iter().rev().copied().find(|&l| self.lanes[l].is_none())
            else {
                continue;
            };
            self.lanes[lane] = Some(id);
        }
    }

    fn sample_long(&mut self, dt: f64) {
        self.long_n += dt;
        if self.long_n < LONG_SAMPLE_MS {
            return;
        }
        let d = std::mem::replace(&mut self.long_acc, [Duration::ZERO; TENANTS.len()]);
        let prev = self.long.back().map_or([0.0; TENANTS.len()], |p| p.r);
        let mut r = [0.0; TENANTS.len()];
        for (k, slot) in r.iter_mut().enumerate() {
            *slot = prev[k] * LONG_SMOOTH + (ms(d[k]) / self.long_n) * (1.0 - LONG_SMOOTH);
        }
        self.long.push_back(LongPoint { r, tot: r.iter().sum() });
        if self.long.len() > LONG_BARS {
            self.long.pop_front();
        }
        self.long_n = 0.0;
    }

    fn trim(&mut self) {
        let cut = self.t - WINDOW_MS;
        self.shut.retain(|&(_, b)| b >= cut);
        self.landed.retain(|_, &mut (at, _)| at >= cut);
        while self.ghosts.front().is_some_and(|g| self.real_t - g.born > GHOST_FADE_MS) {
            self.ghosts.pop_front();
        }
        self.tracks.retain(|_, track| {
            track.cells.retain(|c| c.t_b >= cut);
            track.out.is_none_or(|out| out >= cut)
        });
    }
}

// ── the frame the view draws ──────────────────────────────────────────────────

#[derive(Clone, PartialEq)]
struct Band {
    key: usize,
    x: f64,
    w: f64,
}

#[derive(Clone, PartialEq)]
struct Cell {
    key: u64,
    c: Paint,
    o: f64,
    x: f64,
    w: f64,
    y: f64,
    h: f64,
}

#[derive(Clone, PartialEq)]
struct Row {
    key: u32,
    y: f64,
    o: f64,
    x: f64,
    w: f64,
    color: Paint,
    queued_w: f64,
    processing_x: f64,
    processing_w: f64,
    label: String,
    label_left: f64,
}

#[derive(Clone, PartialEq)]
struct Meter {
    key: usize,
    o: f64,
    color: Paint,
    name: String,
    bar_pct: f64,
    tick: String,
    accrued: String,
}

#[derive(Clone, PartialEq)]
struct Line {
    key: usize,
    c: Paint,
    d: String,
}

#[derive(Clone, PartialEq)]
struct Total {
    key: usize,
    color: Paint,
    id: &'static str,
    total: String,
}

/// The panel as pure geometry and formatted readings — everything the view binds to.
struct PanelFrame {
    /// Where the window sits on the track, as a percentage of the track own width. The one
    /// per-frame write the strip and the conveyor need between them.
    track_offset: f64,
    queueing_bands: Vec<Band>,
    cells: Vec<Cell>,
    rows: Vec<Row>,
    window_text: String,
    meters: Vec<Meter>,
    long_lines: Vec<Line>,
    long_totals: Vec<Total>,
    long_peak: String,
    long_window_text: String,
    long_rate_text: String,
    long_fill_text: String,
    queueing_text: &'static str,
    queueing_ink: String,
    product_text: String,
    unattributed_text: String,
    inflight: String,
    gate_label: &'static str,
    rate_text: String,
    clock_text: String,
}

impl Panel {
    fn build(&mut self) -> PanelFrame {
        let start = self.t - WINDOW_MS;
        // Re-base before the window runs off the end of the track. Every cell is rewritten on
        // this one frame and none of them again until the next.
        if self.t - self.epoch > (TRACK_WINDOWS - 1.0) * WINDOW_MS {
            self.epoch = start;
        }
        let span = TRACK_WINDOWS * WINDOW_MS;
        // Track coordinates: a percentage of the track's own width, fixed for the life of a cell.
        // Nothing is clamped to the *window* — that edge moves every frame, which is the very
        // thing this stops; the plot clips instead. The clamp here is to the track's own start,
        // which moves only on a re-base: a gate shut since the run began is one band older than
        // anything the track can hold, and it has to begin somewhere.
        let at = |t: f64| 100.0 * (t.max(self.epoch) - self.epoch) / span;
        let track_offset = at(start);
        let n = self.inflight.max(1);

        let queueing_bands = self
            .shut
            .iter()
            .enumerate()
            .map(|(key, &(a, b))| {
                let x = at(a);
                Band { key, x, w: (at(b) - x).max(track_pct(0.15)) }
            })
            .collect();

        let mut cells: Vec<Cell> = Vec::new();
        for (id, track) in &self.tracks {
            for (i, cell) in track.cells.iter().enumerate() {
                if cell.t_b <= start {
                    continue;
                }
                let x = at(cell.t_a);
                let h = LANES_H / cell.n.max(1) as f64;
                cells.push(Cell {
                    key: (u64::from(*id) << 16) | i as u64,
                    c: track.tenant.map_or(Stage::core.value(), tenant_paint),
                    o: if cell.minting() { 1.0 } else { 0.2 },
                    x,
                    w: (at(cell.t_b) - x).max(track_pct(0.12)),
                    y: cell.pos as f64 * h,
                    h,
                });
            }
        }
        // The ghosts ride the same list, keyed past every live cell so a fade never collides
        // with the cells still growing beside it.
        for (i, ghost) in self.ghosts.iter().enumerate() {
            let p = ((self.real_t - ghost.born) / GHOST_FADE_MS).clamp(0.0, 1.0);
            let h = LANES_H / ghost.n.max(1) as f64;
            let x = at(ghost.out);
            cells.push(Cell {
                key: u64::MAX - i as u64,
                c: ghost.tenant.map_or(Stage::core.value(), tenant_paint),
                o: 0.85 * (1.0 - p),
                x,
                w: (at(self.t.min(ghost.out + GHOST_MS)) - x).max(track_pct(0.12)),
                y: ghost.pos as f64 * h,
                h: h * (1.0 - p * p),
            });
        }

        // The conveyor: finished requests above, ordered by when they left, live ones below in
        // entry order — so a departure lifts the stack and everything above it drifts off.
        let mut stack: Vec<u32> = self.tracks.iter().filter(|(_, t)| t.out.is_some()).map(|(id, _)| *id).collect();
        stack.sort_by(|a, b| self.tracks[a].out.unwrap().total_cmp(&self.tracks[b].out.unwrap()));
        stack.extend(self.tracks.iter().filter(|(_, t)| t.out.is_none()).map(|(id, _)| *id));

        let req_height = REQ_ROWS * REQ_ROW;
        let depth = stack.len();
        let now = self.t;
        let mut rows = Vec::new();
        for (i, id) in stack.into_iter().enumerate() {
            let track = self.tracks.get_mut(&id).expect("the stack is built from the tracks");
            let target = req_height - (depth - i) as f64 * REQ_ROW;
            // Eased toward its slot, and *settled* once it is within half a pixel: an asymptote
            // never quite arrives, and a row still writing a thousandth of a pixel every frame is
            // a row the diff can never skip.
            let y = match track.y {
                Some(y) if (target - y).abs() > 0.5 => y + (target - y) * DRIFT,
                _ => target,
            };
            track.y = Some(y);
            let end = track.out.unwrap_or(now);
            if y < -REQ_ROW || end <= start {
                continue;
            }
            let x0 = at(track.at);
            let xa = at(track.admit);
            let x1 = at(end);
            let ext = (x1 - x0).max(track_pct(0.12));
            // Whether the bar has cleared the now-rule by enough room for its reading — the one
            // window-relative question a row asks, and it flips once in a row's life.
            let cleared = plot_pct(x1, track_offset) < NOW_X - 30.0;
            let label = match cleared {
                true => format!(
                    "{:.1} ms queue + {:.1} ms processing = {:.1} ms ({} µs blame)",
                    track.admit - track.at,
                    end - track.admit,
                    end - track.at,
                    grouped(us(track.owed)),
                ),
                false => String::new(),
            };
            rows.push(Row {
                key: id,
                y,
                o: if track.out.is_none() { 1.0 } else { 0.62 },
                x: x0,
                w: ext,
                color: track.tenant.map_or(Stage::core.value(), tenant_paint),
                queued_w: (100.0 * (xa - x0) / ext).max(0.0),
                processing_x: 100.0 * (xa - x0) / ext,
                processing_w: (100.0 * (x1 - xa) / ext).max(0.4),
                label,
                // The label only shows once the bar has cleared the rule, so it never needs
                // holding back off the right edge — the clamp that used to do it moved with the
                // window and would have rewritten every row every frame.
                label_left: x1 + track_pct(0.6),
            });
        }

        let meters = self
            .order
            .iter()
            .map(|&lane| match self.lanes[lane].and_then(|id| Some((id, self.tracks.get(&id)?))) {
                None => Meter {
                    key: lane,
                    o: 0.35,
                    color: Stage::core.value(),
                    name: "free lane".to_string(),
                    bar_pct: 0.0,
                    tick: "no occupier".to_string(),
                    accrued: "—".to_string(),
                },
                Some((id, track)) => {
                    let held = us(track.owed);
                    Meter {
                        key: lane,
                        o: 1.0,
                        color: track.tenant.map_or(Stage::core.value(), tenant_paint),
                        name: format!(
                            "#{id} {}",
                            track.tenant.map_or("", |k| TENANTS[k].id),
                        ),
                        bar_pct: (100.0 * held / METER_FULL_US).min(100.0),
                        tick: match self.shut_now {
                            true => format!("+{}µs (1/{n})", slots(format!("{:.0}", us(self.charge)), 4)),
                            false => "no queue — paused".to_string(),
                        },
                        accrued: grouped(held),
                    }
                }
            })
            .collect();

        let peak = self.long.iter().fold(0.0f64, |a, p| a.max(p.tot));
        let span = self.long.len();
        let point_at = |i: usize, v: f64| {
            let x = if span < 2 { 0.0 } else { LONG_W * i as f64 / (LONG_BARS - 1) as f64 };
            let y = LONG_H - 4.0 - (LONG_H - 8.0) * v.min(1.0);
            format!("{:.1},{:.1}", x, y)
        };
        let polyline = |pick: &dyn Fn(&LongPoint) -> f64| {
            self.long.iter().enumerate().map(|(i, p)| point_at(i, pick(p))).collect::<Vec<_>>().join(" ")
        };
        let long_lines = (0..TENANTS.len())
            .map(|k| Line { key: k, c: tenant_paint(k), d: polyline(&|p: &LongPoint| p.r[k]) })
            .collect();
        let long_totals = (0..TENANTS.len())
            .map(|k| Total {
                key: k,
                color: tenant_paint(k),
                id: TENANTS[k].id,
                total: format!("{} µs", slots(grouped(us(self.fares[k])), 7)),
            })
            .collect();

        let occupancy = self.inflight;
        PanelFrame {
            track_offset,
            queueing_bands,
            cells,
            rows,
            window_text: format!("{:.0} ms", WINDOW_MS),
            meters,
            long_lines,
            long_totals,
            long_peak: format!("axis 0 → 1.00 · window peak {}", slots(format!("{peak:.2}"), 4)),
            long_window_text: format!("{:.1} s", LONG_SAMPLE_MS * LONG_BARS as f64 / 1000.0),
            long_rate_text: format!(
                "{} µs per µs of wall clock",
                slots(format!("{:.2}", self.long.back().map_or(0.0, |p| p.tot)), 5)
            ),
            long_fill_text: match span < LONG_BARS {
                true => format!("filling — {} s so far", slots(format!("{:.1}", span as f64 * LONG_SAMPLE_MS / 1000.0), 4)),
                false => "full".to_string(),
            },
            queueing_text: if self.shut_now { "queueing" } else { "no queue" },
            queueing_ink: match self.shut_now {
                true => format!("font-weight:600;color:{}", Stage::amber.value()),
                false => format!("font-weight:600;color:{}", crate::styles::Palette::ink_faint.value()),
            },
            product_text: match (self.shut_now, occupancy) {
                (true, 0) | (false, _) => format!("{} × {occupancy} → {}", u8::from(self.shut_now), slots("nothing minted", 22)),
                (true, _) => format!("1 × {occupancy} → {} µs each per slice", slots(format!("{:.0}", us(self.charge)), 4)),
            },
            unattributed_text: slots(grouped(us(self.unattributed)), 7),
            inflight: slots(occupancy, 2),
            gate_label: if self.shut_now { "shut" } else { "open" },
            rate_text: slots(grouped(us(self.meter)), 7),
            clock_text: format!("{} ms", slots(format!("{now:.0}"), 5)),
        }
    }
}

// ── the island ────────────────────────────────────────────────────────────────

/// The blame panel's messages: a frame tick (its ms delta), the stage's measured width, and the
/// two axes the reader steers — which view the diagram shows, how it is broken down, and how the
/// three tenants divide the arrival stream.
#[derive(Debug)]
pub enum BlameMsg {
    Tick(f64),
    Resize(f64),
    View(usize),
    Breakdown,
    /// Move one tenant's arrival rate, in requests per second.
    Rate(&'static str, f64),
    /// The run/pause control.
    Toggle,
    /// Simulation speed, raw 0–100 (the queue visualiser's own log scale).
    Speed(f64),
    /// Rebuild the machine, leaving the knobs where the reader put them.
    Reset,
    /// The overview stage's laid-out box, or one of the tenant dots' or the server's. The wires
    /// between them are a pure function of these, so they re-bow on any reflow.
    Measured(Measured),
}

/// How a tenant's traffic has been going — which is the colour its outbound requests are drawn in.
///
/// The fan-out sorts its wires by whether the server a client stuck to shed anything; this sorts
/// them by what the tenant's own traffic is doing. Either way the reading is a *layer*: how the
/// stream as a whole has been faring, over and above where each of its requests has got to.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Traffic {
    /// Getting through.
    Served,
    /// Some of it is coming back shed.
    Shed,
}

impl Traffic {
    /// How each tenant's traffic has been going, read off what actually came back. Nothing here
    /// asks whether a tenant is *sending* — a knob's position is a request for traffic, not
    /// traffic — and nothing needs to: a wire with no requests behind it has no dashes to colour.
    fn of_each(done: &VecDeque<Record>) -> Vec<Traffic> {
        TENANTS
            .iter()
            .map(|tenant| {
                let shed = done.iter().any(|r| {
                    r.tenant == tenant.id
                        && matches!(r.outcome, Outcome::QueueTimeout | Outcome::Rejected)
                });
                match shed {
                    true => Traffic::Shed,
                    false => Traffic::Served,
                }
            })
            .collect()
    }
}

/// One tenant's rate knob: which stream it moves, how its rail is labelled, and the signal the
/// rail and readout follow. The three travel as one record, so a rail can never end up labelled
/// with one tenant and wired to another.
#[derive(Clone)]
struct RateKnob {
    tenant: &'static str,
    name: Name,
    at: Signal<f64>,
    tint: Paint,
}

fn memo<T: Clone + PartialEq + 'static>(
    ctx: &Ctx<Setup, BlameMsg>,
    frame: &Signal<Rc<PanelFrame>>,
    g: impl Fn(&PanelFrame) -> T + 'static,
) -> Signal<T> {
    let frame = frame.clone();
    ctx.computed(move |cx| g(&frame.get(cx))).read()
}

pub(crate) async fn run(ctx: Ctx<Setup, BlameMsg>, _seed: crate::PageSeed, _key: SimKey) -> idyll::Result {
    let mut engine = SimEngine::tenanted();
    engine.set_speed(SPEED);
    let mut panel = Panel::new();
    let mut ledger = Latencies::default();
    let mut lay = Layout::of(STAGE_W);
    let mut vs = ViewState::new(lay);
    let mut ps = PaintState::new();
    let mut accum = 0.0f64;
    let mut summary_at = f64::NEG_INFINITY;
    // How each tenant's traffic has been faring — the layer its wire is drawn in. Read on the
    // boxplot's slow cadence (it is a reading of the whole ledger, not of this instant), and
    // carried here so the frame that redraws the wires can wear it.
    let mut shed = vec![false; TENANTS.len()];
    // Which breakdown is in force. The signal drives the switch own knob; this is the same
    // fact where the fold needs it, outside any reactive read.
    let mut tenant_mode = false;

    let opening = build_frame(engine.obs(), &vs, &mut ps);
    let machine = ctx.mutable_signal(Rc::new(opening));
    let panel_frame = ctx.mutable_signal(Rc::new(panel.build()));
    // The breakdown table. `Phase` cuts one round trip into its parts, so it is one table with a
    // total under it; `Tenant` cuts the same population by who paid for it, where a total is a
    // number the panel above already answers — so it is two tables, waiting and working, against
    // one axis so the two read against each other.
    let bars = ctx.mutable_signal(Vec::<Bar>::new());
    let queue_bars = ctx.mutable_signal(Vec::<Bar>::new());
    let work_bars = ctx.mutable_signal(Vec::<Bar>::new());
    let axis = ctx.mutable_signal(1.0f64);

    let layout = ctx.mutable_signal(lay);
    let running = ctx.mutable_signal(false);
    // The stage holds a still whenever the machine is paused — the same tie every other sim
    // makes between "not running" and "not moving".
    let armed = {
        let running = running.read();
        ctx.computed(move |cx| !running.get(cx)).read()
    };
    let run_lbl = {
        let running = running.read();
        ctx.computed(move |cx| if running.get(cx) { "pause" } else { "run" }.to_string()).read()
    };
    let reset_btn = ctx.constant("reset".to_string());
    let speed = ctx.mutable_signal(raw_from_speed(SPEED));
    let speed_at = speed.read();
    ctx.frames(&running.read(), BlameMsg::Tick);
    ctx.resizes(BlameMsg::Resize);


    // Axis 1: what the diagram is — the picture, or the distribution it produced.
    let view = ctx.mutable_signal(0usize);
    let view_items: Vec<ToggleItem> = ["Details", "Overview"]
        .into_iter()
        .enumerate()
        .map(|(i, label)| {
            let at = view.read();
            let on = ctx.computed(move |cx| at.get(cx) == i).read();
            (i, Rc::from(label), on, Tone::Normal)
        })
        .collect();
    let view_knob = knob(&ctx, &view_items);
    let detailed = {
        let view = view.read();
        ctx.computed(move |cx| view.get(cx) == 0).read()
    };

    // Axis 2: how it is broken down. `Phase` is the default and leaves the motion model's
    // colour law alone; `Tenant` is the mode that trades it for identity.
    let breakdown = ctx.mutable_signal(false);
    let by_tenant = breakdown.read();
    // The picture's colour key follows its colours: stations in `Phase`, tenants in `Tenant`.
    let key = {
        let by_tenant = breakdown.read();
        ctx.computed(move |cx| match by_tenant.get(cx) {
            false => Vec::new(),
            true => TENANTS
                .iter()
                .map(|t| Chip { col: t.tint, label: t.id.to_string() })
                .collect(),
        })
        .read()
    };

    // The invitation stands until the reader first takes the switch.
    let untouched = ctx.mutable_signal(true);
    let untouched_when = untouched.read();
    // The speed knob's invitation is not offered at all until the reader has had [`ORIENT_MS`] to
    // read the panel at the speed it opens at, and is withdrawn the moment they raise the speed —
    // taking the control, not merely touching it.
    let speed_invited = ctx.mutable_signal(false);
    let speed_invited_when = speed_invited.read();
    let opening_speed = raw_from_speed(SPEED);
    let (mut speed_offered, mut speed_taken, mut breakdown_resolved) = (false, false, false);

    // The control that rides the diagram's corner. A slot rather than a nested view: the frame
    // decides where it goes, and its `=>` still binds to this loop's inbox.
    let breakdown_corner = {
        let (when, at) = (untouched_when.clone(), by_tenant.clone());
        ctx.slot(move |scope| {
            (live_view! {
                Invite when=(when.clone()) hint=("Switch to Tenant to see which tenant is responsible for each call") {
                    Switch off=("Phase") on=("Tenant") at=(at.clone())
                        flipped=>(|_| BlameMsg::Breakdown)
                }
            })(scope)
        })
    };

    // One rail per tenant, each carrying that tenant's own arrival rate. Nothing writes these
    // but the knob that was moved, so the browser keeps the grab it owns.
    let rates: Vec<_> = TENANTS.iter().map(|t| (t.id, ctx.mutable_signal(t.qps))).collect();
    let knobs: Vec<RateKnob> = rates
        .iter()
        .enumerate()
        .map(|(k, (tenant, at))| RateKnob {
            tenant,
            name: Name::new(tenant, 62),
            at: at.read(),
            // The tenant's own colour, so the knob and everything it moves read as one subject.
            tint: tenant_paint(k),
        })
        .collect();

    // ── the overview stage: three tenants on the left, the one server they share on the right ──
    //
    // Same shape as the fan-out's, and the same alignment mechanism: the boxes are laid out by
    // flexbox and the wires are an SVG over them, so each wire waits on three measurements. What
    // differs is that this engine is *live* — the box's counts and the boxplot move as it runs,
    // where the fan-out's are a settled batch's constants.
    let stage_rect = ctx.mutable_signal::<Option<Rect>>(None);
    let client_rects = ctx.mutable_signal::<Vec<Option<Rect>>>(vec![None; TENANTS.len()]);
    let server_rects = ctx.mutable_signal::<Vec<Option<Rect>>>(vec![None; 1]);
    let counts = ctx.mutable_signal(engine.counts());
    // Who is on each busy core, as the box's pips wear it. Empty outside tenant mode, which
    // leaves the pips the machine's own blue — the same switch the swarm's colours make.
    let core_ink = ctx.mutable_signal(Vec::<Paint>::new());
    let idle = {
        let counts = counts.read();
        ctx.computed(move |cx| {
            let c = counts.get(cx);
            c.inflight == 0 && c.busy == 0
        })
        .read()
    };
    // One wire per tenant — three tenants, one server they share. The dashes are written
    // straight from the engine: outbound, one per request the snapshot places on the leg;
    // homeward, one per verdict still travelling. Nothing here is a phase, so there is nothing
    // to advance and nothing to stop — a paused machine simply reports the same positions again.
    let wires = ctx.mutable_signal(
        (0..TENANTS.len())
            .map(|k| Wire {
                tenant: k,
                server: 0,
                shed: false,
                sent: Vec::new(),
                home: vec![Vec::new(); HOMEWARD.len()],
            })
            .collect::<Vec<_>>(),
    );
    let tenant_dots: Vec<TenantDot> =
        TENANTS.iter().map(|t| TenantDot { name: t.id.to_string(), tint: t.tint }).collect();
    // No tenant lines: this chapter's box is about the machine, and the panel above it already
    // says what each tenant is drawing. The three queue depths stay.
    let server_rows = vec![ServerRow {
        name: server_name(0),
        counts: counts.read(),
        idle,
        ink: core_ink.read(),
        tenants: ctx.constant(Vec::new()),
    }];
    let (wires_read, stage_read, dots_read, boxes_read) =
        (wires.read(), stage_rect.read(), client_rects.read(), server_rects.read());

    let panel_reads = panel_frame.read();
    // The shut intervals are drawn twice — amber in the queueing row, red under the cells — so
    // the two rows read off one signal rather than two computations of the same thing.
    // The one write the sliding window costs: the track carries every settled cell and moves as a
    // single transform, so the cells themselves are written when minted and not again.
    let track = memo(&ctx, &panel_reads, |p| {
        format!("width:{TRACK_W}%;transform:translateX({:.4}%)", -p.track_offset)
    });
    let track_blame = track.clone();
    let track_req = track.clone();
    let bands = memo(&ctx, &panel_reads, |p| p.queueing_bands.clone());
    let bands_red = bands.clone();
    let cells = memo(&ctx, &panel_reads, |p| p.cells.clone());
    let rows = memo(&ctx, &panel_reads, |p| p.rows.clone());
    let meters = memo(&ctx, &panel_reads, |p| p.meters.clone());
    let long_lines = memo(&ctx, &panel_reads, |p| p.long_lines.clone());
    let long_totals = memo(&ctx, &panel_reads, |p| p.long_totals.clone());
    let window_text = memo(&ctx, &panel_reads, |p| p.window_text.clone());
    let long_peak = memo(&ctx, &panel_reads, |p| p.long_peak.clone());
    let long_window_text = memo(&ctx, &panel_reads, |p| p.long_window_text.clone());
    let long_rate_text = memo(&ctx, &panel_reads, |p| p.long_rate_text.clone());
    let long_fill_text = memo(&ctx, &panel_reads, |p| p.long_fill_text.clone());
    let queueing_text = memo(&ctx, &panel_reads, |p| p.queueing_text.to_string());
    let queueing_ink = memo(&ctx, &panel_reads, |p| p.queueing_ink.clone());
    let product_text = memo(&ctx, &panel_reads, |p| p.product_text.clone());
    let unattributed_text = memo(&ctx, &panel_reads, |p| p.unattributed_text.clone());
    let inflight = memo(&ctx, &panel_reads, |p| p.inflight.clone());
    let gate_label = memo(&ctx, &panel_reads, |p| p.gate_label.to_string());
    let rate_text = memo(&ctx, &panel_reads, |p| p.rate_text.clone());
    let clock_text = memo(&ctx, &panel_reads, |p| p.clock_text.clone());

    let mut ctx = ctx.render(live_view! {
        div css=[cstyles::QV] {
          div css=[crate::atoms::sim_card::styles::CARD] {
            div css=[styles::HEAD] {
                span css=[styles::READOUT] {
                    "N = " $inflight " · gate " $gate_label " · rate " $rate_text
                    " µs · unattributed " $unattributed_text " µs · clock " $clock_text
                }
                span css=[styles::HEAD_READ] {
                    span style=($queueing_ink) { $queueing_text }
                    " · " $product_text
                }
            }

            div css=[styles::PANE] {
                div css=[styles::ROW] {
                    span css=[styles::LABEL] { "queueing" }
                    div css=[styles::PLOT] style=(format!("height:{QUEUE_H}px")) {
                        div css=[styles::TRACK] style=($track) {
                            @for b in $bands [key = b.key] {
                                div css=[styles::QUEUEING] style=(band_style(&$b)) {}
                            }
                        }
                        div css=[styles::NOW] style=(format!("left:{NOW_X}%")) {}
                    }
                }

                div css=[styles::ROW] {
                    span css=[styles::LABEL_STRONG] { "× occupiers = blame" }
                    div css=[styles::PLOT] style=(format!("height:{LANES_H}px")) {
                        div css=[styles::TRACK] style=($track_blame) {
                            @for b in $bands_red [key = b.key] {
                                div css=[styles::UNMINTED] style=(band_style(&$b)) {}
                            }
                            @for c in $cells [key = c.key] {
                                div css=[styles::CELL] style=(cell_style(&$c)) {}
                            }
                        }
                        div css=[styles::NOW] style=(format!("left:{NOW_X}%")) {}
                    }
                }

                div css=[styles::ROW_TOP] {
                    span css=[styles::LABEL] { "requests" }
                    div css=[styles::CONVEYOR] style=(format!("height:{}px", REQ_ROWS * REQ_ROW)) {
                        div css=[styles::TRACK] style=($track_req) {
                        @for r in $rows [key = r.key] {
                            div css=[styles::REQ] style=(req_style(&$r)) {
                                div css=[styles::BAR] style=(bar_style(&$r)) {
                                    div css=[styles::SEG_QUEUED] style=(queued_style(&$r)) {}
                                    div css=[styles::SEG_RUN] style=(processing_style(&$r)) {}
                                }
                                span css=[styles::REQ_LABEL] style=(label_style(&$r)) { (row_label(&$r)) }
                            }
                        }
                        }
                        div css=[styles::NOW] style=(format!("left:{NOW_X}%")) {}
                    }
                }

                div css=[styles::ROW] {
                    span {}
                    div css=[styles::AXIS] {
                        span { "−" $window_text }
                        span { "now →" }
                    }
                }

                div css=[styles::ROW_METERS] {
                    span css=[styles::LABEL] { "taximeters" }
                    div css=[styles::METERS] {
                        @for m in $meters [key = m.key] {
                            div css=[styles::METER] style=(meter_style(&$m)) {
                                span css=[styles::SWATCH] style=(swatch_style(&$m)) {}
                                span css=[styles::METER_NAME] { (meter_name(&$m)) }
                                div css=[styles::RAIL] {
                                    div css=[styles::FILL] style=(fill_style(&$m)) {}
                                }
                                span css=[styles::TICK] { (meter_tick(&$m)) }
                                span css=[styles::ACCRUED] { (meter_accrued(&$m)) }
                            }
                        }
                    }
                }
            }

            div css=[styles::PANE, styles::RULE] {
                div css=[styles::LONG_HEAD] {
                    span css=[styles::LONG_TITLE] { "Total blame · sliding " $long_window_text " window" }
                    span css=[styles::LONG_NOW] { "now " $long_rate_text " · " $long_fill_text }
                    div css=[styles::LONG_KEY] {
                        @for t in $long_totals [key = t.key] {
                            span css=[styles::KEY_ITEM] {
                                span css=[styles::SWATCH] style=(total_swatch(&$t)) {}
                                span css=[styles::KEY_ID] { (total_id(&$t)) }
                                span css=[styles::KEY_TOTAL] { (total_value(&$t)) }
                            }
                        }
                    }
                }
                div css=[styles::CHART_BOX] {
                    svg css=[styles::CHART] viewBox=(format!("0 0 {LONG_W} {LONG_H}")) preserveAspectRatio=("none") {
                        @for l in $long_lines [key = l.key] {
                            polyline css=[styles::TENANT_LINE] points=(line_points(&$l)) style=(line_style(&$l)) {}
                        }
                    }
                    div css=[styles::PEAK] { $long_peak }
                }
            }

            div css=[styles::VIEWS] {
                ToggleGroup items=(view_items) knob=(view_knob) picked=>(BlameMsg::View)
            }
            Framed corner=(breakdown_corner) {
                @if ($detailed) {
                    MachineView frame=(machine) layout=(layout) charts_on=(false) armed=(armed)
                        on_click=>(|_| BlameMsg::Toggle) ?nested=(true) ?key=(key)
                } else {
                    div css=[styles::SUMMARY] {
                        TenantStage tenants=(tenant_dots) servers=(server_rows) wires=(wires_read)
                            stage=(stage_read) tenant_rects=(dots_read) server_rects=(boxes_read)
                            measured=>(BlameMsg::Measured)
                        @if ($by_tenant) {
                            div css=[styles::CUT] {
                                span css=[styles::CUT_TITLE] { "queue time" }
                                LatencyTable bars=(queue_bars) axis=(axis)
                            }
                            div css=[styles::CUT] {
                                span css=[styles::CUT_TITLE] { "processing time" }
                                LatencyTable bars=(work_bars) axis=(axis)
                            }
                        } else {
                            LatencyTable bars=(bars) axis=(axis)
                        }
                    }
                }
            }

            div css=[cstyles::CONTROLS] {
                div css=[cstyles::ROW] {
                    span css=[cstyles::GRP] { "simulation" }
                    div css=[cstyles::ROW_BODY] {
                        Button kind=(ButtonKind::Cta) label=(run_lbl) pressed=>(|_| BlameMsg::Toggle)
                        Invite when=(speed_invited_when) hint=("Once you understand what each section shows, increase speed to show behaviour over time") {
                            Slider name=(Name::new("speed", 44)) scale=(Scale::new(0, 100, 1))
                                at=(speed_at) fmt=(fmt_speed) moved=>(BlameMsg::Speed)
                        }
                        Button kind=(ButtonKind::Solid) label=(reset_btn) pressed=>(|_| BlameMsg::Reset)
                    }
                }
                div css=[cstyles::ROW] {
                    span css=[cstyles::GRP] { "tenants" }
                    div css=[cstyles::ROW_BODY] {
                        @for s in (knobs) {
                            Slider name=(s.name) scale=(QPS_SCALE) at=(s.at) fmt=(fmt_qps)
                                moved=>(move |v| BlameMsg::Rate(s.tenant, v)) ?tint=(s.tint)
                        }
                    }
                }
            }
          }
        }
    }).await?;

    loop {
        let (msg, turn) = ctx.recv().await?;
        match msg {
            BlameMsg::Tick(dt) => {
                // Fixed-timestep motion out of an accumulator: whole quanta into the engine, one
                // publish per rendered frame. The panel's own wall clock advances once, here —
                // the fades it paces are the reader's, not the machine's.
                panel.tick_real(dt);
                // Long enough on the page to have read it, *and* done with the invitation already
                // standing: at most one asks at a time.
                if !speed_offered && !speed_taken && breakdown_resolved && panel.real_t >= ORIENT_MS
                {
                    speed_offered = true;
                    speed_invited.set(&turn, true);
                }
                accum = (accum + dt.max(0.0)).min(ENGINE_STEP_MS * MAX_CATCH_UP);
                while accum >= ENGINE_STEP_MS {
                    accum -= ENGINE_STEP_MS;
                    engine.tick(ENGINE_STEP_MS);
                    vs.absorb_subtick(engine.obs());
                    // A request enters the window when it lands on the server. Its tenant is the
                    // engine's to name, which the snapshot borrow forbids — so the landings are
                    // read off first and opened before the frame is folded in.
                    let landings: Vec<(u32, f64)> = engine
                        .obs()
                        .hops
                        .iter()
                        .filter(|h| matches!(h.station, Station::SynBacklog { .. }))
                        .map(|h| (h.id, h.t))
                        .collect();
                    for (id, at) in landings {
                        let tenant = tenant_of(engine.whose(id));
                        // The picture keeps the name, because it keeps the dot for longer
                        // than the engine keeps the request.
                        vs.name(id, tenant);
                        panel.land(id, at, tenant);
                    }
                    panel.absorb(engine.obs());
                    panel.bank(engine.take_fares());
                    // The boxplot's fold needs the tenant too, and for the same reason it is
                    // read off the engine before its snapshot is borrowed.
                    let named: HashMap<u32, &'static str> = engine
                        .obs()
                        .latencies
                        .iter()
                        .map(|&(id, _)| id)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .map(|id| (id, engine.whose(id).unwrap_or(crate::engine::SOLO)))
                        .collect();
                    ledger.absorb(engine.obs(), &|id| {
                        named.get(&id).copied().unwrap_or(crate::engine::SOLO)
                    });
                }
                vs.step(engine.obs());
                let mut fresh = build_frame(engine.obs(), &vs, &mut ps);
                if tenant_mode {
                    fresh.tint(|id| vs.named(id).map(tenant_paint));
                }
                machine.set(&turn, Rc::new(fresh));
                panel_frame.set(&turn, Rc::new(panel.build()));
                // The overview reads the same running machine: its box's counts move with the
                // engine, and its wires carry the requests the engine has on them.
                counts.set(&turn, engine.counts());
                let ink = match tenant_mode {
                    true => core_ink_of(&mut engine),
                    false => Vec::new(),
                };
                if core_ink.now(&turn) != ink {
                    core_ink.set(&turn, ink);
                }
                let now = engine.obs().t;
                // Either direction, the snapshot already places every leg, so the dashes are read
                // off it rather than derived — the same channel the machine picture positions its
                // dots from.
                let on_wire = on_the_wire(&mut engine);
                let next: Vec<Wire> = on_wire
                    .out
                    .into_iter()
                    .zip(on_wire.home)
                    .enumerate()
                    .map(|(k, (out, home))| Wire {
                        tenant: k,
                        server: 0,
                        shed: shed.get(k).copied().unwrap_or(false),
                        sent: out,
                        home,
                    })
                    .collect();
                if wires.now(&turn) != next {
                    wires.set(&turn, next);
                }

                // The boxplot is recomputed on its own cadence: percentiles over the whole
                // sample are far too dear per frame, and its bars ease over 0.4 s regardless.
                if now - summary_at >= SUMMARY_MS {
                    summary_at = now;
                    let summary =
                        Summary::recent(ledger.records().iter().cloned().collect(), HALF_LIFE_MS);
                    match tenant_mode {
                        true => {
                            let queue = summary.bars(summary.by_tenant(QUEUE_TIME));
                            let work = summary.bars(summary.by_tenant(PROCESSING_TIME));
                            // One axis over both sections: the reading each tenant's waiting is
                            // worth is what its working cost beside it.
                            axis.set(&turn, axis_for(&queue).max(axis_for(&work)));
                            queue_bars.set(&turn, queue);
                            work_bars.set(&turn, work);
                        }
                        false => {
                            let rows = summary.bars(summary.phases());
                            axis.set(&turn, axis_for(&rows));
                            bars.set(&turn, rows);
                        }
                    }
                    // Which layer each tenant's wire belongs in, on the same cadence: a wire's
                    // colour is a reading of how its traffic has been going, and the ledger it is
                    // read from is the one the boxplot uses. The next frame's wires carry it.
                    shed = Traffic::of_each(ledger.records())
                        .into_iter()
                        .map(|t| t == Traffic::Shed)
                        .collect();
                }
            }
            BlameMsg::Resize(width) => {
                let next = Layout::of(width);
                if next != lay {
                    lay = next;
                    layout.set(&turn, next);
                    // A new geometry restarts the picture's motion — every dot's route was
                    // walked in the old width.
                    vs = ViewState::new(next);
                    ps = PaintState::new();
                }
            }
            BlameMsg::View(index) => view.set(&turn, index),
            BlameMsg::Breakdown => {
                tenant_mode = !tenant_mode;
                breakdown.set(&turn, tenant_mode);
                untouched.set(&turn, false);
                breakdown_resolved = true;
                // The table's rows are a different set in each breakdown, so it is rebuilt at
                // the switch rather than at the next sample.
                summary_at = f64::NEG_INFINITY;
            }
            BlameMsg::Rate(tenant, qps) => {
                engine.set_tenant_qps(tenant, qps);
                if let Some((_, at)) = rates.iter().find(|(id, _)| *id == tenant) {
                    at.set(&turn, qps);
                }
            }
            BlameMsg::Measured(Measured::Stage(rect)) => stage_rect.set(&turn, Some(rect)),
            BlameMsg::Measured(Measured::Tenant(k, rect)) => {
                client_rects.update(&turn, |v| place(v, k, rect))
            }
            BlameMsg::Measured(Measured::Server(k, rect)) => {
                server_rects.update(&turn, |v| place(v, k, rect))
            }
            BlameMsg::Toggle => running.update(&turn, |r| *r = !*r),
            BlameMsg::Speed(raw) => {
                speed.set(&turn, raw);
                engine.set_speed(speed_from_raw(raw));
                // Taken, not merely touched: the invitation asks for *more* speed, so only a
                // move above where the panel opened answers it.
                if raw > opening_speed {
                    speed_taken = true;
                    speed_invited.set(&turn, false);
                }
            }
            BlameMsg::Reset => {
                // A fresh machine at the knobs the reader left: the rates they set are theirs,
                // the run they were watching is not.
                engine = SimEngine::tenanted();
                engine.set_speed(speed_from_raw(speed.now(&turn)));
                for (tenant, at) in &rates {
                    engine.set_tenant_qps(tenant, at.now(&turn));
                }
                panel = Panel::new();
                ledger = Latencies::default();
                vs = ViewState::new(lay);
                ps = PaintState::new();
                accum = 0.0;
                summary_at = f64::NEG_INFINITY;
            }
        }
    }
}

// ── the inline geometry each list item carries ────────────────────────────────

/// Where a track coordinate lands on the plot, once the window's offset is taken off it: `0` is
/// the window's left edge and [`NOW_X`] is `now`. The track is the truth and this is the reading
/// of it — the one place the two spaces are related.
fn plot_pct(track: f64, offset: f64) -> f64 {
    (track - offset) * TRACK_W / 100.0
}

/// A width in plot percent, as the track measures it — [`plot_pct`]'s scale inverted, for the
/// minimum sizes that keep a sub-pixel span visible.
fn track_pct(plot: f64) -> f64 {
    plot * 100.0 / TRACK_W
}

/// A tenant dot wears its hue as its ring, on the stage's own channel fill.


fn band_style(b: &Band) -> String {
    format!("left:{:.3}%;width:{:.3}%", b.x, b.w)
}

fn cell_style(c: &Cell) -> String {
    format!(
        "background:{};opacity:{:.3};left:{:.3}%;width:{:.3}%;top:{:.2}px;height:{:.2}px",
        c.c, c.o, c.x, c.w, c.y, c.h
    )
}

fn req_style(r: &Row) -> String {
    format!("top:{:.2}px;opacity:{:.2}", r.y, r.o)
}

fn bar_style(r: &Row) -> String {
    format!("left:{:.3}%;width:{:.3}%", r.x, r.w)
}

fn queued_style(r: &Row) -> String {
    format!("background:{};left:0%;width:{:.3}%", r.color, r.queued_w)
}

fn processing_style(r: &Row) -> String {
    format!("background:{};left:{:.3}%;width:{:.3}%", r.color, r.processing_x, r.processing_w)
}

fn label_style(r: &Row) -> String {
    format!("left:{:.3}%", r.label_left)
}

fn row_label(r: &Row) -> String {
    r.label.clone()
}

fn meter_name(m: &Meter) -> String {
    m.name.clone()
}

fn meter_tick(m: &Meter) -> String {
    m.tick.clone()
}

fn meter_accrued(m: &Meter) -> String {
    m.accrued.clone()
}

fn total_id(t: &Total) -> String {
    t.id.to_string()
}

fn total_value(t: &Total) -> String {
    t.total.clone()
}

fn line_points(l: &Line) -> String {
    l.d.clone()
}

fn meter_style(m: &Meter) -> String {
    format!("opacity:{:.2}", m.o)
}

fn swatch_style(m: &Meter) -> String {
    format!("background:{}", m.color)
}

fn fill_style(m: &Meter) -> String {
    format!("background:{};width:{:.2}%", m.color, m.bar_pct)
}

fn total_swatch(t: &Total) -> String {
    format!("background:{}", t.color)
}

fn line_style(l: &Line) -> String {
    format!("stroke:{}", l.c)
}

#[styles]
pub mod styles {
    use idyll_styles::Style;

    use crate::atoms::stage::Stage;
    use crate::atoms::tokens::Face;
    use crate::styles::Palette;

    /// The running readout above the card's own header — what the machine is doing right now.
    pub const READOUT: Style = css! {{
        font_family: Face::mono,
        font_size: "12px",
        color: Palette::ink_muted,
        margin: "2px 4px 12px",
    }};

    pub const HEAD: Style = css! {{
        display: "flex",
        align_items: "baseline",
        gap: "12px",
        flex_wrap: "wrap",
        margin: "0 4px 12px",
    }};    pub const HEAD_READ: Style = css! {{
        margin_left: "auto",
        font_family: Face::mono,
        font_size: "12px",
        color: Palette::ink_muted,
    }};

    /// An inner panel: the reading surface inside the card.
    /// One section of the panel. Flat: the card around it is the only raised surface, so a
    /// section is marked by the room it takes, not by a second elevation inside the first.
    pub const PANE: Style = css! {{
        display: "flex",
        flex_direction: "column",
        gap: "8px",
        padding: "4px 2px 18px",
    }};

    /// The division between one section and the next, where spacing alone is not enough — a
    /// hairline, never a surface.
    pub const RULE: Style = css! {{
        border_top: "1px solid transparent",
        border_color: Palette::line,
        margin_top: "6px",
        padding_top: "16px",
    }};

    /// A labelled row: a fixed label column, then the plot. Too narrow for a gutter and the
    /// label takes its own line, the plot the full width under it — the same move the sim's
    /// control rows make.
    pub const ROW: Style = css! {{
        display: "grid",
        grid_template_columns: "150px 1fr",
        gap: "12px",
        align_items: "center",
        max_width(680px): { grid_template_columns: "minmax(0,1fr)", row_gap: "4px" },
    }};
    pub const ROW_TOP: Style = css! {{
        display: "grid",
        grid_template_columns: "150px 1fr",
        gap: "12px",
        align_items: "flex-start",
        margin_top: "2px",
        max_width(680px): { grid_template_columns: "minmax(0,1fr)", row_gap: "4px" },
    }};
    pub const ROW_METERS: Style = css! {{
        display: "grid",
        grid_template_columns: "150px 1fr",
        gap: "12px",
        align_items: "flex-start",
        margin_top: "8px",
        padding_top: "10px",
        border_top: "1px solid transparent",
        border_color: Palette::line,
        max_width(680px): { grid_template_columns: "minmax(0,1fr)", row_gap: "4px" },
    }};

    pub const LABEL: Style = css! {{
        font_family: Face::mono,
        font_size: "11.5px",
        color: Palette::control_ink,
    }};
    pub const LABEL_STRONG: Style = css! {{
        font_family: Face::mono,
        font_size: "11.5px",
        font_weight: 600,
        color: Palette::ink,
    }};
    /// A plot bed: white, so any amber or red in it is the signal itself.
    pub const PLOT: Style = css! {{
        position: "relative",
        background: "#ffffff",
        border: "1px solid transparent",
        border_color: Palette::line,
        border_radius: "5px",
        overflow: "hidden",
    }};
    /// The strip's moving part — and the only one. Everything on it is placed in virtual-time
    /// coordinates that never move; the window slides by translating this. `will-change` keeps it
    /// on its own compositor layer, so a frame is a transform rather than a re-layout.
    pub const TRACK: Style = css! {{
        position: "absolute",
        top: "0",
        bottom: "0",
        left: "0",
        will_change: "transform",
    }};

    pub const NOW: Style = css! {{
        position: "absolute",
        top: "0",
        bottom: "0",
        width: "1px",
        background: Palette::ink,
        opacity: 0.35,
    }};
    pub const QUEUEING: Style = css! {{
        position: "absolute",
        top: "0",
        bottom: "0",
        background: Stage::wash_timeout,
    }};
    /// Painted under the cells: what shows through is shut-time nobody can be billed for.
    pub const UNMINTED: Style = css! {{
        position: "absolute",
        top: "0",
        bottom: "0",
        background: Stage::red,
        opacity: 0.9,
    }};
    pub const CELL: Style = css! {{ position: "absolute" }};

    pub const AXIS: Style = css! {{
        display: "flex",
        justify_content: "space-between",
        font_family: Face::mono,
        font_size: "10.5px",
        color: Palette::ink_faint,
    }};

    pub const CONVEYOR: Style = css! {{
        position: "relative",
        overflow: "hidden",
    }};
    pub const REQ: Style = css! {{
        position: "absolute",
        left: "0",
        right: "0",
        height: "11px",
    }};
    pub const BAR: Style = css! {{
        position: "absolute",
        top: "0",
        bottom: "0",
        border_radius: "4px",
        overflow: "hidden",
    }};
    pub const SEG_QUEUED: Style = css! {{
        position: "absolute",
        top: "0",
        bottom: "0",
        opacity: 0.22,
    }};
    pub const SEG_RUN: Style = css! {{
        position: "absolute",
        top: "0",
        bottom: "0",
    }};
    pub const REQ_LABEL: Style = css! {{
        position: "absolute",
        top: "-1px",
        font_family: Face::mono,
        font_size: "9.5px",
        color: Palette::ink_muted,
        white_space: "nowrap",
        pointer_events: "none",
    }};

    /// The seats are keyed and drawn oldest lane first, so a lane that frees moves to the back
    /// and the rows genuinely reorder — several times a second under load. That is the design,
    /// but it makes the block the worst possible scroll anchor: the browser picks a row, keyed
    /// reconciliation moves it, and the page is scrolled to keep it still. The block is a fixed
    /// height, so nothing outside it shifts; refusing to be an anchor is the other half.
    /// Fourteen seats and the gaps between them — `14 × 16 + 13 × 6` at full width, and
    /// `14 × 22 + 13 × 6` where the readings ride the bar. Stated here rather than measured
    /// from the rows, because a block sized by its contents is a block that resizes when a
    /// lane's reading changes width.
    pub const METERS: Style = css! {{
        display: "flex",
        flex_direction: "column",
        gap: "6px",
        height: "302px",
        overflow_anchor: "none",
        max_width(680px): { height: "386px" },
    }};
    /// A seat. Its height is fixed so the block is the same size whether a lane is occupied
    /// or free.
    ///
    /// Where the row cannot hold the readings *beside* the bar, they ride **on** it: the rail
    /// leaves the flow and becomes the seat's own surface, and the readings sit over it. One
    /// line and one layer — a second, differently-wrapped copy of the text is exactly what
    /// this must not have, so nothing here may wrap.
    pub const METER: Style = css! {{
        display: "flex",
        align_items: "center",
        gap: "10px",
        height: "16px",
        flex_shrink: 0,
        max_width(680px): {
            position: "relative",
            gap: "8px",
            height: "22px",
            overflow: "hidden",
            border_radius: "5px",
        },
    }};
    pub const SWATCH: Style = css! {{
        width: "9px",
        height: "9px",
        border_radius: "2px",
        flex_shrink: 0,
        max_width(680px): { position: "relative", z_index: 1, margin_left: "6px" },
    }};
    pub const METER_NAME: Style = css! {{
        font_family: Face::mono,
        font_size: "11.5px",
        font_weight: 600,
        color: Palette::control_ink,
        width: "112px",
        flex_shrink: 0,
        max_width(680px): {
            position: "relative",
            z_index: 1,
            width: "auto",
            color: Palette::ink,
            white_space: "nowrap",
        },
    }};
    /// The bar. Beside the readings at full width; behind them once it is the seat's surface,
    /// where it leaves the flow so the readings lay out against the seat itself.
    pub const RAIL: Style = css! {{
        flex_grow: 1,
        height: "8px",
        border_radius: "4px",
        background: Palette::rail,
        overflow: "hidden",
        max_width(680px): {
            position: "absolute",
            left: "0",
            top: "0",
            right: "0",
            bottom: "0",
            height: "auto",
            border_radius: "5px",
        },
    }};
    /// A wash rather than a block where the readings ride it: what is behind text has to stay
    /// behind it, and the tenant's own colour is still the thing being shown.
    pub const FILL: Style = css! {{
        height: "100%",
        border_radius: "4px",
        max_width(680px): { opacity: 0.3, border_radius: "5px" },
    }};
    /// The rate a lane is being charged at — the widest reading on the row, so it is the one
    /// given the room left over once the fixed readings have theirs.
    pub const TICK: Style = css! {{
        font_family: Face::mono,
        font_size: "11px",
        color: Palette::ink_muted,
        width: "170px",
        text_align: "right",
        flex_shrink: 0,
        max_width(680px): {
            position: "relative",
            z_index: 1,
            width: "auto",
            flex_grow: 1,
            flex_basis: "0",
            min_width: "0",
            text_align: "left",
            color: Palette::ink,
            white_space: "nowrap",
            overflow: "hidden",
        },
    }};
    pub const ACCRUED: Style = css! {{
        font_family: Face::mono,
        font_size: "13px",
        font_weight: 600,
        color: Palette::ink,
        width: "76px",
        text_align: "right",
        flex_shrink: 0,
        // Held at a width even where everything else gives: a total that sized itself would
        // take the room the rate needs, and the rate would be cut mid-figure to pay for it.
        // Sized for the largest a lane can hold (`METER_FULL_US`, grouped).
        max_width(680px): {
            position: "relative",
            z_index: 1,
            width: "56px",
            margin_right: "8px",
            white_space: "nowrap",
        },
    }};

    pub const LONG_HEAD: Style = css! {{
        display: "flex",
        align_items: "baseline",
        gap: "14px",
        flex_wrap: "wrap",
    }};
    pub const LONG_TITLE: Style = css! {{
        font_size: "11px",
        font_weight: 600,
        letter_spacing: ".08em",
        text_transform: "uppercase",
        color: Palette::ink_faint,
    }};
    /// One cut of the breakdown table, where the table is drawn in more than one. The rows
    /// below the heading keep the table atom's own gap; this is the gap between the cuts.
    pub const CUT: Style = css! {{ margin_top: "14px" }};
    pub const CUT_TITLE: Style = css! {{
        display: "block",
        font_size: "11px",
        font_weight: 600,
        letter_spacing: ".08em",
        text_transform: "uppercase",
        color: Palette::ink_faint,
    }};
    pub const LONG_NOW: Style = css! {{
        font_family: Face::mono,
        font_size: "11px",
        color: Palette::ink_muted,
    }};
    pub const LONG_KEY: Style = css! {{
        display: "flex",
        gap: "14px",
        flex_wrap: "wrap",
        margin_left: "auto",
    }};
    pub const KEY_ITEM: Style = css! {{
        display: "inline-flex",
        align_items: "center",
        gap: "6px",
    }};
    pub const KEY_ID: Style = css! {{
        font_family: Face::mono,
        font_size: "11px",
        color: Palette::ink_muted,
    }};
    pub const KEY_TOTAL: Style = css! {{
        font_family: Face::mono,
        font_size: "12px",
        font_weight: 600,
        color: Palette::ink,
    }};

    pub const CHART_BOX: Style = css! {{
        position: "relative",
        background: "#ffffff",
        border: "1px solid transparent",
        border_color: Palette::line,
        border_radius: "5px",
        overflow: "hidden",
    }};
    pub const CHART: Style = css! {{
        display: "block",
        width: "100%",
        height: "96px",
    }};
    pub const TENANT_LINE: Style = css! {{
        fill: "none",
        stroke_width: "1.2px",
        stroke_linejoin: "round",
        stroke_linecap: "round",
        vector_effect: "non-scaling-stroke",
        opacity: 0.85,
    }};
    pub const PEAK: Style = css! {{
        position: "absolute",
        right: "8px",
        top: "6px",
        font_family: Face::mono,
        font_size: "10px",
        color: Palette::ink_faint,
    }};
    /// The view pick, over the diagram it changes.
    pub const VIEWS: Style = css! {{
        display: "flex",
        margin: "4px 4px 8px",
    }};
    /// The summary sits on the card the whole panel does — no surface of its own. The head room
    /// is the corner control's; the table's first row would otherwise run under it.
    pub const SUMMARY: Style = css! {{ padding: "62px 6px 18px" }};
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::atoms::wires::marks;
    use crate::engine::Station;

    /// Speed is a display knob — how much virtual time one rendered frame buys — so a test that
    /// asserts on the model pins its own and says how long it wants the world to run, rather than
    /// inheriting whatever position the panel happens to open its control at.
    const TEST_SPEED: f64 = 1.0;

    /// Drive the panel exactly as the message loop does — the engine, the landings, the frame —
    /// handing each frame's window to `watch` before the next.
    fn drive(frames: usize, mut watch: impl FnMut(&Panel)) -> Panel {
        let mut engine = SimEngine::tenanted();
        engine.set_speed(TEST_SPEED);
        let mut panel = Panel::new();
        for _ in 0..frames {
            panel.tick_real(ENGINE_STEP_MS);
            engine.tick(ENGINE_STEP_MS);
            let landings: Vec<(u32, f64)> = engine
                .obs()
                .hops
                .iter()
                .filter(|h| matches!(h.station, Station::SynBacklog { .. }))
                .map(|h| (h.id, h.t))
                .collect();
            for (id, at) in landings {
                let tenant = tenant_of(engine.whose(id));
                panel.land(id, at, tenant);
            }
            panel.absorb(engine.obs());
            panel.bank(engine.take_fares());
            watch(&panel);
        }
        panel
    }

    /// Invariant 2: area in the blame strip **is** blame. A cell is `LANES_H / n` tall and spans
    /// `t_b − t_a`, so its area over the strip's is `(t_b − t_a) / n` — and that is exactly what
    /// the meter wound for its occupier, which is what the request was billed.
    #[test]
    fn a_cells_area_is_the_meters_own_wind() {
        let mut solid = 0usize;
        drive(600, |panel| {
            for track in panel.tracks.values() {
                for cell in &track.cells {
                    assert!(cell.pos < cell.n, "a cell sits inside the lattice it declares");
                    match cell.minting() {
                        false => {
                            assert_eq!(cell.accrual, Duration::ZERO, "free residency mints nothing")
                        }
                        true => {
                            solid += 1;
                            let area = (cell.t_b - cell.t_a) / cell.n as f64;
                            assert!(
                                (area - ms(cell.accrual)).abs() < 0.01,
                                "area {area} ms vs the meter's {} ms",
                                ms(cell.accrual),
                            );
                        }
                    }
                }
                // The bill runs from admission; the strip only keeps the window, so the cells
                // still drawn are a tail of it.
                assert!(
                    track.cells.iter().map(|c| c.accrual).sum::<Duration>() <= track.owed,
                    "the drawn cells are part of the bill, never more than it",
                );
            }
        });
        assert!(solid > 1000, "the panel drew blame being minted: {solid} solid cells");
    }

    /// The window is what it says it is, and everything drawn falls inside the plot.
    #[test]
    fn the_window_holds_its_geometry() {
        let mut panel = drive(600, |_| {});
        let frame = panel.build();
        // Everything drawn lives on the track, and the track is long enough that the re-base
        // always gets there first — an overrun would silently slide cells off the end.
        let on_track = |x: f64, w: f64| x >= 0.0 && x + w <= 100.0;
        // ...and reads, through the window's offset, as a span that has happened and has not yet
        // scrolled away: nothing is drawn in the future, nothing wholly in the past is retained.
        let visible = |x: f64, w: f64| {
            let (l, r) = (plot_pct(x, frame.track_offset), plot_pct(x + w, frame.track_offset));
            (0.0..=NOW_X + 0.2).contains(&r) && l <= NOW_X
        };
        for band in &frame.queueing_bands {
            assert!(on_track(band.x, band.w), "a band on the track: {} + {}", band.x, band.w);
            assert!(visible(band.x, band.w), "a band inside the axis");
        }
        for cell in &frame.cells {
            assert!(cell.y + cell.h <= LANES_H + 0.01, "a cell inside the strip");
            assert!(on_track(cell.x, cell.w), "a cell on the track: {} + {}", cell.x, cell.w);
            assert!(visible(cell.x, cell.w), "a cell inside the axis");
        }
        assert!(!frame.meters.is_empty(), "the meters show a lane per occupier");
        assert_eq!(frame.long_lines.len(), TENANTS.len(), "one line per tenant, and no other");
        assert!(frame.long_lines.iter().all(|l| !l.d.is_empty()), "the chart has samples by now");
        assert_eq!(frame.window_text, "800 ms");
        assert_eq!(frame.long_window_text, "36.0 s");
    }

    /// Lanes are recycled, not reassigned: a lane keeps its place in the meters until something
    /// else takes it, so the rows do not shuffle under the reader.
    #[test]
    fn lanes_are_recycled_and_ordered() {
        let panel = drive(600, |_| {});
        let mut seen = panel.order.clone();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), panel.lanes.len(), "every lane appears in the order exactly once");
        let seated: Vec<u32> = panel.lanes.iter().flatten().copied().collect();
        let mut unique = seated.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), seated.len(), "no request holds two lanes");
        assert_eq!(panel.lanes.len(), LANES, "the meter block is a fixed set of seats");
        assert_eq!(
            seated.len(),
            panel.occupiers.len().min(LANES),
            "every occupier there is a seat for has one",
        );
    }

    /// The boxplot's feed partitions the round trip: a request's sections sum to what the client
    /// waited, so the bars add up to the total row rather than to some other number.
    #[test]
    fn the_ledger_partitions_the_round_trip() {
        let mut engine = SimEngine::tenanted();
        engine.set_speed(TEST_SPEED);
        let mut ledger = Latencies::default();
        for _ in 0..600 {
            engine.tick(ENGINE_STEP_MS);
            let named: HashMap<u32, &'static str> = engine
                .obs()
                .latencies
                .iter()
                .map(|&(id, _)| id)
                .collect::<Vec<_>>()
                .into_iter()
                .map(|id| (id, engine.whose(id).unwrap_or(crate::engine::SOLO)))
                .collect();
            ledger.absorb(engine.obs(), &|id| {
                named.get(&id).copied().unwrap_or(crate::engine::SOLO)
            });
        }
        // Far more requests than the sample holds have finished by now, so it is full and has
        // been rolling — the older ones dropped rather than accumulating.
        assert_eq!(ledger.records().len(), crate::multi::SAMPLE, "the sample fills and is bounded");
        for record in ledger.records() {
            let parts: f64 = record.sections.values().sum();
            assert!(
                (parts - record.total_ms).abs() < 0.51,
                "sections {parts} ms vs the round trip {} ms",
                record.total_ms,
            );
            assert!(TENANTS.iter().any(|t| t.id == record.tenant), "every record names a tenant");
        }

        let summary = Summary::of(ledger.records().iter().cloned().collect());
        let phases = summary.bars(summary.phases());
        let queue = summary.bars(summary.by_tenant(QUEUE_TIME));
        let work = summary.bars(summary.by_tenant(PROCESSING_TIME));
        assert!(phases.len() > 1, "the phase breakdown has rows, and its total");
        assert!(phases.iter().any(|b| b.total), "the parts of a round trip sit under their total");
        for rows in [&queue, &work] {
            assert_eq!(rows.len(), TENANTS.len(), "every tenant is a row, and nothing else is");
            assert!(!rows.iter().any(|b| b.total), "a cut by tenant has no total to sit under");
        }

        // Phases are the parts of one round trip, so each row's share of it is a reading and the
        // shares sum to 100. Tenants are separate distributions over the same population — a
        // share of the total would exceed it, so the column does not apply and no row carries one.
        let shares: u32 = phases.iter().filter(|b| !b.total).filter_map(|b| b.pct).sum();
        assert!((99..=101).contains(&shares), "the phases partition the total: {shares}%");
        assert!(phases.iter().all(|b| b.pct.is_some()), "every phase row carries its share");
        assert!(
            queue.iter().chain(&work).all(|b| b.pct.is_none()),
            "no tenant row claims a share of a total",
        );

        // The two tenant cuts are parts of the same round trip, taken apart: what each tenant
        // waited and what it was worked on, and neither can exceed the whole.
        for (q, w) in queue.iter().zip(&work) {
            assert_eq!(q.label, w.label, "the cuts run in the same tenant order");
            assert!(
                q.p50 + w.p50 <= summary.total.p95_ms + 0.51,
                "{}: {} ms queued + {} ms worked is not a round trip",
                q.label, q.p50, w.p50,
            );
        }

        // The axis fits what is drawn. Phases are bounded by the total, so it is the total's own
        // p95 that sets it; a tenant can reach past the total, and the axis has to follow.
        for rows in [&phases, &queue, &work] {
            let widest = rows.iter().fold(0.0_f64, |wide, b| wide.max(b.p95));
            assert!(axis_for(rows) >= widest, "no bar is drawn past the end of its axis");
        }
    }

    /// The dashes on a wire *are* the requests on it, each at the `p` the engine computed for it.
    /// So a still engine reports the same positions and the wire draws the same pattern, and a
    /// tenant with nothing on the leg has a bare wire rather than a stalled one. Three earlier
    /// attempts advanced a phase on the browser's clock instead: they moved a silent tenant's wire
    /// exactly as fast as a busy one's, and kept moving while the machine stood still.
    #[test]
    fn a_wire_carries_the_requests_the_engine_has_on_it() {
        let mut engine = SimEngine::tenanted();
        engine.set_speed(TEST_SPEED);

        let mut carried = vec![0usize; TENANTS.len()];
        for _ in 0..2000 {
            engine.tick(ENGINE_STEP_MS);
            for (n, at) in carried.iter_mut().zip(on_the_wire(&mut engine).out) {
                assert!(at.iter().all(|p| (0.0..=1.0).contains(p)), "every dash is on the wire");
                *n += at.len();
            }
        }
        assert!(carried.iter().all(|n| *n > 0), "every sending tenant sends: {carried:?}");

        let held: Vec<String> = on_the_wire(&mut engine).out.into_iter().map(marks).collect();
        let again: Vec<String> = on_the_wire(&mut engine).out.into_iter().map(marks).collect();
        assert_eq!(held, again, "an engine that did not move moves no dash");

        engine.set_tenant_qps(TENANTS[0].id, 0.0);
        for _ in 0..600 {
            engine.tick(ENGINE_STEP_MS);
        }
        let quiet = on_the_wire(&mut engine).out;
        assert!(quiet[0].is_empty(), "a silent tenant has nothing on its wire");
        assert_eq!(marks(quiet[0].clone()), marks(Vec::new()), "so its wire is drawn bare");
    }

    /// The wire home carries what the engine has on it, in the lane of the verdict each reply
    /// wears — read off the same snapshot as the wire out, so a reply cannot be somewhere the
    /// machine does not have it.
    ///
    /// A rejection is still a reply, which is why rejecting isn't free. A response timeout is not:
    /// the client stopped waiting, so there is nobody to send it to. It sends no [`Reply`], so it
    /// can be on neither a lane nor a leg — and the lanes drawn are exactly the verdicts that can.
    #[test]
    fn only_the_verdicts_someone_is_waiting_for_travel_home() {
        assert!(HOMEWARD.contains(&Outcome::Rejected), "a rejection is still a reply");
        for verdict in HOMEWARD {
            assert!(verdict.reply().is_some(), "every lane drawn is a verdict that travels");
        }
        assert!(
            Outcome::ResponseTimeout.reply().is_none()
                && !HOMEWARD.contains(&Outcome::ResponseTimeout),
            "nobody is waiting for this one, so nothing goes back down the wire",
        );

        let mut engine = SimEngine::tenanted();
        engine.set_speed(TEST_SPEED);
        let mut lanes = vec![0usize; HOMEWARD.len()];
        for _ in 0..2000 {
            engine.tick(ENGINE_STEP_MS);
            for tenant in on_the_wire(&mut engine).home {
                for (n, at) in lanes.iter_mut().zip(tenant) {
                    assert!(at.iter().all(|p| (0.0..=1.0).contains(p)), "every reply is on the wire");
                    *n += at.len();
                }
            }
        }
        assert!(lanes[0] > 0, "served replies ride home: {lanes:?}");
        assert!(lanes.iter().sum::<usize>() > lanes[0], "and so do the shed ones: {lanes:?}");
    }

    /// The layer a wire is drawn in is read off what came back, never off the knob.
    #[test]
    fn a_wires_layer_is_read_from_what_came_back() {
        let mut engine = SimEngine::tenanted();
        engine.set_speed(TEST_SPEED);
        // It sends first, so its requests are in the rolling sample when it goes quiet — which is
        // the case worth pinning: silence is the rate's to say, not the sample's.
        let hush = TENANTS[0].id;

        let mut ledger = Latencies::default();
        for _ in 0..500 {
            engine.tick(ENGINE_STEP_MS);
            let named: HashMap<u32, &'static str> = engine
                .obs()
                .latencies
                .iter()
                .map(|&(id, _)| id)
                .collect::<Vec<_>>()
                .into_iter()
                .map(|id| (id, engine.whose(id).unwrap_or(crate::engine::SOLO)))
                .collect();
            ledger.absorb(engine.obs(), &|id| {
                named.get(&id).copied().unwrap_or(crate::engine::SOLO)
            });
        }

        // The layer is read off what came back, and nothing in it consults a rate.
        let traffic = Traffic::of_each(ledger.records());
        assert_eq!(traffic.len(), TENANTS.len());
        assert!(
            ledger.records().iter().any(|r| r.tenant == hush),
            "the sample holds this tenant's traffic, so the layer is a reading and not a default",
        );
    }

    /// The ghost fade is the reader's, not the machine's: paced by the wall clock, so it reads the
    /// same at every speed. A virtual-time fade takes minutes at the speed the panel opens at —
    /// not a slower truth, since the departure already happened, just an unreadable one.
    #[test]
    fn the_ghost_fade_is_paced_by_the_wall_clock_not_the_speed_knob() {
        // Half the fade's worth of real frames, at whatever the speed knob is on.
        let half_faded = |speed: f64| {
            let mut panel = Panel::new();
            panel.ghosts.push_back(Ghost { tenant: Some(0), pos: 0, n: 1, out: 0.0, born: 0.0 });
            for _ in 0..(GHOST_FADE_MS / 2.0 / ENGINE_STEP_MS) as usize {
                panel.tick_real(ENGINE_STEP_MS);
                panel.t += ENGINE_STEP_MS * speed;
            }
            panel.build().cells.first().map(|c| c.o)
        };
        let (crawling, racing) = (half_faded(0.01), half_faded(3.0));
        assert_eq!(crawling, racing, "the fade ignores the speed knob entirely");
        let o = crawling.expect("the ghost is still on screen half way through its fade");
        assert!((o - 0.425).abs() < 0.03, "half the fade is half the opacity: {o}");
    }

    /// A tenant taken to zero leaves the schedule, and coming back draws a fresh gap rather than
    /// firing the backlog its stale send instant would have owed.
    #[test]
    fn a_silenced_tenant_stops_arriving_and_returns_cleanly() {
        let mut engine = SimEngine::tenanted();
        engine.set_speed(1.0);
        let hush = TENANTS[0].id;
        for _ in 0..120 {
            engine.tick(ENGINE_STEP_MS);
        }
        engine.set_tenant_qps(hush, 0.0);
        // Whatever it sent before the silence is still entitled to land: a request is on the
        // wire for the handshake plus the inbound leg, so let that window drain first.
        let drain = ((crate::engine::HANDSHAKE_MS + crate::engine::NET_MS) / ENGINE_STEP_MS).ceil();
        for _ in 0..drain as usize + 1 {
            engine.tick(ENGINE_STEP_MS);
        }
        let mut after = 0usize;
        for _ in 0..400 {
            engine.tick(ENGINE_STEP_MS);
            let fresh: Vec<u32> = engine
                .obs()
                .hops
                .iter()
                .filter(|h| matches!(h.station, Station::SynBacklog { .. }))
                .map(|h| h.id)
                .collect();
            after += fresh.iter().filter(|&&id| engine.whose(id) == Some(hush)).count();
        }
        assert_eq!(after, 0, "a silenced tenant sends nothing");

        engine.set_tenant_qps(hush, TENANTS[0].qps);
        let mut burst = 0usize;
        engine.tick(ENGINE_STEP_MS);
        let fresh: Vec<u32> = engine
            .obs()
            .hops
            .iter()
            .filter(|h| matches!(h.station, Station::SynBacklog { .. }))
            .map(|h| h.id)
            .collect();
        burst += fresh.iter().filter(|&&id| engine.whose(id) == Some(hush)).count();
        // One tick's worth of arrivals at the tenant's rate, with slack for the draw — the
        // backlog this guards against would be the whole silence at once, hundreds deep.
        let one_tick = TENANTS[0].qps * ENGINE_STEP_MS / 1000.0;
        assert!(
            (burst as f64) <= one_tick * 3.0,
            "waking redraws a gap rather than firing the silence as a backlog: {burst}"
        );
    }
}
