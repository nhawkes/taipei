use pin_project_lite::pin_project;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio_util::sync::PollSemaphore;
use tower::{Layer, Service};

pub use crate::debt::DebtSemaphore;

// --- RuntimeInstrumentation ---

/// Opaque handle to runtime instrumentation. Pass to [`CpuBackpressureLayer::new`].
#[derive(Clone)]
pub struct RuntimeInstrumentation {
    pub(crate) semaphore: Arc<DebtSemaphore>,
}

impl RuntimeInstrumentation {
    pub fn new(semaphore: Arc<DebtSemaphore>) -> Self {
        Self { semaphore }
    }

    pub fn debt(&self) -> usize {
        self.semaphore.debt()
    }

    /// Admissions the gate would grant right now — idle cores less the reserve.
    pub fn available(&self) -> usize {
        self.semaphore.available()
    }
}

// --- InstrumentedRuntime ---

pub trait InstrumentedRuntime {
    fn instrumentation(&self) -> RuntimeInstrumentation;
}

// --- Layer ---

#[derive(Clone)]
pub struct CpuBackpressureLayer {
    semaphore: Arc<DebtSemaphore>,
}

impl CpuBackpressureLayer {
    pub fn new(instr: &RuntimeInstrumentation) -> Self {
        Self { semaphore: Arc::clone(&instr.semaphore) }
    }
}

impl<S> Layer<S> for CpuBackpressureLayer {
    type Service = CpuBackpressureService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        CpuBackpressureService {
            inner,
            semaphore: Arc::clone(&self.semaphore),
            poll_sem: PollSemaphore::new(Arc::clone(&self.semaphore.semaphore)),
            permit: None,
        }
    }
}

// --- Service ---

pub struct CpuBackpressureService<S> {
    inner: S,
    semaphore: Arc<DebtSemaphore>,
    poll_sem: PollSemaphore,
    /// The admission this service reserved in `poll_ready` and has not yet spent — the
    /// debt-aware guard, not tokio's permit, so dropping the service repays it the same
    /// way finishing a request does.
    permit: Option<Admission>,
}

impl<S: Clone> Clone for CpuBackpressureService<S> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            semaphore: Arc::clone(&self.semaphore),
            poll_sem: self.poll_sem.clone(),
            permit: None,
        }
    }
}

impl<S, Req> Service<Req> for CpuBackpressureService<S>
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
                Poll::Ready(None) => panic!("semaphore should not be closed"),
                // Hand the admission to the debt semaphore the moment it is reserved,
                // not when it is spent: the permit's own `Drop` calls `add_permits`,
                // which would mint capacity while a core is owed one. Holding an
                // `Admission` instead means a reservation this service never spends is
                // repaid like any other, rather than growing the pool.
                Poll::Ready(Some(permit)) => {
                    permit.forget();
                    self.permit = Some(Admission(Arc::clone(&self.semaphore)));
                }
            }
        }
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Req) -> Self::Future {
        let admission = self.permit.take().expect("poll_ready should be called before call");
        ResponseFuture { inner: self.inner.call(req), admission: Some(admission) }
    }
}

// --- ResponseFuture ---

/// Holds the admission until the request's first poll hands it to the runtime;
/// releases debt-aware on drop, which also covers cancellation before that poll.
struct Admission(Arc<DebtSemaphore>);

impl Drop for Admission {
    fn drop(&mut self) {
        self.0.release();
    }
}

pin_project! {
    pub struct ResponseFuture<F> {
        #[pin]
        inner: F,
        admission: Option<Admission>,
    }
}

impl<F: Future> Future for ResponseFuture<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let result = this.inner.poll(cx);
        // Released after the first poll — the request is now in the runtime's hands,
        // and its cores show up as busy through park/unpark instead.
        drop(this.admission.take());
        result
    }
}
