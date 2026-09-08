//! The multi-server model — a fleet of servers, a crowd of clients, and one batch between them.
//!
//! Every server here is the same real taipei stack the single-server visualiser runs
//! ([`assemble`]): a real [`QueueLayer`](taipei::queue::QueueLayer) fronting a gated
//! leaf app, shedding on its real 100 ms deadline. They share one [`World`], so there is one
//! virtual clock and one discrete-event pump however many machines the picture draws.
//!
//! What the chapter is after is what **client-side routing** costs when the choice is
//! uninformed. Each client picks one server uniformly at random and stays with it — the
//! sticky, no-feedback policy — then sends all of its requests down that one connection at
//! once. Ten balls into ten bins: a third of the fleet is left idle while some server holds
//! three clients' worth of load, and the queue on that server sheds. The latency table is
//! where that shows up. How lopsided that gets is a ratio, which is why the three numbers that
//! set it travel together as a [`Batch`].
//!
//! These are clients the chapter *owns*, so a refusal is not where a request ends: the client
//! picks again — just as blindly — and asks again, and keeps asking until somebody answers.
//! What a shedding fleet costs is therefore a length rather than an absence. It is the whole
//! reason the percentile sim has anything to draw, and it is why a round trip here is a
//! [`Trip`] of several [`Attempt`]s and not one request one server answered.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use blog_core::SimKey;
use taipei::clock::{sleep_until, Instant};
use taipei::compose::Stage;

use crate::atoms::latency_table::Bar;
use crate::atoms::stage::Paint;
use crate::engine::{
    arrive, assemble, make_phases, serve, Answer, Arrivals, Cores, Gate, Load, NetLegs, Obs,
    Outcome, Rng, Server, ServerCounts, Sim, SimReq, Station, Svc, World, HANDSHAKE_MS, NET_MS,
};

pub const SERVERS: usize = 10;
pub const REQS_PER_CLIENT: usize = 10;

/// The client a reading belongs to where the sim draws no crowd — the counterpart of
/// [`SOLO`](crate::engine::SOLO), and for the same reason: a field that is always answered is
/// one nothing has to check for.
pub const SOLO_CLIENT: usize = 0;

/// A sim's seed: a fixed base xored with the fence ordinal, so two fences of the same
/// sim differ but each is reproducible. The one place that convention lives.
pub fn sim_seed(key: &SimKey) -> u64 {
    0x5eed ^ key.ordinal as u64
}

/// One server as the fleet view draws it: how many clients (and so requests) routing
/// sent its way, and its live counts. `load` is the ball-count — the balls-in-bins
/// imbalance the chapter is about — so a `0` here is a server left idle.
#[derive(Clone, Copy)]
pub struct FleetServer {
    pub load: usize,
    pub counts: ServerCounts,
}

// ----- the latency breakdown --------------------------------------------------

/// One row of the timing table: where a request's wall-clock time went.
///
/// The sections are exactly the [`Station`]s a request passes through, so the breakdown is
/// read off the engine's own itinerary rather than measured separately — see
/// [`crate::engine::Hop`]. The handshake is the one exception: it is the client's own
/// knowledge, because no server state covers a connection the server has not accepted yet.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Section {
    /// TCP + TLS, once per connection — the cost of talking to a new machine, paid before
    /// the server has any state to stamp.
    Handshake,
    /// The request's travel out to the server.
    NetworkIn,
    /// The kernel's accept queue: handshake done, waiting for a worker to `accept()`.
    SynBacklog,
    /// The accept burst on a core.
    Accept,
    /// Waiting for admission. For one request that is its wait in the real taipei queue, capped
    /// by the queue's 100 ms deadline; for a whole round trip — where a refusal is not the end
    /// and the client asks again — it is the span from the first queue the client joined to the
    /// one it was finally let in from — a retry being a queue the client keeps on its own side.
    AppQueue,
    /// Waiting for a worker thread (the tokio run queue).
    RunQueue,
    /// On a core, computing.
    Cpu,
    /// Sleeping on a dependency.
    Io,
    /// The response's travel home — paid by every verdict someone is still waiting for, a
    /// rejection included.
    NetworkOut,
}

/// The two-way cut: what a request spent **waiting** and what it spent being **worked on**.
/// The same vocabulary the blame strip labels a row with ("N ms queue + N ms processing"),
/// and the coarse reading of the same stations [`Summary::phases`] cuts finer.
pub const QUEUE_TIME: &[Section] = &[
    Section::NetworkIn,
    Section::SynBacklog,
    Section::AppQueue,
    Section::RunQueue,
    Section::NetworkOut,
];
pub const PROCESSING_TIME: &[Section] = &[Section::Accept, Section::Cpu, Section::Io];

/// One phase of a round trip: what it is called, which stations it covers, and the colour it
/// wears. The colour is a field rather than a palette handed out by position — a phase nothing
/// spent time in is dropped from the rows, and by position the phase after it would inherit
/// the missing one's hue.
pub struct Phase {
    pub label: &'static str,
    pub sections: &'static [Section],
    pub ink: Paint,
}

impl Phase {
    /// Whether this is the waiting — the phase a routing policy decides, and so the only row
    /// two policies read against each other would mean anything on.
    pub fn is_queue_wait(&self) -> bool {
        self.sections == QUEUE_TIME
    }
}

/// The finer cut, in the order a request meets it. One table: [`Summary::phases`] reads it
/// across a whole batch and [`phase_ms`] reads it for a single request, and two copies of the
/// mapping would let those two disagree about what "CPU" means.
pub const PHASES: [Phase; 4] = [
    Phase {
        label: "handshake",
        sections: &[Section::Handshake],
        ink: crate::atoms::stage::Stage::teal.value(),
    },
    Phase {
        label: "queue wait",
        sections: QUEUE_TIME,
        ink: crate::atoms::stage::Stage::orange.value(),
    },
    Phase {
        label: "CPU",
        sections: &[Section::Accept, Section::Cpu],
        ink: crate::atoms::stage::Stage::blue.value(),
    },
    Phase { label: "IO", sections: &[Section::Io], ink: crate::atoms::stage::Stage::purple.value() },
];

/// What one request spent in each phase, paired with the phase itself. The waterfall reads a
/// single client's experience where the table reads the batch's spread; both are this cut.
pub fn phase_ms(record: &Record) -> Vec<(&'static Phase, f64)> {
    PHASES
        .iter()
        .map(|p| {
            let ms = p.sections.iter().filter_map(|s| record.sections.get(s)).sum();
            (p, ms)
        })
        .collect()
}

/// A request's stamped stations, folded into where its time went: a station's residence is
/// the gap to the next stamp, and the last one runs to `receipt_t` — the instant the client
/// had its answer, which is where the last station (the leg home) ends. The handshake is the
/// caller's to add; it is the only one that knows it.
pub(crate) fn sections_of(hops: &[(Station, f64)], receipt_t: f64) -> HashMap<Section, f64> {
    let mut sections: HashMap<Section, f64> = HashMap::new();
    for (i, &(station, t)) in hops.iter().enumerate() {
        let until = hops.get(i + 1).map_or(receipt_t, |&(_, next)| next);
        *sections.entry(Section::of(station)).or_default() += (until - t).max(0.0);
    }
    sections
}

impl Section {
    /// The section a station's residence counts toward. Total both ways: every station a
    /// request can occupy is somewhere the time went, and every section but
    /// [`Section::Handshake`] is some station's — including the leg home, which is a station
    /// like any other.
    pub(crate) fn of(station: Station) -> Section {
        match station {
            Station::NetworkIn { .. } => Section::NetworkIn,
            Station::NetworkOut { .. } => Section::NetworkOut,
            Station::SynBacklog { .. } => Section::SynBacklog,
            Station::Accept { .. } => Section::Accept,
            Station::AppQueue { .. } => Section::AppQueue,
            Station::RunQueue { .. } => Section::RunQueue,
            Station::Cpu { .. } => Section::Cpu,
            Station::Io { .. } => Section::Io,
        }
    }
}

/// One finished request as a latency sample: how it ended, how long it took, and the
/// partition of that time across the stations it passed through. (Routing — who sent it,
/// where — is read from [`MultiEngine::assignment`], not carried per record.)
#[derive(Clone)]
pub struct Record {
    /// Who it was billed to — [`SOLO`](crate::engine::SOLO) wherever tenants are not modelled,
    /// which is every sim but the blame panel.
    pub tenant: &'static str,
    /// Who waited for it — [`SOLO_CLIENT`] wherever clients are not modelled, which is every
    /// sim driven by an arrival stream rather than by a crowd. Routing is this chapter's
    /// subject, so a reading that could not be traced back to the client that waited for it
    /// could not be drawn beside that client.
    pub client: usize,
    /// How the client picked the machine it sent to — `None` where nothing chose, which is
    /// every sim but the policy card: a spike draws once and lives with it, and a crowd of one
    /// against one machine has nothing to choose between.
    ///
    /// Stamped at the send, not inferred from when the answer landed. A policy changed while
    /// requests are in the air is still owed answers under the old one, and a reading credited
    /// to the policy that happened to be showing when it came home would credit the new one
    /// with the queue the old one had already put it in.
    pub policy: Option<Policy>,
    pub outcome: Outcome,
    pub total_ms: f64,
    pub sections: HashMap<Section, f64>,
    /// How many times the fleet turned this request away before it landed. A client that owns
    /// its own retry policy keeps asking, so a refusal is a *cost* inside a round trip rather
    /// than the way one ends — which is why the count rides here and not in
    /// [`outcome`](Record::outcome).
    pub refusals: usize,
    /// Virtual ms at which the client had its answer — when this reading *happened*.
    ///
    /// It is what lets a live panel weight recent requests over old ones. Position in the
    /// sample will not do: a fleet pools one deque per server, so the concatenation is two
    /// runs of history rather than one, and the oldest record of the second server would
    /// count as the newest of the batch.
    pub at: f64,
}

/// How many finished requests a [`Latencies`] keeps. The boxplot is a reading of the recent
/// past, not of everything since the machine started — a sample that never forgets would stop
/// moving when the reader turns a knob.
///
/// Long enough that a percentile over it is worth drawing. What keeps the table current is not
/// a short sample but [`HALF_LIFE_MS`] — a live reading weights recent requests over old ones,
/// so length buys resolution instead of costing reaction.
pub(crate) const SAMPLE: usize = 200;

/// Keep one more reading, forgetting the oldest where the window is `bound`ed. A window with no
/// bound is one something else has already bounded — a spike batch stops asking, so its whole
/// history is one event and every part of it is wanted.
pub(crate) fn remember<T>(done: &mut VecDeque<T>, bound: Option<usize>, reading: T) {
    if bound.is_some_and(|bound| done.len() >= bound) {
        done.pop_front();
    }
    done.push_back(reading);
}

/// How long a reading takes to lose half its say in a live panel, in virtual ms.
///
/// These panels exist to be *driven*: the reader moves a cost or a budget and looks straight at
/// the bars. A second is about how long they will wait before deciding nothing happened, so a
/// request from two seconds ago counts a quarter of a fresh one and the older end of the sample
/// fills in the shape of the distribution without deciding it.
pub const HALF_LIFE_MS: f64 = 1000.0;

/// Virtual ms between boxplot recomputes. Percentiles over the whole sample are far too dear
/// for every frame, and the table's own bars ease over 0.4 s — a faster feed would be motion
/// the reader cannot follow anyway.
pub const SUMMARY_MS: f64 = 250.0;

/// The boxplot's feed: every live request's stamped stations, folded into a [`Record`] the
/// moment its verdict lands.
///
/// One of these reads one engine. Ids are an engine's own counter, so a fleet keeps one per
/// server and pools the [`records`](Latencies::records) — sharing a single ledger between two
/// engines would have their requests overwrite each other by id.
#[derive(Default)]
pub struct Latencies {
    hops: HashMap<u32, Vec<(Station, f64)>>,
    done: VecDeque<Record>,
}

impl Latencies {
    /// Fold one frame of an engine's snapshot in: new stamps, and a [`Record`] for every
    /// request whose verdict landed. `tenant` names an id's tenant — the engine's to answer,
    /// and asked before the snapshot is borrowed.
    pub fn absorb(&mut self, obs: &Obs, tenant: &dyn Fn(u32) -> &'static str) {
        for hop in &obs.hops {
            self.hops.entry(hop.id).or_default().push((hop.station, hop.t));
        }
        for &(id, round_trip) in &obs.latencies {
            let Some(hops) = self.hops.remove(&id) else { continue };
            let Some(&(_, send_t)) = hops.first() else { continue };
            let Some(&(_, outcome)) = obs.departures.iter().find(|(d, _)| *d == id) else { continue };
            // The receipt: the send, plus the round trip it measures — where the last station
            // (the leg home) ends.
            let sections = sections_of(&hops, send_t + round_trip);
            remember(&mut self.done, Some(SAMPLE), Record {
                tenant: tenant(id),
                client: SOLO_CLIENT,
                policy: None,
                outcome,
                total_ms: round_trip,
                sections,
                refusals: outcome.refused() as usize,
                at: send_t + round_trip,
            });
        }
        // A request the engine dropped without a verdict leaves its stamps behind; the live set
        // is the only thing that says which those are.
        let live = obs.live_ids();
        self.hops.retain(|id, _| live.contains(id));
    }

    pub fn records(&self) -> &VecDeque<Record> {
        &self.done
    }
}

/// What the client task knows about one attempt that no server state does: the connection it
/// had to open for it, when it sent, and the answer that came back.
struct Attempt {
    /// The round trip this attempt is part of, named by its first attempt. A refusal does not
    /// end a request, so the ledger folds attempts into trips rather than reporting them.
    trip: u32,
    client: usize,
    /// How the client picked this machine, captured where it picked it.
    policy: Option<Policy>,
    server: usize,
    /// The balancer this attempt went through, when it went through one — whose wire its
    /// server legs ride.
    via: Option<usize>,
    handshake_ms: f64,
    send_t: f64,
    verdict: Option<(Outcome, f64)>,
}

/// One call inside a round trip: the machine the client asked, and the answer it got.
#[derive(Clone, Copy)]
pub struct Call {
    pub server: usize,
    pub outcome: Outcome,
}

