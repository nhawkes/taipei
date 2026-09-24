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
use tokio::time::{sleep, sleep_until, Instant, Sleep};
use tokio_util::sync::{PollSendError, PollSender};
use tokio_util::task::{AbortOnDropHandle, TaskTracker};
use tower::{Service, ServiceExt};

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

type Admission<Resp> = Result<AbortOnDropHandle<Resp>, QueueError>;

struct QueueItem<Req, Resp> {
    req: Req,
    tx: oneshot::Sender<Admission<Resp>>,
    enqueued_at: Instant,
    deadline: Instant,
    limit: Duration,
}

pub struct QueueLayer {
    queue_timeout: QueueTimeout,
    processing_timeout: Option<Duration>,
    capacity: usize,
}

impl Default for QueueLayer {
    fn default() -> Self {
        Self::new()
    }
}

#[bon::bon]
impl QueueLayer {
    #[builder(
        start_fn(name = builder, vis = "pub"),
        finish_fn(name = build, vis = "pub"),
        builder_type(name = QueueLayerBuilder, vis = "pub")
    )]
    fn configured(
        #[builder(
            default = Duration::from_millis(100),
            setters(name = with_custom_queue_timeout, option_fn(vis = ""))
        )]
        queue_timeout: Duration,
        #[builder(default = 1 << 16, setters(name = with_custom_capacity, option_fn(vis = "")))]
        capacity: usize,
        #[builder(setters(option_fn(vis = "")))] processing_timeout: Option<Duration>,
    ) -> Self {
        Self {
            queue_timeout: QueueTimeout::new(queue_timeout),
            processing_timeout,
            capacity,
        }
    }
}

impl QueueLayer {
    pub fn new() -> Self {
        Self::builder().build()
    }

    pub fn with_custom_queue_timeout(queue_timeout: Duration) -> Self {
        Self::builder()
            .with_custom_queue_timeout(queue_timeout)
            .build()
    }

    pub fn with_custom_capacity(capacity: usize) -> Self {
        Self::builder().with_custom_capacity(capacity).build()
    }

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

        let svc = tokio::select! {
            biased;
            _ = sleep_until(item.deadline) => {
                let _ = item.tx.send(Err(QueueError::Timeout {
                    waited: item.enqueued_at.elapsed(),
                    limit: item.limit,
                }));
                return false;
            },
            result = self.service.ready() => match result {
                Ok(svc) => svc,
                Err(e) => match e {},
            },
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

pub struct QueueServiceFuture<Resp> {
    state: QueueServiceFutureState<Resp>,
}

enum QueueServiceFutureState<Resp> {
    Queued {
        handle: Pin<Box<oneshot::Receiver<Admission<Resp>>>>,
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
        rx: oneshot::Receiver<Admission<Resp>>,
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
                QueueServiceFutureState::Queued {
                    handle: rx,
                    processing_timeout,
                } => match rx.as_mut().poll(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Ok(Ok(handle))) => {
                        let started_at = Instant::now();
                        self.state = QueueServiceFutureState::Started {
                            handle: Box::pin(handle),
                            sleep: processing_timeout.map(|timeout| ProcessingTimeout {
                                timeout,
                                sleep: Box::pin(sleep(timeout)),
                            }),
                            started_at,
                        };
                    }
                    Poll::Ready(Ok(Err(error))) => return Poll::Ready(Err(error)),
                    Poll::Ready(Err(_closed)) => {
                        return Poll::Ready(Err(QueueError::ServerRequestDropped));
                    }
                },
                QueueServiceFutureState::Started {
                    handle,
                    sleep,
                    started_at,
                } => {
                    if let Poll::Ready(result) = handle.as_mut().poll(cx) {
                        return match result {
                            Ok(resp) => Poll::Ready(Ok(resp)),
                            Err(err) if err.is_panic() => {
                                std::panic::resume_unwind(err.into_panic())
                            }
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
                    return Poll::Ready(Err(error
                        .take()
                        .unwrap_or(QueueError::ServerRequestDropped)));
                }
            }
        }
    }
}
