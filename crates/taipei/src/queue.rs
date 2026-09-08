use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use thiserror::Error;
use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
#[cfg(not(any(target_arch = "wasm32", feature = "virtual-clock")))]
use tokio::time::Instant;

#[cfg(any(target_arch = "wasm32", feature = "virtual-clock"))]
use crate::clock::Instant;
use tokio_util::sync::{PollSendError, PollSender};
use tokio_util::task::{AbortOnDropHandle, TaskTracker};
use tower::{Service, ServiceExt};

// ── platform-agnostic Sleep ───────────────────────────────────────────────────

#[pin_project::pin_project(project = SleepProj)]
enum Sleep {
    #[cfg(not(any(target_arch = "wasm32", feature = "virtual-clock")))]
    Tokio(#[pin] tokio::time::Sleep),
    #[cfg(any(target_arch = "wasm32", feature = "virtual-clock"))]
    Virtual(crate::clock::Sleep),
}

impl Sleep {
    #[cfg(not(any(target_arch = "wasm32", feature = "virtual-clock")))]
    fn until(deadline: Instant) -> Self { Sleep::Tokio(tokio::time::sleep_until(deadline)) }
    #[cfg(any(target_arch = "wasm32", feature = "virtual-clock"))]
    fn until(deadline: Instant) -> Self { Sleep::Virtual(crate::clock::sleep_until(deadline)) }

    #[cfg(not(any(target_arch = "wasm32", feature = "virtual-clock")))]
    fn duration(d: Duration) -> Self { Sleep::Tokio(tokio::time::sleep(d)) }
    #[cfg(any(target_arch = "wasm32", feature = "virtual-clock"))]
    fn duration(d: Duration) -> Self { Sleep::Virtual(crate::clock::sleep(d)) }
}

impl Future for Sleep {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        match self.project() {
            #[cfg(not(any(target_arch = "wasm32", feature = "virtual-clock")))]
            SleepProj::Tokio(s) => s.poll(cx),
            #[cfg(any(target_arch = "wasm32", feature = "virtual-clock"))]
            SleepProj::Virtual(s) => Pin::new(s).poll(cx),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────

pub const DEFAULT_CAPACITY: usize = 1 << 16;
pub const DEFAULT_TIMEOUT: Duration = Duration::from_millis(100);
pub const DEFAULT_QUEUE_TIMEOUT: Duration = DEFAULT_TIMEOUT;

// --- QueueTimeout ---

/// A cloneable, live handle to the queue's shed deadline. Every clone shares one
/// cell, so retuning it from anywhere changes the deadline stamped on subsequently
/// enqueued requests — the timeout counterpart to [`crate::limit::ConcurrencyLimit`].
/// Requests already waiting keep the deadline they were enqueued with.
#[derive(Clone)]
pub struct QueueTimeout(Arc<AtomicU64>);

impl QueueTimeout {
    pub fn new(timeout: Duration) -> Self {
        Self(Arc::new(AtomicU64::new(timeout.as_micros() as u64)))
    }

    pub fn set(&self, timeout: Duration) {
        self.0.store(timeout.as_micros() as u64, Ordering::Relaxed);
    }

    pub fn get(&self) -> Duration {
        Duration::from_micros(self.0.load(Ordering::Relaxed))
    }
}

// --- QueueError ---

#[derive(Debug, Error)]
pub enum QueueError {
    #[error("request waited {waited:?} in queue, exceeding the {limit:?} limit")]
    Timeout { waited: Duration, limit: Duration },
    #[error("request processed for {processed:?}, exceeding the {limit:?} limit")]
    ProcessingTimeout {
        processed: Duration,
        limit: Duration,
    },
    #[error("queue worker dropped")]
    ServerWorkerDropped,
    #[error("queued request dropped")]
    ServerRequestDropped,
}

// --- Internals ---

struct QueueItem<Req, Resp> {
    req: Req,
    tx: oneshot::Sender<Result<AbortOnDropHandle<Resp>, QueueError>>,
    enqueued_at: Instant,
    deadline: Instant,
    /// The deadline this request was enqueued with. Kept so a shed reports the limit
    /// the request actually raced, not whatever the live handle reads when it sheds —
    /// the two differ if the timeout was retuned while the request waited.
    limit: Duration,
}

// --- QueueLayer ---

pub struct QueueLayer {
    queue_timeout: QueueTimeout,
    processing_timeout: Option<Duration>,
    capacity: usize,
}

impl QueueLayer {
    pub fn new(queue_timeout: Duration) -> Self {
        Self {
            queue_timeout: QueueTimeout::new(queue_timeout),
            processing_timeout: None,
            capacity: DEFAULT_CAPACITY,
        }
    }

