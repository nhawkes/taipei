use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use pin_project_lite::pin_project;
use tower::{Layer, Service};

use crate::tenant::Tenant;

pub trait Limits: Send + Sync + 'static {
    fn write_blame(&self, tenant: &str, blame: Duration);

    fn drop_pct(&self, tenant: &str) -> f64;
}

pub struct EnforcerLayer<L> {
    limits: Arc<L>,
    tally: Tally,
}

impl<L: Limits> EnforcerLayer<L> {
    pub fn new(limits: Arc<L>) -> Self {
        EnforcerLayer {
            limits,
            tally: Tally::default(),
        }
    }
}

impl<L> Clone for EnforcerLayer<L> {
    fn clone(&self) -> Self {
        EnforcerLayer {
            limits: Arc::clone(&self.limits),
            tally: self.tally.clone(),
        }
    }
}

impl<S, L: Limits> Layer<S> for EnforcerLayer<L> {
    type Service = EnforcerService<S, L>;

    fn layer(&self, inner: S) -> Self::Service {
        EnforcerService {
            inner,
            limits: Arc::clone(&self.limits),
            tally: self.tally.clone(),
        }
    }
}

#[derive(Default)]
struct Run {
    share: f64,
    seen: u64,
    refused: u64,
}

#[derive(Clone, Default)]
struct Tally(Arc<Mutex<HashMap<String, Run>>>);

impl Tally {
    fn count(&self, tenant: &str, share: f64) -> Refusal {
        let share = share.clamp(0.0, 1.0);
        let Ok(mut runs) = self.0.lock() else {
            return Refusal::Admit;
        };
        let run = match runs.get_mut(tenant) {
            Some(run) => run,
            None => runs.entry(tenant.to_owned()).or_default(),
        };
        if run.share != share {
            *run = Run {
                share,
                ..Run::default()
            };
        }
        run.seen += 1;
        match (run.refused + 1) as f64 <= run.seen as f64 * share {
            true => {
                run.refused += 1;
                Refusal::Refuse
            }
            false => Refusal::Admit,
        }
    }
}

enum Refusal {
    Admit,
    Refuse,
}

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
    L: Limits,
    Req: Tenant,
{
    type Response = S::Response;
    type Error = TenantQuotaError<S::Error>;
    type Future = ResponseFuture<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx).map_err(TenantQuotaError::Inner)
    }

    fn call(&mut self, req: Req) -> Self::Future {
        match self
            .tally
            .count(req.tenant(), self.limits.drop_pct(req.tenant()))
        {
            Refusal::Admit => ResponseFuture::Inner {
                future: self.inner.call(req),
            },
            Refusal::Refuse => ResponseFuture::Refused,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TenantQuotaError<E> {
    #[error("tenant over quota")]
    Throttled,
    #[error(transparent)]
    Inner(E),
}

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
{
    type Output = Result<T, TenantQuotaError<E>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.project() {
            ResponseProj::Inner { future } => future.poll(cx).map_err(TenantQuotaError::Inner),
            ResponseProj::Refused => Poll::Ready(Err(TenantQuotaError::Throttled)),
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

    #[derive(Default)]
    struct Fixed {
        shares: Mutex<HashMap<&'static str, f64>>,
        banked: Mutex<Vec<(String, Duration)>>,
    }

    impl Fixed {
        fn at(shares: &[(&'static str, f64)]) -> Arc<Fixed> {
            let shares = Mutex::new(shares.iter().copied().collect());
            Arc::new(Fixed {
                shares,
                ..Fixed::default()
            })
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
            self.shares
                .lock()
                .unwrap()
                .get(tenant)
                .copied()
                .unwrap_or(0.0)
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

    async fn run(limits: Arc<Fixed>, tenant: &'static str, n: usize) -> Vec<bool> {
        let mut svc = EnforcerLayer::new(limits).layer(Echo);
        let mut refused = Vec::new();
        for _ in 0..n {
            let call = svc.ready().await.unwrap().call(Req(tenant));
            refused.push(match call.await {
                Ok(_) => false,
                Err(e) => matches!(e, TenantQuotaError::Throttled),
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

    #[tokio::test]
    async fn the_refused_count_is_the_share_of_the_traffic() {
        for (share, n, want) in [
            (0.25, 100, 25),
            (0.1, 100, 10),
            (0.75, 40, 30),
            (0.3, 10, 3),
        ] {
            let limits = Fixed::at(&[("Alice", share)]);
            let refused = run(limits, "Alice", n).await.iter().filter(|&&r| r).count();
            assert_eq!(refused, want, "{n} requests at a share of {share}");
        }
    }

    #[tokio::test]
    async fn tenants_are_charged_separately() {
        let limits = Fixed::at(&[("Alice", 1.0), ("Bob", 0.0)]);
        let mut svc = EnforcerLayer::new(limits).layer(Echo);
        for tenant in ["Alice", "Bob", "Alice", "Bob"] {
            let refused = svc.ready().await.unwrap().call(Req(tenant)).await.is_err();
            assert_eq!(refused, tenant == "Alice", "{tenant}");
        }
    }

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
        assert_eq!(
            refused,
            vec![true, true, true, true, false, false, false, false]
        );

        limits.set("Alice", 0.5);
        let mut refused = Vec::new();
        for _ in 0..4 {
            refused.push(svc.ready().await.unwrap().call(Req("Alice")).await.is_err());
        }
        assert_eq!(
            refused,
            vec![false, true, false, true],
            "counted from the change, not from zero"
        );
    }

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
