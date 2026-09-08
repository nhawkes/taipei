//! Simulation engine for the queue visualiser — the sim live's model half.
//!
//! The **stack is real `taipei`**: arrivals are fed into an actual
//! [`taipei::queue::QueueService`], pulled by a real `QueueWorker`, shed by its
//! real 100 ms deadline, and gated by a real `CpuBackpressureLayer` or
//! `DynamicConcurrencyLimitLayer` — the subject of the chapter runs verbatim. The
//! surrounding world is modelled: a virtual clock, Poisson arrivals, and a leaf
//! [`App`] service that burns CPU/IO time.
//!
//! The CPU gate reads [`Cores`] as its instrumentation. Taking a core is a worker
//! unparking and dropping one is a worker parking, which is the signal
//! `taipei::tokio` feeds the layer from a multi-threaded runtime — so the layer
//! needs no wasm concession, and a wrong reserve shows up here as a gate that never
//! shuts. Only the OS-CPU signal is modelled, in `App::poll_ready`: sampling the OS
//! is the alternative the chapter compares against, not a taipei layer.
//!
//! Async tasks mutate the shared [`Sim`] state; each frame [`SimEngine::tick`]
//! advances virtual time, pumps the runtime, and snapshots `Sim` into [`Obs`]
//! for the live's DOM fold.

use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use taipei::backpressure::{DebtSemaphore, RuntimeInstrumentation};
use taipei::clock::{self, Clock, Instant};
use taipei::compose::{Layers, Stage, REJECT_LIMIT};
use taipei::cpu_concurrency::{Config as CpuConfig, CpuConcurrencyController};
use taipei::limit::ConcurrencyLimit;
use taipei::queue::QueueError;
use taipei::rate_limit::{Limits, Throttled};

use crate::limits::Store;
use taipei::reject::{BoxError, Overloaded};
use taipei::tenant::TenantReporter;
// The sim stage's palette, aliased: `Stage` here is already the protection stack's.
use crate::atoms::stage::{Paint, Stage as Hue};
use crate::hotswap::{HotSwap, HotSwapHandle};
use crate::unbounded_queue::UnboundedQueueError;
use tokio::runtime::{Handle, Runtime};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tower::util::BoxCloneService;
use tower::{Service, ServiceExt};

// ----- config ----------------------------------------------------------------

/// Real milliseconds are scaled by this into virtual ones. The whole picture is paced
/// against the network model below: an in-region hop is small next to a 100 ms queue
/// deadline, so the sim has to run slow enough that the short legs are still travel and
/// not a blink.
pub const DEFAULT_SPEED: f64 = 0.1;
pub const DEFAULT_QPS: f64 = 30.0;
pub const CORES: usize = 8;
pub const ADMISSION_FRACTION: f64 = 0.5;
pub const ADMISSION_LIMIT: usize = (CORES as f64 * ADMISSION_FRACTION) as usize;
/// Mirrors `taipei::queue::DEFAULT_TIMEOUT`, in virtual milliseconds.
pub const TIMEOUT_MS: f64 = 100.0;
/// The accept burst: virtual ms of CPU a worker spends on a freshly-`spawn`ed
/// connection before the request reaches the app's admission queue. Tagged
/// [`ACCEPT_PHASE`] so the view shows it feeding the queue *from* a core. The
/// handshake/kernel latency before it is the inbound-network leg, not here.
const ACCEPT_CPU_MS: f64 = 1.0;
pub const ACCEPT_PHASE: usize = usize::MAX;
/// Round-trip time between a client and the fleet: one region-to-region hop,
/// `us-east-1` ↔ `us-east-2` (N. Virginia ↔ Ohio), which measures ~12 ms.
///
/// This is the deployment the chapters are about — a service calling a fleet a region
/// away, not across an ocean. It sets the scale everything else is read against: the
/// network is small next to the queue's 100 ms deadline, so *waiting* is what costs,
/// and going back to ask a different server is cheap. A cross-continent fleet (~80 ms
/// RTT) flips that, which is a different lesson.
pub const RTT_MS: f64 = 12.0;
/// One-way travel — half the round trip. Both legs are real sleeps on the virtual
/// clock: the inbound one carries the request down the SYN pipe (as
/// [`Station::NetworkIn`]) from its send instant to its arrival, and the outbound one
/// carries the *response* home (as [`Station::NetworkOut`]) from the verdict to its
/// receipt. A rejection is still a reply the client must receive, which is why shedding
/// costs a round trip too.
pub const NET_MS: f64 = RTT_MS / 2.0;
/// What a client pays before its first byte reaches a machine it has not met: the TCP
/// three-way handshake (one RTT — the request can ride the final ACK) plus TLS 1.3's
/// full handshake (one more).
pub const HANDSHAKE_MS: f64 = 2.0 * RTT_MS;
/// One CPU burst of a hung (accept-then-hang) handler — it runs these back-to-back
/// forever, never freeing its worker, so the core ring keeps sweeping instead of
/// stalling after a single lap.
const HANG_BURST_MS: f64 = 55.0;

// ----- request ---------------------------------------------------------------

#[derive(Clone)]
pub struct SimReq {
    pub id: u32,
    pub enq_t: f64,
    /// Who this request is billed to. Sims that do not model tenants run [`SOLO`].
    pub tenant: &'static str,
    pub phases: Vec<Phase>,
}

impl taipei::tenant::Tenant for SimReq {
    fn tenant(&self) -> &str {
        self.tenant
    }
}

/// The one tenant a sim runs when it does not model them — every sim but the blame panel.
pub const SOLO: &str = "all";

/// A tenant sharing the server: how fast it sends, how big one of its requests is, and the
/// colour it is drawn in. The rate and the weight are the tenant's two **variables** — both
/// live, both retunable while the panel runs ([`SimEngine::set_tenant_qps`],
/// [`SimEngine::set_tenant_work`]) — and they vary independently on purpose: sending twice as
/// often and sending requests twice as big are different ways to occupy a server, and a scheme
/// that bills by counting requests can only see one of them.
///
/// The colour rides here because it is the tenant's, not a position's: everything that draws a
/// tenant — its dots, its wire, its strip cells, its taximeter, its rate knob, its core pips and
/// its table rows — reads it from the one place the tenant is declared, so they cannot drift.
#[derive(Clone, Copy)]
pub struct TenantSpec {
    pub id: &'static str,
    /// Requests per second this tenant opens at.
    pub qps: f64,
    /// The weight this tenant opens at: what each of its requests scales the shared CPU burst
    /// baseline by.
    pub work: f64,
    pub tint: Paint,
}

/// The weight every tenant opens at. **Equal**, so what separates the three is only how much
/// they send — the scheme billing that correctly is the first thing to show, and weight is the
/// second, moved from the panel once the first has landed.
///
/// The value is what keeps the three together asking the server for more CPU than the admission
/// reserve will let through ([`ADMISSION_LIMIT`] of [`CORES`]). The gate has to shut for there
/// to be any blame to bill at all.
const EQUAL_WEIGHT: f64 = 0.27;

/// The blame panel's three tenants: the rates are the arrival split the reader can move, and
/// the weights open level.
pub const TENANTS: [TenantSpec; 3] = [
    TenantSpec { id: "Alice", qps: 100.0, work: EQUAL_WEIGHT, tint: Hue::purple.value() },
    TenantSpec { id: "Bob", qps: 60.0, work: EQUAL_WEIGHT, tint: Hue::blue.value() },
    TenantSpec { id: "Charlie", qps: 80.0, work: EQUAL_WEIGHT, tint: Hue::teal.value() },
];

#[derive(Clone, Copy)]
pub enum Phase {
    Cpu(Duration),
    Io(Duration),
}

/// How the leaf server treats a connection — the failure modes the "modelling a server"
/// chapter contrasts. Orthogonal to [`Stage`] (which is the *protection* stack).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Behavior {
    /// Accepts, runs the work, replies — a healthy server.
    Good,
    /// Opens the socket but never `accept()`s: SYNs pile in the backlog until the
    /// client gives up. No CPU is ever spent.
    NeverAccept,
    /// Accepts the connection, then the handler hangs forever — the request holds its
    /// worker (a core) until the client gives up. Cores fill and never free.
    AcceptHang,
}

impl Behavior {
    pub fn from_name(name: &str) -> Behavior {
        match name {
            "never-accept" => Behavior::NeverAccept,
            "accept-hang" => Behavior::AcceptHang,
            _ => Behavior::Good,
        }
    }
}

/// The admission-gate signal — how the stack decides "can the server take more work".
/// The gate chapter's comparison; orthogonal to [`Behavior`], varies within a stage.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum Gate {
    /// Runtime-level CPU tracking: admit while more than [`ADMISSION_LIMIT`] cores sit
    /// parked — the real `CpuBackpressureLayer` over [`Cores`], taipei's default.
    #[default]
    RuntimeCpu,
    /// A fixed in-flight ceiling held for the whole request (IO included) — the real
    /// [`DynamicConcurrencyLimitLayer`] between the queue and the app.
    ConcurrencyLimit,
    /// OS-level CPU utilisation, averaged over [`OS_CPU_WINDOW_MS`], steering a real
    /// [`DynamicConcurrencyLimitLayer`] through a real [`CpuConcurrencyController`].
    /// Cheap and uninvasive, and covers threads the runtime never sees — but the
    /// average lags every load change by seconds, so it tunes a ceiling slowly
    /// instead of gating on the spot, which is the lesson.
    OsCpu,
}

impl Gate {
    pub fn from_signal(signal: blog_core::GateSignal) -> Gate {
        match signal {
            blog_core::GateSignal::ConcurrencyLimit => Gate::ConcurrencyLimit,
            blog_core::GateSignal::OsCpu => Gate::OsCpu,
            blog_core::GateSignal::RuntimeCpu => Gate::RuntimeCpu,
        }
    }
}

/// The OS-CPU gate's trailing average window, in virtual ms — "on linux it is
/// typically an average over a few seconds".
pub const OS_CPU_WINDOW_MS: f64 = 3000.0;
/// The load the OS-CPU controller steers toward.
pub const OS_CPU_MAX: f64 = 0.75;
/// How often the OS-CPU controller looks, in virtual ms. One `observe` per period —
/// the controller has no clock of its own, so this cadence *is* its refresh period.
const OS_CPU_REFRESH_MS: f64 = 100.0;
/// The ceiling the OS-CPU controller starts from, before the load has told it
/// anything. It converges from here in either direction.
const OS_CPU_INITIAL_LIMIT: usize = 16;
/// The OS-CPU controller's upper clamp (`CpuConfig::max_limit`): high enough that the
/// steered limit never withholds readiness at the top of its range.
const LIMIT_OFF: usize = 1 << 20;
// ----- cores as runtime instrumentation ---------------------------------------

/// The sim's cores, doubling as the instrumentation the real
/// [`CpuBackpressureLayer`](taipei::backpressure::CpuBackpressureLayer) reads. Taking a
/// core is a worker unparking and dropping it is a worker parking — precisely the signal
/// `taipei::tokio` feeds the layer on a multi-threaded runtime. So the gate the chapter
/// recommends runs here verbatim, on virtual time, rather than being modelled a second
/// time. The reserve is baked in at construction: a stage that composes no backpressure
/// layer never reads it, so it need not be retuned.
#[derive(Clone)]
pub(crate) struct Cores {
    cores: Arc<Semaphore>,
    idle: Arc<DebtSemaphore>,
}

impl Cores {
    fn new(reserve: i64) -> Self {
        let idle = Arc::new(DebtSemaphore::new(0));
        // Every core starts parked, so capacity opens at `CORES - reserve`, the same
        // resting state an idle production runtime reports.
        shift(&idle, CORES as i64 - reserve);
        Self { cores: Arc::new(Semaphore::new(CORES)), idle }
    }

    fn instrumentation(&self) -> RuntimeInstrumentation {
        RuntimeInstrumentation::new(self.idle.clone())
    }

    /// Occupy a core — a worker unparking.
    async fn acquire(&self) -> Core {
        let permit = self.cores.clone().acquire_owned().await.expect("cores open");
        self.idle.acquire();
        Core { _permit: permit, idle: self.idle.clone() }
    }
}

/// Move capacity by `delta`, in whichever direction it points.
fn shift(idle: &DebtSemaphore, delta: i64) {
    for _ in 0..delta.unsigned_abs() {
        match delta > 0 {
            true => idle.release(),
            false => idle.acquire(),
        }
    }
}

/// An occupied core. Dropping it parks the worker, which is what reopens the gate;
/// a core that is never dropped is a worker leaked forever.
pub(crate) struct Core {
    _permit: OwnedSemaphorePermit,
    idle: Arc<DebtSemaphore>,
}

impl Drop for Core {
    fn drop(&mut self) {
        self.idle.release();
    }
}

// ----- observable snapshot ----------------------------------------------------

pub struct Obs {
    pub t: f64,
    /// Requests in the inbound-network leg (sent, not yet landed) — a count; the
    /// per-request truth rides `live` as [`Station::NetworkIn`].
    pub net_in: usize,
    /// Replies in the outbound-network leg (a verdict given, not yet received) — the same
    /// count for the other direction, with [`Station::NetworkOut`] on `live`.
    pub net_out: usize,
    pub syn_backlog: Vec<(u32, f64)>,
    pub queue_stubs: Vec<(u32, f64)>,
    pub cpu: Vec<CpuTask>,
    /// Core-occupancy extremes over the frame's pump — the internal digest behind the
    /// sub-frame excursion animations. `cpu.len()` is the end-of-frame truth the
    /// readout shows; these never reach the viewer as numbers.
    pub busy_peak: usize,
    pub busy_min: usize,
    /// Count of requests sleeping in IO (the picture only needs the total).
    pub io_sleeping: usize,
    /// Tasks waiting for a worker thread (Tokio-ready).
    pub ready: Vec<u32>,
    pub stats: Stats,
    pub spark: VecDeque<SparkPoint>,
    /// Every live request and its station this frame — the motion layer's positional
    /// truth. Derived from the collections above; they stay the source of truth.
    pub live: Vec<(u32, Station)>,
    /// Every station **entered** since the last snapshot, in order — the itinerary.
    /// The snapshot samples at 30 Hz but the engine transitions continuously; a station
    /// whose residence fits inside one frame never appears in `live`, yet the request
    /// still passed through it. Motion routes along these hops, so nothing the engine
    /// does between frames can be misrendered. Params are entry-time values (`p`/`age`
    /// 0, index at entry); an id's last hop always agrees with its `live` station.
    pub hops: Vec<Hop>,
    /// Requests that reached a verdict this frame — the only per-frame event the
    /// motion layer needs, since an outcome is not recoverable from "it left".
    pub departures: Vec<(u32, Outcome)>,
    /// The client-perceived round trip of each request that finished this frame. A
    /// latency *sample*, not a motion event: the picture never reads it, and a ledger
    /// that does needs the verdict instant, which `departures` does not carry.
    pub latencies: Vec<(u32, f64)>,
    /// Which protection layers the active composition includes — drives which
    /// parts of the picture are drawn (present ⟺ shown).
    pub layers: Layers,
    /// `None` while the queue's shed deadline is disabled.
    pub queue_timeout_ms: Option<f64>,
    pub backpressure: bool,
    /// The admission gate at the frame's end: open ⇒ the gate would admit now.
    pub gate_open_end: bool,
    pub last_latency_ms: Option<f64>,
    pub response_timeout_ms: f64,
    pub processing_timeout_enabled: bool,
    /// The OS-CPU gate's trailing average as a percent — `Some` only under that gate,
    /// so the picture can show the delayed signal next to the instantaneous one.
    pub os_cpu_pct: Option<u32>,
    /// Shut-time and who it was billed to, sampled at every pump event. Empty unless the
    /// sim runs the tenant reporter.
    pub blame: BlameFrame,
    /// 5 s rolling counts, read off `Sim`'s windows (the source of truth,
    /// trimmed under the lock in `snapshot()`). Consumers only ever need the
    /// lengths, so the snapshot carries counts, not per-tick deque clones.
    success_5s: usize,
    done_5s: usize,
    timeout_5s: usize,
    last_spark_t: f64,
}

impl Default for Obs {
    fn default() -> Self {
        Self {
            t: 0.0,
            net_in: 0,
            net_out: 0,
            syn_backlog: Vec::new(),
            queue_stubs: Vec::new(),
            cpu: Vec::new(),
            busy_peak: 0,
            busy_min: 0,
            io_sleeping: 0,
            ready: Vec::new(),
            stats: Stats::default(),
            spark: VecDeque::new(),
            live: Vec::new(),
            hops: Vec::new(),
            departures: Vec::new(),
            latencies: Vec::new(),
            layers: Layers::default(),
            queue_timeout_ms: Some(TIMEOUT_MS),
            backpressure: true,
            gate_open_end: true,
            last_latency_ms: None,
            response_timeout_ms: 1000.0,
            processing_timeout_enabled: false,
            os_cpu_pct: None,
            blame: BlameFrame::default(),
            success_5s: 0,
            done_5s: 0,
            timeout_5s: 0,
            last_spark_t: 0.0,
        }
    }
}

impl Obs {
    pub fn busy(&self) -> usize {
        self.cpu.len()
    }

