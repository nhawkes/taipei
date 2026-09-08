//! Attributing shut-time — wall-clock while the admission gate is shut, the server saturated and
//! making callers wait — to the requests *occupying* the server, split evenly among them and
//! accumulated per tenant. It blames the causers (the occupiers), never the waiters.
//!
//! It is placed as a tower [`Layer`] BELOW the queue, wrapping the gate: a request is an
//! "occupier" — counted in `N` — from admission ([`Service::call`]) to completion. Requests still
//! waiting in the queue sit ABOVE this layer and are never counted, so they never pay.
//!
//! **The gate is read from the tower protocol, not from a handle.** [`Service::poll_ready`] is
//! where a caller is told whether the server will take more work, so a `Pending` there *is* the
//! gate shut, and the span until it returns `Ready` *is* the shut-time. Nothing else has to be
//! instrumented and nothing is inferred from how long a handler ran: a busy server whose gate never
//! shuts mints nothing at all, because occupying a server nobody is waiting on costs no one
//! anything.
//!
//! Since the span is observed at discrete polls rather than continuously, it is **flushed** into the
//! accounting at every point where the numbers could otherwise go stale — each `poll_ready`, and
//! each change of membership ([`Accumulator::enter`] / [`Accumulator::leave`]). Flushing on
//! membership is what makes the split honest: the elapsed shut-time is banked against the `N` that
//! was actually present for it, so a request that arrives mid-span pays only from where it joined,
//! and a span that runs on with nobody left is unattributable rather than billed to whoever comes
//! next.
//!
//! Each flush winds on one shared **meter**, the way a taxi's runs while the cab sits in traffic:
//! `rate` accrues `dt / N`, so a span is divided evenly among everyone riding — collective
//! attribution, since which co-tenant is individually to blame is unknowable. A request reads the
//! meter when it gets in and, after each poll of its own future, **draws** what the meter has moved
//! since: its share, clamped to what the measured whole still has room for, added to its running
//! fare. Drawing repeatedly rather than once at the end keeps the clamp local — the residual is
//! checked against the pool as it is taken, not reconciled at exit — and it costs nothing while the
//! gate has been open, because then the meter has not moved.
//!
//! Three totals are kept: the **accumulated** whole (`Σ dt` over shut spans that had an occupier),
//! the **attributed** blame actually drawn, and the **unattributed** shut-time that elapsed while
//! the server held *no* occupier — no admitted request to blame, so it goes into a bucket of its own
//! rather than vanishing or landing on a waiter. Attribution is clamped so attributed can never
//! exceed accumulated, absorbing the fixed-point division's rounding slop.
//!
//! Everything is counted in **nanoseconds**, which is what the clocks measure in and leaves the
//! arithmetic with one rounding step: `dt / N`, losing under a nanosecond per flush. A draw is then
//! a plain subtraction, so how often a request draws provably cannot change what it owes. The meter
//! only ever winds forward, so like any odometer it eventually rolls over and a draw is a
//! `wrapping_sub` — a saturating subtraction would drop every occupier's fare across the roll —
//! though at nanosecond units the u64 does not roll until ~584 years of continuous saturation.
//!
//! **Domain: one caller.** The shut span is stamped by whoever polls `poll_ready`, so this layer
//! expects the single caller it was built for — the [`QueueWorker`](crate::queue) below which it
//! sits, whose `service.ready()` races the shed deadline. Two callers concurrently polling their own
//! clones would each stamp the same wall-clock and mint it twice.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use pin_project_lite::pin_project;
use tower::{Layer, Service};

// The ambient monotonic clock, aliased exactly as the queue does it: the tokio runtime clock on a
// real server, the simulation's virtual clock under `virtual-clock`/wasm.
#[cfg(not(any(target_arch = "wasm32", feature = "virtual-clock")))]
use tokio::time::Instant;
#[cfg(any(target_arch = "wasm32", feature = "virtual-clock"))]
use crate::clock::Instant;

/// The word [`Gate::Open`] encodes to — reserved, never a stamp. See [`Gate`].
const OPEN: u64 = u64::MAX;

/// The admission gate, as [`Accumulator::gate`] holds it.
///
/// One word has to carry both *whether* a span is running and *since when*, so that a single swap
/// settles the two together and the flush stays exactly-once. That word and this enum are the same
/// thing in two representations: [`decode`](Self::decode) reads every `u64` as exactly one `Gate`,
/// [`encode`](Self::encode) is its inverse, and `Gate::decode(w).encode() == w` for every `w` —
/// asserted in the tests. So no state the atomic can hold is unrepresentable or misread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Gate {
    /// Admitting — no span is running.
    Open,
    /// Refusing, with a span running whose elapsed time was last banked at this stamp (nanos from
    /// [`Accumulator::epoch`]).
    Shut { since: u64 },
}

