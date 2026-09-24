use pin_project_lite::pin_project;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio_util::sync::PollSemaphore;
use tower::{Layer, Service};

pub use crate::debt::DebtSemaphore;

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

    pub fn available(&self) -> usize {
        self.semaphore.available()
    }
}

pub trait InstrumentedRuntime {
    fn instrumentation(&self) -> RuntimeInstrumentation;
}

#[derive(Clone)]
pub struct CpuBackpressureLayer {
    semaphore: Arc<DebtSemaphore>,
}

impl CpuBackpressureLayer {
    pub fn new(instr: &RuntimeInstrumentation) -> Self {
        Self {
            semaphore: Arc::clone(&instr.semaphore),
        }
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

pub struct CpuBackpressureService<S> {
    inner: S,
    semaphore: Arc<DebtSemaphore>,
    poll_sem: PollSemaphore,
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
                Poll::Ready(Some(permit)) => {
                    permit.forget();
                    self.permit = Some(Admission(Arc::clone(&self.semaphore)));
                }
            }
        }
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Req) -> Self::Future {
        let admission = self
            .permit
            .take()
            .expect("poll_ready should be called before call");
        ResponseFuture {
            inner: self.inner.call(req),
            admission: Some(admission),
        }
    }
}

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
        drop(this.admission.take());
        result
    }
}