    /// The ids still live this frame — the motion layer's "don't peel a zombie off its
    /// core" set, derived from [`live`](Self::live).
    pub fn live_ids(&self) -> std::collections::HashSet<u32> {
        self.live.iter().map(|(id, _)| *id).collect()
    }

    /// Fast completions (< response_timeout_ms) per second over the last 5 s.
    pub fn goodput_ps(&self) -> f64 {
        self.success_5s as f64 / 5.0
    }

    /// All completions + queue timeouts per second over the last 5 s.
    pub fn offered_throughput_ps(&self) -> f64 {
        (self.done_5s + self.timeout_5s) as f64 / 5.0
    }

    fn maybe_sample_spark(&mut self) {
        if self.t - self.last_spark_t < 100.0 {
            return;
        }
        self.last_spark_t = self.t;
        if self.spark.len() >= 130 {
            self.spark.pop_front();
        }
        let in_flight = (self.busy() + self.io_sleeping + self.ready.len()) as f32;
        self.spark.push_back(SparkPoint {
            in_flight,
            // Waiting-for-a-worker backlog: the app queue (admission stages) plus the
            // Tokio run queue (a request is in exactly one of them at a time).
            queue: (self.queue_stubs.len() + self.ready.len()) as f32,
            goodput: self.goodput_ps() as f32,
            offered_throughput: self.offered_throughput_ps() as f32,
        });
    }
}

/// One occupier's share of one interval between two pump events — the cell the blame strip
/// draws. `pos` and `n` place it (entry order, oldest at top); `accrual` is what the meter
/// wound for each of the `n`, so the cell's area is the library's own number rather than a
/// second calculation of the same thing.
#[derive(Clone, Copy, PartialEq)]
pub struct BlameSpan {
    pub id: u32,
    pub t_a: f64,
    pub t_b: f64,
    pub pos: usize,
    pub n: usize,
    pub accrual: Duration,
}

impl BlameSpan {
    /// Whether the meter wound across this interval — blame was minted, so the cell is solid.
    pub fn minting(&self) -> bool {
        self.accrual > Duration::ZERO
    }

    /// Absorb the interval that follows this one, if it is the same cell continued: same
    /// occupier, same lattice, same minting state. Nothing already drawn changes height, so
    /// a change in any of them starts a new cell instead.
    pub fn extend(&mut self, next: &BlameSpan) -> bool {
        let same_cell = self.id == next.id
            && self.t_b == next.t_a
            && self.pos == next.pos
            && self.n == next.n
            && self.minting() == next.minting();
        if same_cell {
            self.t_b = next.t_b;
            self.accrual += next.accrual;
        }
        same_cell
    }
}

/// The blame accounting over one frame, sampled at every pump event. Empty in a sim that
/// models no tenants.
#[derive(Clone, Default)]
pub struct BlameFrame {
    /// Intervals over which the gate was shut — someone was being made to wait.
    pub shut: Vec<(f64, f64)>,
    /// Every occupier's share of every interval, oldest interval first.
    pub spans: Vec<BlameSpan>,
    /// The reporter's readings at the frame's end.
    pub meter: Duration,
    pub accumulated: Duration,
    pub attributed: Duration,
    pub unattributed: Duration,
    pub shut_now: bool,
    /// The divisor the library is splitting shut-time across right now.
    pub inflight: u64,
    /// Who those occupiers are, in entry order.
    pub occupiers: Vec<u32>,
}

/// One reading of the accounting, taken at a pump event.
struct BlameSample {
    t: f64,
    meter: Duration,
    shut: bool,
    occupiers: Vec<u32>,
}

/// Turns consecutive readings into the intervals between them. It outlives a frame, so the
/// interval straddling two frames is one interval like any other.
#[derive(Default)]
pub(crate) struct BlameSampler {
    prev: Option<BlameSample>,
}

impl BlameSampler {
    /// Fold one pump event into `out`.
    ///
    /// The clock stands still inside a drain and only advances between two events, and the
    /// library banks a running span *before* any `enter`/`leave` moves `N` — so the
    /// membership read at the earlier event is exactly the membership that earned the meter's
    /// next wind, and the difference of the meter across the pair is what each of them
    /// accrued over it.
    fn event(&mut self, out: &mut BlameFrame, now: f64, meter: Duration, shut: bool, occupiers: &[u32]) {
        match self.prev.as_mut() {
            None => {
                self.prev =
                    Some(BlameSample { t: now, meter, shut, occupiers: occupiers.to_vec() })
            }
            Some(prev) => {
                if now > prev.t {
                    if prev.shut {
                        out.shut.push((prev.t, now));
                    }
                    let accrual = meter.saturating_sub(prev.meter);
                    let n = prev.occupiers.len();
                    let span = |(pos, &id): (usize, &u32)| BlameSpan {
                        id, t_a: prev.t, t_b: now, pos, n, accrual,
                    };
                    out.spans.extend(prev.occupiers.iter().enumerate().map(span));
                }
                prev.t = now;
                prev.meter = meter;
                prev.shut = shut;
                prev.occupiers.clear();
                prev.occupiers.extend_from_slice(occupiers);
            }
        }
    }
}

#[derive(Clone)]
pub struct CpuTask {
    pub id: u32,
    /// Which of the request's CPU bursts this is — with `id`, the burst's identity
    /// (the view keys countdown rings on it so a ring lives exactly one burst).
    pub phase: usize,
    pub remaining_ms: f64,
    pub dur_ms: f64,
    pub slot: Option<usize>,
}

#[derive(Default, Clone)]
pub struct Stats {
    pub arrived: u32,
    pub queue_timeout: u32,
    pub rejected: u32,
    pub rate_limited: u32,
    pub success: u32,
    pub response_timeout: u32,
    pub processing_timeout: u32,
}

#[derive(Clone)]
pub struct SparkPoint {
    pub in_flight: f32,
    pub queue: f32,
    pub goodput: f32,
    pub offered_throughput: f32,
}

// ----- station model ---------------------------------------------------------

/// Where a live request sits in its lifecycle, derived fresh from the snapshot each
/// frame — the single source of truth the picture reads for both colour (instant)
/// and target position (pursued). The engine owns the partition; the view maps each
/// station to a colour and an anchor. `p` is the engine's real progress through a
/// timed leg, in `0..=1`. A request is live until its reply *reaches the client*: the
/// leg home is [`Station::NetworkOut`] like any other station, and `departures` reports
/// the receipt. The one verdict nobody is waiting for sends no [`Reply`], so it cannot take
/// that station at all — it is received where it was given.
#[derive(Clone, Copy, PartialEq)]
pub enum Station {
    /// Inbound network io — the client has sent, the request is in flight down the SYN
    /// pipe. `slot` is the run-queue index it will land on if nothing drains: the run
    /// queue now plus the sends still in flight ahead of it — so the glide is *placed*
    /// to touch down on that slot at exactly its arrival instant, never chased.
    NetworkIn { p: f64, slot: usize },
    /// Outbound network io — the verdict is in and its reply is in flight home. It carries
    /// the [`Reply`] because that is what the leg *is*: the exit it rides and the colour it
    /// wears are what the stack decided.
    NetworkOut { p: f64, reply: Reply },
    /// On a core for the 1 ms accept burst that stamps the queue-start time — `accept()`
    /// plus the handler `spawn`. Drawn in the accept colour on the same core the burst
    /// runs on; the dot heads there like any CPU phase.
    Accept { slot: usize, p: f64 },
    /// In the real taipei queue awaiting admission — the only station with a radial.
    /// `age` is progress toward the shed deadline (`0..=1`), the radial's fill.
    AppQueue { idx: usize, age: f64 },
    /// The **kernel accept queue**: the connection completed its handshake (the kernel
    /// does that alone) and sits established on the listen socket until a worker runs
    /// the accept task. FIFO — nothing the runtime does can insert ahead of it, so its
    /// index only ever decreases. Grows when accepting starves (all workers busy).
    SynBacklog { idx: usize },
    /// The **tokio run queue**: tasks awaiting a worker — freshly `spawn`ed handlers
    /// and IO-woken returns. A userspace queue the kernel's connections never enter;
    /// it and [`Station::SynBacklog`] share only the worker pool.
    RunQueue { idx: usize },
    /// On a core for a compute burst. `hung` = a leaked worker (accept-then-hang) that
    /// never frees — drawn dead, not busy.
    Cpu { slot: usize, p: f64, hung: bool },
    /// Sleeping in IO.
    Io { p: f64 },
}

/// One server's live counts, for the fleet view's box — busy cores, the three queue
/// depths, in-flight work, and the running outcome tallies.
#[derive(Clone, Copy, Default)]
pub struct ServerCounts {
    pub busy: usize,
    pub tcp: usize,
    pub req: usize,
    pub run: usize,
    pub inflight: usize,
    pub retried: usize,
    pub success: usize,
    pub failed: usize,
    /// Turned away at the door for being over a tenant's share. Not a failure of the server and
    /// not something the client retries into — its own tally.
    pub rate_limited: usize,
}

/// A request entering a station, stamped with the instant it did. The visualiser walks
/// the sequence to route a dot through every waypoint; a latency breakdown differences
/// consecutive stamps to get the time spent in each section.
#[derive(Clone, Copy)]
pub struct Hop {
    pub id: u32,
    pub station: Station,
    pub t: f64,
}

/// How a request left the system — the terminal it exits through.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Success,
    QueueTimeout,
    Rejected,
    /// Refused to hold its tenant inside its share of the fleet's budget. Distinct from
    /// [`Outcome::Rejected`], and the distinction is the rate-limiting chapter's subject: a
    /// rejection means the server had nothing for anyone, this means it had something but
    /// not for you.
    RateLimited,
    ResponseTimeout,
    ProcessingTimeout,
}

/// The three pipes a reply can leave by. Three verdicts share the timeout pipe: shed at the
/// gate, refused outright, and refused for being over a share are the same answer to the
/// client, however differently the stack arrived at it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    Timeout,
    Success,
    Processing,
}

/// A verdict that **travels home**: what was decided, and the exit it leaves by. Only
/// [`Outcome::reply`] builds one, so the client's own give-up — the verdict with no pipe and
/// nobody to receive it — cannot be put on a leg home, drawn down an exit, or sent down a
/// wire. There is no state to check for, because there is none to represent.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Reply {
    outcome: Outcome,
    exit: Exit,
}

impl Reply {
    /// What was decided — the reply's colour, and which lane of a wire home it rides.
    pub fn outcome(self) -> Outcome {
        self.outcome
    }

    /// The pipe it leaves by.
    pub fn exit(self) -> Exit {
        self.exit
    }
}

impl Outcome {
    /// Did this request reach the handler? A verdict reached at the door — shed at the queue,
    /// rejected, or refused for being over a share — never ran any of the work, so it has no
    /// processing time to report and no queue to have waited in.
    ///
    /// It is the population any reading of *how long the work took* is over. Include the
    /// turned-away and a heavily limited tenant's timings collapse toward the accept burst,
    /// which reads as a tenant whose requests are cheap rather than one that is not being
    /// served — the opposite of the truth.
    pub fn served(self) -> bool {
        match self {
            Outcome::Success | Outcome::ResponseTimeout | Outcome::ProcessingTimeout => true,
            Outcome::QueueTimeout | Outcome::Rejected | Outcome::RateLimited => false,
        }
    }

    /// Was this the fleet saying "not me, not now"? A shed request got no answer and did no
    /// work, so a client that owns its own retry policy asks again rather than reporting it —
    /// and a tally of what a fleet turned away counts exactly these.
    ///
    /// [`Outcome::RateLimited`] is not one. It is the fleet saying "not you", and asking a
    /// different machine the same question gets the same answer: a client that retried into it
    /// would spend the rest of the batch being refused.
    pub fn refused(self) -> bool {
        match self {
            Outcome::QueueTimeout | Outcome::Rejected => true,
            Outcome::Success
            | Outcome::ResponseTimeout
            | Outcome::ProcessingTimeout
            | Outcome::RateLimited => false,
        }
    }

    /// The reply this verdict sends home, if it sends one. `None` is the client that stopped
    /// waiting: there is nobody left to send to, so the verdict is received where it was
    /// given. A rejection *does* send one, which is why rejecting is not free.
    pub fn reply(self) -> Option<Reply> {
        let exit = match self {
            Outcome::Success => Exit::Success,
            Outcome::QueueTimeout | Outcome::Rejected | Outcome::RateLimited => Exit::Timeout,
            Outcome::ProcessingTimeout => Exit::Processing,
            Outcome::ResponseTimeout => return None,
        };
        Some(Reply { outcome: self, exit })
    }
}

/// What a machine is carrying, as **what is waiting and what is being worked on**.
///
/// One number would put a machine running eight requests with nobody waiting behind a machine
/// running none with one waiting, and it is the second that will make a fresh request wait.
/// So the two are kept apart and compared in that order: a queue is what hurts, and work in
/// progress is only what a machine is for.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Debug)]
pub struct Load {
    pub queued: usize,
    pub processed: usize,
}

/// What a client gets back when it asks: the verdict, the round trip it measured, and the
/// load the machine [`reported`](Sim::reported) alongside it — stamped at the verdict, because
/// that is the last instant the machine spoke. A refusal reports it too: a "no" from a full
/// queue is the freshest reading of that queue a client will ever hold.
pub(crate) struct Answer {
    pub outcome: Outcome,
    pub round_trip: f64,
    pub load: Load,
}

/// The reply lanes a wire home carries — one per outcome, because a reply wears its outcome's
/// colour and a path wears one stroke.
///
/// [`Outcome::ResponseTimeout`] is absent, and its absence is the reading: nobody is waiting for
/// one of those any more, so it sends no [`Reply`] and has no lane to ride — the same reason it
/// gets no exit pipe in the machine picture.
pub const HOMEWARD: [Outcome; 5] = [
    Outcome::Success,
    Outcome::QueueTimeout,
    Outcome::Rejected,
    Outcome::RateLimited,
    Outcome::ProcessingTimeout,
];

/// A request in a network leg: in flight since `since`, for `dur_ms` of it. Both legs are
/// real sleeps on the shared clock, so a leg's progress is these same two numbers whichever
/// way it points.
struct Flight {
    id: u32,
    since: f64,
    dur_ms: f64,
}

impl Flight {
    fn p(&self, now: f64) -> f64 {
        ((now - self.since) / self.dur_ms).clamp(0.0, 1.0)
    }
}

// ----- shared mutable state --------------------------------------------------

struct CpuRec {
    id: u32,
    phase: usize,
    slot: usize,
    start_ms: f64,
    dur_ms: f64,
    /// A hung handler that will never free this worker (accept-then-hang) — the client
    /// gives up but the core stays occupied. Drawn as a dead/red core, not busy work.
    hung: bool,
}

struct IoRec {
    id: u32,
    start_ms: f64,
    dur_ms: f64,
}

/// State mutated by the async tasks and snapshotted into [`Obs`] each frame.
pub(crate) struct Sim {
    /// Requests that reached a verdict since the last snapshot — the only event the
    /// view needs, since an outcome is not recoverable from a request simply leaving.
    pub(crate) departures: Vec<(u32, Outcome)>,
    /// Round trips of the requests that reached a verdict since the last snapshot (see
    /// [`Obs::latencies`]).
    latencies: Vec<(u32, f64)>,
    /// Stations entered since the last snapshot, in order (see [`Obs::hops`]).
    pub(crate) hops: Vec<Hop>,
    /// Requests in the inbound-network sleep, in send order — network io, exactly like an
    /// entry in `io` except that its wait is *blockable* downstream (the run queue it lands
    /// in can back up along the SYN pipe).
    net_in: Vec<Flight>,
    /// Replies in the outbound-network sleep, each with what it carries: the same leg
    /// pointing the other way, from the instant the stack answered to the instant the client
    /// has it.
    net_out: Vec<(Flight, Reply)>,
    syn_backlog: Vec<(u32, f64)>,
    queue_stubs: Vec<(u32, f64)>,
    cpu: Vec<CpuRec>,
    io: Vec<IoRec>,
    ready: Vec<u32>,
    /// Requests admitted past the admission gate, in entry order — the membership the tenant
    /// reporter divides shut-time across. An [`Occupancy`] holds a place for exactly the span
    /// of the leaf service's response future, which is the span of the reporter's own handle.
    occupiers: Vec<u32>,
    stats: Stats,
    last_latency_ms: Option<f64>,
    done_window: VecDeque<f64>,
    success_window: VecDeque<f64>,
    timeout_window: VecDeque<f64>,
    response_timeout_ms: f64,
    processing_timeout_enabled: bool,
    /// IO latency multiplier (1.0 = baseline). Lower = slower dependency.
    io_speed: f64,
    /// The active admission signal `App::poll_ready` applies. Shared state so a gate
    /// switch is live: in-flight work, the queue, and the stats all carry across.
    gate: Gate,
    /// Piecewise-constant core-occupancy history, `(t, busy)` — what the OS-CPU
    /// controller reads instead of the OS. Recorded at each pump step (occupancy is
    /// constant between events), trimmed to the trailing [`OS_CPU_WINDOW_MS`].
    util_samples: VecDeque<(f64, usize)>,
}