impl Gate {
    fn decode(word: u64) -> Gate {
        match word {
            OPEN => Gate::Open,
            since => Gate::Shut { since },
        }
    }

    fn encode(self) -> u64 {
        match self {
            Gate::Open => OPEN,
            // The reserved word belongs to `Open`, so a stamp that lands on it takes the nanosecond
            // below instead of decoding back as an open gate. Only reachable ~584 years in.
            Gate::Shut { since } => since.min(OPEN - 1),
        }
    }
}

/// A request that carries the tenant it is billed to.
pub trait Tenant {
    fn tenant(&self) -> &str;
}

// ── the shared accumulator ─────────────────────────────────────────────────────

struct Accumulator {
    /// The shared meter, in nanos: `Σ (dt / N)` over flushed shut spans. Read as a wrapping delta
    /// between draws.
    rate: AtomicU64,
    /// Occupiers on the server right now — the divisor a span is shared across.
    inflight: AtomicU64,
    /// Total measured shut-time that had someone to bill (`Σ dt`) — the whole that drawn blame
    /// sums to.
    accumulated: AtomicU64,
    /// Blame actually drawn by occupiers. Held ≤ [`accumulated`](Self::accumulated) at all times by
    /// [`attribute`](Self::attribute), so rounding can never bill past the whole.
    attributed: AtomicU64,
    /// Shut-time that elapsed with no occupier — time no admitted request can be blamed for.
    unattributed: AtomicU64,
    /// The gate, as the word a [`Gate`] encodes to.
    gate: AtomicU64,
    /// The fixed reference the counters measure from — an opaque monotonic [`Instant`] becomes the
    /// `u64` the atomics hold as nanos since here.
    epoch: Instant,
}

impl Accumulator {
    /// Nanos of `t` measured from [`epoch`](Self::epoch) — a stamp, in the form the atomics hold.
    fn nanos(&self, t: Instant) -> u64 {
        t.duration_since(self.epoch).as_nanos() as u64
    }

    fn now(&self) -> u64 {
        self.nanos(Instant::now())
    }

    /// Bank the shut span elapsed since it was last banked, leaving it running from `now`. The
    /// swap-or-retry is what makes it exactly-once: the flusher that wins the [`gate`](Self::gate) owns
    /// that segment of wall-clock and is the only one to accrue it, so concurrent flushes divide the
    /// span between them rather than each banking the whole of it.
    fn flush(&self, now: u64) {
        let mut word = self.gate.load(Relaxed);
        loop {
            let Gate::Shut { since } = Gate::decode(word) else { return };
            let restamped = Gate::Shut { since: now }.encode();
            match self.gate.compare_exchange_weak(word, restamped, Relaxed, Relaxed) {
                Ok(_) => return self.accrue(Duration::from_nanos(now.saturating_sub(since))),
                Err(observed) => word = observed,
            }
        }
    }

    /// The gate just refused a caller: start a span if one is not already running. Restamping here
    /// would discard the elapsed span, so only the open→shut edge writes.
    fn shut(&self, now: u64) {
        let started = Gate::Shut { since: now }.encode();
        let _ = self.gate.compare_exchange(Gate::Open.encode(), started, Relaxed, Relaxed);
    }

    /// The gate just admitted: bank the final segment and stop the clock, in one swap so no flush
    /// can slip between the two and re-open the span.
    fn open(&self, now: u64) {
        if let Gate::Shut { since } = Gate::decode(self.gate.swap(Gate::Open.encode(), Relaxed)) {
            self.accrue(Duration::from_nanos(now.saturating_sub(since)));
        }
    }

    /// Fold one banked span into the accounting: share it across the occupiers present. The only
    /// rounding in the whole scheme is this division — under a nanosecond per flush, which falls out
    /// as slack between attributed and accumulated. `checked_div` by the occupier count is the
    /// parse: `None` is exactly the no-occupier case, whose span has no one to bill and is
    /// unattributable.
    fn accrue(&self, dt: Duration) {
        let dt = dt.as_nanos() as u64;
        match dt.checked_div(self.inflight.load(Relaxed)) {
            Some(share) => {
                self.rate.fetch_add(share, Relaxed);
                self.accumulated.fetch_add(dt, Relaxed);
            }
            None => {
                self.unattributed.fetch_add(dt, Relaxed);
            }
        }
    }

