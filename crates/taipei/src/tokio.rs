//! A tokio runtime that reports core availability as it happens.
//!
//! A worker parks when it runs out of work and unparks when it picks work up, so
//! the count of parked workers *is* the idle-core count — no sampling window, no
//! multi-second average. Feeding park/unpark into a [`DebtSemaphore`] turns that
//! count into the admission gate [`CpuBackpressureLayer`](crate::backpressure)
//! polls.
//!
//! The permit arithmetic is the whole design, so it is stated here:
//!
//! ```text
//! available = idle_cores - reserved        reserved = cores * buffer_pct
//! ```
//!
//! Capacity is what is left *after* the reserve, which is why the semaphore
//! starts `reserved` in debt rather than holding `reserved` permits: parks must
//! first repay the reserve before any of them become admissions. Seed it with
//! permits instead and the reserve becomes headroom added to a fully-loaded
//! machine — the gate floors at `reserved` and never closes.
//!
//! The reserve is the point. Admitting only while spare cores are parked leaves
//! someone to run an IO completion the moment it lands, instead of making it wait
//! behind a core that is busy with freshly-admitted work.

use crate::backpressure::{DebtSemaphore, InstrumentedRuntime, RuntimeInstrumentation};
use delegate::delegate;
use std::sync::Arc;

pub struct InstrumentedTokioRuntime {
    pub runtime: tokio::runtime::Runtime,
    semaphore: Arc<DebtSemaphore>,
}

impl InstrumentedTokioRuntime {
    pub fn new() -> std::io::Result<Self> {
        Self::builder().enable_all().build()
    }

    pub fn builder() -> InstrumentedBuilder {
        let num_cpus = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        InstrumentedBuilder {
            inner: tokio::runtime::Builder::new_multi_thread(),
            num_cpus,
            buffer_pct: 0.5,
        }
    }
}

impl InstrumentedRuntime for InstrumentedTokioRuntime {
    fn instrumentation(&self) -> RuntimeInstrumentation {
        RuntimeInstrumentation { semaphore: Arc::clone(&self.semaphore) }
    }
}

pub struct InstrumentedBuilder {
    inner: tokio::runtime::Builder,
    num_cpus: usize,
    buffer_pct: f64,
}

impl InstrumentedBuilder {
    pub fn buffer_pct(&mut self, pct: f64) -> &mut Self {
        self.buffer_pct = pct;
        self
    }

    pub fn worker_threads(&mut self, val: usize) -> &mut Self {
        self.num_cpus = val;
        self.inner.worker_threads(val);
        self
    }

    delegate! {
        to self.inner {
            #[expr({ $; self })]
            pub fn enable_all(&mut self) -> &mut Self;
            #[expr({ $; self })]
            pub fn enable_time(&mut self) -> &mut Self;
            #[expr({ $; self })]
            pub fn thread_stack_size(&mut self, size: usize) -> &mut Self;
            #[expr({ $; self })]
            pub fn thread_name(&mut self, name: String) -> &mut Self;
        }
    }

    pub fn build(&mut self) -> std::io::Result<InstrumentedTokioRuntime> {
        let reserved = (self.num_cpus as f64 * self.buffer_pct) as usize;
        let semaphore = Arc::new(DebtSemaphore::in_debt(reserved));
        let sem = Arc::clone(&semaphore);
        let sem_ = Arc::clone(&semaphore);
        let runtime = self
            .inner
            .worker_threads(self.num_cpus)
            .on_thread_park(move || sem.release())
            .on_thread_unpark(move || sem_.acquire())
            .build()?;
        Ok(InstrumentedTokioRuntime { runtime, semaphore })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Park/unpark accounting without a runtime: `park` is a core going idle,
    /// `unpark` a core picking work up.
    fn gate(cores: usize, buffer_pct: f64) -> Arc<DebtSemaphore> {
        Arc::new(DebtSemaphore::in_debt((cores as f64 * buffer_pct) as usize))
    }

    #[test]
    fn admissions_are_idle_cores_less_the_reserve() {
        let sem = gate(8, 0.5);
        for parked in 1..=8usize {
            sem.release();
            assert_eq!(sem.available(), parked.saturating_sub(4));
        }
        for busy in 1..=8usize {
            sem.acquire();
            assert_eq!(sem.available(), (8 - busy).saturating_sub(4));
        }
    }

    #[test]
    fn the_gate_closes_while_every_core_is_busy() {
        let sem = gate(8, 0.5);
        for _ in 0..8 {
            sem.release();
        }
        for _ in 0..8 {
            sem.acquire();
        }
        assert_eq!(sem.available(), 0, "a fully-loaded machine admits nothing");
    }

    #[test]
    fn an_idle_runtime_offers_its_spare_cores() {
        let rt = InstrumentedTokioRuntime::builder()
            .worker_threads(8)
            .enable_all()
            .build()
            .unwrap();
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(rt.instrumentation().semaphore.available(), 4);
    }
}
