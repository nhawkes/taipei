//! A semaphore whose permit count can be lowered even while permits are held.
//!
//! A plain `Semaphore` can't: you can't claw back a permit a task already holds.
//! [`DebtSemaphore`] solves that by pairing the semaphore with an atomic `debt`
//! counter. Lowering capacity ([`acquire`](DebtSemaphore::acquire)) forgets a
//! free permit if one exists, otherwise books a unit of debt; raising capacity
//! ([`release`](DebtSemaphore::release)) cancels a unit of debt first, otherwise
//! hands a real permit back. As held permits are returned (also via `release`),
//! they cancel debt instead of growing the pool, so live capacity converges on
//! the target without ever revoking an in-flight permit.
//!
//! This is the shared core of both taipei's CPU backpressure (where capacity
//! tracks idle CPU via runtime park/unpark — see
//! [`backpressure`](crate::backpressure)) and its dynamic concurrency limit
//! (where capacity is the tunable ceiling — see [`limit`](crate::limit)).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::Semaphore;

pub struct DebtSemaphore {
    pub(crate) semaphore: Arc<Semaphore>,
    debt: AtomicUsize,
}

impl DebtSemaphore {
    pub fn new(initial_permits: usize) -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(initial_permits)),
            debt: AtomicUsize::new(0),
        }
    }

    /// Start `owed` below zero: the first `owed` [`release`](Self::release) calls
    /// only cancel debt, so capacity stays at zero until releases outnumber them.
    /// This is how a gate expresses a reserve — capacity is what remains after the
    /// reserve is covered, not the reserve itself.
    pub fn in_debt(owed: usize) -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(0)),
            debt: AtomicUsize::new(owed),
        }
    }

    /// Lower capacity by one: forget a free permit if available, otherwise record
    /// that one is owed. (Backpressure calls this on thread unpark; the dynamic
    /// limit calls it when its ceiling is lowered.)
    pub fn acquire(&self) {
        if self.semaphore.forget_permits(1) == 0 {
            self.debt.fetch_add(1, Ordering::AcqRel);
        }
    }

    /// Raise capacity by one: cancel a unit of debt if any is outstanding,
    /// otherwise add a permit. (Backpressure calls this on thread park; the
    /// dynamic limit calls it when its ceiling is raised or a request finishes.)
    pub fn release(&self) {
        let prev_debt = self
            .debt
            .try_update(Ordering::AcqRel, Ordering::Acquire, |d| {
                if d > 0 { Some(d - 1) } else { None }
            });
        if prev_debt.is_err() {
            self.semaphore.add_permits(1);
        }
    }

    /// Permits available to acquire right now.
    pub fn available(&self) -> usize {
        self.semaphore.available_permits()
    }

    /// Permits owed because capacity was lowered below what's held.
    pub fn debt(&self) -> usize {
        self.debt.load(Ordering::Acquire)
    }
}
