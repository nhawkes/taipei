//! The separate service — the half of rate limiting that is not on the server.
//!
//! [`taipei::rate_limit`] gives a server two things to do with a tenant: bank what it cost,
//! and refuse a share of it. Neither says what the share should be, and neither can: a
//! server sees its own traffic, and a split decided per machine would have each machine
//! reaching a different answer about the same tenant. So this is the other side of that
//! seam — the shared store both servers talk to, and the thing that reads it each second
//! and writes back what to refuse.
//!
//! # The three seconds
//!
//! The chapter's point is the delay, and it is not incidental. Three things happen once a
//! second and each reads what the one before it left *last* second:
//!
//! 1. **write** — the blame the servers accrued goes to the store,
//! 2. **calculate** — the allocator reads the store and divides the budget,
//! 3. **read** — the servers pick up the shares and start refusing by them.
//!
//! So a tenant that starts flooding at `t` is not refused by its new share until `t + 3`.
//! [`Store::epoch`] performs all three in that one order, which is what produces the lag:
//! shifting the last stage first means each stage moves a value the stage behind it wrote
//! an epoch ago.
//!
//! # Why it predicts rather than reacts
//!
//! A loop with three seconds of lag in it will oscillate if it is steered by the last reading
//! alone, and this one oscillates especially hard, because refusing a tenant destroys the
//! evidence it is judged on: refuse it, measure almost nothing, read that as a tenant that has
//! gone away, lift the refusal, and the flood is back. So the middle stage does not divide the
//! budget between what was *measured* — it divides it between what each tenant is predicted
//! to want, from its whole record under [`RECENCY`] decay. One starved second cannot talk it
//! out of what a tenant has been doing all along.

use std::sync::Mutex;
use std::time::Duration;

use crate::engine::TENANTS;

/// One reading per tenant, in [`TENANTS`] order — the shape everything here travels in, so a
/// number can never end up beside the wrong tenant.
pub type PerTenant<T> = [T; TENANTS.len()];

/// The share of a tenant's traffic the allocator will never refuse.
///
/// Refusing *all* of a tenant is a trap, not a limit: with nothing getting through there is
/// no blame to measure, so the store stops being able to tell a tenant that went away from
/// one still hammering the door, and the estimate it is judged on stops being evidence.
/// A trickle keeps the measurement alive.
const MAX_DROP: f64 = 0.95;

/// How much less each epoch further back counts, per epoch.
///
/// The prediction is what makes the loop stable. Judged on the last epoch alone the estimate
/// flips between enormous and nothing: refuse a tenant hard and its blame falls to nearly
/// zero, which reads as a tenant that has gone away, which lifts the refusal, which lets the
/// flood back. The decay is the middle: at `0.9` the prediction turns inside a few epochs,
/// while ten-second-old readings still carry enough weight that one starved second cannot
/// talk it out of what a tenant has been doing. And because an epoch only ever fades — there
/// is no window for it to age out of — the prediction never jerks when a spike's reading
/// would have left one.
const RECENCY: f64 = 0.9;

/// What the allocator worked out for one tenant — the row the panel draws, and the drop
/// percentage the servers will eventually enforce.
#[derive(Clone, Copy, Default, PartialEq)]
pub struct Share {
    /// What this tenant is predicted to want next epoch, had nothing been refused — the
    /// decayed read of its whole record, not of the second just gone.
    pub estimated: Duration,
    /// What the budget gave it.
    pub granted: Duration,
    pub drop_pct: f64,
}

/// One epoch of the store, as the panel reads it: what is being billed right now, what the
/// last write left, what the allocator made of it, and what the servers are enforcing. The
/// three seconds are these four columns, so the lag is something a reader can see rather
/// than something the prose has to assert.
#[derive(Clone, Default)]
pub struct Snapshot {
    pub accruing: PerTenant<Duration>,
    pub written: PerTenant<Duration>,
    pub calculated: PerTenant<Share>,
    pub enforced: PerTenant<f64>,
}