/// A finished round trip as a route rather than as a duration. A [`Record`] says what a request
/// cost; this says where it went, which is the only thing a wire can draw. Every call but the
/// last was refused, so the chain *is* the retries the client made.
pub struct Journey {
    /// The trip's id — its first attempt's, unique for the engine's whole life.
    pub trip: u32,
    pub client: usize,
    pub calls: Vec<Call>,
}

/// One leg's departure: what left which wire, which way, and — riding home — the outcome it
/// carries. Emitted at the instant the leg's own sleep begins, drained once per frame; the
/// picture launches a pulse per departure and the browser owns the travel from there.
pub struct WireEvent {
    pub leg: Leg,
    pub homeward: bool,
    /// The reply's outcome on a homeward leg, known the instant the answer departs. `None`
    /// outbound — a request in flight has no verdict yet.
    pub outcome: Option<Outcome>,
    /// The instant it left, on the engine's clock — read by the contract test that pins a
    /// departure inside the frame it is drained on.
    pub at: f64,
}

/// The wire a leg is on. A server leg's sender is the client on a direct card and the
/// balancer ([`Attempt::via`]) behind an edge; edge legs run client to balancer.
pub enum Leg {
    Server { sender: usize, server: usize },
    Edge { client: usize, lb: usize },
}

/// A client's request as it accumulates: what it has paid so far, when it first joined a queue,
/// and the machines it has asked in order.
struct Trip {
    client: usize,
    policy: Option<Policy>,
    /// When the client first asked — before the handshake its first attempt opened with.
    began: f64,
    /// The instant it first joined a queue at a server: the start of the span its waiting is
    /// measured over, and — while it is still `None` — the sign that the attempt being folded
    /// in is the first, whose handshake and leg out are the only ones outside that span.
    queued: Option<f64>,
    sections: HashMap<Section, f64>,
    calls: Vec<Call>,
}

/// What a fleet has answered since it started: every round trip that closed, and every refusal
/// it took to close them.
///
/// Counted as they land rather than read off [`Ledger::done`], which is a window on the recent
/// past and forgets — a tally that stopped climbing when the window filled would report a fleet
/// as having gone quiet.
#[derive(Clone, Copy, Default)]
pub struct Answered {
    pub trips: usize,
    pub refusals: usize,
}

/// The instant an attempt first joined a queue at the server: the kernel's accept queue, which
/// is where a completed connection lands the moment its inbound leg does. The wire ahead of it
/// is travel rather than waiting, and every queue behind it is behind this one.
fn joined_a_queue(hops: &[(Station, f64)]) -> Option<f64> {
    hops.iter().find_map(|&(station, t)| matches!(station, Station::SynBacklog { .. }).then_some(t))
}

/// Admission, and the work it admitted the request to: the stamp after the last time the attempt
/// waited in the taipei queue. Leaving that queue is what being let in *is*, so everything from
/// here on is a request being served and everything before it is a request trying to be.
fn admitted(hops: &[(Station, f64)]) -> Option<(f64, &[(Station, f64)])> {
    let waited = hops.iter().rposition(|(station, _)| matches!(station, Station::AppQueue { .. }))?;
    let worked = hops.get(waited + 1..)?;
    let &(_, at) = worked.first()?;
    Some((at, worked))
}

/// The batch's latency ledger. Client tasks write each attempt's facts as they make it; the
/// engine folds in each server's stamped hops after every pump and absorbs the attempts whose
/// verdict has landed.
#[derive(Default)]
struct Ledger {
    attempts: HashMap<u32, Attempt>,
    trips: HashMap<u32, Trip>,
    hops: HashMap<u32, Vec<(Station, f64)>>,
    /// The finished requests the panels read, oldest first.
    done: VecDeque<Record>,
    /// How many of them to hold on to. A spike is bounded by its own batch — it asks for
    /// [`requests`](Batch::requests) answers and then stops — so it keeps every one and the
    /// ranking the percentile card draws is the whole event. Demand that does not stop has no
    /// such bound: at six hundred a second, a fleet left running would grow ten records a frame
    /// for as long as the reader watches it, so it keeps the last [`SAMPLE`] — the same window,
    /// and for the same reason, that [`Latencies`] keeps.
    keep: Option<usize>,
    answered: Answered,
    /// Closed trips as routes, held for the frame they closed on — see [`MultiEngine::journeys`].
    /// They ride here rather than on a [`Record`] because a route is only ever wanted once, on
    /// that frame, and the records outlive it by the whole settle.
    journeys: Vec<Journey>,
    /// Each client's open trips right now: up where a trip's first attempt goes out, down where
    /// its answer lands. The orange a client circle wears is exactly this being nonzero.
    outstanding: Vec<usize>,
    /// The legs that departed since the frame began. Server legs are derived from the hop
    /// stamps as they fold in; edge legs are pushed by the client tasks at their own sleeps.
    /// Held per frame like [`journeys`](Ledger::journeys) — a departure is wanted on the
    /// frame it happened or not at all.
    wire_events: Vec<WireEvent>,
}

/// One more open trip against `client`, growing the tally to fit — a ledger does not know the
/// crowd's size until the crowd asks.
fn charge(outstanding: &mut Vec<usize>, client: usize) {
    if outstanding.len() <= client {
        outstanding.resize(client + 1, 0);
    }
    outstanding[client] += 1;
}

impl Ledger {
    /// Fold one answered attempt into its trip.
    ///
    /// A refusal leaves the trip open, because the client is already asking somebody else — and
    /// everything it pays between joining its first queue and being let in is one span of
    /// waiting: the queue it was turned away from, the "no" travelling home, the ask travelling
    /// out again, the connection it had to open to ask somewhere new, and the wait it finally
    /// got in from. A retry is a queue the client keeps on its own side, so the span is what
    /// [`Section::AppQueue`] holds for a round trip. What sits outside it is the first
    /// handshake, the first leg out, and — from admission on — the work itself.
    ///
    /// Any other verdict is the answer, and closes the trip into a [`Record`] of what it cost
    /// and a [`Journey`] of where it went.
    ///
    /// Attempts must arrive in the order they were made — see [`MultiEngine::tick`].
    fn absorb(&mut self, id: u32, outcome: Outcome, round_trip: f64) {
        let Some(attempt) = self.attempts.remove(&id) else { return };
        let hops = self.hops.remove(&id).unwrap_or_default();
        // The receipt: the send, plus the round trip it measures. `serve` measures from the
        // first byte, so the connection this attempt opened first is the client's to add.
        let receipt = attempt.send_t + round_trip;
        let mut trip = self.trips.remove(&attempt.trip).unwrap_or_else(|| Trip {
            client: attempt.client,
            policy: attempt.policy,
            began: attempt.send_t - attempt.handshake_ms,
            queued: None,
            sections: HashMap::new(),
            calls: Vec::new(),
        });
        let queued = match trip.queued {
            Some(queued) => queued,
            None => {
                let joined = joined_a_queue(&hops).unwrap_or(attempt.send_t);
                *trip.sections.entry(Section::Handshake).or_default() += attempt.handshake_ms;
                *trip.sections.entry(Section::NetworkIn).or_default() += joined - attempt.send_t;
                *trip.queued.insert(joined)
            }
        };
        trip.calls.push(Call { server: attempt.server, outcome });
        match outcome.refused() {
            true => {
                self.trips.insert(attempt.trip, trip);
            }
            false => {
                let (admitted, worked) = admitted(&hops).unwrap_or((receipt, &[]));
                *trip.sections.entry(Section::AppQueue).or_default() += admitted - queued;
                for (section, ms) in sections_of(worked, receipt) {
                    *trip.sections.entry(section).or_default() += ms;
                }
                let Trip { client, policy, began, sections, calls, .. } = trip;
                if let Some(open) = self.outstanding.get_mut(client) {
                    *open = open.saturating_sub(1);
                }
                let refusals = calls.len() - 1;
                self.answered.trips += 1;
                self.answered.refusals += refusals;
                remember(&mut self.done, self.keep, Record {
                    tenant: crate::engine::SOLO,
                    client,
                    policy,
                    outcome,
                    total_ms: receipt - began,
                    sections,
                    refusals,
                    at: receipt,
                });
                self.journeys.push(Journey { trip: attempt.trip, client, calls });
            }
        }
    }
}

// ----- percentiles ------------------------------------------------------------

/// A section's distribution across the batch — the row's bar and its box plot.
#[derive(Clone, Copy)]
pub struct Spread {
    pub mean_ms: f64,
    pub p25_ms: f64,
    pub p50_ms: f64,
    pub p95_ms: f64,
}

/// One reading and how much of the answer it is owed. A settled batch weighs every reading the
/// same; a running machine weighs a recent one more, so the two travel together rather than as
/// two lists that have to stay the same length.
struct Sample {
    ms: f64,
    weight: f64,
}

/// The weighted nearest-rank percentile of an already-sorted sample: the first reading at which
/// the weight behind it reaches `q` of the whole. With equal weights this is the ordinary
/// nearest-rank percentile, so an unweighted batch reads exactly as it always did.
///
/// Empty ⇒ zero, which is honest: no request spent time in a section nothing reached.
fn percentile(sorted: &[Sample], q: f64) -> f64 {
    let total: f64 = sorted.iter().map(|s| s.weight).sum();
    if sorted.is_empty() || total <= 0.0 {
        return 0.0;
    }
    let want = q * total;
    let mut seen = 0.0;
    for sample in sorted {
        seen += sample.weight;
        // A hair of slack: `seen` is a running sum and `want` a product of the same weights, so
        // the last rank can miss by an ulp and fall through to the final reading anyway.
        if seen >= want - f64::EPSILON * total {
            return sample.ms;
        }
    }
    sorted[sorted.len() - 1].ms
}

fn spread(mut xs: Vec<Sample>) -> Spread {
    xs.sort_by(|a, b| a.ms.total_cmp(&b.ms));
    let total: f64 = xs.iter().map(|s| s.weight).sum();
    Spread {
        mean_ms: match total > 0.0 {
            true => xs.iter().map(|s| s.ms * s.weight).sum::<f64>() / total,
            false => 0.0,
        },
        p25_ms: percentile(&xs, 0.25),
        p50_ms: percentile(&xs, 0.50),
        p95_ms: percentile(&xs, 0.95),
    }
}

/// The axis a set of bars is drawn against: the widest reading among them, with headroom.
///
/// Derived from the bars themselves rather than from the total, because a row is not always a
/// part of the total — one tenant's p95 can exceed the population's, and scaling it against the
/// total would draw it past the end of its own track.
pub(crate) fn axis_for(bars: &[Bar]) -> f64 {
    (bars.iter().fold(0.0f64, |wide, b| wide.max(b.p95)) * 1.05).max(1.0)
}

/// The boxplot's rows, and which cut of the batch they are.
///
/// The two are not interchangeable, and the difference decides whether a row's mean is a
/// *share* of the total or merely comparable to it — so the rows carry it rather than the
/// call site remembering to.
pub enum Rows {
    /// Sections of one request — `(label, spread, the phase's own colour)`. They partition it,
    /// so each row's mean is a share of the total and the shares sum to 100%. The colour rides
    /// the row for the same reason a tenant's does: a phase with nothing in it is dropped, and
    /// a palette applied by position would slide onto the phase after it.
    Phases(Vec<(String, Spread, Paint)>),
    /// One distribution per tenant — `(label, spread, the tenant's own colour)` — each over
    /// the same population rather than a piece of it. A row can exceed the total (a tenant
    /// slower than average) and the rows do not partition anything, so there is no share to
    /// show. The colour rides the row because a row here *is* a subject: it wears what that
    /// tenant wears in the swarm, on its wire, and in the strip.
    Tenants(Vec<(String, Spread, Paint)>),
}

impl Rows {
    /// Whether a row's mean is a fraction of the total, which is what makes a percentage
    /// meaningful. [`Rows::Tenants`] rows are distributions over the same whole, not parts
    /// of it, so the question does not apply to them.
    fn shares_of_the_total(&self) -> bool {
        matches!(self, Rows::Phases(_))
    }
}

/// The finished batch, folded into what the table draws.
pub struct Summary {
    pub records: Vec<Record>,
    pub total: Spread,
    /// How fast a reading loses its say, in virtual ms — `None` for a settled batch, where
    /// every request is part of the one event and none of them is more current than another.
    half_life_ms: Option<f64>,
}

impl Summary {
    /// Requests the fleet turned away — queue-timed-out or rejected outright. The one
    /// definition of "shed", so every view reads the same number.
    ///
    /// Counted per request, not per refusal: a client that is refused keeps asking, and a
    /// request refused four times is one client being turned away, not four. That also keeps
    /// this reading the same shape it had before clients retried, which is what a drop-rate
    /// curve is asking about — how likely you are to hear no, not how many times.
    pub fn shed(&self) -> usize {
        self.records.iter().filter(|r| r.refusals > 0).count()
    }

    /// The four phases the boxplot draws, each summing several stations so the bars add
    /// up to the total.
    ///
    /// A phase nothing spent time in is not a row: a sim that models no handshake would
    /// otherwise draw an empty one, and an axis with a zero bar on it reads as a fault.
    pub fn phases(&self) -> Rows {
        Rows::Phases(self.phase_groups())
    }

    fn phase_groups(&self) -> Vec<(String, Spread, Paint)> {
        PHASES
            .iter()
            .map(|p| (p.label.to_string(), self.spread_over(p.sections, |_| true), p.ink))
            .filter(|(_, spread, _)| spread.mean_ms > 0.0)
            .collect()
    }

    /// The same round trip cut the other way: one group per tenant, over the part of it
    /// `sections` covers, in the order [`TENANTS`](crate::engine::TENANTS) names them. Cut by
    /// *who*, a reading is only worth drawing next to the thing it is a part of — which is why
    /// the caller says which part.
    ///
    /// Over the requests that were [`served`](Outcome::served), and only those. A request
    /// turned away at the door did no work and waited in no queue, so its timings are the
    /// accept burst and nothing else; averaged in, a tenant being refused reads as one whose
    /// requests are cheap instead of one that is not getting in. What happened to the rest is
    /// the drop percentage's to say, not the boxplot's.
    ///
    /// **Every** tenant is a row, including one nothing has come back for yet. The cast is
    /// fixed and the rows are read against each other, so a row that comes and goes as its
    /// tenant's requests finish would shift the rows under it — and the tenant whose work is
    /// slowest to come back is the one a reader is looking for. An empty row reads as "nothing
    /// yet", which is the truth; a missing one reads as "not here", which is not. (A *phase*
    /// nothing spent time in is dropped, and for the opposite reason: it is not a part of the
    /// round trip at all.)
    pub fn by_tenant(&self, sections: &[Section]) -> Rows {
        Rows::Tenants(
            crate::engine::TENANTS
                .iter()
                .map(|t| {
                    let of = |r: &Record| r.tenant == t.id && r.outcome.served();
                    (t.id.to_string(), self.spread_over(sections, of), t.tint)
                })
                .collect(),
        )
    }

