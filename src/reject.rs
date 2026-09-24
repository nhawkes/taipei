use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use pin_project_lite::pin_project;
use tower::{Layer, Service};

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
        RejectionService {
            inner,
            ready: false,
        }
    }
}

pub struct RejectionService<S> {
    inner: S,
    ready: bool,
}

impl<S: Clone> Clone for RejectionService<S> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            ready: false,
        }
    }
}

impl<S, Req> Service<Req> for RejectionService<S>
where
    S: Service<Req>,
{
    type Response = S::Response;
    type Error = RejectError<S::Error>;
    type Future = ResponseFuture<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.ready = match self.inner.poll_ready(cx) {
            Poll::Ready(Ok(())) => true,
            Poll::Ready(Err(e)) => return Poll::Ready(Err(RejectError::Inner(e))),
            Poll::Pending => false,
        };
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Req) -> Self::Future {
        if std::mem::take(&mut self.ready) {
            ResponseFuture::Inner {
                future: self.inner.call(req),
            }
        } else {
            ResponseFuture::Rejected
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RejectError<E> {
    #[error("service overloaded")]
    Overloaded,
    #[error(transparent)]
    Inner(E),
}

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
{
    type Output = Result<T, RejectError<E>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.project() {
            ResponseProj::Inner { future } => future.poll(cx).map_err(RejectError::Inner),
            ResponseProj::Rejected => Poll::Ready(Err(RejectError::Overloaded)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::task::Poll;
    use tower::{Service, ServiceExt};

    #[derive(Clone)]
    struct Gate {
        ready: std::rc::Rc<std::cell::Cell<bool>>,
    }
    impl Service<u32> for Gate {
        type Response = u32;
        type Error = std::convert::Infallible;
        type Future = std::future::Ready<Result<u32, std::convert::Infallible>>;
        fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            if self.ready.get() {
                Poll::Ready(Ok(()))
            } else {
                Poll::Pending
            }
        }
        fn call(&mut self, req: u32) -> Self::Future {
            std::future::ready(Ok(req * 2))
        }
    }

    #[tokio::test]
    async fn admits_when_inner_ready() {
        let gate = Gate {
            ready: std::rc::Rc::new(std::cell::Cell::new(true)),
        };
        let mut svc = RejectionLayer::new().layer(gate);
        let resp = svc.ready().await.unwrap().call(21).await.unwrap();
        assert_eq!(resp, 42);
    }

    #[tokio::test]
    async fn rejects_when_inner_not_ready() {
        let gate = Gate {
            ready: std::rc::Rc::new(std::cell::Cell::new(false)),
        };
        let mut svc = RejectionLayer::new().layer(gate);
        let svc = svc.ready().await.unwrap();
        let err = svc.call(21).await.unwrap_err();
        assert!(matches!(err, RejectError::Overloaded));
    }

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
        assert!(
            matches!(err, RejectError::Inner(Broke)),
            "the inner error, not Overloaded"
        );
    }
}
