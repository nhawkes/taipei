//! Immediate load-shed: reject a request the moment the inner service can't take
//! it, instead of making the caller wait.
//!
//! [`RejectionLayer`] is the "no queue" answer to overload. It always reports
//! itself ready (so the caller is never parked), but remembers whether the inner
//! service was actually ready when probed. If a request arrives while the inner
//! service is withholding readiness — e.g. behind a
//! [`CpuBackpressureService`](crate::backpressure::CpuBackpressureService) with
//! every core busy — the request is rejected straight away with
//! [`Overloaded`] so the client can retry elsewhere. Pair it with a
//! [`QueueLayer`](crate::queue::QueueLayer) instead when you'd rather let the
//! request wait a little for a core to free up.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use pin_project_lite::pin_project;
use tower::{Layer, Service};

/// Boxed error so the rejection error and the inner service's error share a type.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

// --- Layer ---

/// Wraps a service so that requests are rejected immediately while the inner
/// service is not ready, rather than queued or awaited.
#[derive(Clone, Copy, Default)]
pub struct RejectionLayer;

impl RejectionLayer {
    pub fn new() -> Self {
        Self
    }
}

impl<S> Layer<S> for RejectionLayer {
    type Service = RejectionService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        RejectionService { inner, ready: false }
    }
}

// --- Service ---

pub struct RejectionService<S> {
    inner: S,
    /// Whether `inner` reported ready on the most recent `poll_ready`.
    ready: bool,
}

impl<S: Clone> Clone for RejectionService<S> {
    fn clone(&self) -> Self {
        Self { inner: self.inner.clone(), ready: false }
    }
}

impl<S, Req> Service<Req> for RejectionService<S>
where
    S: Service<Req>,
    S::Error: Into<BoxError>,
{
    type Response = S::Response;
    type Error = BoxError;
    type Future = ResponseFuture<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // Never park the caller: report ready unconditionally, but remember whether the
        // inner could actually take work. `Pending` is backpressure — a request that
        // arrives now is shed. `Ready(Err)` is a broken inner, not backpressure, so it
        // surfaces as itself rather than being reclassified as an overload shed.
        self.ready = match self.inner.poll_ready(cx) {
            Poll::Ready(Ok(())) => true,
            Poll::Ready(Err(e)) => return Poll::Ready(Err(e.into())),
            Poll::Pending => false,
        };
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Req) -> Self::Future {
        if std::mem::take(&mut self.ready) {
            // Inner was ready when probed — admit the request.
            ResponseFuture::Inner { future: self.inner.call(req) }
        } else {
            // Inner was withholding readiness — shed the request immediately.
            ResponseFuture::Rejected
        }
    }
}

// --- Error ---

/// Returned when a request is rejected because the inner service was not ready.
#[derive(Debug, thiserror::Error)]
#[error("service overloaded; request rejected without queueing")]
pub struct Overloaded;

// --- Future ---

pin_project! {
    #[project = ResponseProj]
    pub enum ResponseFuture<F> {
        Inner { #[pin] future: F },
        Rejected,
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
            ResponseProj::Rejected => Poll::Ready(Err(Box::new(Overloaded) as BoxError)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::task::Poll;
    use tower::{ServiceExt, Service};

    /// A service whose readiness we control, to drive the reject decision.
    #[derive(Clone)]
    struct Gate {
        ready: std::rc::Rc<std::cell::Cell<bool>>,
    }
    impl Service<u32> for Gate {
        type Response = u32;
        type Error = std::convert::Infallible;
        type Future = std::future::Ready<Result<u32, std::convert::Infallible>>;
        fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            if self.ready.get() { Poll::Ready(Ok(())) } else { Poll::Pending }
        }
        fn call(&mut self, req: u32) -> Self::Future {
            std::future::ready(Ok(req * 2))
        }
    }

    #[tokio::test]
    async fn admits_when_inner_ready() {
        let gate = Gate { ready: std::rc::Rc::new(std::cell::Cell::new(true)) };
        let mut svc = RejectionLayer::new().layer(gate);
        let resp = svc.ready().await.unwrap().call(21).await.unwrap();
        assert_eq!(resp, 42);
    }

    #[tokio::test]
    async fn rejects_when_inner_not_ready() {
        let gate = Gate { ready: std::rc::Rc::new(std::cell::Cell::new(false)) };
        let mut svc = RejectionLayer::new().layer(gate);
        // poll_ready resolves immediately (load-shed never parks the caller)…
        let svc = svc.ready().await.unwrap();
        // …but the call is shed because the inner gate was closed.
        let err = svc.call(21).await.unwrap_err();
        assert!(err.downcast_ref::<Overloaded>().is_some());
    }

    /// A broken inner is not backpressure: its error surfaces as itself, not as an
    /// overload shed.
    #[derive(Clone)]
    struct Broken;
    #[derive(Debug, thiserror::Error)]
    #[error("inner is broken")]
    struct Broke;
    impl Service<u32> for Broken {
        type Response = u32;
        type Error = Broke;
        type Future = std::future::Ready<Result<u32, Broke>>;
        fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Err(Broke))
        }
        fn call(&mut self, _: u32) -> Self::Future {
            std::future::ready(Err(Broke))
        }
    }

    #[tokio::test]
    async fn a_broken_inner_surfaces_its_error_not_a_shed() {
        let mut svc = RejectionLayer::new().layer(Broken);
        let err = match svc.ready().await {
            Ok(_) => panic!("a broken inner must not report ready-ok"),
            Err(e) => e,
        };
        assert!(err.downcast_ref::<Broke>().is_some(), "the inner error, not Overloaded");
        assert!(err.downcast_ref::<Overloaded>().is_none());
    }
}