    /// The distribution of time spent in `sections`, across the records `of` picks. Summed per
    /// request *then* spread — a percentile of a sum is not the sum of per-section percentiles.
    ///
    /// Weighted by age where this is a running machine's summary: a reading halves its say
    /// every [`half_life_ms`](Summary::half_life_ms) of virtual time it is older than the
    /// newest one here. The sample can then be long enough for a percentile to mean something
    /// while the table still turns when the reader moves a knob.
    fn spread_over(&self, sections: &[Section], of: impl Fn(&Record) -> bool) -> Spread {
        let taken: Vec<&Record> = self.records.iter().filter(|r| of(r)).collect();
        let newest = taken.iter().fold(f64::MIN, |t, r| t.max(r.at));
        spread(
            taken
                .iter()
                .map(|r| Sample {
                    ms: sections.iter().filter_map(|s| r.sections.get(s)).sum(),
                    weight: match self.half_life_ms {
                        Some(half_life) if half_life > 0.0 => {
                            0.5f64.powf((newest - r.at) / half_life)
                        }
                        _ => 1.0,
                    },
                })
                .collect(),
        )
    }

    /// The boxplot's rows: one bar per group, and — for a cut whose groups are *parts* of the
    /// round trip — the total underneath them. Rows that are separate distributions over the
    /// same population have no total to sit under: the population's own spread is not another
    /// one of them.
    pub fn bars(&self, rows: Rows) -> Vec<Bar> {
        let mean_total = self.total.mean_ms.max(1.0);
        let shares = rows.shares_of_the_total();
        let row = |label: String, sp: Spread, hue: Paint| Bar {
            label,
            p25: sp.p25_ms,
            p50: sp.p50_ms,
            p95: sp.p95_ms,
            pct: shares.then(|| (sp.mean_ms / mean_total * 100.0).round() as u32),
            hue,
            total: false,
        };
        // Every row arrives carrying the colour it wears elsewhere — a phase the one [`PHASES`]
        // names, a tenant the one it wears in the swarm and on its wire.
        let mut bars: Vec<Bar> = match rows {
            Rows::Phases(groups) | Rows::Tenants(groups) => {
                groups.into_iter().map(|(label, sp, hue)| row(label, sp, hue)).collect()
            }
        };
        if shares {
            let t = self.total;
            bars.push(Bar {
                label: "total".into(),
                p25: t.p25_ms,
                p50: t.p50_ms,
                p95: t.p95_ms,
                pct: Some(100),
                hue: crate::styles::Palette::control_ink.value(),
                total: true,
            });
        }
        bars
    }

    /// A settled batch: one event, every request equally part of it.
    pub(crate) fn of(records: Vec<Record>) -> Summary {
        Summary::fold(records, None)
    }

    /// A running machine, read now. The sample can be long — long enough for a percentile to
    /// be worth drawing — because a reading halves its say every `half_life_ms` of virtual time
    /// it is older than the newest. The table then follows the machine instead of averaging it
    /// against a history the reader has already changed.
    pub(crate) fn recent(records: Vec<Record>, half_life_ms: f64) -> Summary {
        Summary::fold(records, Some(half_life_ms))
    }

    fn fold(records: Vec<Record>, half_life_ms: Option<f64>) -> Summary {
        let newest = records.iter().fold(f64::MIN, |t, r| t.max(r.at));
        let weight = |at: f64| match half_life_ms {
            Some(half_life) if half_life > 0.0 => 0.5f64.powf((newest - at) / half_life),
            _ => 1.0,
        };
        Summary {
            total: spread(
                records.iter().map(|r| Sample { ms: r.total_ms, weight: weight(r.at) }).collect(),
            ),
            half_life_ms,
            records,
        }
    }
}

// ----- the engine -------------------------------------------------------------

/// A line to one server: everything a client task needs to send it a request, held free of the
/// engine so a refused request can pick a different machine without asking anybody.
#[derive(Clone)]
struct Line {
    state: Arc<Mutex<Sim>>,
    cores: Cores,
    svc: Svc,
}

/// When each of a client's connections becomes usable, by server. A handshake is paid once per
/// machine and everything sent before it completes waits for that same one — which is the whole
/// reason a client puts all ten of its requests down one connection.
type Connections = Arc<Mutex<Vec<Option<f64>>>>;

// ----- how a client picks ------------------------------------------------------

/// What a client last heard a machine carrying, and when the machine said it. The load drifts
/// optimistic between hearings — the client adds one for each of its own sends — but the stamp
/// moves only when the machine itself speaks, because freshness is about the truth, not about
/// the client's arithmetic on top of it.
#[derive(Clone, Copy)]
struct Heard {
    load: Load,
    at: f64,
}

/// What the crowd last heard about a machine, as anything steering by it reads the reading:
/// the load the machine reported, and its age now. Age is the whole of its trustworthiness —
/// see [`BLIND_AT_MS`], where trust runs out entirely.
#[derive(Clone, Copy)]
pub struct Believed {
    pub load: Load,
    pub age_ms: f64,
}

/// How old a heard counter must be before it says nothing at all. Trust decays on the square
/// root of age — a tenth gone at 100 ms, half at a quarter of this, all of it here — because
/// an old counter is not worthless: the client's own sends are counted onto it exactly, and
/// only the heard base underneath has drifted. Acting on it *most* of the time beats going
/// blind, while the residual randomness keeps everyone from herding onto the same stale
/// bargain. Measured against linear decays at every crowd size, this is the curve that never
/// reads worse than random.
pub const BLIND_AT_MS: f64 = 10_000.0;

/// One client's routing state. Shared with every request it has in the air, because a refusal
/// picks again from the same place the next send will read — and every answer that comes home
/// deposits what it heard here, whatever policy sent it.
struct Routing {
    home: usize,
    /// What this client last heard from each machine — `None` until it has.
    heard: Vec<Option<Heard>>,
    /// The machines this client picks between: the whole fleet everywhere but the pool card,
    /// which shrinks it so the counters it does hold stay fresh.
    pool: Vec<usize>,
}

/// How a client chooses a machine — the three the chapter compares.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Policy {
    /// A fresh uniform draw for every request, and no memory at all.
    AlwaysRandom,
    /// Stay where you are until a machine turns you away, then draw somewhere else. The only
    /// feedback is the refusal itself.
    RepickOnQueueTimeout,
    /// Two machines drawn per request, the less loaded of the two taken.
    ///
    /// Idealised: the reading is everything [`committed`](Sim::committed) to the machine *now*,
    /// read for free. No client can really have that — a machine cannot see its own SYN backlog
    /// to report it, and a counter that could be reported has to travel — but what the choice is
    /// worth and what knowing costs are two questions, and this is the first of them answered on
    /// its own.
    PowerOfTwoVirtual,
    /// The same two draws, paid for: the counters are what the machines *said* — each answer
    /// carries the load its machine [`reported`](Sim::reported) at the verdict — read from the
    /// client's own [`Heard`] table, aged by [`BLIND_AT_MS`], drawn from its pool.
    PowerOfTwo,
}

impl Policy {
    /// Which machine the client sends to now.
    fn send(self, routing: &mut Routing, now: f64, fleet: &[Line], rng: &mut Rng) -> usize {
        match self {
            Policy::AlwaysRandom => rng.below(fleet.len()),
            Policy::RepickOnQueueTimeout => routing.home,
            Policy::PowerOfTwoVirtual => two_choices(fleet, rng),
            Policy::PowerOfTwo => two_heard(routing, now, rng),
        }
    }

    /// Which machine it asks after being turned away. Every policy picks again — a client whose
    /// answer is still owed cannot stay where it was told no — but only the informed ones have
    /// learned anything from being refused.
    fn refused(self, routing: &mut Routing, now: f64, fleet: &[Line], rng: &mut Rng) -> usize {
        match self {
            Policy::PowerOfTwoVirtual => two_choices(fleet, rng),
            Policy::PowerOfTwo => two_heard(routing, now, rng),
            _ => rng.below(fleet.len()),
        }
    }
}

/// Draw two machines and take the one carrying less.
fn two_choices(fleet: &[Line], rng: &mut Rng) -> usize {
    let n = fleet.len();
    let a = rng.below(n);
    // Two machines, not one machine twice: a collision would compare a reading with itself and
    // report it as the better of two.
    let b = match n > 1 {
        true => (a + 1 + rng.below(n - 1)) % n,
        false => a,
    };
    let load = |i: usize| fleet[i].state.lock().unwrap().committed_load();
    match load(b) < load(a) {
        true => b,
        false => a,
    }
}

/// The same two draws over what the client actually knows: two machines from its pool, the one
/// whose heard counter reads lower — when the counters are trusted. Trust runs out on
/// `√(staleness / BLIND_AT_MS)` (the staler of the two, a counter never heard being wholly
/// stale): with that probability the pick is random between them instead. The pick is then
/// counted before it lands — the client knows its own send raises that machine's load by one,
/// so it says so to itself rather than waiting to be told.
fn two_heard(routing: &mut Routing, now: f64, rng: &mut Rng) -> usize {
    let n = routing.pool.len();
    let ai = rng.below(n);
    // Two machines, not one machine twice — the same offset draw [`two_choices`] makes.
    let bi = match n > 1 {
        true => (ai + 1 + rng.below(n - 1)) % n,
        false => ai,
    };
    let (a, b) = (routing.pool[ai], routing.pool[bi]);
    let age = |server: usize| match routing.heard[server] {
        Some(heard) => (((now - heard.at).max(0.0) / BLIND_AT_MS).sqrt()).clamp(0.0, 1.0),
        None => 1.0,
    };
    let blind = rng.f64() < age(a).max(age(b));
    let pick = match (routing.heard[a], routing.heard[b]) {
        (Some(ha), Some(hb)) if !blind => match hb.load < ha.load {
            true => b,
            false => a,
        },
        _ => match rng.below(2) {
            0 => a,
            _ => b,
        },
    };
    if let Some(heard) = routing.heard[pick].as_mut() {
        // The client knows its own send is one more request on that machine, and knows where
        // the machine will put it: behind a queue that already has someone in it, on a core
        // otherwise. Guessing the other way would have a client believe an idle machine was
        // queueing, which is the one thing this counter exists to tell it.
        match heard.load.queued > 0 {
            true => heard.load.queued += 1,
            false => heard.load.processed += 1,
        }
    }
    pick
}

/// `size` distinct machines, drawn uniformly — one client's pool. A partial shuffle, so a pool
/// the size of the fleet is the fleet.
fn draw_pool(servers: usize, size: usize, rng: &mut Rng) -> Vec<usize> {
    let mut all: Vec<usize> = (0..servers).collect();
    let size = size.clamp(1, servers);
    for i in 0..size {
        let j = i + rng.below(servers - i);
        all.swap(i, j);
    }
    all.truncate(size);
    all
}

/// The shape of a batch: how many clients, how big a fleet they have to choose from, and how many
/// requests each one sends down its connection.
///
/// The three travel together because only their ratio means anything. Requests per server is what
/// decides whether a fleet is at capacity or past it, and past it a client that keeps asking makes
/// work for the fleet that refused it — so doubling the clients without the servers does not
/// double the batch, it squares the asking.
#[derive(Clone, Copy)]
pub struct Batch {
    pub clients: usize,
    pub servers: usize,
    pub reqs_per_client: usize,
}

impl Batch {
    /// `clients` clients against the ten-machine fleet sims 1 and 2 draw, ten requests each.
    pub fn of(clients: usize) -> Batch {
        Batch { clients, servers: SERVERS, reqs_per_client: REQS_PER_CLIENT }
    }

    /// Every request the batch will ask for at least once.
    pub fn requests(&self) -> usize {
        self.clients * self.reqs_per_client
    }
}

pub struct MultiEngine {
    world: World,
    servers: Vec<Server>,
    states: Vec<Arc<Mutex<Sim>>>,
    ledger: Arc<Mutex<Ledger>>,
    /// Attempt ids, minted by the client tasks as they send — one per ask, not one per request,
    /// because a retry visits its own stations on a machine of its own.
    ids: Arc<AtomicU32>,
    rng: Rng,
    batch: Batch,
    /// Which server each client is stuck to, once a batch has been fired.
    assignment: Vec<usize>,
    fired: bool,
    /// The crowd, when the fleet is under continuous load rather than taking one spike.
    flow: Option<Flow>,
    /// The balancer tier, when the crowd reaches the fleet through one.
    edge: Option<EdgeTier>,
}

/// A crowd of clients sending on and on, and what each of them carries between sends: the clock
/// it sends on, the connections it has kept warm, and what it knows about the fleet.
///
/// A spike has none of this. It fires once, so a client has nothing to remember and nothing to
/// remember it for — which is [`MultiEngine::fire`]'s whole argument, and why the two are one
/// fleet with two kinds of demand rather than two fleets.
struct Flow {
    clocks: Vec<Arrivals>,
    routings: Vec<Arc<Mutex<Routing>>>,
    /// Kept across sends, so a client that comes back to a machine it has used pays no
    /// handshake — the constraint that makes re-picking cost something.
    open: Vec<Connections>,
    policy: Policy,
    /// Every draw the policy makes, on a stream of its own.
    ///
    /// The three policies do not draw the same number of times per send — none, one, two — so
    /// routing off the engine's rng would slide the arrival gaps and the phase plans along with
    /// the choice, and two policies at one seed would be answering different work at different
    /// instants. Split, a seed names one stream of demand and the policy is the only thing that
    /// differs between two runs of it.
    picks: Rng,
}