impl Sim {
    fn new(
        response_timeout_ms: f64,
        processing_timeout_enabled: bool,
        io_speed: f64,
        gate: Gate,
    ) -> Self {
        Self {
            departures: Vec::new(),
            latencies: Vec::new(),
            hops: Vec::new(),
            net_in: Vec::new(),
            net_out: Vec::new(),
            syn_backlog: Vec::new(),
            queue_stubs: Vec::new(),
            cpu: Vec::new(),
            io: Vec::new(),
            ready: Vec::new(),
            occupiers: Vec::new(),
            stats: Stats::default(),
            last_latency_ms: None,
            done_window: VecDeque::new(),
            success_window: VecDeque::new(),
            timeout_window: VecDeque::new(),
            response_timeout_ms,
            processing_timeout_enabled,
            io_speed,
            gate,
            util_samples: VecDeque::new(),
        }
    }

    /// Everything committed to this machine that it has not yet answered: on the wire in,
    /// waiting to be accepted, waiting for admission, waiting for a worker, on a core, or asleep
    /// on a dependency. What a request sent now would have to get past.
    ///
    /// No machine can report this about itself. A connection in the SYN backlog is the kernel's
    /// until the server `accept()`s it, and a request still on the wire has not reached the
    /// machine at all — so a process too busy to accept reports a load that stopped growing at
    /// exactly the moment it started falling behind. Both are already spoken for, which is what
    /// a router is asking about. The leg home is the one thing in flight that is not: the stack
    /// has answered, and nothing sent now waits behind a reply.
    pub(crate) fn committed(&self) -> usize {
        self.net_in.len()
            + self.syn_backlog.len()
            + self.queue_stubs.len()
            + self.ready.len()
            + self.cpu.len()
            + self.io.len()
    }

    /// The load this machine can *say*: what is waiting, and what is being worked on. What
    /// [`committed`](Sim::committed) holds beyond this is exactly what no process can report
    /// about itself: the kernel's SYN backlog and the requests still on the wire.
    pub(crate) fn reported(&self) -> Load {
        Load {
            queued: self.queue_stubs.len() + self.ready.len(),
            processed: self.cpu.len() + self.io.len(),
        }
    }

    /// The same split over everything committed to the machine, the SYN backlog and the wire
    /// included — the reading no real client could take, which is the point of the policy that
    /// takes it.
    pub(crate) fn committed_load(&self) -> Load {
        Load {
            queued: self.net_in.len() + self.syn_backlog.len() + self.queue_stubs.len()
                + self.ready.len(),
            processed: self.cpu.len() + self.io.len(),
        }
    }

    /// A one-server snapshot for the fleet view — the live counts a box draws, read off
    /// the same state the single-server sim draws from.
    pub(crate) fn counts(&self) -> ServerCounts {
        ServerCounts {
            busy: self.cpu.len(),
            tcp: self.syn_backlog.len(),
            req: self.queue_stubs.len(),
            run: self.ready.len(),
            inflight: self.committed() + self.net_out.len(),
            retried: (self.stats.rejected + self.stats.queue_timeout) as usize,
            success: self.stats.success as usize,
            failed: (self.stats.response_timeout + self.stats.processing_timeout) as usize,
            rate_limited: self.stats.rate_limited as usize,
        }
    }

    /// Record a request entering a station, stamped now — the itinerary the view replays
    /// into dot motion and a latency ledger differences (see [`Hop`]).
    fn hop(&mut self, id: u32, station: Station, t: f64) {
        self.hops.push(Hop { id, station, t });
    }

    /// Push `id` onto the run queue and stamp the hop — its run-queue slot is its index at
    /// the tail. Three paths enter here: a fresh accept, an IO return, the serve accept.
    fn enter_run_queue(&mut self, id: u32, t: f64) {
        self.ready.push(id);
        let idx = self.ready.len() - 1;
        self.hop(id, Station::RunQueue { idx }, t);
    }

    /// Take a core for `id` — choosing the free slot and occupying it are **one step**,
    /// so no two requests can select the same core between a look and a push.
    ///
    /// A caller holds a core permit before it gets here. There are [`CORES`] permits,
    /// each permit-holder occupies at most one slot, and this one has not occupied
    /// its slot yet — so a free slot exists.
    fn claim_core(&mut self, id: u32, phase: usize, dur_ms: f64, now_ms: f64, hung: bool) -> usize {
        let slot = (0..CORES)
            .find(|s| !self.cpu.iter().any(|c| c.slot == *s))
            .expect("a core permit is held, so a core is free");
        self.cpu.push(CpuRec { id, phase, slot, start_ms: now_ms, dur_ms, hung });
        slot
    }

    /// A connection leaves the kernel's accept queue. The queue is ordered by handshake
    /// completion: `accept()` takes the head, but a client that gives up waiting leaves
    /// from wherever it stands — so this is by id, and the requests behind a departure
    /// close up by one slot.
    fn leave_backlog(&mut self, id: u32) {
        self.syn_backlog.retain(|(sid, _)| *sid != id);
    }

    /// Record the current occupancy and trim the window. Occupancy is constant between
    /// events, so sampling at each pump step makes the history exact, not approximate.
    fn util_sample(&mut self, now_ms: f64) {
        if self.util_samples.back().is_some_and(|&(_, busy)| busy == self.cpu.len()) {
            return;
        }
        self.util_samples.push_back((now_ms, self.cpu.len()));
        let start = now_ms - OS_CPU_WINDOW_MS;
        // Keep one sample straddling the window edge — it carries the occupancy at `start`.
        while self.util_samples.get(1).is_some_and(|&(t, _)| t <= start) {
            self.util_samples.pop_front();
        }
    }

    /// The trailing-window CPU average the OS-CPU gate reads, `0..=1`. History shorter
    /// than the window reads low (a fresh server), which is honest.
    fn os_cpu_avg(&self, now_ms: f64) -> f64 {
        let start = now_ms - OS_CPU_WINDOW_MS;
        let mut busy_ms = 0.0;
        for (i, &(t, busy)) in self.util_samples.iter().enumerate() {
            let end = self.util_samples.get(i + 1).map_or(now_ms, |&(t2, _)| t2);
            let (a, b) = (t.max(start), end.min(now_ms));
            if b > a {
                busy_ms += busy as f64 * (b - a);
            }
        }
        busy_ms / (OS_CPU_WINDOW_MS * CORES as f64)
    }
}

// ----- the world -------------------------------------------------------------

fn now_ms(clock: &Clock) -> f64 {
    clock.now().as_micros() as f64 / 1000.0
}

/// The virtual world every server and client in a sim shares: one tokio runtime and
/// one [`Clock`]. Servers are independent state machines — a request on one is
/// invisible to the others — but time is not, so a sim with ten servers still runs
/// one discrete-event pump and one timer queue.
pub struct World {
    rt: Runtime,
    clock: Clock,
}

impl World {
    pub(crate) fn new() -> World {
        World {
            rt: tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("current-thread runtime"),
            clock: Clock::new(),
        }
    }

    pub(crate) fn now_ms(&self) -> f64 {
        now_ms(&self.clock)
    }

    pub(crate) fn clock(&self) -> Clock {
        self.clock.clone()
    }

    /// Spawn a client task onto the world's runtime.
    pub(crate) fn spawn(&self, task: impl Future<Output = ()> + Send + 'static) {
        self.rt.spawn(task);
    }

    /// Advance virtual time to `t_end` as a discrete-event step, calling `sample` at
    /// each event with the clock where it stands.
    ///
    /// Sleepers park on registered deadlines ([`taipei::clock`]'s timer queue), so the
    /// frame hops straight from event to event: settle the runnable tasks, jump to the
    /// next deadline (or the frame end), fire the due wakers, repeat. Work is O(events
    /// this frame); parked sleepers cost nothing.
    ///
    /// Quiescence is judged across **every** server in `states`: a wake chain that
    /// crosses servers (a client giving up on one and retrying on another) settles
    /// inside the frame exactly like a chain within one.
    pub(crate) fn pump(&self, t_end: f64, states: &[Arc<Mutex<Sim>>], mut sample: impl FnMut(f64)) {
        let scope = self.clock.enter();
        let t_end_us = (t_end * 1000.0) as u64;
        let clock = self.clock.clone();
        self.rt.block_on(async {
            loop {
                drain(states).await;
                sample(now_ms(&clock));
                // `advance_to` is monotonic, so a deadline at/behind `now` fires
                // where the clock already stands.
                let target_us = match clock.next_deadline() {
                    Some(d) if d.as_micros() <= t_end_us => d.as_micros(),
                    _ => t_end_us,
                };
                clock.advance_to(Instant::from_micros(target_us));
                clock.fire_due();
                if target_us == t_end_us {
                    // Tasks fired at the frame edge park strictly later (every
                    // modeled duration is > 0), so one more drain finishes the frame.
                    drain(states).await;
                    sample(now_ms(&clock));
                    break;
                }
            }
        });
        drop(scope);
    }
}

/// Collection sizes plus the monotonic departure count, summed over every server: any
/// transition anywhere moves one of these, so equal fingerprints mean the whole world
/// went quiescent.
fn fingerprint(states: &[Arc<Mutex<Sim>>]) -> (usize, usize, usize, usize, usize, usize) {
    let mut f = (0, 0, 0, 0, 0, 0);
    for state in states {
        let s = state.lock().unwrap();
        f.0 += s.cpu.len();
        f.1 += s.io.len();
        f.2 += s.ready.len();
        f.3 += s.queue_stubs.len();
        f.4 += s.syn_backlog.len();
        f.5 += s.departures.len();
    }
    f
}

/// Yield until the runnable set goes quiescent — until a full poll round changes
/// nothing observable — so a whole wake chain (burst ends → core permit freed → real
/// layer admits the next waiter → worker pulls it) completes *within this frame*,
/// not one hop per frame.
///
/// A wake chain can step through a poll that changes nothing observable (the completion
/// handoff resolves the oneshot a round before the client records `Done`), so quiescence
/// is [`STABLE`] *consecutive* unchanged rounds. The iteration cap only guards a runaway.
async fn drain(states: &[Arc<Mutex<Sim>>]) {
    const STABLE: u32 = 8;
    let mut last = fingerprint(states);
    let mut stable = 0u32;
    for _ in 0..256 {
        tokio::task::yield_now().await;
        let cur = fingerprint(states);
        if cur == last {
            stable += 1;
            if stable >= STABLE {
                return;
            }
        } else {
            stable = 0;
            last = cur;
        }
    }
}

// ----- the leaf application service ------------------------------------------

/// The leaf service the real [`QueueLayer`] wraps. Its response future walks the
/// request's phases, occupying a core for each CPU phase and sleeping for each IO
/// phase. The CPU gate is not here — it is the real [`CpuBackpressureLayer`]
/// wrapped around this service, reading [`Cores`] as its instrumentation.
pub(crate) struct App {
    state: Arc<Mutex<Sim>>,
    clock: Clock,
    cores: Cores,
    /// The bare app has no request queue between the accept and the handler, so the
    /// acceptor tails straight into the handler: the accept burst and the first CPU are
    /// one occupation, run at the head of `run_phases`. Every other stage runs the accept
    /// as its own burst before the request queue (in `serve`), so here it is `false` and
    /// the first phase is just the handler's first CPU.
    accept_in_phases: bool,
}

impl Clone for App {
    fn clone(&self) -> Self {
        Self {
            state: self.state.clone(),
            clock: self.clock.clone(),
            cores: self.cores.clone(),
            accept_in_phases: self.accept_in_phases,
        }
    }
}

/// One request's place in [`Sim::occupiers`], released when its response future ends —
/// whether it resolved or was cancelled, which is exactly how `taipei::tenant::Blame`
/// releases the place it took in the same `call`.
pub(crate) struct Occupancy {
    state: Arc<Mutex<Sim>>,
    id: u32,
}

impl Occupancy {
    fn enter(state: &Arc<Mutex<Sim>>, id: u32) -> Occupancy {
        state.lock().unwrap().occupiers.push(id);
        Occupancy { state: state.clone(), id }
    }
}

impl Drop for Occupancy {
    // A cancelled request drops this mid-unwind, so it must not panic.
    fn drop(&mut self) {
        if let Ok(mut s) = self.state.lock() {
            s.occupiers.retain(|o| *o != self.id);
        }
    }
}

impl Service<SimReq> for App {
    type Response = ();
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<(), Infallible>> + Send>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        // The leaf never withholds. Every admission signal the chapter compares is a
        // real taipei layer wrapped around this service, so nothing is left to model
        // here — a request that reaches this poll has already been admitted.
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: SimReq) -> Self::Future {
        let id = req.id;
        let occupancy = Occupancy::enter(&self.state, id);
        {
            let mut s = self.state.lock().unwrap();
            if self.accept_in_phases {
                // Bare app: the notify enqueues the accept task — it leaves the SYN
                // backlog and joins the run queue, waiting for a worker to run the accept
                // burst (the head of `run_phases`).
                s.leave_backlog(id);
                s.enter_run_queue(id, now_ms(&self.clock));
            }
            // Request-queue stages: the accept already ran (in `serve`) and the request is
            // admitted out of its queue now — but the picture keeps it in the request queue
            // until a worker frees, rather than drawing the underlying run-queue wakeup.
            // `run_phases` pulls it off the request queue when it takes a core.
        }
        Box::pin(run_phases(
            self.state.clone(),
            self.clock.clone(),
            self.cores.clone(),
            req,
            self.accept_in_phases,
            occupancy,
        ))
    }
}

async fn run_phases(
    state: Arc<Mutex<Sim>>,
    clock: Clock,
    cores: Cores,
    req: SimReq,
    accept_in_phases: bool,
    _occupancy: Occupancy,
) -> Result<(), Infallible> {
    let id = req.id;
    for (i, ph) in req.phases.iter().enumerate() {
        match *ph {
            Phase::Cpu(d) => {
                let dur_ms = d.as_secs_f64() * 1000.0;
                // One of CORES cores must be free to run CPU work.
                let permit = cores.acquire().await;
                // Where the request waited for this core: the bare app's first burst was
                // the accept task in the run queue; the request-queue first burst waited in
                // its request queue; a post-IO burst waited in the run queue.
                let merge_accept = i == 0 && accept_in_phases;
                let slot = {
                    let mut s = state.lock().unwrap();
                    if i == 0 && !accept_in_phases {
                        s.queue_stubs.retain(|(qid, _)| *qid != id);
                    } else {
                        s.ready.retain(|x| *x != id);
                    }
                    // Bare-app first burst: the acceptor tails into the handler, so the
                    // accept and its first CPU are one core occupation (accept burst first,
                    // in the accept colour). Otherwise straight to the handler's CPU.
                    let (phase, dur) = match merge_accept {
                        true => (ACCEPT_PHASE, ACCEPT_CPU_MS),
                        false => (i, dur_ms),
                    };
                    let slot = s.claim_core(id, phase, dur, now_ms(&clock), false);
                    let station = match merge_accept {
                        true => Station::Accept { slot, p: 0.0 },
                        false => Station::Cpu { slot, p: 0.0, hung: false },
                    };
                    s.hop(id, station, now_ms(&clock));
                    slot
                };
                if merge_accept {
                    // Accept burst done — hand the same core straight to the handler's
                    // first CPU (no release, so no run-queue detour between them).
                    clock::sleep(Duration::from_micros((ACCEPT_CPU_MS * 1000.0) as u64)).await;
                    let mut s = state.lock().unwrap();
                    s.cpu.retain(|c| c.id != id);
                    s.cpu.push(CpuRec { id, phase: i, slot, start_ms: now_ms(&clock), dur_ms, hung: false });
                    s.hop(id, Station::Cpu { slot, p: 0.0, hung: false }, now_ms(&clock));
                }
                clock::sleep(d).await;
                state.lock().unwrap().cpu.retain(|c| c.id != id);
                drop(permit);
            }
            Phase::Io(d) => {
                // Stretch IO latency by the current multiplier (a slow
                // dependency / incident makes each wait take longer).
                let io_speed = state.lock().unwrap().io_speed.max(0.05);
                let d = d.div_f64(io_speed);
                let dur_ms = d.as_secs_f64() * 1000.0;
                {
                    let mut s = state.lock().unwrap();
                    s.io.push(IoRec { id, start_ms: now_ms(&clock), dur_ms });
                    s.hop(id, Station::Io { p: 0.0 }, now_ms(&clock));
                }
                clock::sleep(d).await;
                {
                    let mut s = state.lock().unwrap();
                    s.io.retain(|x| x.id != id);
                    // Back to the run queue for the next CPU phase.
                    s.enter_run_queue(id, now_ms(&clock));
                }
            }
        }
    }
    Ok(())
}

// ----- the client's journey ---------------------------------------------------

/// The front service every client calls. Boxed so each [`Stage`]'s differently
/// typed stack (bare app, reject layer, or queue) shares one handle, with errors
/// unified to [`BoxError`] (`QueueError`, `Overloaded`, …).
pub(crate) type Svc = BoxCloneService<SimReq, (), BoxError>;

/// The travel either way between a client and the server it is talking to. Both legs
/// are real sleeps on the shared clock, so a distant server is slower in exactly the
/// way a distant server is slower — nothing about latency is bookkeeping.
#[derive(Clone, Copy)]
pub struct NetLegs {
    pub in_ms: f64,
    pub out_ms: f64,
}