/// The budget the fleet divides, per epoch — the prose's default.
pub const DEFAULT_BUDGET: Duration = Duration::from_millis(50);

/// How long one turn of the loop takes, in virtual milliseconds. Every stage moves on this
/// beat, so "three epochs to react" is three of these.
pub const EPOCH_MS: f64 = 1000.0;

struct Inner {
    /// Blame the servers are banking right now, and the shares in force while they bank it —
    /// the pair is what makes the estimate possible, so they are taken together.
    accruing: PerTenant<Duration>,
    written: PerTenant<Duration>,
    written_under: PerTenant<f64>,
    calculated: PerTenant<Share>,
    enforced: PerTenant<f64>,
    /// What each tenant is estimated to have wanted, epoch by epoch under [`RECENCY`] decay —
    /// the record the service predicts the coming second from. In seconds, which is what the
    /// arithmetic wants; the panel is the one that reads in milliseconds.
    demand: PerTenant<Demand>,
    weights: PerTenant<f64>,
    /// What the fleet will spend per epoch, or `None` while it is not rate limiting.
    budget: Option<Duration>,
}

/// The store both servers talk to: they bank blame into it, and read their shares back out.
///
/// One store, not one per server — that is the whole reason it exists. A tenant's share is a
/// statement about what it is doing to the *fleet*.
pub struct Store(Mutex<Inner>);

#[cfg(test)]
impl Store {
    /// Set what was being refused while the last epoch's blame was measured. Only a test can
    /// say this directly — in the running loop it is whatever the servers were enforcing.
    fn refusing_while_measured(&self, tenant: usize, drop_pct: f64) {
        if let Ok(mut inner) = self.0.lock() {
            inner.written_under[tenant] = drop_pct;
        }
    }
}

impl Default for Store {
    fn default() -> Self {
        Store::new()
    }
}

impl Store {
    /// A store at rest: nothing billed, nothing refused, no budget to divide until it is given
    /// one, and the weights split evenly.
    pub fn new() -> Store {
        Store(Mutex::new(Inner {
            accruing: [Duration::ZERO; TENANTS.len()],
            written: [Duration::ZERO; TENANTS.len()],
            written_under: [0.0; TENANTS.len()],
            calculated: [Share::default(); TENANTS.len()],
            enforced: [0.0; TENANTS.len()],
            demand: [Demand::default(); TENANTS.len()],
            weights: [1.0 / TENANTS.len() as f64; TENANTS.len()],
            budget: None,
        }))
    }

    /// One turn of the loop. The stages move back to front — the servers pick up what was
    /// calculated last epoch, the allocator reads what was written last epoch, and only then
    /// does this epoch's blame become what was written. Each stage therefore moves a value
    /// its predecessor left an epoch ago, which is the three seconds.
    pub fn epoch(&self) {
        let Ok(mut inner) = self.0.lock() else { return };
        // What was in force while the blame about to be written was being measured. Read
        // before the shares move, because the shift below is what makes it the past.
        let was_enforced = inner.enforced;

        inner.enforced = std::array::from_fn(|k| inner.calculated[k].drop_pct);
        // What the blame just read implies each tenant *wanted*: the measurement, divided by
        // the fraction that was let through to produce it. Onto the front of its window.
        for k in 0..TENANTS.len() {
            let admitted = (1.0 - inner.written_under[k].clamp(0.0, MAX_DROP)).max(1.0 - MAX_DROP);
            let wanted = inner.written[k].as_secs_f64() / admitted;
            inner.demand[k].observe(wanted);
        }
        let predicted = std::array::from_fn(|k| inner.demand[k].predict());
        inner.calculated = allocate(&predicted, &inner.weights, inner.budget);
        inner.written = std::mem::replace(&mut inner.accruing, [Duration::ZERO; TENANTS.len()]);
        inner.written_under = was_enforced;
    }

