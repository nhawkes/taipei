//! Turning a tenant's traffic away by a share someone else decided.
//!
//! [`tenant`](crate::tenant) measures: it mints shut-time and splits it between the
//! requests that were on the server while the gate was closed. This is the other half —
//! acting on that measurement. A server writes what each tenant cost it and reads back
//! the fraction of that tenant's traffic to refuse, and [`EnforcerLayer`] refuses it.
//!
//! Both directions go through [`Limits`], and neither is answered here. The share is a
//! *fleet* decision: one server's blame is not the bill, and one server's idea of a fair
//! split would fight the next server's. So the store is somewhere else, whoever computes
//! the split is somewhere else, and everything this crate knows about the round trip is
//! that it is stale — the number a request is judged against was written for traffic that
//! has already been and gone.
//!
//! Place the layer **outermost**, above any queue: a request that is going to be refused
//! should not first take a queue slot from one that is not.
//!
//! # Which requests get refused
//!
//! Not a coin flip: a tally. Each tenant's requests are counted as they arrive, and one is
//! refused whenever refusing it would leave the refusals no further along than the share of
//! the traffic seen so far. Over `n` requests at a constant share `p` that refuses exactly
//! `⌊n·p⌋` of them, spaced out — where sampling would refuse a run of five and then let a
//! run of five through, and would need a seeded generator to be reproducible at all.
//!
//! The tally resets when a tenant's share moves, which is the only thing that keeps it
//! honest: a share is a statement about the traffic *now*, so a tenant refused hard for a
//! minute and then forgiven must not spend the next minute paying off the old rate.
//!
//! The tally is the *server's*, not a connection's: clones of a [`EnforcerService`] share
//! it, because a per-clone tally on a stack that clones per request would never count past
//! one.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use pin_project_lite::pin_project;
use tower::{Layer, Service};

use crate::reject::BoxError;
use crate::tenant::Tenant;

/// The store a rate-limited server talks to: it writes what a tenant cost it, and reads
/// back the share of that tenant's traffic to refuse.
///
/// Both halves are stale by construction, and the implementor owns how stale. Blame is
/// accumulated somewhere shared so that it is the fleet's and not one machine's, and the
/// share is written by whoever divides the budget — so a `drop_pct` read here answers a
/// question asked about traffic that has already passed.
///
/// A tenant the store has never heard of is not being limited: return `0.0`.
pub trait Limits: Send + Sync + 'static {
    /// Bank what one finished request cost this tenant in shut-time. Called from the
    /// request's completion, which can run mid-unwind, so it must not panic.
    fn write_blame(&self, tenant: &str, blame: Duration);

    /// The share of this tenant's traffic to refuse, in `0.0..=1.0`. Values outside that
    /// range are clamped into it.
    fn drop_pct(&self, tenant: &str) -> f64;
}

// --- Layer ---

/// Refuses a share of each tenant's requests, as [`Limits`] currently says.
pub struct EnforcerLayer<L> {
    limits: Arc<L>,
    tally: Tally,
}

impl<L: Limits> EnforcerLayer<L> {
    pub fn new(limits: Arc<L>) -> Self {
        EnforcerLayer { limits, tally: Tally::default() }
    }
}

impl<L> Clone for EnforcerLayer<L> {
    fn clone(&self) -> Self {
        EnforcerLayer { limits: Arc::clone(&self.limits), tally: self.tally.clone() }
    }
}

impl<S, L: Limits> Layer<S> for EnforcerLayer<L> {
    type Service = EnforcerService<S, L>;

    fn layer(&self, inner: S) -> Self::Service {
        EnforcerService { inner, limits: Arc::clone(&self.limits), tally: self.tally.clone() }
    }
}

// --- the tally ---

/// One tenant's run at one share: how many of its requests have been seen, and how many
/// refused. The share travels with the counts because it is what they mean — counts kept
/// under an old share are not evidence about the new one.
#[derive(Default)]
struct Run {
    share: f64,
    seen: u64,
    refused: u64,
}

/// Every tenant's run. Shared by every clone of the service, so the count is the server's
/// whatever the stack above does with its handles.
#[derive(Clone, Default)]
struct Tally(Arc<Mutex<HashMap<String, Run>>>);