/// The inbound leg and the landing: the client sends, the request spends [`NetLegs::in_ms`]
/// in flight (network io, live the whole way), then the completed connection joins the
/// kernel's accept queue. Shared by every client — one server or ten. Returns the send
/// instant the round trip is measured from.
pub(crate) async fn arrive(state: &Arc<Mutex<Sim>>, clock: &Clock, id: u32, enq_t: f64, net: NetLegs) -> f64 {
    let send_t = enq_t - net.in_ms;
    clock::sleep_until(Instant::from_micros((send_t * 1000.0) as u64)).await;
    {
        let mut s = state.lock().unwrap();
        s.net_in.push(Flight { id, since: send_t, dur_ms: net.in_ms });
        s.hop(id, Station::NetworkIn { p: 0.0, slot: 0 }, now_ms(clock));
    }
    clock::sleep_until(Instant::from_micros((enq_t * 1000.0) as u64)).await;
    let mut s = state.lock().unwrap();
    s.net_in.retain(|f| f.id != id);
    s.stats.arrived += 1;
    s.syn_backlog.push((id, enq_t));
    let idx = s.syn_backlog.len() - 1;
    s.hop(id, Station::SynBacklog { idx }, now_ms(clock));
    send_t
}

/// A healthy server serving one already-arrived request, from the accept burst to the
/// verdict — the whole journey through the real taipei stack, with the server's own
/// counters kept as it goes.
///
/// Returns — once the reply has *reached the client* — the outcome and the client-perceived
/// round trip (send → receipt, spanning both network legs), or `None` when the verdict is
/// dropped and nothing travels back. A caller that keeps a latency ledger writes that pair
/// down; the single-server visualiser reads the same facts off `departures` and `stats`.
pub(crate) async fn serve(
    state: Arc<Mutex<Sim>>,
    clock: Clock,
    cores: Cores,
    mut svc: Svc,
    req: SimReq,
    send_t: f64,
    net: NetLegs,
    uses_app_queue: bool,
) -> Option<Answer> {
    let id = req.id;
    // The kernel completed the handshake; the connection sits in its accept queue. A
    // notify wakes the accept task but does not *run* it — the task waits its turn for a
    // worker (the run queue), and the connection is `accept()`ed only when a worker runs
    // it: a short CPU burst. A busy server just leaves the accept task waiting, and the
    // kernel backlog grows behind it.
    //
    // With a request queue, the handler is a *separate* task from the acceptor, so the
    // accept burst runs here (run queue → accept) and then the handler goes to the request
    // queue for admission — a real round through the stack between the accept and the
    // first work. The bare app has no such split: the acceptor tails straight into the
    // handler, so its accept is the head of the handler's first CPU burst (`run_phases`),
    // and the request stays on the SYN backlog until `App::call` admits it.
    if uses_app_queue {
        {
            let mut s = state.lock().unwrap();
            s.leave_backlog(id);
            s.enter_run_queue(id, now_ms(&clock));
        }
        let permit = cores.acquire().await;
        {
            let mut s = state.lock().unwrap();
            s.ready.retain(|x| *x != id);
            let slot = s.claim_core(id, ACCEPT_PHASE, ACCEPT_CPU_MS, now_ms(&clock), false);
            s.hop(id, Station::Accept { slot, p: 0.0 }, now_ms(&clock));
        }
        clock::sleep(Duration::from_micros((ACCEPT_CPU_MS * 1000.0) as u64)).await;
        {
            let mut s = state.lock().unwrap();
            s.cpu.retain(|c| c.id != id);
            s.queue_stubs.push((id, now_ms(&clock)));
            let idx = s.queue_stubs.len() - 1;
            s.hop(id, Station::AppQueue { idx, age: 0.0 }, now_ms(&clock));
        }
        drop(permit);
    }

    // Hand the request to the real QueueService and await its verdict.
    let fut = match svc.ready().await {
        Ok(svc) => svc.call(req),
        Err(_) => return None,
    };
    let r = fut.await;

    // The verdict is in. Latency is client-perceived — the round trip from send to
    // receipt — so it spans both network legs: the server-side elapsed plus the inbound
    // and outbound travel. The leg home is a scheduled sleep of known length, so the round
    // trip is settled here, before the reply has travelled it. A rejection is still a reply
    // the client must receive, which is why it too costs a round trip.
    let round_trip = (now_ms(&clock) - send_t) + net.out_ms;
    let (outcome, load) = {
        let mut s = state.lock().unwrap();
        // Whichever way it went, the request is out of the queue: admitted, shed, or dropped.
        s.queue_stubs.retain(|(qid, _)| *qid != id);
        // And out of the kernel's accept queue. A connection admitted through the stack left
        // it on the way in, but one the stack *refused* was answered without ever being
        // accepted — and a connection with a verdict is not still waiting to be accepted.
        // Leaving it there put the same request on the SYN pipe and on its way home at once.
        s.leave_backlog(id);
        let outcome = match r {
            Ok(()) => {
                if round_trip < s.response_timeout_ms {
                    Outcome::Success
                } else if s.processing_timeout_enabled {
                    // The work finished, but too slowly for the client to still care.
                    Outcome::ProcessingTimeout
                } else {
                    Outcome::ResponseTimeout
                }
            }
            Err(e) if matches!(e.downcast_ref::<QueueError>(), Some(QueueError::Timeout { .. })) => Outcome::QueueTimeout,
            Err(e) if e.downcast_ref::<Overloaded>().is_some() => Outcome::Rejected,
            Err(e) if e.downcast_ref::<Throttled>().is_some() => Outcome::RateLimited,
            // A dropped verdict: nothing travels back, the dot just despawns.
            Err(_) => return None,
        };
        (outcome, s.reported())
    };

    // The reply's own leg: a real sleep home, live on [`Station::NetworkOut`] the whole way,
    // so a response travels the picture exactly as the request did. A verdict that sends no
    // reply is the client's own give-up, and goes nowhere.
    if let Some(reply) = outcome.reply() {
        {
            let mut s = state.lock().unwrap();
            let since = now_ms(&clock);
            s.net_out.push((Flight { id, since, dur_ms: net.out_ms }, reply));
            s.hop(id, Station::NetworkOut { p: 0.0, reply }, since);
        }
        clock::sleep(Duration::from_micros((net.out_ms * 1000.0) as u64)).await;
        state.lock().unwrap().net_out.retain(|(f, _)| f.id != id);
    }

    // Receipt: the client has its answer. Every tally is stamped at the end of the leg it
    // belongs to, the way `stats.arrived` is stamped when a request lands.
    let mut s = state.lock().unwrap();
    s.last_latency_ms = Some(round_trip);
    let now = now_ms(&clock);
    match outcome {
        // Served completions (offered throughput's `done` window); a successful one
        // also counts toward goodput.
        Outcome::Success => {
            s.stats.success += 1;
            s.done_window.push_back(now);
            s.success_window.push_back(now);
        }
        Outcome::ResponseTimeout => {
            s.stats.response_timeout += 1;
            s.done_window.push_back(now);
        }
        Outcome::ProcessingTimeout => {
            s.stats.processing_timeout += 1;
            s.done_window.push_back(now);
        }
        // Shed before service (offered throughput's `timeout` window).
        Outcome::QueueTimeout => {
            s.stats.queue_timeout += 1;
            s.timeout_window.push_back(now);
        }
        Outcome::Rejected => {
            s.stats.rejected += 1;
            s.timeout_window.push_back(now);
        }
        Outcome::RateLimited => {
            s.stats.rate_limited += 1;
            s.timeout_window.push_back(now);
        }
    }
    s.departures.push((id, outcome));
    s.latencies.push((id, round_trip));
    Some(Answer { outcome, round_trip, load })
}

// ----- engine ----------------------------------------------------------------

fn box_err<E: std::error::Error + Send + Sync + 'static>(e: E) -> BoxError {
    Box::new(e)
}

/// The layer set a stage composes under the current controls — what the picture draws.
/// Decided beside [`build_inner`], from the same inputs, so the drawing can never claim a
/// composition the machine isn't running.
fn layers_for(
    stage: Stage,
    gate: Option<Gate>,
    backpressure_on: bool,
    limit: usize,
    timeout: Duration,
) -> Layers {
    let mut layers = stage.manifest(timeout);
    match gate {
        None => {}
        Some(Gate::RuntimeCpu) => {
            layers.limit = None;
            layers.backpressure = true;
        }
        Some(Gate::ConcurrencyLimit) => {
            layers.backpressure = false;
            layers.limit = Some(limit);
        }
        Some(Gate::OsCpu) => {
            layers.limit = Some(OS_CPU_INITIAL_LIMIT);
            layers.backpressure = false;
        }
    }
    layers.backpressure &= backpressure_on;
    if layers.limit.is_some() && gate != Some(Gate::OsCpu) {
        layers.limit = Some(limit);
    }
    layers
}

/// A queue worker to drive, type-erased so every stage's worker spawns the same way.
type Worker = Pin<Box<dyn Future<Output = ()> + Send>>;

/// What a tenanted stack bills through: the reporter that mints the shut-time, where a
/// finished request's fare is banked, and — where the fleet is limiting — the store that
/// says whose traffic to refuse.
///
/// The store is `Option` because the two tenanted sims want different halves of the seam:
/// the blame panel only measures, and this chapter acts on the measurement.
struct Billing<'a> {
    reporter: &'a TenantReporter,
    ledger: &'a Ledger,
    limits: Option<&'a Arc<Store>>,
}

/// Build the stage's real tower stack via [`crate::compose`] — the exact functions whose
/// source the panel shows. Returns the boxed front, the worker to spawn (queue stages),
/// and the concurrency handle the OS-CPU controller steers (that gate only).
fn build_inner(
    state: &Arc<Mutex<Sim>>,
    clock: &Clock,
    cores: &Cores,
    rt: &Handle,
    stage: Stage,
    gate: Option<Gate>,
    backpressure_on: bool,
    layers: Layers,
    limit: usize,
    timeout: Duration,
    billing: Option<Billing<'_>>,
) -> (Svc, Option<Worker>, Option<ConcurrencyLimit>) {
    let app = App {
        state: state.clone(),
        clock: clock.clone(),
        cores: cores.clone(),
        // A composition with no request queue tails the accept into the handler's first
        // CPU; with one, the accept runs before it, in `serve`. Read off the manifest so
        // this can never disagree with the stack it describes.
        accept_in_phases: !layers.queue,
    };
    let instr = cores.instrumentation();
    match stage {
        Stage::App => (BoxCloneService::new(app.map_err(box_err::<Infallible>)), None, None),
        Stage::Backpressure if backpressure_on => (
            BoxCloneService::new(crate::compose::backpressure(app, &instr).map_err(box_err::<Infallible>)),
            None,
            None,
        ),
        Stage::Backpressure => {
            (BoxCloneService::new(app.map_err(box_err::<Infallible>)), None, None)
        }
        Stage::Reject => (BoxCloneService::new(crate::compose::reject(app, limit)), None, None),
        Stage::Wait => {
            let (svc, worker) = crate::compose::wait(app, limit, rt.clone());
            (
                BoxCloneService::new(svc.map_err(box_err::<UnboundedQueueError>)),
                Some(Box::pin(worker.serve())),
                None,
            )
        }
        // Billing settles the gate rather than choosing beside it: the reporter is specified
        // against the backpressure gate's `poll_ready`, so a tenanted stack is always that gate.
        Stage::Queue => match (billing, gate) {
            // The fleet's limit in front, and blame going to the store that decides it. The
            // front already speaks `BoxError`, so there is nothing to map.
            (Some(Billing { reporter, ledger, limits: Some(limits) }), _) => {
                let store = Arc::clone(limits);
                let banked = ledger.writer();
                let write_blame = move |tenant: &str, fare: Duration| {
                    banked(tenant, fare);
                    store.write_blame(tenant, fare);
                };
                let (svc, worker) = crate::compose::queue_rate_limited(
                    app,
                    &instr,
                    rt.clone(),
                    timeout,
                    reporter,
                    write_blame,
                    Arc::clone(limits),
                );
                (BoxCloneService::new(svc), Some(Box::pin(worker.serve())), None)
            }
            (Some(Billing { reporter, ledger, limits: None }), _) => {
                let (svc, worker) =
                    crate::compose::queue_tenant(app, &instr, rt.clone(), timeout, reporter, ledger.writer());
                (
                    BoxCloneService::new(svc.map_err(box_err::<QueueError>)),
                    Some(Box::pin(worker.serve())),
                    None,
                )
            }
            (None, Some(Gate::RuntimeCpu)) => {
                let (svc, worker) = crate::compose::queue(app, &instr, rt.clone(), timeout);
                (
                    BoxCloneService::new(svc.map_err(box_err::<QueueError>)),
                    Some(Box::pin(worker.serve())),
                    None,
                )
            }
            (None, Some(Gate::OsCpu)) => {
                // The controller's handle — the one place a limit stays live, because a
                // controller tuning it is the whole point of the gate.
                let handle = ConcurrencyLimit::new(OS_CPU_INITIAL_LIMIT);
                let (svc, worker) = crate::compose::queue_os_cpu(app, handle.clone(), rt.clone(), timeout);
                (
                    BoxCloneService::new(svc.map_err(box_err::<QueueError>)),
                    Some(Box::pin(worker.serve())),
                    Some(handle),
                )
            }
            (None, _) => {
                let (svc, worker) = crate::compose::queue_naive(app, limit, rt.clone(), timeout);
                (
                    BoxCloneService::new(svc.map_err(box_err::<QueueError>)),
                    Some(Box::pin(worker.serve())),
                    None,
                )
            }
        },
    }
}

/// Assemble a server: the shared state, the cores, and a [`HotSwap`] front over the
/// stage's initial stack. The front is built once and every client task clones it; a
/// control that changes the composition rebuilds only the inner and swaps it in
/// ([`SimEngine::rebuild`]), so the reader watches the machine keep running.
pub(crate) fn assemble(
    world: &World,
    stage: Stage,
    gate: Option<Gate>,
    backpressure: bool,
    queue_timeout_on: bool,
    response_timeout_ms: f64,
    processing: bool,
    io_speed: f64,
    concurrency_limit: usize,
) -> Server {
    let g0 = gate.unwrap_or_default();
    let clock = world.clock.clone();
    let state = Arc::new(Mutex::new(Sim::new(response_timeout_ms, processing, io_speed, g0)));
    // The cores double as the CPU gate's instrumentation; the reserve is consulted only
    // where a backpressure layer is actually composed (the Backpressure stage / runtime-
    // CPU gate), so it can sit at [`ADMISSION_LIMIT`] unconditionally.
    let cores = Cores::new(ADMISSION_LIMIT as i64);

    let timeout = timeout_for(queue_timeout_on);
    let layers = layers_for(stage, gate, backpressure, concurrency_limit, timeout);
    let (inner, worker, concurrency) = build_inner(
        &state,
        &clock,
        &cores,
        world.rt.handle(),
        stage,
        gate,
        backpressure,
        layers,
        concurrency_limit,
        timeout,
        None,
    );
    if let Some(worker) = worker {
        world.rt.spawn(worker);
    }
    let (front, hotswap) = HotSwap::new(inner);
    Server { state, svc: BoxCloneService::new(front), layers, concurrency, cores, hotswap }
}

/// A controller for the OS-CPU signal, when that signal is the gate. It steers the
/// same limit handle the concurrency-limit tab exposes to its slider — the difference
/// between the two tabs is only who turns the dial.
fn os_cpu_controller(
    gate: Option<Gate>,
    limit: &Option<ConcurrencyLimit>,
) -> Option<CpuConcurrencyController> {
    let limit = limit.clone().filter(|_| gate == Some(Gate::OsCpu))?;
    Some(CpuConcurrencyController::new(
        limit,
        CpuConfig {
            cpu_target: OS_CPU_MAX,
            min_limit: 1,
            max_limit: LIMIT_OFF,
            ..CpuConfig::default()
        },
    ))
}

/// One server: its state, its real taipei stack, its cores, and the live handles a
/// caller retunes without a rebuild. Everything here is per-machine — a sim holds one
/// of these per server it draws, all sharing the enclosing [`World`]'s clock.
pub(crate) struct Server {
    state: Arc<Mutex<Sim>>,
    svc: Svc,
    layers: Layers,
    concurrency: Option<ConcurrencyLimit>,
    cores: Cores,
    /// Loads a rebuilt stack into the stable front — see [`crate::hotswap`].
    hotswap: HotSwapHandle<SimReq, (), BoxError>,
}

impl Server {
    pub(crate) fn state(&self) -> Arc<Mutex<Sim>> {
        self.state.clone()
    }

    pub(crate) fn cores(&self) -> Cores {
        self.cores.clone()
    }

    pub(crate) fn svc(&self) -> Svc {
        self.svc.clone()
    }
}

/// The queue shed deadline for the toggle position: real 100 ms, or an hour so
/// long nothing ever sheds.
fn timeout_for(on: bool) -> Duration {
    if on {
        Duration::from_millis(TIMEOUT_MS as u64)
    } else {
        Duration::from_secs(3600)
    }
}