/// The twist that splits the routing draws off the seed the workload is drawn from.
const PICKS: u64 = 0xdec1de;

// ----- the edge ----------------------------------------------------------------

/// Round trip between a client and its load balancer: an edge hop. Cloudflare-class edges
/// put ~95% of users within 50 ms and a typical broadband user within 5–25, so twenty is an
/// ordinary user's distance to the nearest point of presence — farther than the 12 ms
/// region hop behind it, which is the point: the expensive first contact happens on the leg
/// that is one hop long and paid once, not once per machine asked.
pub const EDGE_RTT_MS: f64 = 20.0;
const EDGE_MS: f64 = EDGE_RTT_MS / 2.0;
/// TCP + TLS 1.3 to the balancer — the same two round trips [`HANDSHAKE_MS`] charges, on
/// the edge's distance.
const EDGE_HANDSHAKE_MS: f64 = 2.0 * EDGE_RTT_MS;

/// The most balancers the card offers, and the most clients. Built once at these sizes;
/// the sliders bound who is *in rotation*, because resizing a fleet is routing, not
/// construction — a machine slid away just stops being picked and drains.
pub const MAX_LBS: usize = 20;
pub const MAX_CLIENTS: usize = 400;

/// The load-balancer tier: what stands between a churning crowd and the fleet, and the
/// whole reason the crowd may stay dumb. Each balancer is a [`Routing`] — the same heard
/// table and pool a pool-card client keeps — fed by every answer that passes through it,
/// which is the crowd's entire request rate divided by [`active_lbs`](EdgeTier::active_lbs)
/// rather than by hundreds of clients: the freshness a client could not buy, bought by
/// standing where the traffic is.
struct EdgeTier {
    /// One per balancer, all [`MAX_LBS`] built; `pool` on each is the active servers.
    lbs: Vec<Arc<Mutex<Routing>>>,
    /// How a balancer picks — the card's toggle: blind draws or the paid power-of-two.
    policy: Policy,
    active_lbs: usize,
    active_servers: usize,
    /// Mean client lifetime, virtual ms. Lives are exponential: churn has no schedule.
    lifetime_ms: f64,
    /// The crowd's whole arrival rate — kept so resizing the crowd can re-divide it.
    qps: f64,
    /// How many client slots are alive. Slots above the target stop being reborn.
    target_clients: usize,
    /// Which balancer each client slot lives on — sticky for a life, the way anycast or
    /// DNS pins a session to a point of presence.
    homes: Vec<usize>,
    /// When each slot's current life ends. Zero to begin with, so every slot's first send
    /// is a birth and pays its handshake — a fleet nobody has met yet.
    dies_at: Vec<f64>,
    /// When each slot's connection to its balancer becomes usable — `None` for a life that
    /// has not sent yet. One connection per life: it dies with the client.
    open: Vec<Arc<Mutex<Option<f64>>>>,
    /// Requests currently in flight through each balancer — arrival to final verdict, the
    /// retries inside that span included, because they are the balancer's own doing.
    inflight: Vec<Arc<AtomicUsize>>,
}

/// One balancer as the card draws it: how many living clients call it home, and how much is
/// in flight through it right now.
#[derive(Clone, Copy)]
pub struct BalancerView {
    pub clients: usize,
    pub inflight: usize,
}

impl MultiEngine {
    /// A fleet of real taipei stacks sharing one world, taking the batch `batch` describes.
    pub fn new(seed: u64, batch: Batch) -> MultiEngine {
        let world = World::new();
        let servers: Vec<Server> = (0..batch.servers)
            .map(|_| {
                assemble(
                    &world,
                    Stage::Queue,
                    Some(Gate::RuntimeCpu),
                    true,
                    true,
                    1000.0,
                    false,
                    1.0,
                    crate::engine::default_limit(Stage::Queue),
                )
            })
            .collect();
        let states = servers.iter().map(|s| s.state()).collect();
        MultiEngine {
            world,
            servers,
            states,
            ledger: Arc::new(Mutex::new(Ledger::default())),
            ids: Arc::new(AtomicU32::new(0)),
            rng: Rng::new(seed),
            batch,
            assignment: Vec::new(),
            fired: false,
            flow: None,
            edge: None,
        }
    }

    /// `clients` clients against the default ten-machine fleet — what sims 1 and 2 take.
    pub fn with_clients(seed: u64, clients: usize) -> MultiEngine {
        MultiEngine::new(seed, Batch::of(clients))
    }

    /// The same fleet under demand that does not stop: `clients` clients each sending at
    /// `qps / clients`, so the fleet's load is the one number and the crowd only says how
    /// finely it is divided. Every client starts somewhere uniformly at random — nobody has
    /// heard anything yet, so on the first send there is nothing else any policy could do.
    ///
    /// One client against one server is this with both counts at one: the same clock, the same
    /// ledger, a policy with nothing to choose between.
    pub fn flowing(seed: u64, batch: Batch, qps: f64, policy: Policy) -> MultiEngine {
        let mut engine = MultiEngine::new(seed, batch);
        // Nothing bounds what a fleet under continuous demand will answer, so its ledger keeps
        // a window instead of a history.
        engine.ledger = Arc::new(Mutex::new(Ledger { keep: Some(SAMPLE), ..Ledger::default() }));
        let now = engine.world.now_ms();
        let share = qps / batch.clients as f64;
        let mut picks = Rng::new(seed ^ PICKS);
        let flow = Flow {
            clocks: (0..batch.clients)
                .map(|_| Arrivals::new(share, now, &mut engine.rng))
                .collect(),
            routings: (0..batch.clients)
                .map(|_| {
                    Arc::new(Mutex::new(Routing {
                        home: picks.below(batch.servers),
                        heard: vec![None; batch.servers],
                        pool: (0..batch.servers).collect(),
                    }))
                })
                .collect(),
            open: (0..batch.clients)
                .map(|_| Arc::new(Mutex::new(vec![None; batch.servers])))
                .collect(),
            policy,
            picks,
        };
        engine.flow = Some(flow);
        engine
    }

    /// The fleet behind a balancer tier: `clients` short-lived clients (mean life
    /// `lifetime_ms`), each homed on one of `lbs` balancers for as long as it lives, the
    /// balancers holding the warm lines to the servers and picking per `policy`. Demand is the
    /// same [`flowing`](MultiEngine::flowing) crowd; what changes is who does the choosing.
    ///
    /// Built at [`MAX_CLIENTS`] and [`MAX_LBS`] and bounded by the setters — resizing is a
    /// routing decision here, never a rebuild, so the picture never restarts under the reader.
    pub fn edged(
        seed: u64,
        qps: f64,
        clients: usize,
        lbs: usize,
        lifetime_ms: f64,
        policy: Policy,
    ) -> MultiEngine {
        let batch = Batch { clients: MAX_CLIENTS, servers: SERVERS, reqs_per_client: 1 };
        let mut engine = MultiEngine::flowing(seed, batch, qps, Policy::AlwaysRandom);
        engine.edge = Some(EdgeTier {
            lbs: (0..MAX_LBS)
                .map(|_| {
                    Arc::new(Mutex::new(Routing {
                        home: 0,
                        heard: vec![None; SERVERS],
                        pool: (0..SERVERS).collect(),
                    }))
                })
                .collect(),
            policy,
            active_lbs: lbs.clamp(1, MAX_LBS),
            active_servers: SERVERS,
            lifetime_ms: lifetime_ms.max(1.0),
            qps,
            target_clients: clients.clamp(1, MAX_CLIENTS),
            homes: vec![0; MAX_CLIENTS],
            dies_at: vec![0.0; MAX_CLIENTS],
            open: (0..MAX_CLIENTS).map(|_| Arc::new(Mutex::new(None))).collect(),
            inflight: (0..MAX_LBS).map(|_| Arc::new(AtomicUsize::new(0))).collect(),
        });
        engine.set_clients(clients);
        engine
    }

    /// Change how the crowd picks. It takes effect on the next send each client makes — what is
    /// already in the air was sent under the old policy and is still owed an answer under it,
    /// which is what makes switching a comparison rather than a reset. Behind an edge the
    /// balancers are the ones choosing, so it is their mind that changes.
    pub fn set_policy(&mut self, policy: Policy) {
        if let Some(edge) = self.edge.as_mut() {
            edge.policy = policy;
            return;
        }
        if let Some(flow) = self.flow.as_mut() {
            flow.policy = policy;
        }
    }

    /// Resize the crowd. Slots above the target simply stop being reborn — their lives run
    /// out and nothing replaces them — and slots below it wake at the new share of the rate,
    /// each first send a birth. The fleet's demand is unchanged; only how finely it is
    /// divided, which is the whole experiment.
    pub fn set_clients(&mut self, clients: usize) {
        let Some(edge) = self.edge.as_mut() else { return };
        edge.target_clients = clients.clamp(1, MAX_CLIENTS);
        let (target, qps) = (edge.target_clients, edge.qps);
        let now = self.world.now_ms();
        let Some(flow) = self.flow.as_mut() else { return };
        let share = qps / target as f64;
        for (c, clock) in flow.clocks.iter_mut().enumerate() {
            clock.set_qps(if c < target { share } else { 0.0 }, now, &mut self.rng);
        }
    }

    /// How many balancers are in rotation. A living client homed on one slid out of rotation
    /// re-homes at its next send — a drained balancer's sessions reconnect elsewhere, and pay
    /// the handshake that reconnecting is.
    pub fn set_lbs(&mut self, lbs: usize) {
        if let Some(edge) = self.edge.as_mut() {
            edge.active_lbs = lbs.clamp(1, MAX_LBS);
        }
    }

    /// How many servers are in rotation: every pool becomes the active set — the balancers'
    /// behind an edge, the clients' own on a direct card. A server slid out drains — nothing
    /// new is sent to it — and its heard counter simply goes quiet, still true about a machine
    /// nobody asks.
    ///
    /// This and [`set_pool`](MultiEngine::set_pool) write the same field from two directions:
    /// rotation is which machines exist to be picked, a pool is which of them one client draws
    /// between. A card offers one or the other, never both.
    pub fn set_servers(&mut self, servers: usize) {
        if let Some(edge) = self.edge.as_mut() {
            edge.active_servers = servers.clamp(1, SERVERS);
            for lb in &edge.lbs {
                lb.lock().unwrap().pool = (0..edge.active_servers).collect();
            }
            return;
        }
        let rotation: Vec<usize> = (0..servers.clamp(1, self.batch.servers)).collect();
        let Some(flow) = self.flow.as_ref() else { return };
        for routing in &flow.routings {
            routing.lock().unwrap().pool = rotation.clone();
        }
    }

    /// The crowd's mean lifetime. Takes effect at each slot's next death — lives already
    /// begun keep the span they were born with.
    pub fn set_lifetime(&mut self, lifetime_ms: f64) {
        if let Some(edge) = self.edge.as_mut() {
            edge.lifetime_ms = lifetime_ms.max(1.0);
        }
    }

    /// Shrink every client's pool to `size` machines of its own choosing. Redrawn rather than
    /// truncated — a pool is a client's standing draw, not a prefix of one — and what a client
    /// has [`Heard`] survives: knowledge is per machine, and a machine leaving the pool does
    /// not make what it said less true.
    pub fn set_pool(&mut self, size: usize) {
        let servers = self.batch.servers;
        let Some(flow) = self.flow.as_mut() else { return };
        for routing in &flow.routings {
            routing.lock().unwrap().pool = draw_pool(servers, size, &mut flow.picks);
        }
    }

    /// The whole crowd's arrival rate, in requests per second. Behind an edge only the live
    /// slots share it; a slot waiting to be born stays quiet.
    pub fn set_qps(&mut self, qps: f64) {
        let qps = qps.max(0.0);
        if let Some(edge) = self.edge.as_mut() {
            edge.qps = qps;
            let target = edge.target_clients;
            return self.set_clients(target);
        }
        let now = self.world.now_ms();
        let Some(flow) = self.flow.as_mut() else { return };
        let share = qps / flow.clocks.len() as f64;
        for clock in flow.clocks.iter_mut() {
            clock.set_qps(share, now, &mut self.rng);
        }
    }

    /// Which server each client was routed to, in client order (empty until fired).
    pub fn assignment(&self) -> &[usize] {
        &self.assignment
    }

    /// How many requests routing sent each server — the balls-in-bins load (all zero
    /// until a batch has been fired).
    pub fn per_server_load(&self) -> Vec<usize> {
        let mut load = vec![0usize; self.batch.servers];
        for &s in &self.assignment {
            load[s] += self.batch.reqs_per_client;
        }
        load
    }

    /// Each balancer as the card draws it. A client counts toward the balancer it lives on
    /// once it has lived at all — a slot never yet born is nobody's — and only while it is
    /// inside the crowd's target, because a slot the slider shrank away is on its way out
    /// however alive it looks.
    pub fn balancers(&self) -> Vec<BalancerView> {
        let Some(edge) = self.edge.as_ref() else { return Vec::new() };
        let mut clients = vec![0usize; edge.lbs.len()];
        for c in 0..edge.target_clients {
            if edge.dies_at[c] > 0.0 {
                clients[edge.homes[c]] += 1;
            }
        }
        edge.inflight
            .iter()
            .enumerate()
            .map(|(l, inflight)| BalancerView {
                clients: clients[l],
                inflight: inflight.load(Ordering::Relaxed),
            })
            .collect()
    }

    /// Each server as the fleet view draws it: its routed load and its live counts.
    pub fn fleet(&self) -> Vec<FleetServer> {
        let load = self.per_server_load();
        self.states
            .iter()
            .enumerate()
            .map(|(s, state)| FleetServer { load: load[s], counts: state.lock().unwrap().counts() })
            .collect()
    }