    pub fn with_capacity(mut self, capacity: usize) -> Self {
        self.capacity = capacity;
        self
    }

    pub fn with_processing_timeout(mut self, timeout: Duration) -> Self {
        self.processing_timeout = Some(timeout);
        self
    }

    /// A live handle to this layer's shed deadline — retune it after `build` to
    /// change the timeout of subsequently enqueued requests without rebuilding.
    pub fn timeout(&self) -> QueueTimeout {
        self.queue_timeout.clone()
    }

    pub fn build<S, Req, Resp>(
        self,
        inner: S,
        handle: Handle,
    ) -> (QueueService<Req, Resp>, QueueWorker<S, Req, Resp>)
    where
        S: Service<Req, Response = Resp, Error = Infallible>,
        S::Future: Send + 'static,
        Req: Send + 'static,
        Resp: Send + 'static,
    {
        let (tx, rx) = mpsc::channel::<QueueItem<Req, Resp>>(self.capacity);
        let sender = Sender {
            tx,
            queue_timeout: self.queue_timeout.clone(),
            processing_timeout: self.processing_timeout,
        };
        let worker = QueueWorker {
            rx,
            service: inner,
            handle,
            in_flight: TaskTracker::new(),
        };
        (QueueService::new(sender), worker)
    }
}

struct Sender<Req, Resp> {
    tx: mpsc::Sender<QueueItem<Req, Resp>>,
    queue_timeout: QueueTimeout,
    processing_timeout: Option<Duration>,
}

impl<Req, Resp> Clone for Sender<Req, Resp> {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
            queue_timeout: self.queue_timeout.clone(),
            processing_timeout: self.processing_timeout,
        }
    }
}

// --- QueueWorker ---

pub struct QueueWorker<S, Req, Resp> {
    rx: mpsc::Receiver<QueueItem<Req, Resp>>,
    service: S,
    handle: Handle,
    in_flight: TaskTracker,
}

impl<S, Req, Resp> QueueWorker<S, Req, Resp>
where
    S: Service<Req, Response = Resp, Error = Infallible>,
    S::Future: Send + 'static,
    Req: Send + 'static,
    Resp: Send + 'static,
{
    /// Admits one request, spawning its response future on the stored handle.
    /// Skips expired and abandoned items. Returns `false` when the channel closes.
    pub async fn serve_one(&mut self) -> bool {
        loop {
            let Some(item) = self.rx.recv().await else {
                return false;
            };
            if self.admit(item).await {
                return true;
            }
        }
    }

    pub async fn serve(mut self) {
        while self.serve_one().await {}
        self.in_flight.close();
        self.in_flight.wait().await;
    }

    async fn admit(&mut self, item: QueueItem<Req, Resp>) -> bool {
        if item.tx.is_closed() {
            return false;
        }
        if Instant::now() >= item.deadline {
            let _ = item.tx.send(Err(QueueError::Timeout {
                waited: item.enqueued_at.elapsed(),
                limit: item.limit,
            }));
            return false;
        }

        let svc = loop {
            tokio::select! {
                biased;
                _ = Sleep::until(item.deadline) => {
                    let _ = item.tx.send(Err(QueueError::Timeout {
                        waited: item.enqueued_at.elapsed(),
                        limit: item.limit,
                    }));
                    return false;
                },
                result = self.service.ready() => {
                    break match result {
                        Ok(svc) => svc,
                        Err(e) => match e {},
                    };
                },
            }
        };

        let fut = svc.call(item.req);
        let tx = item.tx;
        let handle = AbortOnDropHandle::new(self.in_flight.spawn_on(
            async move {
                match fut.await {
                    Ok(resp) => resp,
                    Err(e) => match e {},
                }
            },
            &self.handle,
        ));
        let _ = tx.send(Ok(handle));
        true
    }
}

// --- QueueService ---

pub struct QueueService<Req, Resp> {
    sender: Sender<Req, Resp>,
    poll_sender: PollSender<QueueItem<Req, Resp>>,
}

impl<Req, Resp> Clone for QueueService<Req, Resp>
where
    Req: Send + 'static,
    Resp: Send + 'static,
{
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
            poll_sender: PollSender::new(self.sender.tx.clone()),
        }
    }
}