    /// Forget everything measured, keeping the budget and the priorities the reader has set.
    ///
    /// The servers can be replaced; the store cannot, because they hold it. Without this a
    /// reset would hand the reader fresh machines that go on being refused by a window full of
    /// what the *old* ones did — the one state on the panel that survives its own reset.
    pub fn clear(&self) {
        let Ok(mut inner) = self.0.lock() else { return };
        inner.accruing = [Duration::ZERO; TENANTS.len()];
        inner.written = [Duration::ZERO; TENANTS.len()];
        inner.written_under = [0.0; TENANTS.len()];
        inner.calculated = [Share::default(); TENANTS.len()];
        inner.enforced = [0.0; TENANTS.len()];
        inner.demand = [Demand::default(); TENANTS.len()];
    }

    /// Move the fleet's budget — the total shut-time per epoch the tenants are dividing, or
    /// `None` to stop rate limiting. The store goes on measuring either way.
    pub fn set_budget(&self, budget: Option<Duration>) {
        if let Ok(mut inner) = self.0.lock() {
            inner.budget = budget;
        }
    }

    /// Move the priorities. Whatever they arrive as, what the allocator divides by is each
    /// one's share of their total — a priority only means anything against the others.
    pub fn set_weights(&self, weights: PerTenant<f64>) {
        if let Ok(mut inner) = self.0.lock() {
            inner.weights = normalised(&weights);
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        let Ok(inner) = self.0.lock() else { return Snapshot::default() };
        Snapshot {
            accruing: inner.accruing,
            written: inner.written,
            calculated: inner.calculated,
            enforced: inner.enforced,
        }
    }
}

impl taipei::rate_limit::Limits for Store {
    fn write_blame(&self, tenant: &str, blame: Duration) {
        let Some(k) = TENANTS.iter().position(|t| t.id == tenant) else { return };
        if let Ok(mut inner) = self.0.lock() {
            inner.accruing[k] += blame;
        }
    }

    fn drop_pct(&self, tenant: &str) -> f64 {
        let Some(k) = TENANTS.iter().position(|t| t.id == tenant) else { return 0.0 };
        self.0.lock().map(|inner| inner.enforced[k]).unwrap_or(0.0)
    }
}

/// Priorities as shares of their total. All-zero (or worse) divides evenly rather than by
/// zero — a reader who drags every priority to the bottom has said nothing about who matters
/// more, not that nobody may be served.
fn normalised(weights: &PerTenant<f64>) -> PerTenant<f64> {
    let total: f64 = weights.iter().map(|w| w.max(0.0)).sum();
    match total > 0.0 {
        true => std::array::from_fn(|k| weights[k].max(0.0) / total),
        false => [1.0 / TENANTS.len() as f64; TENANTS.len()],
    }
}

/// What one tenant has wanted, every epoch of it, each reading counting [`RECENCY`] times
/// less than the one after it. Held as the weighted mean's running numerator and denominator,
/// so taking a reading in is one multiply-and-add and an old epoch never leaves the record —
/// it only fades.
#[derive(Clone, Copy, Default)]
struct Demand {
    sum: f64,
    weight: f64,
}

impl Demand {
    /// Take this epoch's reading onto the front of the record.
    fn observe(&mut self, wanted: f64) {
        self.sum = wanted + RECENCY * self.sum;
        self.weight = 1.0 + RECENCY * self.weight;
    }