pub struct SimEngine {
    world: World,
    state: Arc<Mutex<Sim>>,
    queue_svc: Svc,
    obs: Obs,
    rng: Rng,
    stage: Stage,
    /// The admission-gate signal — set at construction; switching it rebuilds.
    gate: Gate,
    /// Whether a gate was *selected* (a gate-tabbed sim) rather than inherited from
    /// the stage. It decides whose gate the CPU toggle retunes.
    gated: bool,
    cpu_only: bool,
    /// The leaf server's failure mode — set at construction; switching it rebuilds.
    behavior: Behavior,
    speed: f64,
    backpressure: bool,
    queue_timeout_on: bool,
    response_timeout_ms: f64,
    processing: bool,
    io_speed: f64,
    concurrency_limit: usize,
    /// Live handle the OS-CPU controller steers — its concurrency limit (that gate only).
    concurrency: Option<ConcurrencyLimit>,
    /// Whether the CPU gate is the active signal — mirrors the reserve the layer polls,
    /// for the resting-openness the picture draws.
    backpressure_flag: bool,
    /// The OS-CPU signal's controller, steering the same limit handle. `Some` only
    /// while that signal is the gate; it reads utilisation where a server reads the
    /// OS, and moves the ceiling on [`OS_CPU_REFRESH_MS`].
    os_cpu: Option<CpuConcurrencyController>,
    /// Virtual time of the controller's next look.
    os_cpu_next: f64,
    /// Loads a rebuilt stack into the running front when a control changes the composition.
    hotswap: HotSwapHandle<SimReq, (), BoxError>,
    /// The one sender the untenanted sims model. A tenanted sim races [`TenantMix`]'s clocks
    /// instead, and this one is what its rates sum to.
    arrivals: Arrivals,
    next_id: u32,
    /// The CPU-core semaphore, shared with the leaf `App`. The client task holds one for
    /// the 1 ms accept burst before enqueue — the same pool request processing draws on.
    cores: Cores,
    /// `Some` only in the blame panel's sim. Its presence is what composes the tenant reporter
    /// into the stack and switches arrivals to the per-tenant streams; every other sim leaves it
    /// `None` and is bit-for-bit the sim it was.
    tenants: Option<TenantMix>,
    /// The reporter the stack bills through, paired with `tenants`.
    reporter: Option<TenantReporter>,
    /// The fleet's store, where this server is one of several sharing a budget. `Some` only in
    /// the rate-limiting sim; a tenanted engine without it measures and refuses nobody.
    limits: Option<Arc<Store>>,
    /// Where the reporter's completion callback writes.
    ledger: Ledger,
    /// Carries the last pump event's reading across the frame boundary, so the interval that
    /// straddles two frames is one interval like any other.
    blame: BlameSampler,
    /// Arrivals drawn this frame, handed back from [`TenantMix`] to be spawned.
    pending: Vec<SimReq>,
}

