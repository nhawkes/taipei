use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use pin_project_lite::pin_project;
use tokio::time::Instant;
use tower::{Layer, Service};

const OPEN: u64 = u64::MAX;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Gate {
    Open,
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
            Gate::Shut { since } => since.min(OPEN - 1),
        }
    }
}

pub trait Tenant {
    fn tenant(&self) -> &str;
}

struct Accumulator {
    rate: AtomicU64,
    inflight: AtomicU64,
    accumulated: AtomicU64,
    attributed: AtomicU64,
    unattributed: AtomicU64,
    gate: AtomicU64,
    epoch: Instant,
}

impl Accumulator {
    fn nanos(&self, t: Instant) -> u64 {
        t.duration_since(self.epoch).as_nanos() as u64
    }

    fn now(&self) -> u64 {
        self.nanos(Instant::now())
    }

    fn flush(&self, now: u64) {
        let mut word = self.gate.load(Relaxed);
        loop {
            let Gate::Shut { since } = Gate::decode(word) else {
                return;
            };
            let restamped = Gate::Shut { since: now }.encode();
            match self
                .gate
                .compare_exchange_weak(word, restamped, Relaxed, Relaxed)
            {
                Ok(_) => return self.accrue(Duration::from_nanos(now.saturating_sub(since))),
                Err(observed) => word = observed,
            }
        }
    }

    fn shut(&self, now: u64) {
        let started = Gate::Shut { since: now }.encode();
        let _ = self
            .gate
            .compare_exchange(Gate::Open.encode(), started, Relaxed, Relaxed);
    }

    fn open(&self, now: u64) {
        if let Gate::Shut { since } = Gate::decode(self.gate.swap(Gate::Open.encode(), Relaxed)) {
            self.accrue(Duration::from_nanos(now.saturating_sub(since)));
        }
    }

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

    fn enter(&self) -> u64 {
        self.flush(self.now());
        self.inflight.fetch_add(1, Relaxed);
        self.rate.load(Relaxed)
    }

    fn leave(&self) {
        self.flush(self.now());
        self.inflight.fetch_sub(1, Relaxed);
    }
}

pub struct Blame<R: Report> {
    acc: Arc<Accumulator>,
    report: R,
    tenant: String,
    seen: u64,
    owed: Duration,
    reported: bool,
}

impl<R: Report> Blame<R> {
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
        self.acc.leave();
        self.draw();
        self.report.report(&self.tenant, self.owed);
    }
}

impl<R: Report> Drop for Blame<R> {
    fn drop(&mut self) {
        self.finalize();
    }
}

pub trait Report: Clone {
    fn report(&self, tenant: &str, blame: Duration);
}

impl<F: Fn(&str, Duration) + Clone> Report for F {
    fn report(&self, tenant: &str, blame: Duration) {
        self(tenant, blame)
    }
}

#[derive(Clone)]
pub struct TenantReporter<R: Report> {
    acc: Arc<Accumulator>,
    report: R,
}

#[bon::bon]
impl<R: Report> TenantReporter<R> {
    #[builder]
    pub fn new(report: R) -> Self {
        let epoch = Instant::now();
        TenantReporter {
            acc: Arc::new(Accumulator {
                rate: AtomicU64::new(0),
                inflight: AtomicU64::new(0),
                accumulated: AtomicU64::new(0),
                attributed: AtomicU64::new(0),
                unattributed: AtomicU64::new(0),
                gate: AtomicU64::new(Gate::Open.encode()),
                epoch,
            }),
            report,
        }
    }

    pub fn admit(&self, tenant: impl Into<String>) -> Blame<R> {
        let seen = self.acc.enter();
        Blame {
            acc: Arc::clone(&self.acc),
            report: self.report.clone(),
            tenant: tenant.into(),
            seen,
            owed: Duration::ZERO,
            reported: false,
        }
    }

    pub fn shut(&self) -> bool {
        matches!(Gate::decode(self.acc.gate.load(Relaxed)), Gate::Shut { .. })
    }

    pub fn settle(&self) {
        self.acc.flush(self.acc.now());
    }

    pub fn inflight(&self) -> u64 {
        self.acc.inflight.load(Relaxed)
    }

    pub fn accumulated(&self) -> Duration {
        Duration::from_nanos(self.acc.accumulated.load(Relaxed))
    }

    pub fn attributed(&self) -> Duration {
        Duration::from_nanos(self.acc.attributed.load(Relaxed))
    }

    pub fn unattributed(&self) -> Duration {
        Duration::from_nanos(self.acc.unattributed.load(Relaxed))
    }

    pub fn meter(&self) -> Duration {
        Duration::from_nanos(self.acc.rate.load(Relaxed))
    }
}

impl<S, R: Report> Layer<S> for TenantReporter<R> {
    type Service = TenantReportService<S, R>;

