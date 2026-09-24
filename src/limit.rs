use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};

use pin_project_lite::pin_project;
use tokio_util::sync::PollSemaphore;
use tower::{Layer, Service};

use crate::debt::DebtSemaphore;

struct State {
    sem: DebtSemaphore,
    limit: AtomicUsize,
}

impl State {
    fn new(limit: usize) -> Self {
        Self {
            sem: DebtSemaphore::new(limit),
            limit: AtomicUsize::new(limit),
        }
    }

    fn set_limit(&self, new: usize) {
        let old = self.limit.swap(new, Ordering::AcqRel);
        if new > old {
            for _ in 0..new - old {
                self.sem.release();
            }
        } else if new < old {
            for _ in 0..old - new {
                self.sem.acquire();
            }
        }
    }
}

#[derive(Clone)]
pub struct ConcurrencyLimit {
    state: Arc<State>,
}

impl ConcurrencyLimit {
    pub fn new(limit: usize) -> Self {
        Self {
            state: Arc::new(State::new(limit)),
        }
    }

    pub fn set(&self, limit: usize) {
        self.state.set_limit(limit);
    }

    pub fn get(&self) -> usize {
        self.state.limit.load(Ordering::Acquire)
    }

    pub fn available(&self) -> usize {
        self.state.sem.available()
    }

    pub fn debt(&self) -> usize {
        self.state.sem.debt()
    }
}

#[derive(Clone)]
pub struct DynamicConcurrencyLimitLayer {
    limit: ConcurrencyLimit,
}

impl DynamicConcurrencyLimitLayer {
    pub fn new(limit: usize) -> Self {
        Self {
            limit: ConcurrencyLimit::new(limit),
        }
    }

    pub fn from_handle(limit: ConcurrencyLimit) -> Self {
        Self { limit }
    }

    pub fn handle(&self) -> ConcurrencyLimit {
        self.limit.clone()
    }
}

impl<S> Layer<S> for DynamicConcurrencyLimitLayer {
    type Service = DynamicConcurrencyLimitService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        DynamicConcurrencyLimitService {
            inner,
            poll_sem: PollSemaphore::new(Arc::clone(&self.limit.state.sem.semaphore)),
            state: Arc::clone(&self.limit.state),
            permit: None,
        }
    }
}

pub struct DynamicConcurrencyLimitService<S> {
    inner: S,
    poll_sem: PollSemaphore,
    state: Arc<State>,
    permit: Option<Slot>,
}

impl<S: Clone> Clone for DynamicConcurrencyLimitService<S> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            poll_sem: self.poll_sem.clone(),
            state: Arc::clone(&self.state),
            permit: None,
        }
    }
}

impl<S, Req> Service<Req> for DynamicConcurrencyLimitService<S>
where
    S: Service<Req>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = ResponseFuture<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        if self.permit.is_none() {
            match self.poll_sem.poll_acquire(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => panic!("concurrency-limit semaphore should not be closed"),
                Poll::Ready(Some(permit)) => {
                    permit.forget();
                    self.permit = Some(Slot {
                        state: Arc::clone(&self.state),
                    });
                }
            }
        }
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Req) -> Self::Future {
        let slot = self
            .permit
            .take()
            .expect("poll_ready should be called before call");
        ResponseFuture {
            inner: self.inner.call(req),
            slot,
        }
    }
}

struct Slot {
    state: Arc<State>,
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.state.sem.release();
    }
}

pin_project! {
    pub struct ResponseFuture<F> {
        #[pin]
        inner: F,
        slot: Slot,
    }
}

impl<F: Future> Future for ResponseFuture<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.project().inner.poll(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::convert::Infallible;
    use std::time::Duration;
    use tower::{Service, ServiceExt};

    #[test]
    fn set_limit_grows_and_shrinks_free_capacity() {
        let s = State::new(3);
        assert_eq!(s.sem.available(), 3);
        s.set_limit(5);
        assert_eq!(s.sem.available(), 5);
        s.set_limit(2);
        assert_eq!(s.sem.available(), 2);
    }

    #[test]
    fn lowering_below_in_flight_books_debt_then_repays_on_completion() {
        let s = State::new(2);
        let p1 = Arc::clone(&s.sem.semaphore).try_acquire_owned().unwrap();
        let p2 = Arc::clone(&s.sem.semaphore).try_acquire_owned().unwrap();
        p1.forget();
        p2.forget();
        assert_eq!(s.sem.available(), 0);

        s.set_limit(0);
        assert_eq!(s.sem.debt(), 2);
        assert_eq!(s.sem.available(), 0);

        s.sem.release();
        assert_eq!(s.sem.debt(), 1);
        assert_eq!(s.sem.available(), 0);
        s.sem.release();
        assert_eq!(s.sem.debt(), 0);
        assert_eq!(s.sem.available(), 0);

        s.set_limit(1);
        assert_eq!(s.sem.available(), 1);
    }

    #[derive(Clone)]
    struct HoldForever;
    impl Service<()> for HoldForever {
        type Response = ();
        type Error = Infallible;
        type Future = Pin<Box<dyn Future<Output = Result<(), Infallible>> + Send>>;
        fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
        fn call(&mut self, _: ()) -> Self::Future {
            Box::pin(std::future::pending())
        }
    }

    #[tokio::test]
    async fn enforces_limit_and_admits_when_raised() {
        let layer = DynamicConcurrencyLimitLayer::new(1);
        let mut first = layer.layer(HoldForever);
        let _held = first.ready().await.unwrap().call(());

        let mut second = layer.layer(HoldForever);
        let pending = tokio::time::timeout(Duration::from_millis(50), second.ready()).await;
        assert!(
            pending.is_err(),
            "limit of 1 should keep the second request waiting"
        );

        layer.handle().set(2);
        let admitted = tokio::time::timeout(Duration::from_millis(50), second.ready()).await;
        assert!(
            admitted.is_ok(),
            "raising the limit should admit the waiter"
        );
    }

    #[tokio::test]
    async fn an_unspent_reservation_is_returned_against_the_debt() {
        let limit = ConcurrencyLimit::new(2);
        let layer = DynamicConcurrencyLimitLayer::from_handle(limit.clone());

        let mut a = layer.layer(HoldForever);
        let mut b = layer.layer(HoldForever);
        a.ready().await.unwrap();
        b.ready().await.unwrap();
        assert_eq!(limit.available(), 0, "both slots are reserved");

        limit.set(0);
        assert_eq!(limit.get(), 0);
        assert_eq!(
            limit.debt(),
            2,
            "two reserved slots are owed to the lowered ceiling"
        );

        drop(a);
        drop(b);
        assert_eq!(limit.debt(), 0, "the returned slots paid the debt off");
        assert_eq!(
            limit.available(),
            0,
            "a ceiling of zero admits nothing: {} permits appeared from a limit of {}",
            limit.available(),
            limit.get()
        );
    }
}