    /// The button: every client picks a server uniformly at random, opens one
    /// connection to it, and — once the handshake completes — sends all of its
    /// requests down it at once.
    pub fn fire(&mut self) {
        if self.fired {
            return;
        }
        self.fired = true;
        let t0 = self.world.now_ms();
        let lines = self.lines();
        for client in 0..self.batch.clients {
            // Uninformed and sticky: one uniform draw, then the client lives with it.
            let home = self.rng.below(self.batch.servers);
            self.assignment.push(home);
            let open: Connections = Arc::new(Mutex::new(vec![None; self.batch.servers]));
            for _ in 0..self.batch.reqs_per_client {
                let id = self.ids.fetch_add(1, Ordering::Relaxed);
                let req = SimReq {
                    id,
                    enq_t: 0.0,
                    tenant: crate::engine::SOLO,
                    phases: make_phases(&mut self.rng, false, 1.0),
                };
                self.world.spawn(ask(Ask {
                    req,
                    client,
                    asked_at: t0,
                    server: home,
                    open: open.clone(),
                    lines: lines.clone(),
                    ids: self.ids.clone(),
                    ledger: self.ledger.clone(),
                    clock: self.world.clock(),
                    rng: Rng::new(self.rng.u64()),
                    routing: None,
                }));
            }
        }
    }

    /// Every machine as a client task reaches it.
    fn lines(&self) -> Vec<Line> {
        self.servers
            .iter()
            .map(|s| Line { state: s.state(), cores: s.cores(), svc: s.svc() })
            .collect()
    }

    /// The next send the crowd owes inside this frame, and whose it is.
    fn next_send(&self, t_end: f64) -> Option<(usize, f64)> {
        Arrivals::earliest(self.flow.as_ref()?.clocks.iter(), t_end)
    }

    /// One client's due request goes out. It consults its policy here, at the instant it sends,
    /// so a policy changed between two frames is answered by the first request after it — never
    /// by one already on its way.
    fn send(&mut self, client: usize, at: f64) {
        if self.edge.is_some() {
            return self.send_via(client, at);
        }
        let lines = self.lines();
        let Some(flow) = self.flow.as_mut() else { return };
        let server = {
            let mut routing = flow.routings[client].lock().unwrap();
            let server = flow.policy.send(&mut routing, at, &lines, &mut flow.picks);
            routing.home = server;
            server
        };
        let req = SimReq {
            id: self.ids.fetch_add(1, Ordering::Relaxed),
            enq_t: at,
            tenant: crate::engine::SOLO,
            phases: make_phases(&mut self.rng, false, 1.0),
        };
        self.world.spawn(ask(Ask {
            req,
            client,
            asked_at: at,
            server,
            open: flow.open[client].clone(),
            lines,
            ids: self.ids.clone(),
            ledger: self.ledger.clone(),
            clock: self.world.clock(),
            rng: Rng::new(self.rng.u64()),
            routing: Some((flow.policy, flow.routings[client].clone())),
        }));
        flow.clocks[client].sent(at, &mut self.rng);
    }

    /// One client's due request goes out through its balancer. The send is also where a life
    /// is settled: a slot whose life ran out between sends is reborn here — new balancer, new
    /// death clock, cold connection — and a slot whose balancer left the rotation reconnects
    /// the same way. Nothing else ever touches a life, so churn costs exactly what the sends
    /// that meet it pay.
    fn send_via(&mut self, client: usize, at: f64) {
        let lines = self.lines();
        let (Some(flow), Some(edge)) = (self.flow.as_mut(), self.edge.as_mut()) else { return };
        if at >= edge.dies_at[client] || edge.homes[client] >= edge.active_lbs {
            edge.homes[client] = flow.picks.below(edge.active_lbs);
            // Exponential lives: churn with no schedule, drawn on the routing stream so the
            // same seed lives and dies identically under either balancer policy.
            edge.dies_at[client] = at - edge.lifetime_ms * flow.picks.f64().max(1e-12).ln();
            edge.open[client] = Arc::new(Mutex::new(None));
        }
        let req = SimReq {
            id: self.ids.fetch_add(1, Ordering::Relaxed),
            enq_t: at,
            tenant: crate::engine::SOLO,
            phases: make_phases(&mut self.rng, false, 1.0),
        };
        self.world.spawn(ask_via(EdgeAsk {
            req,
            client,
            asked_at: at,
            lb: edge.lbs[edge.homes[client]].clone(),
            lb_at: edge.homes[client],
            policy: edge.policy,
            open: edge.open[client].clone(),
            inflight: edge.inflight[edge.homes[client]].clone(),
            lines,
            ids: self.ids.clone(),
            ledger: self.ledger.clone(),
            clock: self.world.clock(),
            rng: Rng::new(self.rng.u64()),
        }));
        flow.clocks[client].sent(at, &mut self.rng);
    }

    /// Advance the batch by one frame of virtual time, folding each server's stamped
    /// hops into the ledger and finalising whatever reached a verdict.
    ///
    /// A frame's sends are not a batch: the world is advanced to each due instant before the
    /// send at it is routed, so a client choosing on what the fleet carries sees the sends made
    /// earlier in the same frame land. Read once at the frame's edge, ten sends a frame would
    /// all agree about which machine was idle and all go to it.
    pub fn tick(&mut self, virt_dt_ms: f64) {
        // Last frame's departures, if nobody drained them: a leg's pulse is launched on the
        // frame the leg departs or not at all. Cleared before the pump, because the edge
        // legs push theirs mid-pump.
        self.ledger.lock().unwrap().wire_events.clear();
        let t_end = self.world.now_ms() + virt_dt_ms;
        let states = self.states.clone();
        while let Some((client, at)) = self.next_send(t_end) {
            // A send is handed over up to one NET_MS before the frame reaches it, and the frame
            // is as far as the pump may go, so the last of them route on where it ends.
            self.world.pump(at.min(t_end), &states, |_| {});
            self.send(client, at);
        }
        self.world.pump(t_end, &states, |_| {});

        let mut ledger = self.ledger.lock().unwrap();
        // A route is wanted on the frame the trip closed and never again, so the frame's is all
        // there is to hold: a reader that did not ask for the last one is not going to.
        ledger.journeys.clear();
        for state in &self.states {
            for hop in std::mem::take(&mut state.lock().unwrap().hops) {
                // The network stamps are the server legs' departures — read as they fold,
                // before any absorb, so the attempt that names the wire is still open.
                let leg = match hop.station {
                    Station::NetworkIn { .. } => Some((false, None)),
                    Station::NetworkOut { reply, .. } => Some((true, Some(reply.outcome()))),
                    _ => None,
                };
                if let Some((homeward, outcome)) = leg {
                    let wire = ledger
                        .attempts
                        .get(&hop.id)
                        .map(|a| (a.via.unwrap_or(a.client), a.server));
                    if let Some((sender, server)) = wire {
                        ledger.wire_events.push(WireEvent {
                            leg: Leg::Server { sender, server },
                            homeward,
                            outcome,
                            at: hop.t,
                        });
                    }
                }
                ledger.hops.entry(hop.id).or_default().push((hop.station, hop.t));
            }
            state.lock().unwrap().departures.clear();
        }
        let mut answered: Vec<(u32, Outcome, f64)> = ledger
            .attempts
            .iter()
            .filter_map(|(&id, a)| a.verdict.map(|(outcome, round_trip)| (id, outcome, round_trip)))
            .collect();
        // In the order the client made them. A pump long enough to hold a refusal and the retry
        // behind it answers both in one tick, and a trip's first attempt is what dates it — ids
        // are minted as each attempt is sent, and a HashMap hands them back however it likes.
        answered.sort_unstable_by_key(|&(id, ..)| id);
        for (id, outcome, round_trip) in answered {
            ledger.absorb(id, outcome, round_trip);
        }
    }

    /// The batch's request total — every client's share of it.
    pub fn requests(&self) -> usize {
        self.batch.requests()
    }

    /// Has every request in the batch reached a verdict?
    pub fn settled(&self) -> bool {
        self.fired && self.answered().trips == self.requests()
    }

    /// What the fleet has answered since it started — the tallies a header counts up, which are
    /// the whole run's where [`with_records`](MultiEngine::with_records) is a window on it.
    pub fn answered(&self) -> Answered {
        self.ledger.lock().unwrap().answered
    }

    /// The batch as it stands: every request that has an answer *yet*.
    ///
    /// Read mid-settle this is a live reading and biased the way a live reading is — the quick
    /// requests come back first, so a percentile of a half-finished batch is optimistic. It
    /// becomes the batch's own number when [`settled`](MultiEngine::settled) is true.
    pub fn so_far(&self) -> Summary {
        Summary::of(self.ledger.lock().unwrap().done.iter().cloned().collect())
    }

    /// The answered records themselves, read in place. A reader that wants the records rather
    /// than the statistics over them — the ranking a percentile is an index into — borrows
    /// them for the length of `f`, because it asks every frame and a batch of a thousand is a
    /// thousand records to copy and a whole [`Summary`] to compute and discard.
    pub fn with_records<R>(&self, f: impl FnOnce(&VecDeque<Record>) -> R) -> R {
        f(&self.ledger.lock().unwrap().done)
    }

    /// Each client's open trips right now — the strip's orange. Cloned out: it is a handful
    /// of counters, read once a frame, and a borrow would hold the ledger across view work.
    pub fn outstanding(&self) -> Vec<usize> {
        self.ledger.lock().unwrap().outstanding.clone()
    }

    /// The legs that departed since this was last asked. Draining, like
    /// [`journeys`](MultiEngine::journeys): a departure happened once, and the pulse it
    /// launches is the browser's from then on.
    pub fn wire_events(&mut self) -> Vec<WireEvent> {
        std::mem::take(&mut self.ledger.lock().unwrap().wire_events)
    }

    /// Each client's warm set — the machines its wires reach: the whole fleet on a direct
    /// card, the drawn pool on the pool card. Empty for a spiked batch, whose routing is the
    /// assignment it fired with.
    pub fn pools(&self) -> Vec<Vec<usize>> {
        let Some(flow) = self.flow.as_ref() else { return Vec::new() };
        flow.routings.iter().map(|r| r.lock().unwrap().pool.clone()).collect()
    }

    /// What the crowd currently believes about one machine: the freshest reading any client
    /// holds, and how long ago the machine itself said it.
    ///
    /// This is what the routing picks on — the same [`Heard`] counters [`two_heard`] draws
    /// between — so anything steering by it steers by exactly what the load balancing knows,
    /// staleness included. `None` for a machine nobody has heard from.
    pub fn believed(&self) -> Vec<Option<Believed>> {
        let now = self.world.now_ms();
        let Some(flow) = self.flow.as_ref() else { return Vec::new() };
        let mut freshest: Vec<Option<Heard>> = vec![None; self.batch.servers];
        for routing in &flow.routings {
            for (slot, heard) in routing.lock().unwrap().heard.iter().enumerate() {
                let Some(heard) = *heard else { continue };
                if freshest[slot].is_none_or(|held| heard.at > held.at) {
                    freshest[slot] = Some(heard);
                }
            }
        }
        freshest
            .into_iter()
            .map(|heard| {
                heard.map(|heard| Believed { load: heard.load, age_ms: (now - heard.at).max(0.0) })
            })
            .collect()
    }

    /// Which balancer each client slot lives on — `None` for a slot not yet born or outside
    /// the crowd's target. Exactly the walk the balancer boxes count clients with, and the
    /// pairs the client wires are drawn from.
    pub fn homes(&self) -> Vec<Option<usize>> {
        let Some(edge) = self.edge.as_ref() else { return Vec::new() };
        (0..edge.homes.len())
            .map(|c| (c < edge.target_clients && edge.dies_at[c] > 0.0).then(|| edge.homes[c]))
            .collect()
    }

    /// The round trips that closed since this was last asked, each as the machines its client
    /// reached and what they answered.
    ///
    /// Draining, because a route is something that *happened*: a view plays a journey once, and
    /// a second reading of the same one would play it twice. What is held is only ever the trips
    /// of the frame just pumped — [`tick`](MultiEngine::tick) drops the previous frame's, so a
    /// card that draws no wires costs no memory for the ones it is not drawing.
    pub fn journeys(&mut self) -> Vec<Journey> {
        std::mem::take(&mut self.ledger.lock().unwrap().journeys)
    }

    }

/// One client request's journey, as its own task holds it.
struct Ask {
    req: SimReq,
    client: usize,
    /// When the client asked — before the handshake of the attempt about to be made.
    asked_at: f64,
    server: usize,
    open: Connections,
    lines: Vec<Line>,
    ids: Arc<AtomicU32>,
    ledger: Arc<Mutex<Ledger>>,
    clock: taipei::clock::Clock,
    rng: Rng,
    /// How this client picks again when it is refused, and the knowledge it picks from. A
    /// spiked batch has neither: it draws once, uniformly, and lives with it.
    routing: Option<(Policy, Arc<Mutex<Routing>>)>,
}

/// One client request, from the first ask to an answer. A refusal is not an answer — the server
/// said "not me, not now" — so the client picks again and asks again, and keeps asking. What it
/// pays is the whole sequence: every handshake it opened, every queue it waited in, and every
/// refusal that travelled home.
///
/// Where it asks next is its [`Policy`]'s to say, and every policy may land back on the machine
/// that just refused it — a refusal says this request failed, not that the machine is bad. A
/// spiked batch carries no policy at all: it draws uniformly, because a first send has nothing
/// to have learned from.
async fn ask(a: Ask) {
    let Ask {
        mut req,
        client,
        mut asked_at,
        mut server,
        open,
        lines,
        ids,
        ledger,
        clock,
        mut rng,
        routing,
    } = a;
    let trip = req.id;
    let net = NetLegs { in_ms: NET_MS, out_ms: NET_MS };
    loop {
        // The connection: opened once per machine, and shared by everything the client sends
        // down it. Ten requests fired together all wait out the one handshake; a retry that
        // lands somewhere this client has already been pays nothing and goes straight out.
        let usable_t = *open.lock().unwrap()[server].get_or_insert(asked_at + HANDSHAKE_MS);
        let handshake_ms = (usable_t - asked_at).max(0.0);

        let send_t = asked_at + handshake_ms;
        req.enq_t = send_t + NET_MS;
        let line = lines[server].clone();
        // Booked before the leg out is slept, so the attempt is on the wire the whole time
        // it is on the wire.
        {
            let mut book = ledger.lock().unwrap();
            if req.id == trip {
                charge(&mut book.outstanding, client);
            }
            book.attempts.insert(
                req.id,
                Attempt {
                    trip,
                    client,
                    policy: routing.as_ref().map(|(policy, _)| *policy),
                    server,
                    via: None,
                    handshake_ms,
                    send_t,
                    verdict: None,
                },
            );
        }
        arrive(&line.state, &clock, req.id, req.enq_t, net).await;

        let attempt = req.clone();
        let Some(Answer { outcome, round_trip, load }) =
            serve(line.state, clock.clone(), line.cores, line.svc, attempt, send_t, net, true).await
        else {
            // A verdict dropped on the floor still closes the client's trip — nothing more is
            // coming, so the circle must not stay orange for it.
            if let Some(open) = ledger.lock().unwrap().outstanding.get_mut(client) {
                *open = open.saturating_sub(1);
            }
            return;
        };
        if let Some(a) = ledger.lock().unwrap().attempts.get_mut(&req.id) {
            a.verdict = Some((outcome, round_trip));
        }
        // The answer carried the machine's own load reading, stamped at the verdict — one leg
        // home before the client holds it. Deposited whatever policy asked, because knowledge
        // is not the policy's; only reading it is. A verdict that sent no reply taught nothing.
        if let (Some((_, routing)), Some(_)) = (&routing, outcome.reply()) {
            routing.lock().unwrap().heard[server] =
                Some(Heard { load, at: send_t + round_trip - net.out_ms });
        }
        if !outcome.refused() {
            return;
        }
        // Refused. The client has its "no" in hand — that is where it asks from next.
        asked_at = send_t + round_trip;
        server = match &routing {
            Some((policy, routing)) => {
                let mut r = routing.lock().unwrap();
                let next = policy.refused(&mut r, asked_at, &lines, &mut rng);
                r.home = next;
                next
            }
            None => rng.below(lines.len()),
        };
        req.id = ids.fetch_add(1, Ordering::Relaxed);
    }
}