    fn layer(&self, inner: S) -> Self::Service {
        TenantReportService {
            inner,
            reporter: self.clone(),
        }
    }
}

#[derive(Clone)]
pub struct TenantReportService<S, R: Report> {
    inner: S,
    reporter: TenantReporter<R>,
}

impl<S, R, Req> Service<Req> for TenantReportService<S, R>
where
    S: Service<Req>,
    R: Report,
    Req: Tenant,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = ReportFuture<S::Future, R>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let acc = &self.reporter.acc;
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
        let blame = self.reporter.admit(req.tenant().to_owned());
        ReportFuture {
            inner: self.inner.call(req),
            blame: Some(blame),
        }
    }
}

pin_project! {
    pub struct ReportFuture<F, R: Report> {
        #[pin]
        inner: F,
        blame: Option<Blame<R>>,
    }
}

impl<F: Future, R: Report> Future for ReportFuture<F, R> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let result = this.inner.poll(cx);
        if let Some(blame) = this.blame.as_mut() {
            blame.draw();
        }
        if result.is_ready() {
            this.blame.take();
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    type Ledger = Arc<Mutex<HashMap<String, Duration>>>;

    fn ledger() -> Ledger {
        Arc::new(Mutex::new(HashMap::new()))
    }

    fn quiet(_: &str, _: Duration) {}

    fn writer(ledger: &Ledger) -> impl Fn(&str, Duration) + Clone {
        let ledger = ledger.clone();
        move |tenant: &str, blame: Duration| {
            *ledger
                .lock()
                .unwrap()
                .entry(tenant.to_string())
                .or_insert(Duration::ZERO) += blame;
        }
    }

    fn ns(n: u64) -> Duration {
        Duration::from_nanos(n)
    }

    #[test]
    fn every_gate_word_round_trips() {
        for word in [0, 1, 12_345, OPEN - 1, OPEN] {
            assert_eq!(
                Gate::decode(word).encode(),
                word,
                "{word} did not survive the round trip"
            );
        }
        assert_eq!(Gate::decode(OPEN), Gate::Open);
        assert_eq!(Gate::decode(7), Gate::Shut { since: 7 });
        assert_eq!(
            Gate::decode(Gate::Shut { since: OPEN }.encode()),
            Gate::Shut { since: OPEN - 1 }
        );
    }

    #[test]
    fn shut_time_splits_evenly_and_conserves() {
        let l = ledger();
        let reporter = TenantReporter::builder().report(writer(&l)).build();
        let a = reporter.admit("a");
        let b = reporter.admit("b");

        reporter.acc.accrue(ns(1000));
        reporter.acc.accrue(ns(500));
        drop(a);
        drop(b);

        let m = l.lock().unwrap();
        assert_eq!(
            m["a"],
            ns(750),
            "even split of the 1500 ns of shut-time across two"
        );
        assert_eq!(m["b"], ns(750));
        assert_eq!(
            m["a"] + m["b"],
            reporter.accumulated(),
            "bills sum to the distributable whole"
        );
        assert_eq!(reporter.meter(), ns(750));
    }

    #[test]
    fn membership_reweights_the_share() {
        let l = ledger();
        let reporter = TenantReporter::builder().report(writer(&l)).build();
        let a = reporter.admit("a");
        reporter.acc.accrue(ns(100));
        let b = reporter.admit("b");
        reporter.acc.accrue(ns(200));
        drop(a);
        drop(b);

        let m = l.lock().unwrap();
        assert_eq!(m["a"], ns(200), "100 alone + 100 shared");
        assert_eq!(m["b"], ns(100), "joined only for the halved span");
        assert_eq!(m["a"] + m["b"], reporter.accumulated());
    }

    #[test]
    fn drawing_often_costs_no_more_than_drawing_once() {
        let l = ledger();
        let reporter = TenantReporter::builder().report(writer(&l)).build();
        let mut eager = reporter.admit("eager");
        let lazy = reporter.admit("lazy");
        for _ in 0..4 {
            reporter.acc.accrue(ns(3));
            eager.draw();
        }
        drop(eager);
        drop(lazy);

        let m = l.lock().unwrap();
        assert_eq!(
            m["eager"], m["lazy"],
            "the drawing schedule does not change the bill"
        );
        assert_eq!(m["eager"], ns(4), "four spans of 3 halved and floored");
        assert_eq!(
            reporter.accumulated() - reporter.attributed(),
            ns(4),
            "the halves are the slack"
        );
    }

    #[test]
    fn blame_survives_the_meter_rolling_over() {
        let l = ledger();
        let reporter = TenantReporter::builder().report(writer(&l)).build();
        reporter.acc.rate.store(u64::MAX - 2, Relaxed);
        reporter.acc.accumulated.store(100, Relaxed);
        let r = reporter.admit("late");
        reporter.acc.rate.fetch_add(5, Relaxed);
        drop(r);
        assert_eq!(l.lock().unwrap().get("late").copied(), Some(ns(5)));
    }

    #[test]
    fn attributed_never_exceeds_accumulated() {
        let l = ledger();
        let reporter = TenantReporter::builder().report(writer(&l)).build();
        let a = reporter.admit("a");
        let b = reporter.admit("b");
        reporter.acc.accrue(ns(100));
        reporter.acc.rate.fetch_add(1000, Relaxed);
        drop(a);
        drop(b);

        let m = l.lock().unwrap();
        let billed: Duration = m.values().sum();
        assert_eq!(
            billed,
            reporter.attributed(),
            "the ledger sums to the attributed total"
        );
        assert_eq!(
            reporter.attributed(),
            reporter.accumulated(),
            "attribution fills the whole but the clamp holds it there"
        );
        assert!(reporter.attributed() <= reporter.accumulated());
    }

    mod gate {
        use super::*;
        use std::task::Waker;
        use std::time::Duration;
        use tokio::time::Sleep;

        pub(super) struct Req(pub &'static str);
        impl Tenant for Req {
            fn tenant(&self) -> &str {
                self.0
            }
        }

        #[derive(Clone)]
        pub(super) struct Inner {
            pub ready: Arc<AtomicU64>,
            pub work: u64,
        }
        pub(super) struct Work(Pin<Box<Sleep>>);
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
                Work(Box::pin(tokio::time::sleep(us(self.work))))
            }
        }
        impl Future for Work {
            type Output = Result<(), ()>;
            fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
                self.0.as_mut().poll(cx).map(Ok)
            }
        }

        pub(super) fn us(n: u64) -> Duration {
            Duration::from_micros(n)
        }

        pub(super) async fn advance(n: u64) {
            tokio::time::advance(us(n)).await;
        }

        pub(super) fn cx() -> Context<'static> {
            Context::from_waker(Waker::noop())
        }
    }

    #[tokio::test(start_paused = true)]
    async fn an_open_gate_mints_nothing() {
        use gate::*;
        let l = ledger();
        let reporter = TenantReporter::builder().report(writer(&l)).build();
        let inner = Inner {
            ready: Arc::new(AtomicU64::new(1)),
            work: 500,
        };
        let mut svc = reporter.layer(inner);

        let mut cx = cx();
        assert!(
            svc.poll_ready(&mut cx).is_ready(),
            "the gate admits immediately"
        );
        let fut = std::pin::pin!(svc.call(Req("acme")));
        assert!(fut.await.is_ok());

        assert_eq!(
            reporter.accumulated(),
            Duration::ZERO,
            "an open gate is free residency"
        );
        assert_eq!(reporter.unattributed(), Duration::ZERO);
        assert_eq!(l.lock().unwrap().get("acme").copied(), Some(Duration::ZERO));
    }

    #[tokio::test(start_paused = true)]
    async fn a_shut_gate_bills_the_occupier_holding_it_shut() {
        use gate::*;
        let l = ledger();
        let reporter = TenantReporter::builder().report(writer(&l)).build();
        let ready = Arc::new(AtomicU64::new(1));
        let inner = Inner {
            ready: ready.clone(),
            work: 0,
        };
        let mut svc = reporter.layer(inner);
        let mut cx = cx();

        assert!(svc.poll_ready(&mut cx).is_ready());
        let hog = std::pin::pin!(svc.call(Req("hog")));
        ready.store(0, Relaxed);
        assert!(svc.poll_ready(&mut cx).is_pending(), "the gate is now shut");
        assert!(reporter.shut());

        advance(400).await;
        ready.store(1, Relaxed);
        assert!(svc.poll_ready(&mut cx).is_ready());
        assert!(!reporter.shut());

        assert_eq!(
            reporter.accumulated(),
            us(400),
            "the pending span is the shut-time"
        );
        assert!(hog.await.is_ok());
        assert_eq!(
            l.lock().unwrap().get("hog").copied(),
            Some(us(400)),
            "the lone occupier owes it all"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn membership_changes_flush_the_running_span() {
        use gate::*;
        let l = ledger();
        let reporter = TenantReporter::builder().report(writer(&l)).build();

        let a = reporter.admit("a");
        reporter.acc.shut(reporter.acc.now());

        advance(10).await;
        drop(a);
        advance(10).await;
        let b = reporter.admit("b");
        advance(10).await;
        reporter.acc.open(reporter.acc.now());
        drop(b);

        let m = l.lock().unwrap();
        assert_eq!(m["a"], us(10), "billed only for the leg it was present for");
        assert_eq!(m["b"], us(10));
        assert_eq!(
            reporter.unattributed(),
            us(10),
            "the leg with no occupier is nobody's fault"
        );
        assert_eq!(
            reporter.accumulated(),
            us(20),
            "the distributable whole excludes the empty leg"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn settling_reads_the_span_without_changing_it() {
        use gate::*;
        let l = ledger();
        let reporter = TenantReporter::builder().report(writer(&l)).build();

        let hog = reporter.admit("hog");
        reporter.acc.shut(reporter.acc.now());
        let mut seen = Vec::new();
        let mut last = reporter.meter();
        for _ in 0..3 {
            advance(10).await;
            reporter.settle();
            seen.push(reporter.meter() - last);
            last = reporter.meter();
        }
        assert_eq!(
            seen,
            vec![us(10), us(10), us(10)],
            "each settled span is the time it covered"
        );

        drop(hog);
        assert_eq!(
            reporter.accumulated(),
            us(30),
            "the whole is what elapsed, settled or not"
        );
        assert_eq!(
            l.lock().unwrap().get("hog").copied(),
            Some(us(30)),
            "and the bill is unmoved"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_dropped_request_leaves_and_reports_exactly_once() {
        use gate::*;
        let l = ledger();
        let reporter = TenantReporter::builder().report(writer(&l)).build();

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

        let mut svc = reporter.layer(Pending);
        let mut cx = cx();
        assert!(svc.poll_ready(&mut cx).is_ready());

        let mut fut = Box::pin(svc.call(Req("acme")));
        assert!(fut.as_mut().poll(&mut cx).is_pending());
        assert_eq!(
            reporter.inflight(),
            1,
            "the occupier is in flight while polling"
        );

        drop(fut);
        assert_eq!(
            reporter.inflight(),
            0,
            "dropping the future leaves the occupier set"
        );
        assert_eq!(
            l.lock().unwrap().get("acme").copied(),
            Some(Duration::ZERO),
            "reported exactly once (blame 0 — the gate never shut)"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_panicking_occupier_still_leaves_and_pays_what_it_owes() {
        use gate::*;
        use std::panic::{self, AssertUnwindSafe};
        let l = ledger();
        let reporter = TenantReporter::builder().report(writer(&l)).build();

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

        let mut svc = reporter.layer(Boom);
        let mut cx = cx();
        assert!(svc.poll_ready(&mut cx).is_ready());
        let mut fut = Box::pin(svc.call(Req("acme")));
        reporter.acc.shut(reporter.acc.now());
        advance(250).await;

        let default_hook = panic::take_hook();
        panic::set_hook(Box::new(|_| {}));
        let caught = panic::catch_unwind(AssertUnwindSafe(|| {
            let _ = fut.as_mut().poll(&mut cx);
            drop(fut);
        }));
        panic::set_hook(default_hook);

        assert!(caught.is_err(), "the poll panicked");
        assert_eq!(
            reporter.inflight(),
            0,
            "the panicking occupier still left the set"
        );
        assert_eq!(
            l.lock().unwrap().get("acme").copied(),
            Some(us(250)),
            "the shut-time it held was billed on the unwind path, not lost"
        );
    }

    #[test]
    fn concurrent_churn_keeps_the_invariants() {
        use std::thread;

        let l = ledger();
        let reporter = TenantReporter::builder().report(writer(&l)).build();

        let threads: Vec<_> = (0..8)
            .map(|t| {
                let reporter = reporter.clone();
                thread::spawn(move || {
                    let tenant = format!("t{}", t % 3);
                    for _ in 0..10_000 {
                        let handle = reporter.admit(tenant.clone());
                        reporter.acc.accrue(ns(1));
                        drop(handle);
                    }
                })
            })
            .collect();
        for h in threads {
            h.join().unwrap();
        }

        assert_eq!(
            reporter.inflight(),
            0,
            "every occupier left; no fetch_sub underflow"
        );
        assert!(
            reporter.attributed() <= reporter.accumulated(),
            "billing never exceeds the measured whole"
        );
        let billed: Duration = l.lock().unwrap().values().sum();
        assert_eq!(
            billed,
            reporter.attributed(),
            "the ledger equals what was attributed"
        );
    }

    #[test]
    fn concurrent_flushes_bank_each_span_once() {
        use std::thread;

        let reporter = TenantReporter::builder().report(quiet).build();
        let _occupier = reporter.admit("t");
        reporter
            .acc
            .gate
            .store(Gate::Shut { since: 0 }.encode(), Relaxed);
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let reporter = reporter.clone();
                thread::spawn(move || {
                    for _ in 0..1_000 {
                        reporter.acc.flush(1_000_000);
                    }
                })
            })
            .collect();
        for h in threads {
            h.join().unwrap();
        }
        assert_eq!(
            reporter.accumulated(),
            ns(1_000_000),
            "the span is banked exactly once"
        );
    }
}
