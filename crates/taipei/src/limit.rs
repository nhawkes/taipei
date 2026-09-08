//! A concurrency limit whose ceiling can change at runtime.
//!
//! [`DynamicConcurrencyLimitLayer`] caps the number of in-flight requests, like
//! `tower`'s fixed `ConcurrencyLimit`, but the cap can be raised or lowered live
//! through a shared [`ConcurrencyLimit`] handle — the building block for adaptive
//! limits (AIMD, gradient, latency-driven) that tune capacity to observed load.
//!
//! The hard part is lowering the limit while requests are in flight: you can't
//! claw back a permit a running request already holds. So the limit is the pair
//! (semaphore, `debt`): lowering the limit forgets free permits first, and books
//! the shortfall as **debt** — an atomic counter of permits still owed. As each
//! in-flight request finishes, its returned permit cancels a unit of debt instead
//! of freeing a slot, until the live capacity has converged on the new limit.
//! Raising the limit cancels any outstanding debt first, then frees real slots.
//!
//! Because it is built only from a `tokio::sync::Semaphore` and atomics — no
//! runtime instrumentation, no blocking — it behaves identically on a
//! multi-threaded server and in a single-threaded wasm executor.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};

use pin_project_lite::pin_project;
use tokio_util::sync::PollSemaphore;
use tower::{Layer, Service};

use crate::debt::DebtSemaphore;

// --- shared state ---

struct State {
    /// The dynamic-capacity core. Available permits are free slots; a running
    /// request holds one (forgotten on `call`, handed back via `sem.release()`
    /// on completion). Lowering the limit below the in-flight count is absorbed
    /// as debt and repaid as requests finish.
    sem: DebtSemaphore,
    /// The current target ceiling.
    limit: AtomicUsize,
}

impl State {
    fn new(limit: usize) -> Self {
        Self {
            sem: DebtSemaphore::new(limit),
            limit: AtomicUsize::new(limit),
        }
    }

    /// Move capacity toward `new` — `release` (grow) or `acquire` (shrink) one
    /// unit at a time, letting the debt semaphore reconcile against held permits.
    fn set_limit(&self, new: usize) {
        let old = self.limit.swap(new, Ordering::AcqRel);
        if new > old {
            for _ in 0..new - old { self.sem.release(); }
        } else if new < old {
            for _ in 0..old - new { self.sem.acquire(); }
        }
    }
}

// --- handle ---

/// A cloneable handle to a live concurrency limit. Clones share the same limit,
/// so one task can tune the ceiling while another enforces it via the layer.
#[derive(Clone)]
pub struct ConcurrencyLimit {
    state: Arc<State>,
}

impl ConcurrencyLimit {
    pub fn new(limit: usize) -> Self {
        Self { state: Arc::new(State::new(limit)) }
    }

    /// Set the ceiling. Increases free slots immediately; decreases take effect
    /// as in-flight requests finish (see the module docs on debt).
    pub fn set(&self, limit: usize) {
        self.state.set_limit(limit);
    }

    /// The current ceiling.
    pub fn get(&self) -> usize {
        self.state.limit.load(Ordering::Acquire)
    }

    /// Free slots a new request could take right now.
    pub fn available(&self) -> usize {
        self.state.sem.available()
    }

    /// Permits owed because the limit was lowered below the in-flight count.
    pub fn debt(&self) -> usize {
        self.state.sem.debt()
    }
}

// --- Layer ---

/// Wraps a service in a concurrency limit that can be retuned at runtime.
#[derive(Clone)]
pub struct DynamicConcurrencyLimitLayer {
    limit: ConcurrencyLimit,
}

impl DynamicConcurrencyLimitLayer {
    pub fn new(limit: usize) -> Self {
        Self { limit: ConcurrencyLimit::new(limit) }
    }

    /// Build a layer enforcing an existing, externally-tunable limit.
    pub fn from_handle(limit: ConcurrencyLimit) -> Self {
        Self { limit }
    }