    /// Credit `blame` to the attributed total, clamped so it never exceeds the accumulated whole,
    /// and return the amount actually credited (what the drawer is billed). Under clean operation
    /// the whole has room for every draw and nothing is clamped; the clamp only bites on the
    /// fixed-point division's rounding, or a racing draw. The CAS re-reads the whole each turn, so
    /// the invariant `attributed ≤ accumulated` holds without a lock.
    fn attribute(&self, blame: Duration) -> Duration {
        let blame = blame.as_nanos() as u64;
        let mut attributed = self.attributed.load(Relaxed);
        loop {
            let room = self.accumulated.load(Relaxed).saturating_sub(attributed);
            let credit = blame.min(room);
            if credit == 0 {
                return Duration::ZERO;
            }
            match self.attributed.compare_exchange_weak(
                attributed,
                attributed + credit,
                Relaxed,
                Relaxed,
            ) {
                Ok(_) => return Duration::from_nanos(credit),
                Err(observed) => attributed = observed,
            }
        }
    }

    /// Join the occupier set and return the meter reading to start drawing from. The flush comes
    /// first, so the span so far is banked against the membership that actually served it — the
    /// entrant does not pay for shut-time that elapsed before it arrived.
    fn enter(&self) -> u64 {
        self.flush(self.now());
        self.inflight.fetch_add(1, Relaxed);
        self.rate.load(Relaxed)
    }

    /// Leave the occupier set, banking the span up to this moment first so the departing request is
    /// still counted in the `N` that shared it. What that flush mints is the last thing it draws.
    fn leave(&self) {
        self.flush(self.now());
        self.inflight.fetch_sub(1, Relaxed);
    }
}

// ── the per-request handle ─────────────────────────────────────────────────────

/// One occupier's bill. It draws its share of the shared rate after each poll of its future, and on
/// the first of (its future resolves) or (its future is dropped / cancelled) reports the total to
/// its tenant exactly once and leaves the occupier set.
pub struct Blame<R: Fn(&str, Duration)> {
    acc: Arc<Accumulator>,
    report: R,
    tenant: String,
    /// The meter reading this request has drawn up to — a raw counter rather than a span, because
    /// the delta to the current reading is taken with wrapping arithmetic no duration type offers.
    seen: u64,
    /// The fare so far — the figure reported at exit.
    owed: Duration,
    reported: bool,
}

impl<R: Fn(&str, Duration)> Blame<R> {
    /// Take the share minted since the last draw. The subtraction is exact — all the rounding
    /// happened in [`Accumulator::accrue`] — so drawing on every poll bills exactly what drawing
    /// once at exit would.
    ///
    /// Wrapping, not saturating: the meter only winds forward, and a single draw is far below 2^64,
    /// so the wrapping delta is the exact share either side of a roll-over.
    fn draw(&mut self) {
        let rate = self.acc.rate.load(Relaxed);
        let share = rate.wrapping_sub(self.seen);
        if share == 0 {
            return;
        }
        self.seen = rate;
        self.owed += self.acc.attribute(Duration::from_nanos(share));
    }

    fn finalize(&mut self) {
        if self.reported {
            return;
        }
        self.reported = true;
        // Leave first: the flush inside it banks the running span against the membership that still
        // includes this request, and the draw that follows takes the share that flush just minted.
        self.acc.leave();
        self.draw();
        (self.report)(&self.tenant, self.owed);
    }
}

impl<R: Fn(&str, Duration)> Drop for Blame<R> {
    fn drop(&mut self) {
        self.finalize();
    }
}

// ── the public handle ──────────────────────────────────────────────────────────

/// Shares one [`Accumulator`]; mint layers or admit requests directly. Clones share the accounting.
#[derive(Clone)]
pub struct TenantReporter(Arc<Accumulator>);

impl Default for TenantReporter {
    fn default() -> Self {
        Self::new()
    }
}

impl TenantReporter {
    /// Time is read from the ambient monotonic clock ([`Instant::now`]); `epoch` fixes the zero the
    /// counters measure from, so on `virtual-clock`/wasm this must be constructed within a
    /// `crate::clock::Clock::enter` scope, exactly as the queue expects.
    pub fn new() -> Self {
        let epoch = Instant::now();
        TenantReporter(Arc::new(Accumulator {
            rate: AtomicU64::new(0),
            inflight: AtomicU64::new(0),
            accumulated: AtomicU64::new(0),
            attributed: AtomicU64::new(0),
            unattributed: AtomicU64::new(0),
            gate: AtomicU64::new(Gate::Open.encode()),
            epoch,
        }))
    }

    /// A [`Blame`] handle for a request entering the occupier set — the direct path the
    /// [`TenantReportLayer`] uses, and the one a test or a hand-built stack can drive. `report`
    /// fires once at completion, from the handle's `Drop` — which can run mid-unwind on a
    /// cancelled or panicking request — so it must not itself panic (a bare per-tenant write).
    pub fn admit<R: Fn(&str, Duration)>(&self, tenant: impl Into<String>, report: R) -> Blame<R> {
        let seen = self.0.enter();
        Blame {
            acc: Arc::clone(&self.0),
            report,
            tenant: tenant.into(),
            seen,
            owed: Duration::ZERO,
            reported: false,
        }
    }