/// One client request's journey through its balancer, as its own task holds it.
struct EdgeAsk {
    req: SimReq,
    client: usize,
    /// When the client asked — before the handshake its life may owe.
    asked_at: f64,
    /// The balancer this client lives on: its heard table and its pool.
    lb: Arc<Mutex<Routing>>,
    /// The same balancer by its slot — the wire the trip's edge legs are on.
    lb_at: usize,
    policy: Policy,
    /// When this client's connection to its balancer becomes usable, shared by everything
    /// the life sends down it.
    open: Arc<Mutex<Option<f64>>>,
    /// The balancer's in-flight tally, carried for the whole trip — the retries too, since
    /// they are the balancer's own asking.
    inflight: Arc<AtomicUsize>,
    lines: Vec<Line>,
    ids: Arc<AtomicU32>,
    ledger: Arc<Mutex<Ledger>>,
    clock: taipei::clock::Clock,
    rng: Rng,
}

/// How a balancer picks from its pool: the paid power-of-two over what it has heard, or a
/// blind draw. Both stay inside the pool, because the pool is the rotation — a machine slid
/// out of it is not a machine any policy may reach.
fn edge_pick(policy: Policy, lb: &mut Routing, now: f64, rng: &mut Rng) -> usize {
    match policy {
        Policy::PowerOfTwo => two_heard(lb, now, rng),
        _ => lb.pool[rng.below(lb.pool.len())],
    }
}

/// Ride the shared clock to `t` — an edge leg is a real sleep like every other leg.
async fn until(t: f64) {
    sleep_until(Instant::from_micros((t * 1000.0) as u64)).await;
}

