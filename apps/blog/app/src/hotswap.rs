//! A tower service whose inner composition is swapped at runtime behind a stable front.
//!
//! The queue visualiser shows the reader a `ServiceBuilder` stack and then runs it. For
//! the shown code to *be* the run code, the sim cannot rebuild its whole machine when a
//! tab, gate, or slider changes — the front every client task holds would change with
//! it. Instead the front is a [`HotSwap`]: built once, wrapping whichever composition is
//! currently loaded. Switching tabs rebuilds only the **inner** stack — the exact
//! `#[shown]` composition function whose source the panel displays — and loads it here.
//!
//! The swap is safe against tower's readiness contract because a clone only adopts the
//! newly-loaded inner at [`poll_ready`](Service::poll_ready) — never between a
//! `poll_ready` and its `call`. A reservation, once granted, is honoured by the same
//! inner that granted it; the next `poll_ready` picks up the swap. A request already in
//! flight completes against the inner it was admitted by.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use tower::util::BoxCloneService;
use tower::Service;

/// The shared cell every clone and the handle read: the current inner, and a generation
/// that ticks on each load so clones know to re-adopt.
struct Shared<Req, Resp, E> {
    inner: Mutex<BoxCloneService<Req, Resp, E>>,
    generation: AtomicU64,
}

/// A cloneable front over a swappable inner service. Clone it per client task, as tower
/// services are cloned; each clone tracks the generation it has adopted.
pub struct HotSwap<Req, Resp, E> {
    shared: Arc<Shared<Req, Resp, E>>,
    current: Option<BoxCloneService<Req, Resp, E>>,
    adopted: u64,
}

/// Loads a new inner composition into every present and future clone of a [`HotSwap`].
#[derive(Clone)]
pub struct HotSwapHandle<Req, Resp, E> {
    shared: Arc<Shared<Req, Resp, E>>,
}

impl<Req, Resp, E> HotSwap<Req, Resp, E> {
    /// The front and the handle that reloads it, over an initial composition.
    pub fn new(
        inner: BoxCloneService<Req, Resp, E>,
    ) -> (Self, HotSwapHandle<Req, Resp, E>) {
        let shared = Arc::new(Shared { inner: Mutex::new(inner), generation: AtomicU64::new(0) });
        let front = HotSwap { shared: Arc::clone(&shared), current: None, adopted: u64::MAX };
        (front, HotSwapHandle { shared })
    }
}

impl<Req, Resp, E> HotSwapHandle<Req, Resp, E> {
    /// Replace the composition. In-flight requests finish on the old one; new readiness
    /// polls — including the first from each existing clone — pick this up.
    pub fn load(&self, inner: BoxCloneService<Req, Resp, E>) {
        *self.shared.inner.lock().unwrap() = inner;
        self.shared.generation.fetch_add(1, Ordering::AcqRel);
    }
}

impl<Req, Resp, E> Clone for HotSwap<Req, Resp, E> {
    fn clone(&self) -> Self {
        // A fresh clone holds no inner yet; it adopts the current one on first poll.
        HotSwap { shared: Arc::clone(&self.shared), current: None, adopted: u64::MAX }
    }
}

impl<Req, Resp, E> Service<Req> for HotSwap<Req, Resp, E> {
    type Response = Resp;
    type Error = E;
    type Future = Pin<Box<dyn Future<Output = Result<Resp, E>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // The one place a swap is adopted: at the readiness boundary, so a reservation
        // is never stranded on a replaced inner.
        let generation = self.shared.generation.load(Ordering::Acquire);
        if self.current.is_none() || generation != self.adopted {
            self.current = Some(self.shared.inner.lock().unwrap().clone());
            self.adopted = generation;
        }
        self.current.as_mut().unwrap().poll_ready(cx)
    }

    fn call(&mut self, req: Req) -> Self::Future {
        self.current.as_mut().expect("poll_ready before call").call(req)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::convert::Infallible;
    use tower::{service_fn, ServiceExt};

    fn constant(reply: &'static str) -> BoxCloneService<(), &'static str, Infallible> {
        BoxCloneService::new(service_fn(move |_| async move { Ok::<_, Infallible>(reply) }))
    }

    #[tokio::test]
    async fn a_load_is_seen_on_the_next_readiness() {
        let (front, handle) = HotSwap::new(constant("A"));
        let mut svc = front.clone();
        assert_eq!(svc.ready().await.unwrap().call(()).await.unwrap(), "A");
        handle.load(constant("B"));
        assert_eq!(svc.ready().await.unwrap().call(()).await.unwrap(), "B");
    }

    #[tokio::test]
    async fn a_reservation_is_honoured_by_the_inner_that_granted_it() {
        let (front, handle) = HotSwap::new(constant("A"));
        let mut svc = front.clone();
        // Reserve against A…
        let svc = svc.ready().await.unwrap();
        // …then swap before calling. The reservation must still resolve on A.
        handle.load(constant("B"));
        assert_eq!(svc.call(()).await.unwrap(), "A", "the granted inner honours its own reservation");
    }

    #[tokio::test]
    async fn a_fresh_clone_adopts_the_latest() {
        let (front, handle) = HotSwap::new(constant("A"));
        handle.load(constant("B"));
        let mut late = front.clone();
        assert_eq!(late.ready().await.unwrap().call(()).await.unwrap(), "B", "a new clone starts current");
    }
}