    /// A tower layer that admits every request under its `Req::tenant()` and reports the blame
    /// through `report` when the request finishes.
    pub fn layer<R: Fn(&str, Duration) + Clone>(&self, report: R) -> TenantReportLayer<R> {
        TenantReportLayer { reporter: self.clone(), report }
    }

    /// Whether the gate is shut right now — a caller has been told `Pending` and not yet admitted.
    /// Shut-time is minting for as long as this holds.
    pub fn shut(&self) -> bool {
        matches!(Gate::decode(self.0.gate.load(Relaxed)), Gate::Shut { .. })
    }

    /// Bank the running shut span up to this instant, leaving it running.
    ///
    /// Nothing about the accounting depends on when this is called — [`Accumulator::flush`] is
    /// exactly-once and a span banked in two halves totals what it would have banked whole, give
    /// or take [`accrue`](Accumulator::accrue)'s sub-nanosecond floor. It exists for an observer:
    /// spans are otherwise banked lazily, at the next `poll_ready` or membership change, so
    /// [`meter`](Self::meter) read between two arbitrary moments can show a span that elapsed
    /// well before them. Settling at each observation makes the difference of two readings the
    /// shut-time that elapsed between them.
    pub fn settle(&self) {
        self.0.flush(self.0.now());
    }

    /// Occupiers on the server right now — for the visualiser.
    pub fn inflight(&self) -> u64 {
        self.0.inflight.load(Relaxed)
    }

    /// Measured shut-time that had an occupier to bill — the whole that drawn blame sums to.
    pub fn accumulated(&self) -> Duration {
        Duration::from_nanos(self.0.accumulated.load(Relaxed))
    }

    /// Blame drawn by occupiers so far. Never exceeds [`accumulated`](Self::accumulated); the slack
    /// is `dt / N`'s rounding.
    pub fn attributed(&self) -> Duration {
        Duration::from_nanos(self.0.attributed.load(Relaxed))
    }

    /// Shut-time that elapsed with no occupier — time no request can be blamed for.
    pub fn unattributed(&self) -> Duration {
        Duration::from_nanos(self.0.unattributed.load(Relaxed))
    }

    /// The meter: what a request that had occupied the server for the whole run would owe. The
    /// visualiser draws this, and a request's fare is the difference across its residency.
    pub fn meter(&self) -> Duration {
        Duration::from_nanos(self.0.rate.load(Relaxed))
    }
}

// ── the tower layer ────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct TenantReportLayer<R> {
    reporter: TenantReporter,
    report: R,
}

impl<S, R: Clone> Layer<S> for TenantReportLayer<R> {
    type Service = TenantReportService<S, R>;

    fn layer(&self, inner: S) -> Self::Service {
        TenantReportService { inner, reporter: self.reporter.clone(), report: self.report.clone() }
    }
}

#[derive(Clone)]
pub struct TenantReportService<S, R> {
    inner: S,
    reporter: TenantReporter,
    report: R,
}

impl<S, R, Req> Service<Req> for TenantReportService<S, R>
where
    S: Service<Req>,
    R: Fn(&str, Duration) + Clone,
    Req: Tenant,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = ReportFuture<S::Future, R>;

    /// The gate, measured. One clock read stamps both the flush of the span so far and the edge the
    /// verdict below writes, so the two can never disagree about when this observation happened.
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let acc = &self.reporter.0;
        let now = acc.now();
        acc.flush(now);
        let ready = self.inner.poll_ready(cx);
        match ready {
            Poll::Pending => acc.shut(now),
            Poll::Ready(_) => acc.open(now),
        }
        ready
    }

    fn call(&mut self, req: Req) -> Self::Future {
        let blame = self.reporter.admit(req.tenant().to_owned(), self.report.clone());
        ReportFuture { inner: self.inner.call(req), blame: Some(blame) }
    }
}

pin_project! {
    pub struct ReportFuture<F, R: Fn(&str, Duration)> {
        #[pin]
        inner: F,
        // Reported when the response resolves (or the future is cancelled), whichever comes first.
        blame: Option<Blame<R>>,
    }
}