/// One client request: client → balancer → machine, and home again. The client's part is dumb
/// on purpose — it opens one connection per life and asks its balancer; every routing decision,
/// the pick and the repick a refusal forces, happens on the balancer, out of warm lines and a
/// heard table the whole crowd's traffic keeps fresh. A refusal never travels the edge: the
/// balancer eats it, asks another machine, and the client sees only how long its answer took.
///
/// The first attempt is billed from the client — its send instant, its handshake, a round trip
/// spanning both edge legs. Retries are billed from the balancer's own clock, and whichever
/// attempt finally answers carries the one edge leg home. The ledger's arithmetic then reads
/// exactly as it does without a tier: `began` is the client's ask, the receipt is the client's,
/// and everything a refusal cost sits inside the queue-wait span where a retry belongs.
async fn ask_via(a: EdgeAsk) {
    let EdgeAsk {
        mut req,
        client,
        asked_at,
        lb,
        lb_at,
        policy,
        open,
        inflight,
        lines,
        ids,
        ledger,
        clock,
        mut rng,
    } = a;
    let trip = req.id;
    inflight.fetch_add(1, Ordering::Relaxed);
    let net = NetLegs { in_ms: NET_MS, out_ms: NET_MS };
    // The connection to the balancer: opened once per life. Requests in the air together all
    // wait out the one handshake.
    let usable_t = *open.lock().unwrap().get_or_insert(asked_at + EDGE_HANDSHAKE_MS);
    let mut handshake_ms = (usable_t - asked_at).max(0.0);
    let mut billed_from = asked_at + handshake_ms;
    let mut at_lb = billed_from + EDGE_MS;
    // The trip opens the moment the client asks; the leg departs when its handshake is paid.
    // Behind an edge only this first send is the client's own — the retries are the
    // balancer's asking, invisible on the client's wire.
    charge(&mut ledger.lock().unwrap().outstanding, client);
    until(billed_from).await;
    ledger.lock().unwrap().wire_events.push(WireEvent {
        leg: Leg::Edge { client, lb: lb_at },
        homeward: false,
        outcome: None,
        at: billed_from,
    });
    // The request rides to the balancer before the balancer chooses: the pick is made where
    // and when it is made, on the heard table as it stands at arrival.
    until(at_lb).await;
    loop {
        let server = edge_pick(policy, &mut lb.lock().unwrap(), at_lb, &mut rng);
        req.enq_t = at_lb + NET_MS;
        let line = lines[server].clone();
        {
            let mut book = ledger.lock().unwrap();
            book.attempts.insert(
                req.id,
                Attempt {
                    trip,
                    client,
                    policy: Some(policy),
                    server,
                    via: Some(lb_at),
                    handshake_ms,
                    send_t: billed_from,
                    verdict: None,
                },
            );
        }
        arrive(&line.state, &clock, req.id, req.enq_t, net).await;

        let attempt = req.clone();
        let Some(Answer { outcome, round_trip, load }) =
            serve(line.state, clock.clone(), line.cores, line.svc, attempt, at_lb, net, true).await
        else {
            inflight.fetch_sub(1, Ordering::Relaxed);
            if let Some(open) = ledger.lock().unwrap().outstanding.get_mut(client) {
                *open = open.saturating_sub(1);
            }
            return;
        };
        // The answer passed through the balancer, and the balancer heard what the machine said
        // it was carrying — the deposit that keeps the whole tier informed.
        lb.lock().unwrap().heard[server] =
            Some(Heard { load, at: at_lb + round_trip - net.out_ms });
        if outcome.refused() {
            if let Some(a) = ledger.lock().unwrap().attempts.get_mut(&req.id) {
                a.verdict = Some((outcome, round_trip));
            }
            // Refused. The "no" is back at the balancer, which asks somewhere else at once —
            // no edge leg, no client involvement, and a heard table one refusal fresher.
            at_lb += round_trip;
            billed_from = at_lb;
            handshake_ms = 0.0;
            req.id = ids.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        let rt = (at_lb - billed_from) + round_trip + EDGE_MS;
        // The answer rides the one edge leg home — slept like every other leg, with the
        // verdict landing when the client holds it, not when the balancer let go of it. The
        // balancer's own tally closes here: from its side of the wire the trip is answered.
        inflight.fetch_sub(1, Ordering::Relaxed);
        let answered_at = at_lb + round_trip;
        ledger.lock().unwrap().wire_events.push(WireEvent {
            leg: Leg::Edge { client, lb: lb_at },
            homeward: true,
            outcome: Some(outcome),
            at: answered_at,
        });
        until(answered_at + EDGE_MS).await;
        if let Some(a) = ledger.lock().unwrap().attempts.get_mut(&req.id) {
            a.verdict = Some((outcome, rt));
        }
        return;
    }
}

// Probe hooks: the pieces of a tick, exposed so [`crate::perf_probe`] can drive frames its
// own way and time them — and the pool variants the design conversation measured. Test-only:
// nothing shipped reads them.
#[cfg(test)]
impl MultiEngine {
    /// Pools with equalized membership — a shuffled ring, each client taking the next `size`
    /// machines, so every machine sits in the same number of pools. Measured well (it removes
    /// the membership imbalance uniform pools pay for); the chapter chose uniform anyway, and
    /// this stays as the measured alternative.
    pub(crate) fn set_pool_ring(&mut self, size: usize) {
        let servers = self.batch.servers;
        let Some(flow) = self.flow.as_mut() else { return };
        let mut ring: Vec<usize> = (0..servers).collect();
        for i in 0..servers {
            let j = i + flow.picks.below(servers - i);
            ring.swap(i, j);
        }
        for (c, routing) in flow.routings.iter().enumerate() {
            routing.lock().unwrap().pool =
                (0..size.clamp(1, servers)).map(|k| ring[(c * size + k) % servers]).collect();
        }
    }

    pub(crate) fn probe_lines(&self) -> usize {
        self.lines().len()
    }
    pub(crate) fn probe_now(&self) -> f64 {
        self.world.now_ms()
    }
    pub(crate) fn probe_states(&self) -> Vec<Arc<Mutex<Sim>>> {
        self.states.clone()
    }
    pub(crate) fn probe_next_send(&self, t_end: f64) -> Option<(usize, f64)> {
        self.next_send(t_end)
    }
    pub(crate) fn probe_send(&mut self, client: usize, at: f64) {
        self.send(client, at)
    }
    pub(crate) fn probe_pump(&self, t_end: f64, states: &[Arc<Mutex<Sim>>], steps: &mut usize) {
        self.world.pump(t_end, states, |_| *steps += 1);
    }
    pub(crate) fn probe_fold(&mut self) {
        let mut ledger = self.ledger.lock().unwrap();
        ledger.journeys.clear();
        for state in &self.states {
            for hop in std::mem::take(&mut state.lock().unwrap().hops) {
                ledger.hops.entry(hop.id).or_default().push((hop.station, hop.t));
            }
            state.lock().unwrap().departures.clear();
        }
        let mut answered: Vec<(u32, Outcome, f64)> = ledger
            .attempts
            .iter()
            .filter_map(|(&id, a)| a.verdict.map(|(outcome, round_trip)| (id, outcome, round_trip)))
            .collect();
        answered.sort_unstable_by_key(|&(id, ..)| id);
        for (id, outcome, round_trip) in answered {
            ledger.absorb(id, outcome, round_trip);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One frame of virtual time — ~60fps, the step these tests advance an engine by. The
    /// cards drive their own from the browser's frames; only a test needs a frame it can name.
    const FRAME_MS: f64 = 16.0;

    /// Settle a spiked batch and fold it into the table's numbers.
    ///
    /// Only a test waits for an end: a card ticks its engine from the browser's frames and
    /// draws whatever that leaves, so nothing in the blog ever asks a batch to finish. The
    /// bound is generous because a loaded batch is slow in virtual time rather than in work
    /// — clients that keep asking take seconds to all get through — and a frame with no event
    /// in it costs nothing: the pump hops deadline to deadline.
    fn settle(engine: &mut MultiEngine) -> Summary {
        engine.fire();
        const MAX_SETTLE_FRAMES: usize = 20_000;
        for _ in 0..MAX_SETTLE_FRAMES {
            engine.tick(FRAME_MS);
            if engine.settled() {
                break;
            }
        }
        engine.so_far()
    }

    /// The batch size the invariant tests run: ten clients, ten requests each.
    const CLIENTS: usize = 10;
    const REQUESTS: usize = CLIENTS * REQS_PER_CLIENT;

    /// The chapter's crowd: a hundred clients on the ten-machine fleet.
    fn crowd() -> Batch {
        crowd_of(100)
    }

    fn crowd_of(clients: usize) -> Batch {
        Batch { clients, servers: SERVERS, reqs_per_client: 1 }
    }

    /// Advance a fleet by `secs` of virtual time, a frame at a time.
    fn run_for(engine: &mut MultiEngine, secs: f64) {
        for _ in 0..(secs * 1000.0 / FRAME_MS) as usize {
            engine.tick(FRAME_MS);
        }
    }

    /// A continuously-loaded fleet, `secs` of virtual time in. Handed back as the fleet rather
    /// than as what came home: a reading of the whole run is its [`Answered`] tallies and a
    /// reading of the recent past is its window, and only the test knows which it is asking for.
    fn flow_seeded(seed: u64, batch: Batch, policy: Policy, qps: f64, secs: f64) -> MultiEngine {
        let mut engine = MultiEngine::flowing(seed, batch, qps, policy);
        run_for(&mut engine, secs);
        engine
    }

    fn flow_for(policy: Policy, qps: f64, secs: f64) -> MultiEngine {
        flow_seeded(0x5eed, crowd(), policy, qps, secs)
    }

    /// The load the policy comparison runs at: the fleet busy enough that a bad pick costs
    /// something, and not so busy that every pick is bad. The gate admits on CPU, so what
    /// this is a fraction of is what the gate lets through — near 680 a second here.
    const LOADED_QPS: f64 = 600.0;

    /// Four runs of the same fleet, differing only in their draws.
    const SEEDS: [u64; 4] = [0x5eed, 0xabc1, 0x77f3, 0x1234];

    /// How many refusals a policy takes over five seconds of load, past the first second — so
    /// the reading describes a fleet at its working load rather than one still filling from cold.
    fn refusals(policy: Policy, seed: u64) -> usize {
        refusals_in(crowd(), policy, seed)
    }

    fn refusals_in(batch: Batch, policy: Policy, seed: u64) -> usize {
        let mut engine = MultiEngine::flowing(seed, batch, LOADED_QPS, policy);
        run_for(&mut engine, 1.0);
        let cold = engine.answered().refusals;
        run_for(&mut engine, 5.0);
        engine.answered().refusals - cold
    }

    /// Demand that does not stop is the same fleet with clocks on it, and one client against
    /// one machine is that with both counts at one — not a path of its own.
    #[test]
    fn a_flowing_fleet_answers_and_a_crowd_of_one_is_a_crowd() {
        let crowd = flow_for(Policy::AlwaysRandom, LOADED_QPS, 4.0);
        let answered = crowd.answered().trips;
        assert!(answered > 500, "a loaded fleet answers steadily: {answered}");
        assert!(
            crowd.with_records(|rs| rs.iter().any(|r| r.client != rs[0].client)),
            "the whole crowd is served",
        );

        let solo = flow_seeded(0x5eed, crowd_of(1), Policy::PowerOfTwoVirtual, 20.0, 4.0);
        assert!(solo.answered().trips > 0, "one client on one clock still gets answers");
        assert!(
            solo.with_records(|rs| rs.iter().all(|r| r.client == 0)),
            "and they are all its own",
        );
    }

    /// Staying put until you are refused is the worst of the three: a client that holds its
    /// machine through a queue that is filling keeps feeding it, where one that draws again
    /// spreads the same requests over the fleet.
    ///
    /// Across runs, not within one. A few seconds of a fleet this size turns away a couple of
    /// dozen requests, and which side of a couple of dozen a single run lands on is the run's
    /// own luck — two of the four seeds here put sticky ahead. It is the count over several
    /// that separates, which is the honest shape of the claim.
    #[test]
    fn staying_put_until_refused_is_refused_more_than_picking_again() {
        let over = |policy| SEEDS.iter().map(|&s| refusals(policy, s)).sum::<usize>();
        let (sticky, random) = (over(Policy::RepickOnQueueTimeout), over(Policy::AlwaysRandom));
        assert!(sticky > random, "sticky {sticky} vs random {random} over {} runs", SEEDS.len());
    }

    /// The second choice earns its keep. Reading the machines for nothing is the best any
    /// policy could do with two draws, so this is the ceiling the later ones are measured
    /// against rather than a claim about what a real client can know.
    #[test]
    fn a_second_choice_is_refused_less_than_a_blind_one() {
        let over = |policy| SEEDS.iter().map(|&s| refusals(policy, s)).sum::<usize>();
        let (two, one) = (over(Policy::PowerOfTwoVirtual), over(Policy::AlwaysRandom));
        assert!(two < one, "two choices {two} vs one {one} over {} runs", SEEDS.len());
    }

    /// The paid pick-2 at the pool card's crowd: counters the clients actually heard — aged on
    /// the chapter's decay, optimistically bumped — still beat blind draws. This is the pool
    /// card's whole claim, so it is pinned at the card's own shape: twenty clients, whole-fleet
    /// pools.
    #[test]
    fn heard_counters_beat_blind_draws_at_the_pool_cards_crowd() {
        let over =
            |policy| SEEDS.iter().map(|&s| refusals_in(crowd_of(20), policy, s)).sum::<usize>();
        let (heard, blind) = (over(Policy::PowerOfTwo), over(Policy::AlwaysRandom));
        assert!(heard < blind, "heard {heard} vs blind {blind} over {} runs", SEEDS.len());
    }

    /// A balancer tier at the LB card's opening shape, `secs` of virtual time in.
    fn edge_for(seed: u64, policy: Policy, lifetime_ms: f64, secs: f64) -> MultiEngine {
        let mut engine = MultiEngine::edged(seed, LOADED_QPS, 100, 20, lifetime_ms, policy);
        run_for(&mut engine, secs);
        engine
    }

    /// The LB card's whole claim: balancers that use their heard counters beat balancers that
    /// draw blind — with the same crowd, the same lives, and the same demand, because both
    /// run off the same seeds' streams.
    #[test]
    fn balancers_with_counters_beat_balancers_without() {
        let over = |policy| {
            SEEDS
                .iter()
                .map(|&s| {
                    let mut e = MultiEngine::edged(s, LOADED_QPS, 100, 20, 1000.0, policy);
                    run_for(&mut e, 1.0);
                    let cold = e.answered().refusals;
                    run_for(&mut e, 5.0);
                    e.answered().refusals - cold
                })
                .sum::<usize>()
        };
        let (heard, blind) = (over(Policy::PowerOfTwo), over(Policy::AlwaysRandom));
        assert!(heard < blind, "heard {heard} vs blind {blind} over {} runs", SEEDS.len());
    }

    /// Churn is what the lifetime knob buys: short lives keep paying the edge handshake and
    /// long lives amortise it away — and no trip ever pays more than one.
    #[test]
    fn short_lives_pay_the_edge_handshake_and_long_lives_amortise_it() {
        let shake_share = |lifetime_ms: f64| {
            let e = edge_for(0x5eed, Policy::PowerOfTwo, lifetime_ms, 5.0);
            e.with_records(|rs| {
                let paid: f64 = rs
                    .iter()
                    .map(|r| r.sections.get(&Section::Handshake).copied().unwrap_or(0.0))
                    .sum();
                paid / rs.len() as f64
            })
        };
        let (churning, settled) = (shake_share(200.0), shake_share(10_000.0));
        assert!(
            churning > 4.0 * settled.max(0.1),
            "short lives pay per life: {churning:.1} ms against {settled:.1} ms",
        );
        let e = edge_for(0x5eed, Policy::PowerOfTwo, 200.0, 3.0);
        e.with_records(|rs| {
            for r in rs {
                let shake = r.sections.get(&Section::Handshake).copied().unwrap_or(0.0);
                assert!(shake <= EDGE_HANDSHAKE_MS + 1e-9, "one handshake at most: {shake}");
            }
        });
    }

    /// Resizing is routing: a server slid out of rotation gets nothing new, drains, and the
    /// fleet answers on without it.
    #[test]
    fn a_server_out_of_rotation_drains_and_the_fleet_answers_on() {
        let mut e = edge_for(0x5eed, Policy::PowerOfTwo, 1000.0, 3.0);
        e.set_servers(4);
        run_for(&mut e, 2.0);
        let benched: Vec<usize> = e.fleet()[4..].iter().map(|f| f.counts.success).collect();
        let answered = e.answered().trips;
        run_for(&mut e, 2.0);
        assert_eq!(
            benched,
            e.fleet()[4..].iter().map(|f| f.counts.success).collect::<Vec<_>>(),
            "a benched server's tally has stopped",
        );
        assert!(e.fleet()[4..].iter().all(|f| f.counts.inflight == 0), "and it has drained");
        assert!(e.answered().trips > answered, "while the fleet answers on");
    }

    /// A life ends and its replacement lives on a balancer that is in rotation — nobody keeps
    /// sending to a machine that was drained out from under them.
    #[test]
    fn reborn_clients_home_only_on_active_balancers() {
        let mut e = edge_for(0x5eed, Policy::PowerOfTwo, 500.0, 2.0);
        e.set_lbs(3);
        run_for(&mut e, 3.0);
        let edge = e.edge.as_ref().expect("an edged fleet");
        for &home in &edge.homes[..edge.target_clients] {
            assert!(home < 3, "a live client homes inside the rotation: {home}");
        }
    }

    /// The departures balance the books exactly. Directly: one outbound server event per
    /// attempt — a retry is the client's own next send — and one homeward per verdict, the
    /// amber/green split matching what the fleet answered. Run to quiet, nothing is left
    /// owed and a further frame drains no events at all.
    #[test]
    fn wire_events_balance_the_attempts() {
        let drain = |engine: &mut MultiEngine, secs: f64| {
            let (mut out, mut ok, mut refused) = (0, 0, 0);
            for _ in 0..(secs * 1000.0 / FRAME_MS) as usize {
                engine.tick(FRAME_MS);
                let now = engine.probe_now();
                for event in engine.wire_events() {
                    let Leg::Server { sender, server } = event.leg else {
                        panic!("a direct card has no edge legs")
                    };
                    assert!(sender < crowd().clients && server < crowd().servers);
                    assert!(
                        event.at <= now + 1e-9 && event.at > now - FRAME_MS - 1e-9,
                        "a departure is stamped inside the frame it is drained on: \
                         {} of a frame ending {now}",
                        event.at,
                    );
                    match (event.homeward, event.outcome) {
                        (false, None) => out += 1,
                        (true, Some(outcome)) if outcome.refused() => refused += 1,
                        (true, Some(_)) => ok += 1,
                        (homeward, outcome) => panic!(
                            "a verdict rides home and only home: homeward {homeward}, \
                             carried {}",
                            outcome.is_some(),
                        ),
                    }
                }
            }
            (out, ok, refused)
        };

        let mut fleet = MultiEngine::flowing(0x5eed, crowd(), LOADED_QPS, Policy::AlwaysRandom);
        let loaded = drain(&mut fleet, 2.0);
        assert!(fleet.outstanding().iter().sum::<usize>() > 0, "a loaded crowd is owed answers");
        fleet.set_qps(0.0);
        let settled = drain(&mut fleet, 3.0);
        let (out, ok, refused) =
            (loaded.0 + settled.0, loaded.1 + settled.1, loaded.2 + settled.2);
        let answered = fleet.answered();
        assert_eq!(out, answered.trips + answered.refusals, "one departure per attempt");
        assert_eq!(ok, answered.trips, "one answer rode home per served trip");
        assert_eq!(refused, answered.refusals, "one refusal rode home per retry");
        assert!(fleet.outstanding().iter().all(|&n| n == 0), "nothing is left owed");
        fleet.tick(FRAME_MS);
        assert!(fleet.wire_events().is_empty(), "a quiet frame departs nothing");
    }

    /// Behind the edge every leg family flies: the client's one send out and its one answer
    /// home per trip on the edge wires — retries never touch them — and the balancer's own
    /// asking on the server wires, refusals included.
    #[test]
    fn edged_wire_events_split_by_hop() {
        let mut e = MultiEngine::edged(0x5eed, LOADED_QPS, 100, 20, 1000.0, Policy::PowerOfTwo);
        let (mut edge_out, mut edge_home, mut hop_out, mut hop_refused) = (0, 0, 0, 0);
        let mut drain = |e: &mut MultiEngine, secs: f64| {
            for _ in 0..(secs * 1000.0 / FRAME_MS) as usize {
                e.tick(FRAME_MS);
                for event in e.wire_events() {
                    match (event.leg, event.homeward) {
                        (Leg::Edge { client, lb }, homeward) => {
                            assert!(client < 100 && lb < MAX_LBS);
                            match homeward {
                                false => edge_out += 1,
                                true => {
                                    // A refusal never rides the edge home — the balancer
                                    // eats it and asks again.
                                    assert!(!event.outcome.expect("an answer").refused());
                                    edge_home += 1;
                                }
                            }
                        }
                        (Leg::Server { sender, .. }, homeward) => {
                            assert!(sender < MAX_LBS, "behind an edge the sender is the lb");
                            if !homeward {
                                hop_out += 1;
                            } else if event.outcome.expect("a verdict").refused() {
                                hop_refused += 1;
                            }
                        }
                    }
                }
            }
        };
        drain(&mut e, 2.0);
        e.set_qps(0.0);
        drain(&mut e, 3.0);
        let answered = e.answered();
        assert!(answered.refusals > 0, "the tier retried");
        assert_eq!(edge_out, answered.trips, "one edge send per trip");
        assert_eq!(edge_home, answered.trips, "one edge answer home per trip");
        assert_eq!(hop_out, answered.trips + answered.refusals, "the balancer asked per attempt");
        assert_eq!(hop_refused, answered.refusals);
        assert!(e.outstanding().iter().all(|&n| n == 0));
    }

    /// The wires draw from the same walk the balancer boxes count with: fold the homes and
    /// you get exactly the boxes' client tallies.
    #[test]
    fn homes_and_balancer_counts_are_the_same_walk() {
        let e = edge_for(0x5eed, Policy::PowerOfTwo, 1000.0, 8.0);
        let views = e.balancers();
        let mut counted = vec![0usize; views.len()];
        for &home in e.homes().iter().flatten() {
            counted[home] += 1;
        }
        assert_eq!(counted, views.iter().map(|v| v.clients).collect::<Vec<_>>());
    }

    /// The tier as drawn: every living client counts toward exactly one balancer, and what is
    /// in flight through the tier returns to nothing once the crowd goes quiet.
    #[test]
    fn balancer_views_count_homes_and_drain_when_quiet() {
        // Long enough that every slot's first arrival gap — drawn before `set_clients`
        // re-shared the rate — has fired, so the whole crowd has been born.
        let mut e = edge_for(0x5eed, Policy::PowerOfTwo, 1000.0, 8.0);
        let views = e.balancers();
        assert_eq!(views.len(), MAX_LBS);
        assert_eq!(views.iter().map(|v| v.clients).sum::<usize>(), 100, "everyone lives somewhere");
        assert!(views.iter().any(|v| v.inflight > 0), "a loaded tier has work in the air");
        e.set_qps(0.0);
        run_for(&mut e, 3.0);
        assert!(e.balancers().iter().all(|v| v.inflight == 0), "a quiet crowd empties the tier");
    }

    /// The billing survives the tier: an edged trip's phases still sum to what the client
    /// waited, however many machines its balancer asked — the ledger cannot tell the tier is
    /// there, which is the proof the tier's arithmetic is right.
    #[test]
    fn edged_sections_still_partition_the_round_trip() {
        let e = edge_for(0x5eed, Policy::PowerOfTwo, 500.0, 4.0);
        e.with_records(|rs| {
            assert!(rs.len() > 100, "a loaded tier answers plenty: {}", rs.len());
            for (i, r) in rs.iter().enumerate() {
                let summed: f64 = phase_ms(r).iter().map(|(_, ms)| ms).sum();
                assert!(
                    (summed - r.total_ms).abs() < 1.0,
                    "trip #{i}: phases sum to {summed:.1} ms of a {:.1} ms trip",
                    r.total_ms,
                );
            }
        });
    }

    /// With both machines heard this instant there is nothing to be blind about, so the lower
    /// counter wins every draw — and lower means the queue first: a machine with six requests
    /// in progress and nobody waiting is a better target than one with a single request
    /// waiting and nothing to do, because it is the waiting that the next request joins.
    ///
    /// Each pick is counted onto the counter before it lands, which is the client telling
    /// itself what it just did.
    #[test]
    fn two_heard_takes_the_lower_fresh_counter_and_counts_its_own_sends() {
        let mut routing = Routing {
            home: 0,
            heard: vec![
                Some(Heard { load: Load { queued: 1, processed: 0 }, at: 1000.0 }),
                Some(Heard { load: Load { queued: 0, processed: 6 }, at: 1000.0 }),
            ],
            pool: vec![0, 1],
        };
        let mut rng = Rng::new(7);
        for _ in 0..8 {
            assert_eq!(two_heard(&mut routing, 1000.0, &mut rng), 1);
        }
        let heard = routing.heard[1].unwrap();
        assert_eq!(
            heard.load,
            Load { queued: 0, processed: 14 },
            "eight sends counted onto what the machine said, where the machine would put them",
        );
        assert_eq!(heard.at, 1000.0, "the client's arithmetic never refreshes the stamp");
    }

    /// A pool is `size` distinct machines; asked for more than exist, it is the fleet.
    #[test]
    fn a_pool_is_distinct_machines_capped_by_the_fleet() {
        let mut rng = Rng::new(3);
        let pool = draw_pool(10, 4, &mut rng);
        assert_eq!(pool.len(), 4);
        let mut seen = pool.clone();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), 4, "no machine twice: {pool:?}");
        assert_eq!(draw_pool(10, 99, &mut rng).len(), 10);
    }

    /// A tenant keeps its row while nothing has come back for it. The cast is fixed, so a row
    /// that appeared only once its tenant's requests finished would shift the rows under it —
    /// and the tenant slowest to come back is the one the panel is about.
    #[test]
    fn a_tenant_with_nothing_back_is_still_a_row() {
        use crate::engine::TENANTS;
        let finished = |tenant| Record {
            tenant,
            client: SOLO_CLIENT,
            policy: None,
            outcome: Outcome::Success,
            total_ms: 10.0,
            sections: HashMap::from([(Section::Cpu, 10.0)]),
            refusals: 0,
            at: 0.0,
        };
        // Only the last tenant has anything finished.
        let summary = Summary::of(vec![finished(TENANTS[2].id)]);
        let Rows::Tenants(rows) = summary.by_tenant(PROCESSING_TIME) else {
            panic!("a cut by tenant")
        };
        assert_eq!(
            rows.iter().map(|(label, ..)| label.as_str()).collect::<Vec<_>>(),
            TENANTS.iter().map(|t| t.id).collect::<Vec<_>>(),
            "every tenant keeps its row, in the cast's order",
        );
        assert_eq!(rows[0].1.p50_ms, 0.0, "nothing back reads as nothing, not as absent");
        assert!(rows[2].1.p50_ms > 0.0, "and the one that finished reads its own time");
    }


    /// The sample is long so a percentile means something, and weighted so the table still
    /// turns when the machine does. A settled batch gets neither — every request in it is part
    /// of one event, and weighting the stragglers up would report it as slower than it was.
    #[test]
    fn a_live_reading_follows_the_recent_past_and_a_settled_batch_does_not() {
        use crate::engine::TENANTS;
        let at = |ms: f64, took: f64| Record {
            tenant: TENANTS[0].id,
            client: SOLO_CLIENT,
            policy: None,
            outcome: Outcome::Success,
            total_ms: took,
            sections: HashMap::from([(Section::Cpu, took)]),
            refusals: 0,
            at: ms,
        };
        // A machine that was slow for four seconds and has been fast for the last two: still
        // outnumbered two to one by its own history.
        let mut records: Vec<Record> = (0..40).map(|i| at(i as f64 * 100.0, 400.0)).collect();
        records.extend((0..20).map(|i| at(4_000.0 + i as f64 * 100.0, 20.0)));

        let settled = Summary::of(records.clone()).total;
        let live = Summary::recent(records, HALF_LIFE_MS).total;
        assert_eq!(settled.p50_ms, 400.0, "a batch is its own history, and it was mostly slow");
        assert_eq!(live.p50_ms, 20.0, "a running machine is what it is doing now");
        assert!(
            live.mean_ms < settled.mean_ms / 2.0,
            "and the mean follows too: {:.0} ms live against {:.0} ms settled",
            live.mean_ms,
            settled.mean_ms,
        );
    }

    /// With every reading the same age, the weighted percentile is the ordinary one — so
    /// turning weighting on does not quietly move the numbers of a steady machine.
    #[test]
    fn weighting_a_sample_of_one_age_changes_nothing() {
        use crate::engine::TENANTS;
        let at = |took: f64| Record {
            tenant: TENANTS[0].id,
            client: SOLO_CLIENT,
            policy: None,
            outcome: Outcome::Success,
            total_ms: took,
            sections: HashMap::from([(Section::Cpu, took)]),
            refusals: 0,
            at: 500.0,
        };
        let records: Vec<Record> = (1..=20).map(|i| at(i as f64 * 10.0)).collect();
        let settled = Summary::of(records.clone()).total;
        let live = Summary::recent(records, HALF_LIFE_MS).total;
        assert_eq!(
            (settled.p25_ms, settled.p50_ms, settled.p95_ms),
            (live.p25_ms, live.p50_ms, live.p95_ms),
        );
        assert!((settled.mean_ms - live.mean_ms).abs() < 1e-9);
    }

    /// A tenant being refused is not a tenant whose work is cheap. Requests turned away at the
    /// door did none of it — their whole timing is the accept burst — so averaging them into a
    /// tenant's row would report a heavily limited tenant as the fastest on the panel.
    #[test]
    fn a_refused_request_is_not_a_reading_of_what_the_work_cost() {
        use crate::engine::TENANTS;
        let record = |tenant, outcome, ms| Record {
            tenant,
            client: SOLO_CLIENT,
            policy: None,
            outcome,
            total_ms: ms,
            sections: HashMap::from([(Section::Cpu, ms)]),
            refusals: 0,
            at: 0.0,
        };
        // One tenant's expensive work, and a flood of refusals for the same tenant.
        let mut records = vec![record(TENANTS[0].id, Outcome::Success, 400.0)];
        records.extend(
            (0..50).map(|_| record(TENANTS[0].id, Outcome::RateLimited, 1.0)),
        );
        let Rows::Tenants(rows) = Summary::of(records).by_tenant(PROCESSING_TIME) else {
            panic!("a cut by tenant")
        };
        assert_eq!(
            rows[0].1.p50_ms, 400.0,
            "the row reports the work that ran, not the fifty requests that never started",
        );
    }

    /// The whole batch reaches a verdict, and the four phases it is cut into add up to the time
    /// it actually took — the ledger is a partition of the round trip, not a sample of it.
    ///
    /// The waiting is a span now, from the first queue the client joined to the one it was
    /// finally let in from, so a trip that was refused pays what the refusal cost *inside* that
    /// row rather than beside it: one connection's worth of handshake sits outside the span
    /// however many the client opened, and no round trip is billed an accept burst as though it
    /// were work it had been admitted for.
    #[test]
    fn sections_partition_the_round_trip() {
        let s = settle(&mut MultiEngine::with_clients(0x5eed, CLIENTS));
        assert_eq!(s.records.len(), REQUESTS, "every request reaches a verdict");
        assert!(s.shed() > 0, "and this batch is loaded enough that some were refused first");
        for (i, r) in s.records.iter().enumerate() {
            let summed: f64 = phase_ms(r).iter().map(|(_, ms)| ms).sum();
            assert!(
                (summed - r.total_ms).abs() < 1.0,
                "request #{i}: phases sum to {summed:.1} ms but the round trip was {:.1} ms",
                r.total_ms
            );
            assert!(
                r.sections[&Section::Handshake] <= HANDSHAKE_MS,
                "request #{i}: every connection after the first was opened inside the span",
            );
            assert_eq!(
                r.sections.get(&Section::Accept),
                None,
                "request #{i}: a burst paid to be told no is time spent getting in, not work",
            );
        }
    }

    /// A retry is a queue the client keeps on its own side. Everything one costs — the wait it
    /// was refused from, the "no" coming home, the ask going out again — is inside the queue
    /// wait, so a client that had to ask three times reads as one that waited a long time.
    #[test]
    fn a_refusal_is_paid_for_in_the_queue_wait() {
        let s = settle(&mut MultiEngine::with_clients(0x5eed, CLIENTS));
        let queue = |r: &Record| -> f64 {
            QUEUE_TIME.iter().filter_map(|section| r.sections.get(section)).sum()
        };
        for r in s.records.iter().filter(|r| r.refusals > 0) {
            let legs = (r.refusals + 1) as f64 * 2.0 * NET_MS;
            assert!(
                queue(r) >= legs,
                "{} refusals is {legs:.0} ms of travel alone, but the wait reads {:.0} ms",
                r.refusals,
                queue(r),
            );
        }
    }

    /// A fleet under demand that does not stop has no last request, so its ledger is a window on
    /// the recent past rather than a history — and what the whole run came to is counted as it
    /// lands rather than walked. A batch is its own bound and keeps all of itself.
    #[test]
    fn a_flowing_ledger_is_a_window_and_a_batch_is_its_own_bound() {
        let mut fleet = flow_for(Policy::AlwaysRandom, LOADED_QPS, 4.0);
        let kept = fleet.with_records(VecDeque::len);
        let answered = fleet.answered();
        assert_eq!(kept, SAMPLE, "the window fills and stops filling");
        assert!(
            answered.trips > 4 * SAMPLE,
            "while the run's own tally keeps climbing: {} answered",
            answered.trips,
        );
        assert!(
            fleet.journeys().len() < SAMPLE,
            "and routes nobody drew are dropped with the frame that closed them",
        );

        let batch = settle(&mut MultiEngine::with_clients(0x5eed, CLIENTS));
        assert_eq!(batch.records.len(), REQUESTS, "a batch that stops asking keeps all of itself");
    }

    /// A reading belongs to the policy that *sent* it. Switching leaves requests in the air that
    /// were chosen for under the old policy and are still owed an answer under it — credited to
    /// the new one, the queue the old one had already put them in would read as the new one's
    /// doing, which is backwards for the only comparison the card exists to make.
    #[test]
    fn a_reading_belongs_to_the_policy_that_sent_it() {
        let mut fleet = flow_for(Policy::AlwaysRandom, LOADED_QPS, 2.0);
        // Nothing new goes out, so everything that lands from here was sent under the old policy.
        fleet.set_qps(0.0);
        fleet.set_policy(Policy::PowerOfTwoVirtual);
        let before = fleet.answered().trips;
        run_for(&mut fleet, 0.2);
        let landed = fleet.answered().trips - before;
        assert!(landed > 0, "the fleet still owes answers for what was in the air");
        let old = fleet.with_records(|rs| {
            rs.iter().rev().take(landed).filter(|r| r.policy == Some(Policy::AlwaysRandom)).count()
        });
        assert_eq!(old, landed, "every one of them belongs to the policy that chose its machine");
    }

    /// A refusal is a cost inside a round trip, not the way one ends. The client owns its own
    /// retry policy and keeps asking, so every request in a shedding batch comes back served —
    /// and the ones that were turned away are the slow ones, which is the whole reading the
    /// percentile sim takes.
    #[test]
    fn a_refused_request_keeps_asking_until_it_lands() {
        let s = settle(&mut MultiEngine::with_clients(0x5eed, CLIENTS));
        assert!(s.shed() > 0, "this batch is loaded enough to be refused somewhere");
        assert!(
            s.records.iter().all(|r| r.outcome.served()),
            "a client that keeps asking eventually gets an answer",
        );
        let median = |refused: bool| {
            let mut xs: Vec<f64> = s
                .records
                .iter()
                .filter(|r| (r.refusals > 0) == refused)
                .map(|r| r.total_ms)
                .collect();
            xs.sort_by(f64::total_cmp);
            xs[xs.len() / 2]
        };
        let (refused, first_try) = (median(true), median(false));
        assert!(
            refused > first_try,
            "being turned away costs the client time: {refused:.0} ms against {first_try:.0} ms",
        );
    }

    /// The shed tally counts requests, not refusals: a client turned away four times is one
    /// client hearing no, and the drop-rate curve is asking how likely that is.
    #[test]
    fn shed_counts_the_requests_refused_not_the_refusals() {
        let refused = |refusals| Record {
            tenant: crate::engine::SOLO,
            client: SOLO_CLIENT,
            policy: None,
            outcome: Outcome::Success,
            total_ms: 10.0,
            sections: HashMap::from([(Section::Cpu, 10.0)]),
            refusals,
            at: 0.0,
        };
        let s = Summary::of(vec![refused(0), refused(1), refused(9)]);
        assert_eq!(s.shed(), 2);
    }

    /// Sticky uniform routing leaves servers idle while others carry several clients —
    /// the imbalance the chapter is about.
    #[test]
    fn sticky_random_routing_leaves_the_fleet_lopsided() {
        let mut e = MultiEngine::with_clients(0x5eed, CLIENTS);
        settle(&mut e);
        // Requests per server, from the sticky client→server assignment.
        let mut per_server = vec![0usize; SERVERS];
        for &s in e.assignment() {
            per_server[s] += REQS_PER_CLIENT;
        }
        assert_eq!(per_server.iter().sum::<usize>(), REQUESTS);
        assert!(
            per_server.contains(&0),
            "some server gets no traffic at all: {per_server:?}"
        );
        let busiest = per_server.iter().max().unwrap();
        assert!(
            *busiest >= 2 * REQS_PER_CLIENT,
            "some server carries more than one client: {per_server:?}"
        );
    }

    /// A loaded fleet, snapshotted mid-batch: routing leaves some servers idle and piles
    /// others up, and the live counts read off the real state show it.
    #[test]
    fn a_loaded_fleet_snapshot_is_lopsided() {
        let mut e = MultiEngine::with_clients(0x5eed, 80);
        e.fire();
        for _ in 0..12 {
            e.tick(16.0);
        }
        let fleet = e.fleet();
        assert_eq!(fleet.len(), SERVERS);
        assert_eq!(fleet.iter().map(|f| f.load).sum::<usize>(), 80 * REQS_PER_CLIENT);
        let (lo, hi) = (
            fleet.iter().map(|f| f.load).min().unwrap(),
            fleet.iter().map(|f| f.load).max().unwrap(),
        );
        assert!(hi >= lo * 2, "routing is lopsided: {lo}..{hi}");
        assert!(fleet.iter().any(|f| f.counts.busy > 0), "a busy server has cores working");
        assert!(
            fleet.iter().any(|f| f.counts.req > 0 || f.counts.retried > 0),
            "an overloaded server has queued or shed work"
        );
    }
}