    /// What the tenant is expected to want next epoch. A record that has seen nothing
    /// predicts nothing.
    ///
    /// A plain mean would take half a minute to notice a tenant that has started flooding;
    /// the last reading alone would forget one the moment a refusal starved it. The decay is
    /// the middle: recent seconds decide the answer, older ones stop it being talked out of
    /// what it has seen.
    fn predict(&self) -> f64 {
        match self.weight > 0.0 {
            true => self.sum / self.weight,
            false => 0.0,
        }
    }
}

/// Divide `budget` between the tenants by weighted fair share, and say what fraction of each
/// one's traffic that means refusing.
///
/// A weight buys a floor, not a ceiling. A tenant is first offered its weighted slice, and a
/// tenant wanting less than its slice takes what it wants and its remainder goes back into
/// the pot for the others — repeatedly, until everyone left is asking for more than their
/// share of what is left. So nobody is refused while the fleet is inside its budget, however
/// lopsided the traffic, which is the promise the chapter makes.
///
/// What the budget is divided between is the **prediction** — each tenant's
/// [`Demand::predict`], in seconds — not the last epoch's reading. The difference is the
/// difference between a loop that settles and one that oscillates.
///
/// No budget is not a budget of zero: it is the fleet not rate limiting at all. Demand is still
/// estimated — the store goes on watching what the tenants are costing — but every tenant is
/// granted what it asks for and nobody is refused.
pub fn allocate(
    demand: &PerTenant<f64>,
    weights: &PerTenant<f64>,
    budget: Option<Duration>,
) -> PerTenant<Share> {
    let weights = normalised(weights);

    let Some(budget) = budget else {
        return std::array::from_fn(|k| {
            let wanted = Duration::from_secs_f64(demand[k].max(0.0));
            Share { estimated: wanted, granted: wanted, drop_pct: 0.0 }
        });
    };

    let mut granted = [0.0f64; TENANTS.len()];
    let mut open: Vec<usize> = (0..TENANTS.len()).collect();
    let mut left = budget.as_secs_f64();
    while !open.is_empty() {
        let total: f64 = open.iter().map(|&k| weights[k]).sum();
        let slice = |k: usize| match total > 0.0 {
            true => left * weights[k] / total,
            false => 0.0,
        };
        // Whoever is asking for less than their slice is settled at what they asked for, and
        // what they did not want goes back to the pot the rest divide again.
        let settled: Vec<usize> = open.iter().copied().filter(|&k| demand[k] <= slice(k)).collect();
        if settled.is_empty() {
            for &k in &open {
                granted[k] = slice(k);
            }
            break;
        }
        for k in settled {
            granted[k] = demand[k];
            left -= demand[k];
            open.retain(|&o| o != k);
        }
    }

    std::array::from_fn(|k| Share {
        estimated: Duration::from_secs_f64(demand[k]),
        granted: Duration::from_secs_f64(granted[k].max(0.0)),
        drop_pct: match demand[k] > 0.0 {
            true => (1.0 - granted[k] / demand[k]).clamp(0.0, MAX_DROP),
            false => 0.0,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use taipei::rate_limit::Limits;

    const EVEN: PerTenant<f64> = [1.0; TENANTS.len()];

    /// Predictions in which each tenant has settled at wanting `v[k]` milliseconds — a
    /// settled fleet, which is what most of these tests are about.
    fn steady(v: [f64; TENANTS.len()]) -> PerTenant<f64> {
        std::array::from_fn(|k| v[k] / 1000.0)
    }

    fn budget() -> Option<Duration> {
        Some(DEFAULT_BUDGET)
    }

    fn drops(shares: &PerTenant<Share>) -> Vec<f64> {
        shares.iter().map(|s| (s.drop_pct * 1000.0).round() / 1000.0).collect()
    }

    /// The promise the chapter makes: a fleet inside its budget refuses nobody, however
    /// lopsidedly the traffic is split.
    #[test]
    fn nothing_is_refused_under_budget() {
        let shares = allocate(&steady([44.0, 1.0, 3.0]), &EVEN, budget());
        assert_eq!(drops(&shares), vec![0.0, 0.0, 0.0]);
    }

    /// Over budget, what gets through is the budget — the grants are what the fleet can
    /// afford, not what the tenants asked for.
    #[test]
    fn grants_never_exceed_the_budget() {
        let shares = allocate(&steady([120.0, 90.0, 30.0]), &EVEN, budget());
        // To the microsecond: the grants are divided in floating point and handed back as
        // `Duration`, so the total can land a nanosecond either side of the budget.
        let granted: Duration = shares.iter().map(|s| s.granted).sum();
        assert!(
            granted.abs_diff(DEFAULT_BUDGET) < Duration::from_micros(1),
            "granted {granted:?} against a budget of {DEFAULT_BUDGET:?} — all of it, and no more",
        );
    }

    /// Rate limiting turned off is not a budget of nothing. However far over any budget the
    /// tenants are, none of them is refused — and the store is still watching what they want.
    #[test]
    fn no_budget_refuses_nobody() {
        let shares = allocate(&steady([5000.0, 90.0, 30.0]), &EVEN, None);
        assert_eq!(drops(&shares), vec![0.0, 0.0, 0.0], "nothing is refused with no budget");
        assert!(
            shares.iter().all(|s| s.granted == s.estimated),
            "and every tenant is granted what it wants",
        );
        assert!(shares[0].estimated > Duration::ZERO, "demand is still estimated");
    }

    /// A quiet tenant is never refused, and the headroom it is not using reaches the tenants
    /// that are — a fair share is a floor, not a ceiling.
    #[test]
    fn a_quiet_tenants_headroom_reaches_the_busy_ones() {
        let alone = allocate(&steady([120.0, 90.0, 30.0]), &EVEN, budget());
        let donated = allocate(&steady([120.0, 90.0, 1.0]), &EVEN, budget());
        assert_eq!(donated[2].drop_pct, 0.0, "the quiet tenant is not refused");
        assert!(
            donated[0].drop_pct < alone[0].drop_pct && donated[1].drop_pct < alone[1].drop_pct,
            "and its unused share eases the two that are over: {:?} against {:?}",
            drops(&donated),
            drops(&alone),
        );
    }

    /// The priority knob does what it says: more of the budget, so less refused.
    #[test]
    fn a_higher_priority_is_refused_less() {
        let demand = steady([90.0, 90.0, 90.0]);
        let level = allocate(&demand, &EVEN, budget());
        let favoured = allocate(&demand, &[3.0, 1.0, 1.0], budget());
        assert!(favoured[0].drop_pct < level[0].drop_pct);
        assert!(favoured[1].drop_pct > level[1].drop_pct, "paid for by the others");
    }

    /// The estimate is the point of measuring under a limit at all: a tenant already being
    /// refused is judged on what it would have cost, not on the trickle that got through.
    #[test]
    fn demand_is_estimated_through_the_share_already_refused() {
        // A tenant refused three quarters, billing 15 ms of the quarter that got through.
        let store = Store::new();
        for _ in 0..30 {
            store.write_blame(TENANTS[0].id, Duration::from_millis(15));
            store.write_blame(TENANTS[1].id, Duration::from_millis(15));
            store.epoch();
            store.refusing_while_measured(0, 0.75);
        }
        let calculated = store.snapshot().calculated;
        assert!(
            calculated[0].estimated.abs_diff(Duration::from_millis(60)) < Duration::from_millis(2),
            "15 ms of a quarter is 60, not 15: {:?}",
            calculated[0].estimated,
        );
        assert!(
            calculated[1].estimated.abs_diff(Duration::from_millis(15)) < Duration::from_millis(2),
            "nothing refused, nothing to undo: {:?}",
            calculated[1].estimated,
        );
    }

    /// The prediction leans on recent seconds without forgetting the record. A tenant that has
    /// just gone quiet is not immediately believed — which is what stops a refusal that starves
    /// its own evidence from lifting itself a second later, and the loop from oscillating.
    #[test]
    fn the_prediction_weights_recent_history_higher() {
        let mut demand = Demand::default();
        for _ in 0..30 {
            demand.observe(1.0);
        }
        assert!((demand.predict() - 1.0).abs() < 1e-9, "a steady tenant predicts its steady rate");

        let mut just_quiet = demand;
        just_quiet.observe(0.0);
        let after_one = just_quiet.predict();
        assert!(after_one < 1.0, "the quiet second is heard");
        assert!(after_one > 0.85, "but one second does not erase thirty: {after_one:.3}");

        // Sustained quiet does win, and within a few epochs rather than thirty.
        for _ in 0..5 {
            just_quiet.observe(0.0);
        }
        assert!(just_quiet.predict() < after_one, "and it keeps falling as the quiet holds");
    }

    /// The record has no edge for history to fall off: once the fleet has been watched long
    /// enough for the weights to settle, a spike's influence shrinks by the same fraction
    /// every epoch — there is no epoch at which it suddenly stops counting, so the refusal
    /// built on it cannot jump.
    #[test]
    fn a_spike_fades_instead_of_dropping_out() {
        let mut demand = Demand::default();
        for _ in 0..60 {
            demand.observe(0.0);
        }
        demand.observe(5.0);
        let mut prev = demand.predict();
        for _ in 0..60 {
            demand.observe(0.0);
            let next = demand.predict();
            assert!(next < prev, "the spike keeps fading: {next} after {prev}");
            assert!(next > prev * 0.85, "and never falls off an edge: {next} after {prev}");
            prev = next;
        }
    }

    /// The whole reason the prediction exists. Left to the last epoch alone the loop flips —
    /// refuse hard, measure nothing, lift the refusal, flood — so a tenant hammering at a
    /// steady rate must reach a steady refusal and stay there.
    #[test]
    fn a_steady_flood_settles_instead_of_oscillating() {
        let store = Store::new();
        store.set_budget(budget());
        store.set_weights(EVEN);
        let mut seen = Vec::new();
        for epoch in 0..60 {
            // The tenant asks for the same thing every second; what it gets billed for is what
            // the current refusal lets through, which is exactly the feedback that oscillates.
            let admitted = 1.0 - store.drop_pct(TENANTS[0].id);
            store.write_blame(TENANTS[0].id, Duration::from_secs_f64(0.400 * admitted));
            store.epoch();
            if epoch >= 40 {
                seen.push(store.drop_pct(TENANTS[0].id));
            }
        }
        let (lo, hi) = (
            seen.iter().copied().fold(f64::MAX, f64::min),
            seen.iter().copied().fold(0.0, f64::max),
        );
        assert!(hi - lo < 0.05, "the refusal settles rather than swinging: {lo:.2}..{hi:.2}");
        assert!(hi > 0.5, "and it does settle on refusing a flood this far over: {hi:.2}");
    }

    /// No tenant is refused outright, however far over it is. With nothing getting through
    /// there is no blame to measure, and the store would lose the ability to tell a tenant
    /// that stopped from one still hammering.
    #[test]
    fn a_trickle_always_gets_through() {
        let shares = allocate(&steady([5000.0, 1.0, 1.0]), &[0.0, 1.0, 1.0], budget());
        assert!(shares[0].drop_pct <= MAX_DROP);
        assert!(shares[0].drop_pct > 0.9, "but it is refused hard: {:?}", drops(&shares));
    }

    /// A reader who drags every priority to the bottom has said nothing about who matters
    /// more — not that nobody may be served.
    #[test]
    fn priorities_at_zero_divide_evenly() {
        let demand = steady([90.0, 90.0, 90.0]);
        assert_eq!(
            drops(&allocate(&demand, &[0.0, 0.0, 0.0], budget())),
            drops(&allocate(&demand, &EVEN, budget())),
        );
    }

    /// Blame written now is not acted on for three epochs: one to be written, one to be
    /// divided, one to be picked up. This is the chapter's whole lesson, so it is pinned.
    #[test]
    fn blame_takes_three_epochs_to_reach_the_servers() {
        let store = Store::new();
        store.set_budget(budget());
        store.set_weights(EVEN);
        // Far over budget, so whatever the allocator makes of it cannot be "refuse nothing".
        for _ in 0..40 {
            store.write_blame("Alice", Duration::from_millis(10));
        }
        assert_eq!(store.drop_pct("Alice"), 0.0, "nothing is enforced from blame not yet written");

        store.epoch();
        assert_eq!(store.drop_pct("Alice"), 0.0, "written, not yet divided");
        assert_eq!(store.snapshot().written[0], Duration::from_millis(400));

        store.epoch();
        assert_eq!(store.drop_pct("Alice"), 0.0, "divided, not yet picked up");
        assert!(store.snapshot().calculated[0].drop_pct > 0.0);

        store.epoch();
        assert!(store.drop_pct("Alice") > 0.0, "and on the third epoch the servers refuse by it");
    }

    /// The switch the reader arrives at: a store that has not been given a budget measures
    /// everything and refuses nothing, and giving it one starts the same three-epoch walk.
    #[test]
    fn a_store_with_no_budget_measures_but_never_refuses() {
        let store = Store::new();
        store.set_weights(EVEN);
        for _ in 0..12 {
            store.write_blame("Alice", Duration::from_millis(400));
            store.epoch();
        }
        assert_eq!(store.drop_pct("Alice"), 0.0, "nothing is refused while nothing is enforcing");
        assert!(
            store.snapshot().written[0] > Duration::ZERO,
            "but the blame was measured all along",
        );

        store.set_budget(budget());
        for _ in 0..3 {
            store.write_blame("Alice", Duration::from_millis(400));
            store.epoch();
        }
        assert!(store.drop_pct("Alice") > 0.0, "and turning it on starts refusing");
    }

    /// Each epoch bills only its own traffic — blame written is blame taken off the counter,
    /// so a busy second cannot go on being paid for in the quiet seconds after it.
    #[test]
    fn an_epochs_blame_is_written_once() {
        let store = Store::new();
        store.write_blame("Bob", Duration::from_millis(7));
        store.epoch();
        assert_eq!(store.snapshot().written[1], Duration::from_millis(7));
        assert_eq!(store.snapshot().accruing[1], Duration::ZERO);
        store.epoch();
        assert_eq!(store.snapshot().written[1], Duration::ZERO);
    }

    /// A reset hands the reader fresh servers. The store is the one thing they cannot replace,
    /// so it has to forget on request — otherwise the new machines are refused by a window
    /// describing the old ones, and the panel opens mid-throttle for no visible reason.
    #[test]
    fn clearing_forgets_the_measurements_and_keeps_the_settings() {
        let store = Store::new();
        store.set_budget(Some(Duration::from_millis(120)));
        store.set_weights([3.0, 1.0, 1.0]);
        // Far past a 120 ms budget, so it settles on refusing hard.
        for _ in 0..10 {
            store.write_blame(TENANTS[0].id, Duration::from_millis(400));
            store.epoch();
        }
        assert!(store.drop_pct(TENANTS[0].id) > 0.0, "it was refusing");

        store.clear();
        assert_eq!(store.drop_pct(TENANTS[0].id), 0.0, "and now refuses nobody");
        let seen = store.snapshot();
        assert_eq!(seen.written, [Duration::ZERO; TENANTS.len()]);
        assert_eq!(seen.accruing, [Duration::ZERO; TENANTS.len()]);
        assert_eq!(seen.calculated[0].estimated, Duration::ZERO, "the window is empty too");

        // The reader's own settings are not measurements, and survive.
        for _ in 0..3 {
            store.write_blame(TENANTS[0].id, Duration::from_millis(400));
            store.write_blame(TENANTS[1].id, Duration::from_millis(400));
            store.epoch();
        }
        assert!(
            store.drop_pct(TENANTS[0].id) < store.drop_pct(TENANTS[1].id),
            "the priority set before the reset is still favouring the first tenant",
        );
    }

    /// The store answers about the tenants it knows and shrugs at the rest, rather than
    /// refusing traffic it has no share for.
    #[test]
    fn an_unknown_tenant_is_not_limited() {
        let store = Store::new();
        store.write_blame("nobody", Duration::from_millis(500));
        store.epoch();
        store.epoch();
        store.epoch();
        assert_eq!(store.drop_pct("nobody"), 0.0);
        assert_eq!(store.snapshot().written, [Duration::ZERO; TENANTS.len()]);
    }
}