    /// A handle to tune (or inspect) this layer's limit from elsewhere.
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

// --- Service ---

pub struct DynamicConcurrencyLimitService<S> {
    inner: S,
    poll_sem: PollSemaphore,
    state: Arc<State>,
    /// The slot this service reserved in `poll_ready` and has not yet spent. It is the
    /// debt-aware guard, not tokio's permit, so dropping the service returns it the same
    /// way finishing a request does.
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
                // Forget the permit's automatic return the moment the slot is reserved,
                // and hold the debt-aware guard in its place. From here `Slot`'s drop is
                // the *only* way this capacity comes back — so a reservation that is
                // never spent (a clone dropped between `poll_ready` and `call`, a stack
                // swapped out under load) is reclaimed against the debt exactly like a
                // completed request, rather than re-growing the pool behind a lowered
                // ceiling.
                Poll::Ready(Some(permit)) => {
                    permit.forget();
                    self.permit = Some(Slot { state: Arc::clone(&self.state) });
                }
            }
        }
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Req) -> Self::Future {
        let slot = self.permit.take().expect("poll_ready should be called before call");
        ResponseFuture { inner: self.inner.call(req), slot }
    }
}

// --- slot guard + future ---

/// Holds a slot for the lifetime of a request; releases it (debt-aware) on drop,
/// covering both normal completion and cancellation.
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
        // `slot` is released when this future is dropped — right after it
        // resolves, or on cancellation.
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
        // Simulate two in-flight requests holding both slots.
        let p1 = Arc::clone(&s.sem.semaphore).try_acquire_owned().unwrap();
        let p2 = Arc::clone(&s.sem.semaphore).try_acquire_owned().unwrap();
        p1.forget();
        p2.forget();
        assert_eq!(s.sem.available(), 0);

        // Drop the ceiling to 0 while 2 are in flight → all of it becomes debt.
        s.set_limit(0);
        assert_eq!(s.sem.debt(), 2);
        assert_eq!(s.sem.available(), 0);

        // Each completion cancels debt rather than freeing a slot…
        s.sem.release();
        assert_eq!(s.sem.debt(), 1);
        assert_eq!(s.sem.available(), 0);
        s.sem.release();
        assert_eq!(s.sem.debt(), 0);
        assert_eq!(s.sem.available(), 0);

        // …so capacity has converged on the new limit; raising it frees slots.
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
        // First request takes the only slot and never completes.
        let _held = first.ready().await.unwrap().call(());

        // A second service sharing the limit cannot become ready.
        let mut second = layer.layer(HoldForever);
        let pending = tokio::time::timeout(Duration::from_millis(50), second.ready()).await;
        assert!(pending.is_err(), "limit of 1 should keep the second request waiting");

        // Raising the limit frees a slot, so it becomes ready.
        layer.handle().set(2);
        let admitted = tokio::time::timeout(Duration::from_millis(50), second.ready()).await;
        assert!(admitted.is_ok(), "raising the limit should admit the waiter");
    }

    /// A slot reserved by `poll_ready` and never spent must come back the debt-aware way.
    /// A service can be readied and then dropped without ever being called — a clone that
    /// loses a race, a stack swapped out under load — and if that capacity returned
    /// through tokio's own permit it would be added straight back to the semaphore,
    /// re-growing the pool behind a ceiling that had already been lowered.
    #[tokio::test]
    async fn an_unspent_reservation_is_returned_against_the_debt() {
        let limit = ConcurrencyLimit::new(2);
        let layer = DynamicConcurrencyLimitLayer::from_handle(limit.clone());

        // Reserve both slots without spending either.
        let mut a = layer.layer(HoldForever);
        let mut b = layer.layer(HoldForever);
        a.ready().await.unwrap();
        b.ready().await.unwrap();
        assert_eq!(limit.available(), 0, "both slots are reserved");

        // Lower the ceiling to nothing: neither slot is back yet, so both are owed.
        limit.set(0);
        assert_eq!(limit.get(), 0);
        assert_eq!(limit.debt(), 2, "two reserved slots are owed to the lowered ceiling");

        // Dropping the services returns what they reserved — as debt, not as capacity.
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