/// Where the tenant reporter's completion callback banks a finished request's fare.
///
/// It keeps its **own** lock rather than writing into [`Sim`]: the callback fires from a request's
/// future — from its `Drop`, which can run mid-unwind — and that is precisely where `Sim` is
/// already held. Two locks that are never nested cannot deadlock; one that is would.
#[derive(Clone, Default)]
pub struct Ledger(Arc<Mutex<Vec<(&'static str, Duration)>>>);

impl Ledger {
    /// The reporter's `report` closure. Bare push, no panic path: it runs during unwind.
    fn writer(&self) -> impl Fn(&str, Duration) + Clone {
        let inner = self.0.clone();
        move |tenant: &str, fare: Duration| {
            // The reporter hands back the `&str` it was admitted under, which is a `TenantSpec`
            // id — so the table is the authority on the name, not the string that came back.
            let named = TENANTS.iter().find(|t| t.id == tenant).map(|t| t.id).unwrap_or(SOLO);
            if let Ok(mut l) = inner.lock() {
                l.push((named, fare));
            }
        }
    }

    fn drain(&self) -> Vec<(&'static str, Duration)> {
        self.0.lock().map(|mut l| std::mem::take(&mut *l)).unwrap_or_default()
    }
}

/// One sender's Poisson clock: how often it sends, and when it next does. A stream of demand
/// is this and whatever the sender adds to it — a tenant adds its identity and its weight, a
/// load-balancing client adds the server it picks — so a sim with one sender and a sim with a
/// hundred differ in how many clocks they hold, not in what a clock is.
///
/// The gap is drawn by the caller, at the moment it sends, because that is where the draw
/// already happens: a clock that advanced itself on [`Arrivals::due`] would pull its gap ahead
/// of the request's own phases and move every number pinned against this stream.
#[derive(Clone)]
pub(crate) struct Arrivals {
    qps: f64,
    next: f64,
}

impl Arrivals {
    pub(crate) fn new(qps: f64, now_ms: f64, rng: &mut Rng) -> Arrivals {
        Arrivals { qps, next: now_ms + rng.exp(qps / 1000.0) }
    }

    /// The send instant this clock is holding, if it is sending at all and if the request
    /// would land by `t_end` — a request is sent [`NET_MS`] before it arrives.
    pub(crate) fn due(&self, t_end: f64) -> Option<f64> {
        (self.qps > 0.0 && self.next - NET_MS <= t_end).then_some(self.next)
    }

    /// The earliest send due by `t_end` among `clocks`, and whose it is — a crowd drawn one
    /// clock at a time, in the order the world reaches them. That order is what keeps the ids a
    /// reader watches running in send order, and what lets a sender that consults the world
    /// consult it as it stands when it sends.
    ///
    /// A clock sending nothing has no next send at all, so it drops out of the race rather than
    /// racing at an instant that never arrives.
    pub(crate) fn earliest<'a>(
        clocks: impl Iterator<Item = &'a Arrivals>,
        t_end: f64,
    ) -> Option<(usize, f64)> {
        clocks
            .enumerate()
            .filter_map(|(k, clock)| Some((k, clock.due(t_end)?)))
            .min_by(|a, b| a.1.total_cmp(&b.1))
    }

    /// The send happened: draw the gap to the next one from the instant it went out.
    pub(crate) fn sent(&mut self, at: f64, rng: &mut Rng) {
        self.next = at + rng.exp(self.qps / 1000.0);
    }

    /// Retune the rate. What has already been sent stands; a sender coming back from silence
    /// draws a fresh gap from `now_ms`, since the send it was holding is long past and
    /// honouring it would fire a burst the reader never asked for.
    pub(crate) fn set_qps(&mut self, qps: f64, now_ms: f64, rng: &mut Rng) {
        let woken = self.qps <= 0.0 && qps > 0.0;
        self.qps = qps.max(0.0);
        if woken {
            self.next = now_ms + rng.exp(self.qps / 1000.0);
        }
    }

    /// Hold the stream: nothing sends until it is [`released`](Arrivals::release). The sim that
    /// places a request by hand advances virtual time to do it, and a stream left running
    /// across that placement would send into it.
    pub(crate) fn hold(&mut self) {
        self.next = f64::INFINITY;
    }

    pub(crate) fn release(&mut self, now_ms: f64, rng: &mut Rng) {
        self.next = now_ms + rng.exp(self.qps / 1000.0);
    }
}

/// The tenants' arrival streams. It owns its own [`Rng`] and its own per-tenant schedule, which is
/// what keeps this feature free: the single-stream path every other sim takes never sees a draw it
/// did not make before, so their pinned numbers cannot move.
/// One tenant's arrival stream: who it is, the clock it sends on, and how big each of those
/// requests is. They travel as one record — a rate or a weight that could drift away from the
/// tenant it belongs to is a bug waiting for a fourth tenant.
struct Stream {
    spec: TenantSpec,
    /// When this tenant next sends, and how often.
    clock: Arrivals,
    /// The weight each of those requests carries, as the reader has set it. The two knobs are
    /// independent on purpose: sending twice as often and sending requests twice as big are
    /// different ways to occupy a server, and the scheme has to bill both.
    work: f64,
}

struct TenantMix {
    rng: Rng,
    streams: Vec<Stream>,
    /// Which tenant sent each request the engine still has, dropped when it departs. The
    /// picture outlives the engine's interest — a dot walks its last leg after the request
    /// is gone — so what the picture needs it keeps itself, and nothing here has to outlive
    /// the thing it names.
    whose: HashMap<u32, &'static str>,
}

impl TenantMix {
    fn new(seed: u64, now_ms: f64) -> TenantMix {
        let mut rng = Rng::new(seed);
        let streams = TENANTS
            .iter()
            .map(|spec| Stream {
                spec: *spec,
                clock: Arrivals::new(spec.qps, now_ms, &mut rng),
                work: spec.work,
            })
            .collect();
        TenantMix { rng, streams, whose: HashMap::new() }
    }

    /// Retune one tenant's rate.
    fn set_qps(&mut self, tenant: &str, qps: f64, now_ms: f64) {
        if let Some(stream) = self.streams.iter_mut().find(|s| s.spec.id == tenant) {
            stream.clock.set_qps(qps, now_ms, &mut self.rng);
        }
    }

    /// Retune one tenant's weight. What it has already sent keeps the weight it was sent with —
    /// a request's size is settled when its phases are drawn — so the change arrives with the
    /// next request, the way a rate change does.
    fn set_work(&mut self, tenant: &str, work: f64) {
        if let Some(stream) = self.streams.iter_mut().find(|s| s.spec.id == tenant) {
            stream.work = work.max(0.0);
        }
    }

    /// Draw every tenant's arrivals whose send instant falls in this frame, oldest first so the
    /// ids a reader watches run in send order whatever the rates are.
    fn arrivals(&mut self, t_end: f64, cpu_only: bool, next_id: &mut u32, out: &mut Vec<SimReq>) {
        loop {
            let due = Arrivals::earliest(self.streams.iter().map(|s| &s.clock), t_end);
            let Some((k, at)) = due else { return };
            let (tenant, work) = (self.streams[k].spec.id, self.streams[k].work);
            let id = *next_id;
            *next_id += 1;
            self.whose.insert(id, tenant);
            out.push(SimReq {
                id,
                enq_t: at,
                tenant,
                phases: make_phases(&mut self.rng, cpu_only, work),
            });
            self.streams[k].clock.sent(at, &mut self.rng);
        }
    }
}

/// The ceiling a stage starts at — its own manifest's, so the slider opens where the
/// composition actually runs and a stage that gates on nothing still has a number to
/// move. Sized in `taipei::compose`, beside the layer that enforces it.
pub fn default_limit(stage: Stage) -> usize {
    stage.manifest(Duration::from_millis(TIMEOUT_MS as u64)).limit.unwrap_or(REJECT_LIMIT)
}

/// The seed the single-server sims run on — one engine per page, deterministic, so a
/// fixed seed is the whole story. The cluster varies it per server ([`SimEngine::seeded`])
/// so ten servers at the same arrival rate are alike without being identical.
const ENGINE_SEED: u64 = 0xdeadbeef_cafe1234;

impl SimEngine {
    pub fn new(lambda: f64, stage: Stage, gate: Option<Gate>, cpu_only: bool, behavior: Behavior) -> Self {
        Self::seeded(ENGINE_SEED, lambda, stage, gate, cpu_only, behavior)
    }

    /// A [`SimEngine`] on a chosen seed — the fleet's ten servers each take their own, so
    /// the run-to-run micro-differences that make a real fleet uneven are the seeds', not a
    /// fudge factor.
    pub fn seeded(
        seed: u64,
        lambda: f64,
        stage: Stage,
        gate: Option<Gate>,
        cpu_only: bool,
        behavior: Behavior,
    ) -> Self {
        let backpressure = true;
        let queue_timeout_on = true;
        let response_timeout_ms = 1000.0;
        let processing = false;
        let io_speed = 1.0;
        let concurrency_limit = default_limit(stage);

        let world = World::new();
        let a = assemble(
            &world, stage, gate, backpressure, queue_timeout_on, response_timeout_ms, processing,
            io_speed, concurrency_limit,
        );

        let mut rng = Rng::new(seed);
        let arrivals = Arrivals::new(lambda, world.now_ms(), &mut rng);

        let mut obs = Obs::default();
        obs.layers = a.layers;
        obs.queue_timeout_ms = a.layers.queue_timeout.is_some().then_some(TIMEOUT_MS);
        obs.backpressure = a.layers.backpressure;
        obs.response_timeout_ms = response_timeout_ms;

        SimEngine {
            world,
            state: a.state,
            queue_svc: a.svc,
            obs,
            rng,
            stage,
            gate: gate.unwrap_or_default(),
            gated: gate.is_some(),
            cpu_only,
            behavior,
            speed: DEFAULT_SPEED,
            backpressure,
            queue_timeout_on,
            response_timeout_ms,
            processing,
            io_speed,
            concurrency_limit,
            os_cpu: os_cpu_controller(gate, &a.concurrency),
            concurrency: a.concurrency,
            backpressure_flag: a.layers.backpressure,
            os_cpu_next: 0.0,
            hotswap: a.hotswap,
            arrivals,
            next_id: 0,
            cores: a.cores,
            tenants: None,
            reporter: None,
            limits: None,
            ledger: Ledger::default(),
            blame: BlameSampler::default(),
            pending: Vec::new(),
        }
    }

    /// The blame panel's engine: the same single server every other single-server sim runs, with
    /// three tenants sharing it and the tenant reporter composed under its queue. Each tenant
    /// opens at its own [`TENANTS`] rate and the reader moves them from there. The reporter is
    /// built inside the clock's scope (its epoch is `Instant::now()`), then the stack is rebuilt
    /// through the ordinary hot-swap — the same path a control change takes.
    pub fn tenanted() -> Self {
        Self::tenanted_at(ENGINE_SEED, None)
    }

    /// One server of the rate-limiting chapter's fleet: tenanted like the blame panel, and
    /// wired to the store the fleet shares — blame goes out through it and the share to refuse
    /// comes back. `share` is this server's slice of each tenant's traffic, so a fleet of two
    /// takes half each and the tenant is asking the *fleet* for what the sliders say.
    pub fn rate_limited(seed: u64, limits: Arc<Store>, share: f64) -> Self {
        let mut engine = Self::tenanted_at(seed, Some(limits));
        for tenant in TENANTS {
            engine.set_tenant_qps(tenant.id, tenant.qps * share);
        }
        engine
    }

    /// Three tenants sharing one server, with the reporter composed under its queue. The
    /// reporter is built inside the clock's scope (its epoch is `Instant::now()`), then the
    /// stack is rebuilt through the ordinary hot-swap — the same path a control change takes.
    fn tenanted_at(seed: u64, limits: Option<Arc<Store>>) -> Self {
        let lambda = TENANTS.iter().map(|t| t.qps).sum();
        let mut engine =
            Self::seeded(seed, lambda, Stage::Queue, Some(Gate::RuntimeCpu), false, Behavior::Good);
        engine.reporter = Some({
            let _scope = engine.world.clock.enter();
            TenantReporter::new()
        });
        engine.limits = limits;
        engine.tenants = Some(TenantMix::new(seed, engine.world.now_ms()));
        engine.rebuild();
        engine
    }

    /// Drain the per-tenant fares reported since the last call — what the completion callback
    /// banked, which is the ledger the long chart totals.
    pub fn take_fares(&mut self) -> Vec<(&'static str, Duration)> {
        self.ledger.drain()
    }

    /// Which tenant a live id belongs to.
    pub fn whose(&self, id: u32) -> Option<&'static str> {
        self.tenants.as_ref()?.whose.get(&id).copied()
    }

    /// Move one tenant's share of the arrival stream. The reader divides a fixed total between
    /// the three, so what this changes is who is sending — never how much the server is asked.
    pub fn set_tenant_qps(&mut self, tenant: &str, qps: f64) {
        let now = self.world.now_ms();
        if let Some(mix) = self.tenants.as_mut() {
            mix.set_qps(tenant, qps, now);
        }
    }

    /// Move one tenant's weight — how much work each of its requests brings. The other way a
    /// tenant occupies a server, and the one counting requests cannot see at all: a tenant can
    /// hold the server shut on a *quiet* stream of heavy requests.
    pub fn set_tenant_work(&mut self, tenant: &str, work: f64) {
        if let Some(mix) = self.tenants.as_mut() {
            mix.set_work(tenant, work);
        }
    }

    /// The gate signal that is actually selected — `Some` for a gate-tabbed sim, `None`
    /// when the stage runs its own composition.
    fn selected_gate(&self) -> Option<Gate> {
        self.gated.then_some(self.gate)
    }

    /// Rebuild the stage's stack under the current controls and hot-swap it into the
    /// running front. In-flight requests finish on the old stack; its worker drains and
    /// stops once its front is dropped. The front every client holds is unchanged, so
    /// the clock, the stats, and the swarm keep going — only new admissions see the swap.
    fn rebuild(&mut self) {
        let timeout = timeout_for(self.queue_timeout_on);
        let gate = self.selected_gate();
        let layers = layers_for(self.stage, gate, self.backpressure, self.concurrency_limit, timeout);
        let (inner, worker, concurrency) = build_inner(
            &self.state,
            &self.world.clock,
            &self.cores,
            self.world.rt.handle(),
            self.stage,
            gate,
            self.backpressure,
            layers,
            self.concurrency_limit,
            timeout,
            self.reporter.as_ref().map(|reporter| Billing {
                reporter,
                ledger: &self.ledger,
                limits: self.limits.as_ref(),
            }),
        );
        if let Some(worker) = worker {
            self.world.rt.spawn(worker);
        }
        self.hotswap.load(inner);
        self.concurrency = concurrency;
        self.os_cpu = os_cpu_controller(gate, &self.concurrency);
        self.os_cpu_next = self.world.now_ms();
        self.backpressure_flag = layers.backpressure;
        self.obs.layers = layers;
        self.obs.queue_timeout_ms =
            (layers.queue_timeout.is_some() && self.queue_timeout_on).then_some(TIMEOUT_MS);
        self.obs.backpressure = layers.backpressure;
    }

    /// Set the concurrency ceiling. It is the slider's to move only where the manifest
    /// shows a limit and no controller is driving it; otherwise the number is stored for
    /// the next rebuild. Moving it rebuilds the stack — the ceiling is a `new(limit)` in
    /// the shown source, so the number that ran is the number shown.
    pub fn set_concurrency_limit(&mut self, n: f64) {
        let n = (n.max(1.0)) as usize;
        self.concurrency_limit = n;
        if self.obs.layers.limit.is_some() && self.selected_gate() != Some(Gate::OsCpu) {
            self.rebuild();
        }
    }

    /// Switch the admission signal. Each gate is its own composition — the one whose
    /// source the panel shows — so the switch rebuilds and swaps it in.
    pub fn set_gate(&mut self, gate: Gate) {
        self.gate = gate;
        self.state.lock().unwrap().gate = gate;
        self.rebuild();
    }

    /// Scale IO latency (1.0 = baseline). Lowering it models a slow dependency —
    /// requests hold their slot longer, so a fixed concurrency limit that was
    /// fine at 1.0 starts rejecting while CPU sits idle.
    pub fn set_io_speed(&mut self, mult: f64) {
        self.io_speed = mult.clamp(0.1, 4.0);
        self.state.lock().unwrap().io_speed = self.io_speed;
    }

    pub fn set_lambda(&mut self, qps: f64) {
        // 0 = no automatic arrivals (manual / user-driven load).
        let now = self.world.now_ms();
        self.arrivals.set_qps(qps.clamp(0.0, 500.0), now, &mut self.rng);
    }

    /// Inject one request sent right now — the blog's "send a request" / "GET /ping"
    /// control. It is an ordinary arrival (its inbound-network sleep starts at this
    /// instant), so nothing downstream special-cases it; only *when* the RNG is read
    /// for its phases differs. Returns the id so a caller can watch for its departure.
    pub fn inject(&mut self) -> u32 {
        let id = self.next_id;
        let req = SimReq {
            id,
            enq_t: self.world.now_ms() + NET_MS,
            tenant: SOLO,
            phases: make_phases(&mut self.rng, self.cpu_only, 1.0),
        };
        self.next_id += 1;
        self.spawn_client(req);
        id
    }

    /// Open the stage with a single request in flight, placed **deterministically** — no
    /// settling. Inject one request and advance exactly half its inbound-network leg, so it
    /// sits at `NetworkIn` p≈0.5, visibly incoming toward TCP SYN: the one request the reader
    /// is about to set moving. The qps stream is held across the placement and rescheduled
    /// from now, so it starts the moment the reader plays — not before. `inject` schedules its
    /// request directly, independent of the held stream, so exactly one request lands.
    pub fn open_incoming(&mut self) {
        self.arrivals.hold();
        self.inject();
        self.tick(NET_MS / 2.0 / self.speed); // one sized advance: virtual time moves NET_MS/2
        let now = self.world.now_ms();
        self.arrivals.release(now, &mut self.rng);
    }

    pub fn set_speed(&mut self, speed: f64) {
        self.speed = speed.clamp(0.00005, 3.0);
    }

    /// Toggle CPU backpressure. When it applies, the composition carries a real
    /// `CpuBackpressureLayer`; toggling it composes — or drops — that layer, so the
    /// change rebuilds and swaps. The stage's manifest decides whether it applies at all.
    pub fn set_backpressure(&mut self, on: bool) {
        self.backpressure = on;
        self.rebuild();
    }

    /// Toggle the queue's shed deadline. The deadline is a `QueueLayer::new(timeout)` in
    /// the shown source, so a change rebuilds with the new value and swaps it in.
    pub fn set_queue_timeout(&mut self, on: bool) {
        self.queue_timeout_on = on;
        self.rebuild();
    }

    pub fn set_processing_timeout(&mut self, on: bool) {
        self.processing = on;
        self.obs.processing_timeout_enabled = on;
        self.state.lock().unwrap().processing_timeout_enabled = on;
    }

    pub fn set_response_timeout_ms(&mut self, ms: f64) {
        self.response_timeout_ms = ms.max(1.0);
        self.obs.response_timeout_ms = self.response_timeout_ms;
        self.state.lock().unwrap().response_timeout_ms = self.response_timeout_ms;
    }

    pub fn tick(&mut self, real_dt_ms: f64) {
        let virt_dt = real_dt_ms * self.speed;
        let t_end = self.world.now_ms() + virt_dt;

        // Spawn every arrival whose *send* falls in this frame (a request is sent
        // NET_MS before it lands). The client task sleeps to its send instant, so
        // sends still stagger correctly as the pump advances virtual time. Drawing the
        // request (phases) then the next gap, per arrival, is the same RNG order as
        // spawning at the enqueue — only *when* it's read moves, so the stream holds.
        // Tenanted sims run their own streams off their own RNG (see `TenantMix`), so this loop —
        // and every number pinned against it — is exactly what it was.
        match self.tenants.as_mut() {
            Some(mix) => mix.arrivals(t_end, self.cpu_only, &mut self.next_id, &mut self.pending),
            None => {
                while let Some(at) = self.arrivals.due(t_end) {
                    let req = SimReq {
                        id: self.next_id,
                        enq_t: at,
                        tenant: SOLO,
                        phases: make_phases(&mut self.rng, self.cpu_only, 1.0),
                    };
                    self.next_id += 1;
                    self.spawn_client(req);
                    self.arrivals.sent(at, &mut self.rng);
                }
            }
        }
        // `TenantMix` cannot spawn (it does not own the world), so it hands its arrivals back here.
        for req in std::mem::take(&mut self.pending) {
            self.spawn_client(req);
        }

        // The admission gate's *resting* openness, mirrored from the same counts the
        // picture reads: whether the stack would admit a request standing here now.
        let limit = self.obs.layers.limit;
        let backpressure = self.backpressure_flag;
        let gate_open = move |s: &Sim| match limit {
            Some(n) => s.cpu.len() + s.io.len() + s.ready.len() < n,
            None => !backpressure || s.cpu.len() < ADMISSION_LIMIT,
        };

        // Core-occupancy extremes over the frame — the digest the sub-frame excursion
        // animations replay (a fill or a dip that reverses before the frame edge).
        let mut busy_peak = 0usize;
        let mut busy_min = usize::MAX;
        let mut gate_open_end = true;
        let states = [self.state.clone()];
        let os_cpu = &mut self.os_cpu;
        let os_cpu_next = &mut self.os_cpu_next;
        let reporter = self.reporter.as_ref();
        let sampler = &mut self.blame;
        let mut blame = BlameFrame::default();
        self.world.pump(t_end, &states, |now| {
            let mut s = states[0].lock().unwrap();
            s.util_sample(now);
            // The pump fires at every transition, so between two of these nothing changed:
            // one interval is one span per occupier, and the meter's wind across it is what
            // each of them accrued. Settling first is what makes that difference the interval's
            // own — otherwise a span banked lazily lands whole in whichever interval sees it.
            if let Some(r) = reporter {
                r.settle();
                sampler.event(&mut blame, now, r.meter(), r.shut(), &s.occupiers);
            }
            // The controller's cycle. It reads utilisation here where a server reads
            // the OS; the rate of these calls is the refresh period it counts in.
            if let Some(cc) = os_cpu.as_mut() {
                if now >= *os_cpu_next {
                    cc.observe(s.os_cpu_avg(now));
                    *os_cpu_next = now + OS_CPU_REFRESH_MS;
                }
            }
            busy_peak = busy_peak.max(s.cpu.len());
            busy_min = busy_min.min(s.cpu.len());
            gate_open_end = gate_open(&s);
        });
        self.obs.gate_open_end = gate_open_end;
        if let Some(r) = self.reporter.as_ref() {
            blame.meter = r.meter();
            blame.accumulated = r.accumulated();
            blame.attributed = r.attributed();
            blame.unattributed = r.unattributed();
            blame.shut_now = r.shut();
            blame.inflight = r.inflight();
            blame.occupiers.clone_from(&self.state.lock().unwrap().occupiers);
        }
        self.obs.blame = blame;

        self.snapshot(busy_peak, busy_min.min(busy_peak));
    }

    fn spawn_client(&self, req: SimReq) {
        let state = self.state.clone();
        let clock = self.world.clock.clone();
        let cores = self.cores.clone();
        let svc = self.queue_svc.clone();
        let behavior = self.behavior;
        // Only a composition with a request queue splits the accept from the handler.
        // Without one the accept burst is the head of the handler's first CPU, so there
        // is nothing to route through here. The assembled manifest is the authority —
        // the same one `assemble` set `accept_in_phases` from.
        let uses_app_queue = self.obs.layers.queue;
        self.world.rt.spawn(async move {
            let id = req.id;
            let enq_t = req.enq_t;
            let net = NetLegs { in_ms: NET_MS, out_ms: NET_MS };
            let send_t = arrive(&state, &clock, id, enq_t, net).await;

            // A server that never accept()s: the SYN sits in the backlog until the
            // client's deadline elapses, then the request fails. No CPU is ever spent.
            if behavior == Behavior::NeverAccept {
                let tmo = state.lock().unwrap().response_timeout_ms;
                clock::sleep(Duration::from_micros((tmo * 1000.0) as u64)).await;
                let mut s = state.lock().unwrap();
                s.leave_backlog(id);
                s.stats.response_timeout += 1;
                s.departures.push((id, Outcome::ResponseTimeout));
                return;
            }

            // A server that accepts then hangs: the accept races the client's deadline.
            // If a worker is free, the connection is accepted and the handler falls into an
            // endless compute loop — the worker is **leaked forever** (the client giving up
            // never frees the server's thread), so once every worker is a zombie no new
            // connection can be accepted at all and later clients give up before they are
            // ever accepted.
            if behavior == Behavior::AcceptHang {
                let deadline = {
                    let tmo = state.lock().unwrap().response_timeout_ms;
                    Instant::from_micros(((enq_t + tmo) * 1000.0) as u64)
                };
                let _permit = tokio::select! {
                    biased;
                    p = cores.acquire() => p,
                    _ = clock::sleep_until(deadline) => {
                        // Never accepted — every worker is already a hung zombie.
                        let mut s = state.lock().unwrap();
                        s.leave_backlog(id);
                        s.stats.response_timeout += 1;
                        s.timeout_window.push_back(now_ms(&clock));
                        s.departures.push((id, Outcome::ResponseTimeout));
                        return;
                    }
                };
                // Accepted: pin the worker forever, burning CPU burst after CPU burst. The
                // slot never frees (the permit is never dropped); the ring sweeps a fresh
                // lap each burst, so the core reads as endlessly-busy dead work.
                let slot = {
                    let mut s = state.lock().unwrap();
                    s.leave_backlog(id);
                    let slot = s.claim_core(id, 0, HANG_BURST_MS, now_ms(&clock), true);
                    // One hop for the zombie's arrival on its core; the repeated bursts
                    // are the same occupation, not new journeys.
                    s.hop(id, Station::Cpu { slot, p: 0.0, hung: true }, now_ms(&clock));
                    slot
                };
                let mut counted = false;
                let mut phase = 0usize;
                loop {
                    clock::sleep(Duration::from_micros((HANG_BURST_MS * 1000.0) as u64)).await;
                    state.lock().unwrap().cpu.retain(|c| c.id != id);
                    phase += 1;
                    // The client gives up once, at its deadline — counted and reported so a
                    // manual ping resolves — but the request stays live (its zombie core
                    // keeps spinning) rather than departing the picture.
                    if !counted && now_ms(&clock) >= (enq_t + state.lock().unwrap().response_timeout_ms) {
                        counted = true;
                        let mut s = state.lock().unwrap();
                        s.stats.response_timeout += 1;
                        s.timeout_window.push_back(now_ms(&clock));
                        s.departures.push((id, Outcome::ResponseTimeout));
                    }
                    // The next burst, on the core this handler never gives back.
                    state.lock().unwrap().cpu.push(CpuRec {
                        id, phase, slot, start_ms: now_ms(&clock), dur_ms: HANG_BURST_MS, hung: true,
                    });
                }
                // Unreachable — the handler hangs forever, holding `permit`.
            }

            // Good server: the shared journey, from the accept burst to the verdict.
            serve(state, clock, cores, svc, req, send_t, net, uses_app_queue).await;
        });
    }

    fn snapshot(&mut self, busy_peak: usize, busy_min: usize) {
        let now = self.world.now_ms();
        self.obs.busy_peak = busy_peak;
        self.obs.busy_min = busy_min;
        {
            let mut s = self.state.lock().unwrap();
            trim_window(&mut s.done_window, now);
            trim_window(&mut s.success_window, now);
            trim_window(&mut s.timeout_window, now);

            self.obs.t = now;
            self.obs.net_in = s.net_in.len();
            self.obs.net_out = s.net_out.len();
            self.obs.hops = std::mem::take(&mut s.hops);
            // Last tick's departures have been read — their latencies folded, their fares
            // banked — so the engine is done naming them. Forgetting here rather than at the
            // departure itself is what gives those readers their tick.
            if let Some(mix) = self.tenants.as_mut() {
                for (id, _) in &self.obs.departures {
                    mix.whose.remove(id);
                }
            }
            self.obs.departures = std::mem::take(&mut s.departures);
            self.obs.latencies = std::mem::take(&mut s.latencies);
            self.obs.syn_backlog = s.syn_backlog.clone();
            self.obs.queue_stubs = s.queue_stubs.clone();
            self.obs.ready = s.ready.clone();
            self.obs.io_sleeping = s.io.len();
            self.obs.cpu = s
                .cpu
                .iter()
                .map(|c| CpuTask {
                    id: c.id,
                    phase: c.phase,
                    remaining_ms: (c.dur_ms - (now - c.start_ms)).max(0.0),
                    dur_ms: c.dur_ms,
                    slot: Some(c.slot),
                })
                .collect();
            self.obs.stats = s.stats.clone();
            self.obs.last_latency_ms = s.last_latency_ms;
            // The smoothed load the controller acted on, not the raw sample — the
            // readout should show what moved the ceiling. And the ceiling itself is
            // the controller's now, so the picture reads it back off the live handle
            // rather than the value it was assembled with.
            self.obs.os_cpu_pct =
                self.os_cpu.as_ref().map(|cc| (cc.load() * 100.0).round() as u32);
            if self.os_cpu.is_some() {
                self.obs.layers.limit = self.concurrency.as_ref().map(|c| c.get());
            }
            self.obs.success_5s = s.success_window.len();
            self.obs.done_5s = s.done_window.len();
            self.obs.timeout_5s = s.timeout_window.len();

            // The station partition: every live id, exactly once, tagged with where it
            // sits and its progress. Motion reads only this (plus `departures`).
            // Two queues at the worker boundary, never merged: the kernel accept queue
            // (SYNs, FIFO by handshake completion) and the tokio run queue (spawned
            // handlers and IO wakes). They share only the core pool.
            let mut live: Vec<(u32, Station)> = Vec::new();
            for (idx, (id, _)) in s.syn_backlog.iter().enumerate() {
                live.push((*id, Station::SynBacklog { idx }));
            }
            for (idx, id) in s.ready.iter().enumerate() {
                live.push((*id, Station::RunQueue { idx }));
            }
            // Inbound network — the real client-side sleep down the SYN pipe. The landing
            // slot assumes no accepts: the backlog now, plus every send still in flight
            // ahead of this one (`net_in` is in send order). Conservative — the real slot
            // is only ever *ahead* — so once it lands the dot pursues forward, never
            // backwards.
            let syn_len = s.syn_backlog.len();
            for (i, f) in s.net_in.iter().enumerate() {
                live.push((f.id, Station::NetworkIn { p: f.p(now), slot: syn_len + i }));
            }
            // Outbound network — the verdict on its way home, placed by its own progress
            // through the same kind of leg, and carrying what the stack decided.
            for (f, reply) in &s.net_out {
                live.push((f.id, Station::NetworkOut { p: f.p(now), reply: *reply }));
            }
            for c in &s.cpu {
                let p = (1.0 - (c.dur_ms - (now - c.start_ms)).max(0.0) / c.dur_ms.max(1.0))
                    .clamp(0.0, 1.0);
                live.push((c.id, if c.phase == ACCEPT_PHASE && !c.hung {
                    Station::Accept { slot: c.slot, p }
                } else {
                    Station::Cpu { slot: c.slot, p, hung: c.hung }
                }));
            }
            for (idx, (id, enq_t)) in s.queue_stubs.iter().enumerate() {
                let age = ((now - enq_t) / TIMEOUT_MS).clamp(0.0, 1.0);
                live.push((*id, Station::AppQueue { idx, age }));
            }
            for r in &s.io {
                let p = if r.dur_ms > 0.0 {
                    (1.0 - (r.dur_ms - (now - r.start_ms)).max(0.0) / r.dur_ms).clamp(0.0, 1.0)
                } else {
                    1.0
                };
                live.push((r.id, Station::Io { p }));
            }
            self.obs.live = live;
        }
        self.obs.maybe_sample_spark();
    }

    pub fn obs(&mut self) -> &mut Obs {
        &mut self.obs
    }

    /// This server's live tallies as the [`ServerBox`](crate::atoms::server_box::ServerBox)
    /// summary draws them — read straight off the running state, so the box and the full
    /// picture of the same engine can never disagree.
    pub fn counts(&self) -> ServerCounts {
        self.state.lock().unwrap().counts()
    }
}

