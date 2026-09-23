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

    pub fn in_debt(owed: usize) -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(0)),
            debt: AtomicUsize::new(owed),
        }
    }

    pub fn acquire(&self) {
        if self.semaphore.forget_permits(1) == 0 {
            self.debt.fetch_add(1, Ordering::AcqRel);
        }
    }

    pub fn release(&self) {
        let prev_debt = self
            .debt
            .try_update(Ordering::AcqRel, Ordering::Acquire, |d| {
                if d > 0 {
                    Some(d - 1)
                } else {
                    None
                }
            });
        if prev_debt.is_err() {
            self.semaphore.add_permits(1);
        }
    }

    pub fn available(&self) -> usize {
        self.semaphore.available_permits()
    }

    pub fn debt(&self) -> usize {
        self.debt.load(Ordering::Acquire)
    }
}
