//! The request queue you get when you don't choose one.
//!
//! A bare concurrency limit parks its callers: `poll_ready` returns `Pending` while
//! every slot is taken, and tokio's semaphore hands slots out in arrival order. That
//! waiter list is a **request queue** — accepted requests, not runnable, waiting for
//! permission — it is simply one nobody picked the shape of. Nothing bounds its depth
//! and nothing puts a clock on it.
//!
//! This makes that queue an object, so the visualiser has something to draw and the
//! reader can watch it grow. Behaviour is the same: arrival order, no ceiling, no
//! deadline, and the only way out is to be admitted or for the client to give up.
//!
//! It is deliberately **not** in `taipei`. Shipping an unbounded, untimed queue as a
//! library primitive would be shipping the mistake the chapter is about;
//! [`taipei::queue`] is the one worth using, and it differs by having a deadline.

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use thiserror::Error;
use tokio::runtime::Handle;
use tokio::sync::{mpsc, oneshot};
use tokio_util::task::{AbortOnDropHandle, TaskTracker};
use tower::{Service, ServiceExt};

/// Neither variant is an overload outcome — an unbounded queue has none. Both mean the
/// machine went away underneath a request.
#[derive(Debug, Error)]
pub enum UnboundedQueueError {
    #[error("queue worker dropped")]
    WorkerDropped,
    #[error("queued request dropped")]
    RequestDropped,
}

struct Item<Req, Resp> {
    req: Req,
    tx: oneshot::Sender<AbortOnDropHandle<Resp>>,
}

/// Builds the pair: the cloneable front every caller holds, and the worker that must be
/// driven for anything to leave the queue.
pub struct UnboundedQueue;

impl UnboundedQueue {
    pub fn build<S, Req, Resp>(
        inner: S,
        handle: Handle,
    ) -> (UnboundedQueueService<Req, Resp>, UnboundedQueueWorker<S, Req, Resp>)
    where
        S: Service<Req, Response = Resp, Error = Infallible>,
        S::Future: Send + 'static,
        Req: Send + 'static,
        Resp: Send + 'static,
    {
        // Unbounded is the point: there is no capacity to reach, so a caller is never
        // turned away at the door — it only ever waits longer.
        let (tx, rx) = mpsc::unbounded_channel();
        (
            UnboundedQueueService { tx },
            UnboundedQueueWorker { rx, service: inner, handle, in_flight: TaskTracker::new() },
        )
    }
}

// --- worker ---

pub struct UnboundedQueueWorker<S, Req, Resp> {
    rx: mpsc::UnboundedReceiver<Item<Req, Resp>>,
    service: S,
    handle: Handle,
    in_flight: TaskTracker,
}

impl<S, Req, Resp> UnboundedQueueWorker<S, Req, Resp>
where
    S: Service<Req, Response = Resp, Error = Infallible>,
    S::Future: Send + 'static,
    Req: Send + 'static,
    Resp: Send + 'static,
{
    pub async fn serve(mut self) {
        while let Some(item) = self.rx.recv().await {
            // A caller that has already given up frees its place without taking a slot.
            if item.tx.is_closed() {
                continue;
            }
            // The only wait: readiness from the limit below. With no deadline racing it
            // there is nothing here that can end the wait early.
            let svc = match self.service.ready().await {
                Ok(svc) => svc,
                Err(e) => match e {},
            };
            let fut = svc.call(item.req);
            let handle = AbortOnDropHandle::new(self.in_flight.spawn_on(
                async move {
                    match fut.await {
                        Ok(resp) => resp,
                        Err(e) => match e {},
                    }
                },
                &self.handle,
            ));
            let _ = item.tx.send(handle);
        }
        self.in_flight.close();
        self.in_flight.wait().await;
    }
}

// --- service ---

pub struct UnboundedQueueService<Req, Resp> {
    tx: mpsc::UnboundedSender<Item<Req, Resp>>,
}

impl<Req, Resp> Clone for UnboundedQueueService<Req, Resp> {
    fn clone(&self) -> Self {
        Self { tx: self.tx.clone() }
    }
}

impl<Req, Resp> Service<Req> for UnboundedQueueService<Req, Resp>
where
    Req: Send + 'static,
    Resp: Send + 'static,
{
    type Response = Resp;
    type Error = UnboundedQueueError;
    type Future = UnboundedQueueFuture<Resp>;

    /// Always ready. An unbounded queue has no back pressure to apply — which is the
    /// property being demonstrated, not an oversight.
    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Req) -> Self::Future {
        let (tx, rx) = oneshot::channel();
        match self.tx.send(Item { req, tx }) {
            Ok(()) => UnboundedQueueFuture::Queued(Box::pin(rx)),
            Err(_) => UnboundedQueueFuture::Failed(Some(UnboundedQueueError::WorkerDropped)),
        }
    }
}

// --- future ---

pub enum UnboundedQueueFuture<Resp> {
    /// Waiting its turn: the worker has not admitted it yet.
    Queued(Pin<Box<oneshot::Receiver<AbortOnDropHandle<Resp>>>>),
    /// Admitted and running.
    Running(Pin<Box<AbortOnDropHandle<Resp>>>),
    /// Never queued at all — taken on the first poll, so the error moves out once.
    Failed(Option<UnboundedQueueError>),
}

impl<Resp> Future for UnboundedQueueFuture<Resp> {
    type Output = Result<Resp, UnboundedQueueError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        loop {
            match &mut *self {
                UnboundedQueueFuture::Queued(rx) => match rx.as_mut().poll(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Ok(handle)) => {
                        *self = UnboundedQueueFuture::Running(Box::pin(handle));
                    }
                    Poll::Ready(Err(_)) => {
                        return Poll::Ready(Err(UnboundedQueueError::RequestDropped))
                    }
                },
                UnboundedQueueFuture::Running(handle) => {
                    return match std::task::ready!(handle.as_mut().poll(cx)) {
                        Ok(resp) => Poll::Ready(Ok(resp)),
                        Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
                        Err(_) => Poll::Ready(Err(UnboundedQueueError::RequestDropped)),
                    };
                }
                UnboundedQueueFuture::Failed(e) => {
                    return Poll::Ready(Err(e.take().unwrap_or(UnboundedQueueError::RequestDropped)));
                }
            }
        }
    }
}