impl<Req, Resp> QueueService<Req, Resp>
where
    Req: Send + 'static,
    Resp: Send + 'static,
{
    fn new(sender: Sender<Req, Resp>) -> Self {
        let poll_sender = PollSender::new(sender.tx.clone());
        Self {
            sender,
            poll_sender,
        }
    }
}

impl<Req, Resp> Service<Req> for QueueService<Req, Resp>
where
    Req: Send + 'static,
    Resp: Send + 'static,
{
    type Response = Resp;
    type Error = QueueError;
    type Future = QueueServiceFuture<Resp>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        match self.poll_sender.poll_reserve(cx) {
            Poll::Ready(Ok(())) => Poll::Ready(Ok(())),
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(PollSendError { .. })) => {
                Poll::Ready(Err(QueueError::ServerWorkerDropped))
            }
        }
    }

    fn call(&mut self, req: Req) -> Self::Future {
        let (tx, rx) = oneshot::channel();
        let enqueued_at = Instant::now();
        let limit = self.sender.queue_timeout.get();
        let deadline = enqueued_at + limit;

        let item = QueueItem {
            req,
            tx,
            enqueued_at,
            deadline,
            limit,
        };

        match self.poll_sender.send_item(item) {
            Ok(()) => QueueServiceFuture::queued(rx, self.sender.processing_timeout),
            Err(PollSendError { .. }) => QueueServiceFuture::error(QueueError::ServerWorkerDropped),
        }
    }
}

// --- QueueServiceFuture ---

pub struct QueueServiceFuture<Resp> {
    state: QueueServiceFutureState<Resp>,
}

enum QueueServiceFutureState<Resp> {
    Queued{
        handle: Pin<Box<oneshot::Receiver<Result<AbortOnDropHandle<Resp>, QueueError>>>>,
        processing_timeout: Option<Duration>,
    },
    Started {
        handle: Pin<Box<AbortOnDropHandle<Resp>>>,
        sleep: Option<ProcessingTimeout>,
        started_at: Instant,
    },
    Error(Option<QueueError>),
}

struct ProcessingTimeout {
    timeout: Duration,
    sleep: Pin<Box<Sleep>>,
}

impl<Resp> QueueServiceFuture<Resp> {
    fn queued(
        rx: oneshot::Receiver<Result<AbortOnDropHandle<Resp>, QueueError>>,
        processing_timeout: Option<Duration>,
    ) -> Self {
        Self {
            state: QueueServiceFutureState::Queued {
                handle: Box::pin(rx),
                processing_timeout,
            },
        }
    }

    fn error(error: QueueError) -> Self {
        Self {
            state: QueueServiceFutureState::Error(Some(error)),
        }
    }
}

impl<Resp> Future for QueueServiceFuture<Resp> {
    type Output = Result<Resp, QueueError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        loop {
            match &mut self.state {
                QueueServiceFutureState::Queued{
                    handle: rx,
                    processing_timeout,
                } => match rx.as_mut().poll(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Ok(Ok(handle))) => {
                        let started_at = Instant::now();
                        self.state = QueueServiceFutureState::Started {
                            handle: Box::pin(handle),
                            sleep: processing_timeout
                                .map(|timeout| ProcessingTimeout {
                                    timeout,
                                    sleep: Box::pin(Sleep::duration(timeout)),
                                }),
                            started_at,
                        };
                    }
                    Poll::Ready(Ok(Err(error))) => return Poll::Ready(Err(error)),
                    Poll::Ready(Err(_closed)) => {
                        return Poll::Ready(Err(QueueError::ServerRequestDropped));
                    }
                }
                QueueServiceFutureState::Started {
                    handle,
                    sleep,
                    started_at,
                } => {
                    if let Poll::Ready(result) = handle.as_mut().poll(cx) {
                        return match result {
                            Ok(resp) => Poll::Ready(Ok(resp)),
                            Err(err) if err.is_panic() => std::panic::resume_unwind(err.into_panic()),
                            Err(_cancelled) => Poll::Ready(Err(QueueError::ServerRequestDropped)),
                        };
                    }
                    if let Some(sleep) = sleep {
                        if sleep.sleep.as_mut().poll(cx).is_ready() {
                            return Poll::Ready(Err(QueueError::ProcessingTimeout {
                                processed: started_at.elapsed(),
                                limit: sleep.timeout,
                            }));
                        }
                    }
                    return Poll::Pending;
                }
                QueueServiceFutureState::Error(error) => {
                    return Poll::Ready(Err(
                        error.take().unwrap_or(QueueError::ServerRequestDropped)
                    ));
                }
            }
        }
    }
}