impl Tally {
    /// Count one request of `tenant`'s and say whether it is one of the refused.
    ///
    /// A poisoned tally admits: a limiter that has lost count is not a reason to start
    /// turning traffic away.
    fn count(&self, tenant: &str, share: f64) -> Refusal {
        let share = share.clamp(0.0, 1.0);
        let Ok(mut runs) = self.0.lock() else { return Refusal::Admit };
        let run = match runs.get_mut(tenant) {
            Some(run) => run,
            None => runs.entry(tenant.to_owned()).or_default(),
        };
        if run.share != share {
            *run = Run { share, ..Run::default() };
        }
        run.seen += 1;
        // Refuse while doing so keeps the refusals inside the share of what has arrived —
        // one multiplication against the running count, so the tally cannot drift the way
        // an accumulated fraction does.
        match (run.refused + 1) as f64 <= run.seen as f64 * share {
            true => {
                run.refused += 1;
                Refusal::Refuse
            }
            false => Refusal::Admit,
        }
    }
}

/// What counting a request against its tenant's share came to.
enum Refusal {
    Admit,
    Refuse,
}

// --- Service ---

pub struct EnforcerService<S, L> {
    inner: S,
    limits: Arc<L>,
    tally: Tally,
}

impl<S: Clone, L> Clone for EnforcerService<S, L> {
    fn clone(&self) -> Self {
        EnforcerService {
            inner: self.inner.clone(),
            limits: Arc::clone(&self.limits),
            tally: self.tally.clone(),
        }
    }
}

impl<S, L, Req> Service<Req> for EnforcerService<S, L>
where
    S: Service<Req>,
    S::Error: Into<BoxError>,
    L: Limits,
    Req: Tenant,
{
    type Response = S::Response;
    type Error = BoxError;
    type Future = ResponseFuture<S::Future>;

    /// Nothing here withholds readiness: refusing is a verdict on a request, and there is
    /// no request to judge yet. Whatever the stack below says about its own capacity is
    /// passed through untouched.
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, req: Req) -> Self::Future {
        match self.tally.count(req.tenant(), self.limits.drop_pct(req.tenant())) {
            Refusal::Admit => ResponseFuture::Inner { future: self.inner.call(req) },
            Refusal::Refuse => ResponseFuture::Refused,
        }
    }
}

// --- Error ---

/// Returned when a request is refused to hold its tenant inside its share of the budget.
///
/// Distinct from [`Overloaded`](crate::reject::Overloaded): that one means the server has
/// nothing to give anyone, this one means the server has something to give but not to you.
#[derive(Debug, thiserror::Error)]
#[error("tenant over its share; request refused before queueing")]
pub struct Throttled;

// --- Future ---