// ----- reading a tenanted engine for the stage --------------------------------

/// Which tenant a name belongs to, as an index into [`TENANTS`] — the one place a name becomes
/// a position, so nothing else has to know the cast's order.
pub fn tenant_of(name: Option<&str>) -> Option<usize> {
    let name = name?;
    TENANTS.iter().position(|t| t.id == name)
}

/// Which wire a leg is on: out to the server, or home in one of the [`HOMEWARD`] lanes.
enum Lane {
    Out,
    Home(usize),
}

/// What each tenant has on the wire, both ways: the requests still travelling out, and the
/// verdicts on their way home in each lane. Indexed by tenant, in [`TENANTS`] order.
pub struct OnTheWire {
    pub out: Vec<Vec<f64>>,
    pub home: Vec<Vec<Vec<f64>>>,
}

/// Where every leg in the machine has got to, as the fraction of its wire covered — the engine's
/// own `p` through the leg, never a reading of the clock. Both directions come off the one
/// snapshot channel, so a reply is placed on its wire home exactly as a request is on its wire
/// out.
///
/// The bows are drawn client-to-server, so a reply that has covered `p` of its leg stands at
/// `1 - p`.
///
/// The tenant is the engine's to name and naming one borrows it, so every leg is read off the
/// snapshot before anyone is asked whose it is.
pub fn on_the_wire(engine: &mut SimEngine) -> OnTheWire {
    let legs: Vec<(u32, Lane, f64)> = engine
        .obs()
        .live
        .iter()
        .filter_map(|&(id, station)| match station {
            Station::NetworkIn { p, .. } => Some((id, Lane::Out, p)),
            Station::NetworkOut { p, reply } => {
                let lane = HOMEWARD.iter().position(|&o| o == reply.outcome())?;
                Some((id, Lane::Home(lane), 1.0 - p))
            }
            _ => None,
        })
        .collect();
    let mut wire = OnTheWire {
        out: vec![Vec::new(); TENANTS.len()],
        home: vec![vec![Vec::new(); HOMEWARD.len()]; TENANTS.len()],
    };
    for (id, lane, at) in legs {
        let Some(k) = tenant_of(engine.whose(id)) else { continue };
        match lane {
            Lane::Out => wire.out[k].push(at),
            Lane::Home(lane) => wire.home[k][lane].push(at),
        }
    }
    wire
}

/// Where one tenant's requests are waiting on a server — the same three queues a box always
/// showed, cut by who is standing in them.
#[derive(Clone, Copy, Default, PartialEq)]
pub struct TenantQueues {
    /// The kernel's accept queue.
    pub tcp: usize,
    /// The taipei admission queue.
    pub app: usize,
    /// The runtime's run queue.
    pub run: usize,
}

/// The three queue depths, per tenant, in [`TENANTS`] order.
///
/// A box shared between tenants has one number that matters more than how deep a queue is:
/// *whose* it is. The depths still sum to the box's own counts — this is the same queue, read
/// by occupant.
///
/// The tenant is the engine's to name and naming one borrows it, so every waiting id is read
/// off the snapshot before anyone is asked whose it is.
pub fn queues_by_tenant(engine: &mut SimEngine) -> Vec<TenantQueues> {
    let obs = engine.obs();
    let (tcp, app, run) = (
        obs.syn_backlog.iter().map(|&(id, _)| id).collect::<Vec<_>>(),
        obs.queue_stubs.iter().map(|&(id, _)| id).collect::<Vec<_>>(),
        obs.ready.clone(),
    );
    let mut queues = vec![TenantQueues::default(); TENANTS.len()];
    for (ids, depth) in [
        (tcp, (|q: &mut TenantQueues| &mut q.tcp) as fn(&mut TenantQueues) -> &mut usize),
        (app, |q| &mut q.app),
        (run, |q| &mut q.run),
    ] {
        for id in ids {
            if let Some(k) = tenant_of(engine.whose(id)) {
                *depth(&mut queues[k]) += 1;
            }
        }
    }
    queues
}

/// Who holds each busy core, in slot order — the colours the server box's pips wear in tenant
/// mode. Slot order, so a pip's colour changes when its core changes hands and not because
/// another core did; one entry per busy core, so the row of pips still says how many are busy.
///
/// The tenant is the engine's to name and naming one borrows it, so the cores are read off the
/// snapshot before anyone is asked whose they are.
pub fn core_ink_of(engine: &mut SimEngine) -> Vec<Paint> {
    let mut on: Vec<(usize, u32)> =
        engine.obs().cpu.iter().filter_map(|c| Some((c.slot?, c.id))).collect();
    on.sort_unstable();
    on.into_iter()
        .map(|(_, id)| {
            tenant_of(engine.whose(id)).map_or(Hue::blue.value(), |k| TENANTS[k].tint)
        })
        .collect()
}

// ----- phase plan ------------------------------------------------------------

/// Drop timestamps older than the 5 s rolling window the rate readouts average over.
fn trim_window(w: &mut std::collections::VecDeque<f64>, now: f64) {
    while w.front().is_some_and(|&t| now - t > 5000.0) {
        w.pop_front();
    }
}

/// A CPU burst's duration, in virtual ms: uniform over `[CPU_BASE_MS, CPU_BASE_MS + CPU_SPREAD_MS]`.
const CPU_BASE_MS: f64 = 12.0;
const CPU_SPREAD_MS: f64 = 20.0;
/// An IO wait's duration, in virtual ms: uniform over `[IO_BASE_MS, IO_BASE_MS + IO_SPREAD_MS]`.
const IO_BASE_MS: f64 = 16.0;
const IO_SPREAD_MS: f64 = 44.0;

/// `work` scales the compute bursts against the shared baseline — a tenant's request size. At
/// `1.0` the arithmetic is the identity, so every sim that does not model tenants draws exactly
/// the phases it always did.
pub(crate) fn make_phases(rng: &mut Rng, cpu_only: bool, work: f64) -> Vec<Phase> {
    let cpu = |rng: &mut Rng| {
        Phase::Cpu(Duration::from_micros(
            ((CPU_BASE_MS + rng.f64() * CPU_SPREAD_MS) * work * 1000.0) as u64,
        ))
    };
    if cpu_only {
        // isolated: accept → compute → reply, no IO.
        return vec![cpu(rng)];
    }
    let r = rng.f64();
    // The ladder [`MEAN_IO_WAITS`] averages. Keep the two together: a band moved here and not
    // there would leave the cost knob reading in a unit the phases do not use.
    let nio = if r < 0.05 { 0 } else if r < 0.25 { 1 } else if r < 0.55 { 2 }
              else if r < 0.80 { 3 } else if r < 0.95 { 4 } else { 5 };
    let mut phases = vec![cpu(rng)];
    for _ in 0..nio {
        phases.push(Phase::Io(Duration::from_millis((IO_BASE_MS + rng.f64() * IO_SPREAD_MS) as u64)));
        phases.push(cpu(rng));
    }
    phases
}

/// The mean of the IO-wait ladder in [`make_phases`]: `0…5` waits at `.05 .20 .30 .25 .15 .05`.
const MEAN_IO_WAITS: f64 = 2.4;

/// How much CPU one of a tenant's requests asks for on average, at weight `work` — what "an
/// expensive request" means in milliseconds, and the unit the cost knob reads in.
///
/// A request is one burst plus one more after every IO wait, and a burst is uniform over
/// `[CPU_BASE_MS, CPU_BASE_MS + CPU_SPREAD_MS]`. Both readings come off the phase plan itself,
/// so the knob's units cannot drift from the work it is setting.
pub fn cpu_cost_ms(work: f64) -> f64 {
    (1.0 + MEAN_IO_WAITS) * (CPU_BASE_MS + CPU_SPREAD_MS / 2.0) * work
}

/// The weight that costs `ms` of CPU per request — [`cpu_cost_ms`] the other way round, so the
/// knob can be read in milliseconds and set in the units the phase plan scales by.
pub fn work_for_cpu_cost(ms: f64) -> f64 {
    ms / cpu_cost_ms(1.0)
}

// ----- xorshift64 RNG --------------------------------------------------------