impl<F: Future, R: Fn(&str, Duration)> Future for ReportFuture<F, R> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let result = this.inner.poll(cx);
        if let Some(blame) = this.blame.as_mut() {
            blame.draw();
        }
        if result.is_ready() {
            this.blame.take(); // report at completion, not at some later drop
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// The "expensive" per-tenant HashMap the write callback feeds — kept caller-side, exactly as
    /// intended, so the lock-free core never owns it.
    type Ledger = Arc<Mutex<HashMap<String, Duration>>>;

    fn ledger() -> Ledger {
        Arc::new(Mutex::new(HashMap::new()))
    }

    fn writer(ledger: &Ledger) -> impl Fn(&str, Duration) + Clone {
        let ledger = ledger.clone();
        move |tenant: &str, blame: Duration| {
            *ledger.lock().unwrap().entry(tenant.to_string()).or_insert(Duration::ZERO) += blame;
        }
    }

    /// The accounting counts nanos, so the small explicit spans these tests drive read as `ns(750)`.
    #[cfg(not(any(target_arch = "wasm32", feature = "virtual-clock")))]
    fn ns(n: u64) -> Duration {
        Duration::from_nanos(n)
    }

    /// The word and the gate are one thing in two representations: every `u64` the atomic can hold
    /// decodes to a `Gate` and re-encodes to itself, so no state is unrepresentable or misread.
    #[test]
    fn every_gate_word_round_trips() {
        for word in [0, 1, 12_345, OPEN - 1, OPEN] {
            assert_eq!(Gate::decode(word).encode(), word, "{word} did not survive the round trip");
        }
        assert_eq!(Gate::decode(OPEN), Gate::Open);
        assert_eq!(Gate::decode(7), Gate::Shut { since: 7 });
        // The reserved word is `Open`'s alone: a stamp that lands on it is canonicalised down
        // rather than decoding back as an open gate.
        assert_eq!(Gate::decode(Gate::Shut { since: OPEN }.encode()), Gate::Shut { since: OPEN - 1 });
    }

    // ── the accounting math, driven by explicit `accrue`; runs on the default (native) clock, whose
    //    ambient `Instant::now()` needs no scope. The virtual clock covers the gate protocol below;
    //    the split mirrors how the queue's own tests are organised. ─────────────────────────────────

    #[cfg(not(any(target_arch = "wasm32", feature = "virtual-clock")))]
    #[test]
    fn shut_time_splits_evenly_and_conserves() {
        let reporter = TenantReporter::new();
        let l = ledger();
        let a = reporter.admit("a", writer(&l));
        let b = reporter.admit("b", writer(&l));

        // Two occupiers present together; bank spans whose measured `dt` sums to 1500.
        reporter.0.accrue(ns(1000));
        reporter.0.accrue(ns(500));
        drop(a);
        drop(b);

        let m = l.lock().unwrap();
        assert_eq!(m["a"], ns(750), "even split of the 1500 ns of shut-time across two");
        assert_eq!(m["b"], ns(750));
        assert_eq!(m["a"] + m["b"], reporter.accumulated(), "bills sum to the distributable whole");
        // The visualiser's readout: the meter wound 500·N⁻¹ + 250·N⁻¹.
        assert_eq!(reporter.meter(), ns(750));
    }

    #[cfg(not(any(target_arch = "wasm32", feature = "virtual-clock")))]
    #[test]
    fn membership_reweights_the_share() {
        let reporter = TenantReporter::new();
        let l = ledger();
        // `a` alone: it owns the whole span.
        let a = reporter.admit("a", writer(&l));
        reporter.0.accrue(ns(100));
        // `b` joins; the next span is halved.
        let b = reporter.admit("b", writer(&l));
        reporter.0.accrue(ns(200));
        drop(a);
        drop(b);

        let m = l.lock().unwrap();
        assert_eq!(m["a"], ns(200), "100 alone + 100 shared");
        assert_eq!(m["b"], ns(100), "joined only for the halved span");
        assert_eq!(m["a"] + m["b"], reporter.accumulated());
    }

    #[cfg(not(any(target_arch = "wasm32", feature = "virtual-clock")))]
    #[test]
    fn drawing_often_costs_no_more_than_drawing_once() {
        // A request polled on every span must be billed exactly what a request polled only at exit
        // is. Odd spans across two occupiers, so every flush leaves a rounding remainder for the
        // schedule to disagree over if a draw were anything but an exact subtraction.
        let reporter = TenantReporter::new();
        let l = ledger();
        let mut eager = reporter.admit("eager", writer(&l));
        let lazy = reporter.admit("lazy", writer(&l));
        for _ in 0..4 {
            reporter.0.accrue(ns(3));
            eager.draw();
        }
        drop(eager);
        drop(lazy);

        let m = l.lock().unwrap();
        assert_eq!(m["eager"], m["lazy"], "the drawing schedule does not change the bill");
        assert_eq!(m["eager"], ns(4), "four spans of 3 halved and floored");
        assert_eq!(reporter.accumulated() - reporter.attributed(), ns(4), "the halves are the slack");
    }

    #[cfg(not(any(target_arch = "wasm32", feature = "virtual-clock")))]
    #[test]
    fn blame_survives_the_meter_rolling_over() {
        // A long-lived server's meter eventually rolls the u64. A request in flight across the roll
        // must still be billed the true delta — the whole reason a fare is read as a difference.
        let reporter = TenantReporter::new();
        let l = ledger();
        reporter.0.rate.store(u64::MAX - 2, Relaxed); // pin the meter just below the roll
        reporter.0.accumulated.store(100, Relaxed); // give the clamp room; this isolates the roll
        let r = reporter.admit("late", writer(&l)); // reads the meter at the brink
        reporter.0.rate.fetch_add(5, Relaxed); // 5 nanos wind on, crossing the roll
        drop(r);
        // Wrapping delta recovers the 5 nanos; a saturating subtraction would have reported 0.
        assert_eq!(l.lock().unwrap().get("late").copied(), Some(ns(5)));
    }

    #[cfg(not(any(target_arch = "wasm32", feature = "virtual-clock")))]
    #[test]
    fn attributed_never_exceeds_accumulated() {
        let reporter = TenantReporter::new();
        let l = ledger();
        let a = reporter.admit("a", writer(&l));
        let b = reporter.admit("b", writer(&l));
        reporter.0.accrue(ns(100)); // the whole measured is 100
        // Force an over-draw: push `rate` so each request's raw share (1050) dwarfs the whole,
        // standing in for rounding slop that would otherwise bill past what was measured.
        reporter.0.rate.fetch_add(1000, Relaxed);
        drop(a);
        drop(b);

        let m = l.lock().unwrap();
        let billed: Duration = m.values().sum();
        assert_eq!(billed, reporter.attributed(), "the ledger sums to the attributed total");
        assert_eq!(
            reporter.attributed(),
            reporter.accumulated(),
            "attribution fills the whole but the clamp holds it there"
        );
        assert!(reporter.attributed() <= reporter.accumulated());
    }

    // ── the gate protocol, driven by the virtual clock (house style: `virtual-clock`) ─────────────

    #[cfg(any(target_arch = "wasm32", feature = "virtual-clock"))]
    mod gate {
        use super::*;
        use crate::clock::Clock;
        use std::task::Waker;
        use std::time::Duration;

        pub(super) struct Req(pub &'static str);
        impl Tenant for Req {
            fn tenant(&self) -> &str {
                self.0
            }
        }

        /// An inner service whose readiness the test drives, and whose futures resolve after
        /// running the clock forward by `work` — the handler occupying the server.
        #[derive(Clone)]
        pub(super) struct Inner {
            pub clock: Clock,
            pub ready: Arc<AtomicU64>,
            pub work: u64,
        }
        pub(super) struct Work {
            clock: Clock,
            work: u64,
        }
        impl Service<Req> for Inner {
            type Response = ();
            type Error = ();
            type Future = Work;
            fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), ()>> {
                match self.ready.load(Relaxed) {
                    0 => Poll::Pending,
                    _ => Poll::Ready(Ok(())),
                }
            }
            fn call(&mut self, _: Req) -> Self::Future {
                Work { clock: self.clock.clone(), work: self.work }
            }
        }
        impl Future for Work {
            type Output = Result<(), ()>;
            fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
                self.clock.advance_to(self.clock.now() + Duration::from_micros(self.work));
                Poll::Ready(Ok(()))
            }
        }

        /// These tests drive the virtual clock in micros, its native unit.
        pub(super) fn us(n: u64) -> Duration {
            Duration::from_micros(n)
        }

        pub(super) fn advance(clock: &Clock, n: u64) {
            clock.advance_to(clock.now() + us(n));
        }

        pub(super) fn cx() -> Context<'static> {
            Context::from_waker(Waker::noop())
        }
    }

    #[cfg(any(target_arch = "wasm32", feature = "virtual-clock"))]
    #[test]
    fn an_open_gate_mints_nothing() {
        use gate::*;
        let clock = crate::clock::Clock::new();
        let _scope = clock.enter();
        let reporter = TenantReporter::new();
        let l = ledger();
        let inner = Inner { clock: clock.clone(), ready: Arc::new(AtomicU64::new(1)), work: 500 };
        let mut svc = reporter.layer(writer(&l)).layer(inner);

        let mut cx = cx();
        assert!(svc.poll_ready(&mut cx).is_ready(), "the gate admits immediately");
        let mut fut = std::pin::pin!(svc.call(Req("acme")));
        assert!(fut.as_mut().poll(&mut cx).is_ready());

        // The handler held the server for 500 µs, but nobody was ever refused, so nobody pays.
        assert_eq!(reporter.accumulated(), Duration::ZERO, "an open gate is free residency");
        assert_eq!(reporter.unattributed(), Duration::ZERO);
        assert_eq!(l.lock().unwrap().get("acme").copied(), Some(Duration::ZERO));
    }

    #[cfg(any(target_arch = "wasm32", feature = "virtual-clock"))]
    #[test]
    fn a_shut_gate_bills_the_occupier_holding_it_shut() {
        use gate::*;
        let clock = crate::clock::Clock::new();
        let _scope = clock.enter();
        let reporter = TenantReporter::new();
        let l = ledger();
        let ready = Arc::new(AtomicU64::new(1));
        let inner = Inner { clock: clock.clone(), ready: ready.clone(), work: 0 };
        let mut svc = reporter.layer(writer(&l)).layer(inner);
        let mut cx = cx();

        // `hog` is admitted, then the gate shuts behind it.
        assert!(svc.poll_ready(&mut cx).is_ready());
        let mut hog = std::pin::pin!(svc.call(Req("hog")));
        ready.store(0, Relaxed);
        assert!(svc.poll_ready(&mut cx).is_pending(), "the gate is now shut");
        assert!(reporter.shut());

        // 400 µs of a caller being made to wait, then the gate opens again.
        advance(&clock, 400);
        ready.store(1, Relaxed);
        assert!(svc.poll_ready(&mut cx).is_ready());
        assert!(!reporter.shut());

        assert_eq!(reporter.accumulated(), us(400), "the pending span is the shut-time");
        assert!(hog.as_mut().poll(&mut cx).is_ready());
        assert_eq!(
            l.lock().unwrap().get("hog").copied(),
            Some(us(400)),
            "the lone occupier owes it all"
        );
    }

    #[cfg(any(target_arch = "wasm32", feature = "virtual-clock"))]
    #[test]
    fn membership_changes_flush_the_running_span() {
        // The span is banked against the `N` that actually served it: `a` alone for the first leg,
        // nobody for the second, `b` alone for the third. A flush only at `poll_ready` would divide
        // the whole 30 µs by whoever happened to be present at the end.
        use gate::*;
        let clock = crate::clock::Clock::new();
        let _scope = clock.enter();
        let reporter = TenantReporter::new();
        let l = ledger();

        let a = reporter.admit("a", writer(&l));
        // Shut the gate with `a` inside. `Inner` is not involved — this drives the accumulator the
        // way the layer's `poll_ready` does, without a second service to keep in step.
        reporter.0.shut(reporter.0.now());

        advance(&clock, 10);
        drop(a); // leave flushes 10 µs at N = 1
        advance(&clock, 10); // 10 µs shut with nobody on the server
        let b = reporter.admit("b", writer(&l)); // enter flushes it at N = 0
        advance(&clock, 10);
        reporter.0.open(reporter.0.now()); // the gate admits; the last 10 µs bank at N = 1
        drop(b);

        let m = l.lock().unwrap();
        assert_eq!(m["a"], us(10), "billed only for the leg it was present for");
        assert_eq!(m["b"], us(10));
        assert_eq!(reporter.unattributed(), us(10), "the leg with no occupier is nobody's fault");
        assert_eq!(reporter.accumulated(), us(20), "the distributable whole excludes the empty leg");
    }

    #[cfg(any(target_arch = "wasm32", feature = "virtual-clock"))]
    #[test]
    fn settling_reads_the_span_without_changing_it() {
        // An observer settling every 10 µs must see each 10 µs as it elapses, and must bill the
        // occupier exactly what a single lazy flush at the end would have.
        use gate::*;
        let clock = crate::clock::Clock::new();
        let _scope = clock.enter();
        let reporter = TenantReporter::new();
        let l = ledger();

        let hog = reporter.admit("hog", writer(&l));
        reporter.0.shut(reporter.0.now());
        let mut seen = Vec::new();
        let mut last = reporter.meter();
        for _ in 0..3 {
            advance(&clock, 10);
            reporter.settle();
            seen.push(reporter.meter() - last);
            last = reporter.meter();
        }
        assert_eq!(seen, vec![us(10), us(10), us(10)], "each settled span is the time it covered");

        drop(hog);
        assert_eq!(reporter.accumulated(), us(30), "the whole is what elapsed, settled or not");
        assert_eq!(l.lock().unwrap().get("hog").copied(), Some(us(30)), "and the bill is unmoved");
    }

    #[cfg(any(target_arch = "wasm32", feature = "virtual-clock"))]
    #[test]
    fn a_dropped_request_leaves_and_reports_exactly_once() {
        use gate::*;
        let clock = crate::clock::Clock::new();
        let _scope = clock.enter();
        let reporter = TenantReporter::new();
        let l = ledger();

        // An inner that never resolves — the request is cancelled by dropping the future mid-flight.
        struct Pending;
        impl Service<Req> for Pending {
            type Response = ();
            type Error = ();
            type Future = std::future::Pending<Result<(), ()>>;
            fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), ()>> {
                Poll::Ready(Ok(()))
            }
            fn call(&mut self, _: Req) -> Self::Future {
                std::future::pending()
            }
        }

        let mut svc = reporter.layer(writer(&l)).layer(Pending);
        let mut cx = cx();
        assert!(svc.poll_ready(&mut cx).is_ready());

        // Owned (not `pin!`, whose value outlives the handle) so the drop reaches the future itself.
        let mut fut = Box::pin(svc.call(Req("acme")));
        assert!(fut.as_mut().poll(&mut cx).is_pending());
        assert_eq!(reporter.inflight(), 1, "the occupier is in flight while polling");

        drop(fut); // cancellation — the gate never shut, so it owes nothing
        assert_eq!(reporter.inflight(), 0, "dropping the future leaves the occupier set");
        assert_eq!(
            l.lock().unwrap().get("acme").copied(),
            Some(Duration::ZERO),
            "reported exactly once (blame 0 — the gate never shut)"
        );
    }

    #[cfg(any(target_arch = "wasm32", feature = "virtual-clock"))]
    #[test]
    fn a_panicking_occupier_still_leaves_and_pays_what_it_owes() {
        use gate::*;
        use std::panic::{self, AssertUnwindSafe};
        let clock = crate::clock::Clock::new();
        let _scope = clock.enter();
        let reporter = TenantReporter::new();
        let l = ledger();

        struct Boom;
        impl Service<Req> for Boom {
            type Response = ();
            type Error = ();
            type Future = BoomFuture;
            fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), ()>> {
                Poll::Ready(Ok(()))
            }
            fn call(&mut self, _: Req) -> Self::Future {
                BoomFuture
            }
        }
        struct BoomFuture;
        impl Future for BoomFuture {
            type Output = Result<(), ()>;
            fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
                panic!("handler blew up mid-poll");
            }
        }

        let mut svc = reporter.layer(writer(&l)).layer(Boom);
        let mut cx = cx();
        assert!(svc.poll_ready(&mut cx).is_ready());
        let mut fut = Box::pin(svc.call(Req("acme")));
        // The occupier holds the server shut for 250 µs before it unwinds.
        reporter.0.shut(reporter.0.now());
        advance(&clock, 250);

        let default_hook = panic::take_hook();
        panic::set_hook(Box::new(|_| {})); // keep the expected panic off the test log
        let caught = panic::catch_unwind(AssertUnwindSafe(|| {
            let _ = fut.as_mut().poll(&mut cx); // panics; unwinding drops the future, then the handle
            drop(fut);
        }));
        panic::set_hook(default_hook);

        assert!(caught.is_err(), "the poll panicked");
        assert_eq!(reporter.inflight(), 0, "the panicking occupier still left the set");
        assert_eq!(
            l.lock().unwrap().get("acme").copied(),
            Some(us(250)),
            "the shut-time it held was billed on the unwind path, not lost"
        );
    }

    // ── the lock-free hot path under real multi-threaded contention (native only) ────────────────

    #[cfg(not(any(target_arch = "wasm32", feature = "virtual-clock")))]
    #[test]
    fn concurrent_churn_keeps_the_invariants() {
        use std::thread;

        let reporter = TenantReporter::new();
        let l = ledger();

        let threads: Vec<_> = (0..8)
            .map(|t| {
                let reporter = reporter.clone();
                let write = writer(&l);
                thread::spawn(move || {
                    let tenant = format!("t{}", t % 3);
                    for _ in 0..10_000 {
                        let handle = reporter.admit(tenant.clone(), write.clone());
                        reporter.0.accrue(ns(1)); // exercise the rate/attribute path under contention
                        drop(handle);
                    }
                })
            })
            .collect();
        for h in threads {
            h.join().unwrap();
        }

        // The hard safety invariants must survive the contention, untouched by any boundary race:
        assert_eq!(reporter.inflight(), 0, "every occupier left; no fetch_sub underflow");
        assert!(
            reporter.attributed() <= reporter.accumulated(),
            "billing never exceeds the measured whole"
        );
        let billed: Duration = l.lock().unwrap().values().sum();
        assert_eq!(billed, reporter.attributed(), "the ledger equals what was attributed");
    }

    #[cfg(not(any(target_arch = "wasm32", feature = "virtual-clock")))]
    #[test]
    fn concurrent_flushes_bank_each_span_once() {
        use std::thread;

        let reporter = TenantReporter::new();
        let _occupier = reporter.admit("t", |_, _| {});
        // Every thread flushes the same running span at the same instant; the swap decides who owns
        // which segment, so the banked total is the span, not the span times the thread count.
        reporter.0.gate.store(Gate::Shut { since: 0 }.encode(), Relaxed);
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let reporter = reporter.clone();
                thread::spawn(move || {
                    for _ in 0..1_000 {
                        reporter.0.flush(1_000_000);
                    }
                })
            })
            .collect();
        for h in threads {
            h.join().unwrap();
        }
        assert_eq!(reporter.accumulated(), ns(1_000_000), "the span is banked exactly once");
    }
}