pin_project! {
    #[project = ResponseProj]
    pub enum ResponseFuture<F> {
        Inner { #[pin] future: F },
        Refused,
    }
}

impl<F, T, E> Future for ResponseFuture<F>
where
    F: Future<Output = Result<T, E>>,
    E: Into<BoxError>,
{
    type Output = Result<T, BoxError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.project() {
            ResponseProj::Inner { future } => future.poll(cx).map_err(Into::into),
            ResponseProj::Refused => Poll::Ready(Err(Box::new(Throttled) as BoxError)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower::{Service, ServiceExt};

    struct Req(&'static str);
    impl Tenant for Req {
        fn tenant(&self) -> &str {
            self.0
        }
    }

    /// A store the test writes the answer into, and which records what was banked against it.
    #[derive(Default)]
    struct Fixed {
        shares: Mutex<HashMap<&'static str, f64>>,
        banked: Mutex<Vec<(String, Duration)>>,
    }

    impl Fixed {
        fn at(shares: &[(&'static str, f64)]) -> Arc<Fixed> {
            let shares = Mutex::new(shares.iter().copied().collect());
            Arc::new(Fixed { shares, ..Fixed::default() })
        }

        fn set(&self, tenant: &'static str, share: f64) {
            self.shares.lock().unwrap().insert(tenant, share);
        }
    }

    impl Limits for Fixed {
        fn write_blame(&self, tenant: &str, blame: Duration) {
            self.banked.lock().unwrap().push((tenant.to_owned(), blame));
        }
        fn drop_pct(&self, tenant: &str) -> f64 {
            self.shares.lock().unwrap().get(tenant).copied().unwrap_or(0.0)
        }
    }

    #[derive(Clone)]
    struct Echo;
    impl Service<Req> for Echo {
        type Response = &'static str;
        type Error = std::convert::Infallible;
        type Future = std::future::Ready<Result<&'static str, std::convert::Infallible>>;
        fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
        fn call(&mut self, req: Req) -> Self::Future {
            std::future::ready(Ok(req.0))
        }
    }

    /// Send `n` requests from one tenant and report which of them were refused.
    async fn run(limits: Arc<Fixed>, tenant: &'static str, n: usize) -> Vec<bool> {
        let mut svc = EnforcerLayer::new(limits).layer(Echo);
        let mut refused = Vec::new();
        for _ in 0..n {
            let call = svc.ready().await.unwrap().call(Req(tenant));
            refused.push(match call.await {
                Ok(_) => false,
                Err(e) => e.downcast_ref::<Throttled>().is_some(),
            });
        }
        refused
    }

    #[tokio::test]
    async fn a_tenant_the_store_has_never_heard_of_is_not_limited() {
        assert_eq!(run(Fixed::at(&[]), "Alice", 8).await, vec![false; 8]);
    }

    #[tokio::test]
    async fn a_zero_share_admits_everything() {
        let limits = Fixed::at(&[("Alice", 0.0)]);
        assert_eq!(run(limits, "Alice", 8).await, vec![false; 8]);
    }

    #[tokio::test]
    async fn half_a_share_refuses_every_other_request() {
        let limits = Fixed::at(&[("Alice", 0.5)]);
        assert_eq!(
            run(limits, "Alice", 6).await,
            vec![false, true, false, true, false, true],
            "the refusals are spaced, not clustered",
        );
    }

    #[tokio::test]
    async fn a_whole_share_refuses_everything() {
        let limits = Fixed::at(&[("Alice", 1.0)]);
        assert_eq!(run(limits, "Alice", 5).await, vec![true; 5]);
    }

    /// The count is exact, not approximate: over `n` requests at share `p`, `⌊n·p⌋` are
    /// refused. This is what a sampled limiter cannot promise.
    #[tokio::test]
    async fn the_refused_count_is_the_share_of_the_traffic() {
        for (share, n, want) in [(0.25, 100, 25), (0.1, 100, 10), (0.75, 40, 30), (0.3, 10, 3)] {
            let limits = Fixed::at(&[("Alice", share)]);
            let refused = run(limits, "Alice", n).await.iter().filter(|&&r| r).count();
            assert_eq!(refused, want, "{n} requests at a share of {share}");
        }
    }

    /// One tenant's share says nothing about another's: the debts are kept apart.
    #[tokio::test]
    async fn tenants_are_charged_separately() {
        let limits = Fixed::at(&[("Alice", 1.0), ("Bob", 0.0)]);
        let mut svc = EnforcerLayer::new(limits).layer(Echo);
        for tenant in ["Alice", "Bob", "Alice", "Bob"] {
            let refused = svc.ready().await.unwrap().call(Req(tenant)).await.is_err();
            assert_eq!(refused, tenant == "Alice", "{tenant}");
        }
    }

    /// Cloning the service — which a stack that clones per request does constantly — must
    /// not hand out a fresh tally, or nothing at a share below 1.0 would ever be refused.
    #[tokio::test]
    async fn clones_share_one_tally() {
        let limits = Fixed::at(&[("Alice", 0.5)]);
        let svc = EnforcerLayer::new(limits).layer(Echo);
        let mut refused = Vec::new();
        for _ in 0..6 {
            let mut per_request = svc.clone();
            let call = per_request.ready().await.unwrap().call(Req("Alice"));
            refused.push(call.await.is_err());
        }
        assert_eq!(refused, vec![false, true, false, true, false, true]);
    }

    /// A share is a statement about the traffic now. A tenant refused hard and then
    /// forgiven is forgiven immediately — it does not spend the next stretch working off
    /// a debt the old share ran up, and a tenant newly limited is not refused in a burst
    /// to catch up on traffic that arrived before anyone was limiting it.
    #[tokio::test]
    async fn a_new_share_starts_a_new_run() {
        let limits = Fixed::at(&[("Alice", 1.0)]);
        let mut svc = EnforcerLayer::new(Arc::clone(&limits)).layer(Echo);
        let mut refused = Vec::new();
        for at in 0..8 {
            if at == 4 {
                limits.set("Alice", 0.0);
            }
            refused.push(svc.ready().await.unwrap().call(Req("Alice")).await.is_err());
        }
        assert_eq!(refused, vec![true, true, true, true, false, false, false, false]);

        limits.set("Alice", 0.5);
        let mut refused = Vec::new();
        for _ in 0..4 {
            refused.push(svc.ready().await.unwrap().call(Req("Alice")).await.is_err());
        }
        assert_eq!(refused, vec![false, true, false, true], "counted from the change, not from zero");
    }

    /// The other half of the seam. The layer never calls it — a server's blame comes from
    /// the reporter under its queue — but it is the same store, so a caller has one thing
    /// to implement and one thing to hold.
    #[test]
    fn blame_written_reaches_the_store() {
        let limits = Fixed::at(&[]);
        limits.write_blame("Alice", Duration::from_micros(40));
        assert_eq!(
            *limits.banked.lock().unwrap(),
            vec![("Alice".to_owned(), Duration::from_micros(40))],
        );
    }
}