pub(crate) struct Rng(u64);
impl Rng {
    pub(crate) fn new(seed: u64) -> Self {
        Self(seed ^ 0x9e3779b97f4a7c15)
    }
    pub(crate) fn u64(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    pub(crate) fn f64(&mut self) -> f64 {
        (self.u64() >> 11) as f64 / (1u64 << 53) as f64
    }
    /// A uniform draw from `0..n` — the blind pick, wherever a client makes one.
    pub(crate) fn below(&mut self, n: usize) -> usize {
        ((self.f64() * n as f64) as usize).min(n - 1)
    }
    fn exp(&mut self, lambda: f64) -> f64 {
        -self.f64().max(1e-12).ln() / lambda
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run the engine for a span of **virtual** time. Speed is a display knob — how
    /// much virtual time one real frame buys — so a test that asserts on the model
    /// pins it and says how long it wants the world to run, rather than counting
    /// frames and inheriting whatever [`DEFAULT_SPEED`] happens to be.
    fn drive(engine: &mut SimEngine, virtual_ms: f64) {
        engine.set_speed(1.0);
        for _ in 0..(virtual_ms / 16.0).ceil() as u32 {
            engine.tick(16.0);
        }
    }

    /// The overload contrast the visualiser teaches, driven natively against the
    /// real taipei stack.
    #[test]
    fn overload_behaviour_across_stages() {
        // Queue stage at a sustainable rate: nothing sheds.
        let mut calm = SimEngine::new(75.0, Stage::Queue, None, false, Behavior::Good);
        drive(&mut calm, 1440.0);
        let o = calm.obs();
        assert!(o.stats.arrived > 0, "arrivals flow");
        assert_eq!(o.stats.queue_timeout, 0, "75 qps keeps up: no queue timeouts");
        assert!(o.stats.success > 0, "sustainable load succeeds");
        // Latency is client-perceived: it spans both network legs, so even the fastest
        // reply cannot beat the inbound + outbound travel time.
        assert!(
            o.last_latency_ms.unwrap_or(0.0) >= RTT_MS,
            "round-trip latency includes both network legs: {:?}",
            o.last_latency_ms
        );

        // Queue stage overloaded: the REAL QueueError::Timeout sheds work.
        let mut hot = SimEngine::new(300.0, Stage::Queue, None, false, Behavior::Good);
        drive(&mut hot, 2880.0);
        let shed = hot.obs().stats.queue_timeout;
        assert!(shed > 20, "300 qps must shed via the real queue deadline, shed={shed}");

        // Bare app at the same load: nothing sheds, the run queue explodes.
        let mut bare = SimEngine::new(300.0, Stage::App, None, false, Behavior::Good);
        drive(&mut bare, 2880.0);
        let o = bare.obs();
        assert_eq!(o.stats.queue_timeout, 0, "no queue, no queue timeouts");
        assert!(
            o.ready.len() + o.io_sleeping + o.busy() > 50,
            "unprotected overload piles up in-flight work: {}",
            o.ready.len() + o.io_sleeping + o.busy()
        );

        // Reject stage overloaded: the real RejectionLayer sheds immediately.
        let mut rej = SimEngine::new(300.0, Stage::Reject, None, false, Behavior::Good);
        drive(&mut rej, 2880.0);
        let o = rej.obs();
        assert!(o.stats.rejected > 20, "the reject layer sheds, rejected={}", o.stats.rejected);
        assert_eq!(o.stats.queue_timeout, 0, "no queue in the reject stage");
    }

    /// The gate chapter's comparison: each admission signal gates the same queue, so
    /// overload always sheds via the REAL queue deadline — never rejection — and the
    /// OS-CPU average visibly lags the load it summarises.
    #[test]
    fn gate_signals_gate_the_queue() {
        // Concurrency-limit gate: the real limit layer withholds readiness between the
        // queue and the app, so under overload the queue sheds on its deadline.
        let mut cc =
            SimEngine::new(300.0, Stage::Queue, Some(Gate::ConcurrencyLimit), false, Behavior::Good);
        drive(&mut cc, 2880.0);
        let o = cc.obs();
        assert!(o.stats.queue_timeout > 20, "limit gate sheds via the queue: {}", o.stats.queue_timeout);
        assert_eq!(o.stats.rejected, 0, "a gate never rejects; only the queue sheds");
        assert!(o.layers.limit.is_some() && !o.layers.backpressure, "manifest shows the limit gate");

        // The runtime-CPU gate is the real `CpuBackpressureLayer`, so what it polls is
        // the reserve the cores keep parked — the same arithmetic a production runtime
        // reports through park/unpark, and the reason a wrong reserve reads as a gate
        // that never shuts.
        let cores = Cores::new(ADMISSION_LIMIT as i64);
        let instr = cores.instrumentation();
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let held: Vec<_> = rt.block_on(async {
            let mut held = Vec::new();
            for _ in 0..ADMISSION_LIMIT {
                held.push(cores.acquire().await);
            }
            held
        });
        assert_eq!(instr.available(), 0, "the reserve is spent; the gate is shut");
        drop(held);
        assert_eq!(instr.available(), ADMISSION_LIMIT, "parked cores reopen it");

        // OS-CPU gate under sustained overload: the average reads hot and the queue sheds.
        let mut os = SimEngine::new(300.0, Stage::Queue, Some(Gate::OsCpu), false, Behavior::Good);
        drive(&mut os, 2880.0);
        let o = os.obs();
        let pct = o.os_cpu_pct.expect("the OS-CPU gate reports its average");
        assert!(pct >= 50, "sustained overload reads hot: {pct}%");
        assert!(o.stats.queue_timeout > 0, "a shut gate backs the queue up to its deadline");

        // A gate switch is live, not a rebuild: mid-incident (the limit gate shedding
        // under overload), switching to runtime CPU keeps every counter and the
        // in-flight work — only the manifest and who withholds readiness change.
        let arrived_before = cc.obs().stats.arrived;
        let inflight_before = {
            let o = cc.obs();
            o.busy() + o.io_sleeping + o.ready.len()
        };
        assert!(inflight_before > 0, "work is in flight at the switch");
        cc.set_gate(Gate::RuntimeCpu);
        let o = cc.obs();
        assert!(o.layers.limit.is_none() && o.layers.backpressure, "manifest follows the switch");
        drive(&mut cc, 288.0);
        let o = cc.obs();
        assert!(o.stats.arrived > arrived_before, "the run carried on");
        assert!(o.busy() + o.io_sleeping + o.ready.len() > 0, "work still flows after the switch");

        // The lag, at the burst's leading edge. The cores are already saturated, but
        // the trailing average still reads the calm past — and a saturated server under
        // a calm reading is precisely the case for *widening* the ceiling. So the
        // controller opens up into a machine that is already full, and only sheds once
        // the average catches up. That overshoot is the delayed signal's real cost.
        let mut burst = SimEngine::new(500.0, Stage::Queue, Some(Gate::OsCpu), false, Behavior::Good);
        drive(&mut burst, 720.0); // ~0.7 s virtual: saturated now, cold on average
        let o = burst.obs();
        assert_eq!(o.busy(), CORES, "the burst saturated the cores");
        let pct = o.os_cpu_pct.expect("the OS-CPU controller reports its average");
        assert!(pct < 75, "the average still reads the calm past: {pct}%");
        let overshoot = o.layers.limit.expect("the controller's ceiling");
        assert!(
            overshoot > OS_CPU_INITIAL_LIMIT,
            "so it widens into a full machine: {overshoot}"
        );

        // Once the average catches up to the overload, it gives the ceiling back.
        drive(&mut burst, 5760.0);
        let o = burst.obs();
        assert!(
            o.layers.limit.is_some_and(|n| n < overshoot),
            "the correction arrives late, but it arrives: {:?}",
            o.layers.limit
        );
    }

    /// Determinism: the discrete-event clock (deadline-ordered, FIFO ties) plus the
    /// seeded RNG make two identical runs bit-identical in outcome — each engine's
    /// clock starts at zero. This is what fixed-timestep ticking preserves across
    /// displays.
    #[test]
    fn identical_runs_produce_identical_stats() {
        let run = || {
            let mut e = SimEngine::new(225.0, Stage::Queue, None, false, Behavior::Good);
            drive(&mut e, 1920.0);
            let o = e.obs();
            (o.stats.arrived, o.stats.success, o.stats.queue_timeout, o.stats.response_timeout)
        };
        let (a, b) = (run(), run());
        assert_eq!(a, b, "identical runs must produce identical stats");
    }

    /// The quiescent pump fills the cores.
    #[test]
    fn saturated_pump_fills_cores() {
        let mut hot = SimEngine::new(375.0, Stage::Queue, None, false, Behavior::Good);
        hot.set_speed(1.0);
        let mut peak = 0usize;
        for _ in 0..180 {
            hot.tick(16.0);
            peak = peak.max(hot.obs().busy_peak);
        }
        // Overload's un-gated IO-returns fill every core; the frame-peak catches it
        // even when a burst ends at the frame edge.
        assert_eq!(peak, CORES, "IO-bound overload fills every core, peak={peak}");
    }

    /// Toggles retune live handles — never a rebuild, no in-flight work lost.
    #[test]
    fn toggles_retune_live_handles() {
        let mut e = SimEngine::new(225.0, Stage::Queue, None, false, Behavior::Good);
        drive(&mut e, 960.0);
        let (arrived0, t0) = { let o = e.obs(); (o.stats.arrived, o.t) };
        assert!(arrived0 > 0, "work flows before the toggles");

        e.set_backpressure(false);
        e.set_queue_timeout(false);
        drive(&mut e, 480.0);
        e.set_backpressure(true);
        e.set_queue_timeout(true);
        drive(&mut e, 480.0);

        let (arrived1, t1) = { let o = e.obs(); (o.stats.arrived, o.t) };
        assert!(arrived1 > arrived0, "arrivals keep flowing across toggles: {arrived0} -> {arrived1}");
        assert!(t1 > t0, "the clock keeps advancing across toggles: {t0} -> {t1}");

        // Queue-timeout off ⇒ an hour-long deadline ⇒ the radial stops; on restores it.
        e.set_queue_timeout(false);
        assert!(e.obs().queue_timeout_ms.is_none(), "timeout off hides the shed deadline");
        e.set_queue_timeout(true);
        assert_eq!(e.obs().queue_timeout_ms, Some(TIMEOUT_MS), "timeout on restores it");
    }

    /// Leaf-server failure modes (bare app, user-driven load via inject).
    #[test]
    fn bad_servers_never_reply() {
        let mut never = SimEngine::new(0.0, Stage::App, None, true, Behavior::NeverAccept);
        never.set_speed(1.0);
        for i in 0..90 {
            if i % 6 == 0 { never.inject(); }
            never.tick(16.0);
            assert_eq!(never.obs().busy(), 0, "never-accept spends no CPU");
        }
        let o = never.obs();
        assert_eq!(o.stats.success, 0, "never-accept never replies");
        assert!(o.stats.response_timeout > 0, "never-accept times the client out");

        let mut hang = SimEngine::new(0.0, Stage::App, None, true, Behavior::AcceptHang);
        hang.set_speed(1.0);
        let mut hang_peak = 0;
        for i in 0..120 {
            if i % 3 == 0 { hang.inject(); }
            hang.tick(16.0);
            hang_peak = hang_peak.max(hang.obs().busy_peak);
        }
        let o = hang.obs();
        assert_eq!(o.stats.success, 0, "accept-hang never replies");
        assert!(o.stats.response_timeout > 0, "accept-hang times the client out");
        assert_eq!(hang_peak, CORES, "accept-hang fills every core with hung handlers");
        // The zombies never free: every worker stays occupied long after the clients
        // gave up — the server is dead until it restarts.
        assert_eq!(hang.obs().busy(), CORES, "hung workers are leaked, not recovered");
    }

    /// The tuned per-sim defaults tell their chapter's story. Mirrors the defaults in
    /// `queue_viz::run`, run for 60 virtual seconds). The invariants the
    /// numbers were chosen for: a healthy bare app never times a client out; pushed
    /// past capacity it collapses; the protection stages shed at the gate and serve
    /// what they admit — response timeouts behind working protection mean the tuning
    /// is wrong.
    #[test]
    fn tuned_defaults_tell_their_chapters_story() {
        let tuned = |qps: f64, stage: Stage, cpu_only: bool, rtmo_ms: f64| {
            let mut e = SimEngine::new(qps, stage, None, cpu_only, Behavior::Good);
            e.set_response_timeout_ms(rtmo_ms);
            drive(&mut e, 60_000.0);
            let o = e.obs();
            (o.stats.clone(), o.ready.len(), o.syn_backlog.len())
        };

        let (s, _, _) = tuned(150.0, Stage::App, true, 2000.0);
        assert_eq!(s.response_timeout, 0, "healthy cpu app times nobody out");
        assert!(s.success > 3000, "healthy cpu app serves its load: {}", s.success);

        let (s, _, _) = tuned(62.5, Stage::App, false, 2000.0);
        assert_eq!(s.response_timeout, 0, "healthy cpu_io app times nobody out (the mixed workload's honest tail fits 2 s)");

        let (s, ready, _syn) = tuned(200.0, Stage::App, false, 2000.0);
        assert!(s.response_timeout > s.success, "an overloaded bare app collapses: rtmo={} success={}", s.response_timeout, s.success);
        // Accept tasks and IO-woken handlers alike wait for a worker, so a saturated
        // server's backlog shows in the run queue (the accept task stuck behind the work
        // is exactly what starves new accepts).
        assert!(ready > 100, "unprotected overload piles up in the run queue: {ready}");

        let (s, _, _) = tuned(75.0, Stage::Reject, false, 2500.0);
        assert_eq!(s.response_timeout, 0, "behind a working limit, admitted work completes in time");
        assert!(s.rejected > 0, "the limit visibly sheds the overflow");
        assert!(s.success > s.rejected, "the limit serves more than it sheds: {} vs {}", s.success, s.rejected);

        let (s, _, _) = tuned(375.0, Stage::Queue, false, 2500.0);
        assert!(s.queue_timeout > s.success, "the queue sheds the spike at the gate");
        assert!(
            s.response_timeout * 50 <= s.success,
            "behind a working queue, response timeouts are a rare few: rtmo={} success={}",
            s.response_timeout, s.success
        );
        assert!(s.success > 2000, "the queue keeps serving at capacity through the spike: {}", s.success);
    }

    /// The station projection is a faithful partition, and hops are its continuous
    /// counterpart: every station entry is reported, and an id's last hop always
    /// agrees with where `live` says it is now.
    #[test]
    fn stations_partition_and_hops_agree() {
        use std::collections::{HashMap, HashSet};
        let kind = |s: &Station| std::mem::discriminant(s);
        let mut proj = SimEngine::new(300.0, Stage::Queue, None, false, Behavior::Good);
        proj.set_speed(1.0);
        let mut departed = 0u32;
        let mut queue_hops = HashSet::new();
        let mut saw = [false; 7]; // netin, accept, appq, syn, runq, cpu/io, netout
        for _ in 0..150 {
            proj.tick(16.0);
            let o = proj.obs();
            let mut last_hop: HashMap<u32, Station> = HashMap::new();
            for hop in &o.hops {
                last_hop.insert(hop.id, hop.station);
                if matches!(hop.station, Station::AppQueue { .. }) {
                    queue_hops.insert(hop.id);
                }
                // Coverage is read off the itinerary, not `live`: a good server's SYN
                // backlog is now a sub-frame transit point (the accept task's wait moved
                // to the run queue), so it appears in hops but rarely dwells in a snapshot.
                match hop.station {
                    Station::NetworkIn { .. } => saw[0] = true,
                    Station::Accept { .. } => saw[1] = true,
                    Station::AppQueue { .. } => saw[2] = true,
                    Station::SynBacklog { .. } => saw[3] = true,
                    Station::RunQueue { .. } => saw[4] = true,
                    Station::Cpu { .. } | Station::Io { .. } => saw[5] = true,
                    Station::NetworkOut { .. } => saw[6] = true,
                }
            }
            for (id, st) in &o.live {
                if let Some(h) = last_hop.get(id) {
                    assert_eq!(kind(h), kind(st), "an id's last hop ends at its live station");
                }
            }
            // Every live id appears exactly once.
            let ids: HashSet<u32> = o.live.iter().map(|(id, _)| *id).collect();
            assert_eq!(ids.len(), o.live.len(), "live ids are unique");
            // The station count matches the collections it is derived from.
            let expected = o.syn_backlog.len() + o.queue_stubs.len() + o.ready.len()
                + o.cpu.len() + o.io_sleeping + o.net_in + o.net_out;
            assert_eq!(o.live.len(), expected, "live partitions the collections");
            departed += o.departures.len() as u32;
        }
        // Departures account for every terminal outcome, no more, no less.
        let s = proj.obs().stats.clone();
        let terminal = s.success + s.queue_timeout + s.rejected + s.response_timeout + s.processing_timeout;
        assert_eq!(departed, terminal, "departures == terminal outcomes: {departed} vs {terminal}");
        assert!(saw.iter().all(|&b| b), "every station kind is exercised: {saw:?}");
        // Every request that reached a queue verdict recorded an AppQueue hop — the
        // itinerary reports the queue even when its residence was sub-frame.
        assert!(
            queue_hops.len() as u32 >= s.success + s.queue_timeout,
            "queue hops cover every queued request: {} vs {}",
            queue_hops.len(),
            s.success + s.queue_timeout
        );
    }

    /// The reply home is *real state*: a verdict is live on the outbound leg for the whole of
    /// it, and its departure — the client's receipt — lands only once the leg is travelled,
    /// never in the frame it set off. The give-up is the one verdict with no leg home, so
    /// nothing rides back for it.
    #[test]
    fn a_verdict_travels_home_before_it_is_received() {
        use std::collections::HashSet;
        let mut e = SimEngine::new(300.0, Stage::Queue, None, false, Behavior::Good);
        // Slow enough that a frame is shorter than the leg, so the travel is observable.
        e.set_speed(0.1);
        let mut flying: HashSet<u32> = HashSet::new();
        let mut received = 0u32;
        let mut seen_mid_leg = false;
        for _ in 0..900 {
            e.tick(16.0);
            let o = e.obs();
            let set_off: Vec<u32> = o
                .hops
                .iter()
                .filter(|h| matches!(h.station, Station::NetworkOut { .. }))
                .map(|h| h.id)
                .collect();
            for &(id, outcome) in &o.departures {
                match outcome.reply() {
                    Some(_) => {
                        assert!(!set_off.contains(&id), "a reply is not received in the frame it set off");
                        assert!(flying.remove(&id), "and it was seen flying home first");
                        received += 1;
                    }
                    // The client gave up: nothing was ever sent, so nothing was ever on a leg.
                    None => assert!(
                        !set_off.contains(&id) && !flying.contains(&id),
                        "nothing rides home for a give-up",
                    ),
                }
            }
            seen_mid_leg |= o.live.iter().any(|(_, s)| matches!(s, Station::NetworkOut { .. }));
            flying.extend(set_off);
        }
        assert!(received > 0, "replies reach clients");
        assert!(seen_mid_leg, "and are on the leg home while they travel");
    }

    /// Weight is a live variable like the rate, and the meter sees it: tenants that open level
    /// are billed by what they send, and raising one tenant's weight — without touching a single
    /// rate — raises its share of the bill. The reader can only be shown that if the panel can
    /// do it, so this drives the capability rather than trusting it.
    /// The cost knob reads in milliseconds, so its units have to be the phase plan's own. Both
    /// [`CPU_BASE_MS`]'s spread and the IO ladder are restated to convert, and a restatement
    /// that drifts would leave the reader a slider labelled in a unit nothing uses.
    #[test]
    fn the_cost_knob_reads_the_cpu_the_phase_plan_draws() {
        let mut rng = Rng::new(0x0c05_7);
        for work in [1.0, EQUAL_WEIGHT, 4.0] {
            const PLANS: usize = 20_000;
            let drawn: f64 = (0..PLANS)
                .map(|_| {
                    make_phases(&mut rng, false, work)
                        .iter()
                        .filter_map(|p| match p {
                            Phase::Cpu(d) => Some(d.as_secs_f64() * 1000.0),
                            Phase::Io(_) => None,
                        })
                        .sum::<f64>()
                })
                .sum();
            let mean = drawn / PLANS as f64;
            let claimed = cpu_cost_ms(work);
            assert!(
                (mean - claimed).abs() < claimed * 0.05,
                "at a weight of {work} the plan draws {mean:.1} ms of CPU, the knob says {claimed:.1}",
            );
            assert!((work_for_cpu_cost(claimed) - work).abs() < 1e-9, "and it converts back");
        }
    }

    #[test]
    fn weight_is_live_and_the_bill_follows_it() {
        let share_of_bill = |e: &mut SimEngine, of: &str, frames: usize| {
            let mut per: HashMap<&'static str, Duration> = HashMap::new();
            for _ in 0..frames {
                e.tick(16.0);
                for (tenant, fare) in e.take_fares() {
                    *per.entry(tenant).or_default() += fare;
                }
            }
            let all: Duration = per.values().sum();
            per.get(of).copied().unwrap_or_default().as_secs_f64() / all.as_secs_f64().max(1e-9)
        };

        // Level weights: what a tenant is billed for is what it sends.
        let mut e = SimEngine::tenanted();
        e.set_speed(1.0);
        let level = share_of_bill(&mut e, TENANTS[1].id, 300);

        // The same tenant, sending at exactly the same rate, with heavier requests.
        e.set_tenant_work(TENANTS[1].id, EQUAL_WEIGHT * 4.0);
        let heavy = share_of_bill(&mut e, TENANTS[1].id, 300);

        assert!(
            heavy > level * 1.5,
            "a heavier tenant is billed more without sending more: {level:.3} -> {heavy:.3}",
        );
    }

    /// The blame strip is the library's bill, not a second calculation of it. Every span the
    /// sampler emits carries the meter's own wind, so the strip's total area is the measured
    /// whole and each occupier's cells sum to at least what it was billed.
    #[test]
    fn sampled_spans_are_the_librarys_own_accounting() {
        let mut e = SimEngine::tenanted();
        e.set_speed(1.0);
        let mut per_id: HashMap<u32, Duration> = HashMap::new();
        let mut fares: Vec<(&'static str, Duration)> = Vec::new();
        let mut minted = false;
        for _ in 0..240 {
            e.tick(16.0);
            let blame = e.obs().blame.clone();
            minted |= !blame.shut.is_empty();
            for sp in &blame.spans {
                *per_id.entry(sp.id).or_default() += sp.accrual;
                assert!(sp.pos < sp.n, "a cell sits inside the lattice it declares");
                assert!(!sp.minting() || sp.n > 0, "minting needs someone to bill");
            }
            fares.extend(e.take_fares());
        }
        assert!(minted, "three tenants on one server make the gate shut");

        let blame = e.obs().blame.clone();
        assert_eq!(
            blame.occupiers.len() as u64,
            blame.inflight,
            "the sampled membership is the one the library divides by"
        );
        assert!(blame.attributed <= blame.accumulated, "billing never exceeds the measured whole");

        let strip: Duration = per_id.values().sum();
        assert!(strip <= blame.accumulated, "the strip's area is inside the whole: {strip:?}");
        assert!(strip >= blame.attributed, "and it covers everything billed: {strip:?}");
        // A request draws its share on every poll of its own future, so what it has drawn is
        // exactly what its cells show it accrued — the ledger the panel totals is the strip.
        let banked: Duration = fares.iter().map(|(_, fare)| *fare).sum();
        assert!(banked <= blame.attributed, "reported fares are drawn blame: {banked:?}");
        for (tenant, _) in &fares {
            assert!(TENANTS.iter().any(|t| t.id == *tenant), "every fare names a tenant: {tenant}");
        }
    }

    /// The opening still holds exactly one request, in flight along the inbound-network leg
    /// (visibly incoming toward TCP SYN), and it is *real* state: the clock advances it off
    /// the leg and the held arrival stream resumes, so play fills the stage.
    #[test]
    fn open_incoming_places_one_request_mid_network() {
        let mut eng = SimEngine::new(120.0, Stage::Queue, None, false, Behavior::Good);
        eng.set_speed(1.0);
        eng.open_incoming();

        {
            let o = eng.obs();
            assert_eq!(o.live.len(), 1, "the opening still holds exactly one request");
            assert_eq!(o.net_in, 1, "and it is the only request, still in the network leg");
            assert!(
                matches!(o.live[0].1, Station::NetworkIn { .. }),
                "the one request is incoming toward SYN, not settled into a queue"
            );
        }

        // Not a painted still: run the clock and the opening request leaves the network leg
        // toward the server, and the stream that was held resumes (more than one request).
        let (mut advanced, mut refilled) = (false, false);
        for _ in 0..150 {
            eng.tick(16.0);
            let o = eng.obs();
            if o.live.iter().any(|(_, s)| !matches!(s, Station::NetworkIn { .. })) {
                advanced = true;
            }
            if o.live.len() > 1 {
                refilled = true;
            }
        }
        assert!(advanced, "the opening request advances past the network leg once the clock runs");
        assert!(refilled, "the held arrival stream resumes so play fills the stage");
    }
}

