//! Virtual time for `wasm32` targets — one [`Clock`] per simulation.
//!
//! On wasm32, tokio 1.52's `Clock::new()` calls `std::time::Instant::now()`
//! unconditionally, which panics because the platform has no monotonic clock.
//! A simulation owns a [`Clock`] — a now-cell plus the timer queue of parked
//! sleepers — advances it, and fires the due sleepers. Two simulations own two
//! clocks; nothing they do can move each other's time.
//!
//! Futures resolve their clock the way `tokio::time` resolves the ambient
//! runtime: the driver wraps each pump in [`Clock::enter`], and [`sleep`] /
//! [`Instant::now`] read the clock in scope. A [`Sleep`] binds its clock at
//! creation, so a parked sleeper stays with the clock that made it.

use std::cell::RefCell;
use std::future::Future;
use std::marker::PhantomData;
use std::ops::{Add, AddAssign};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll};
use std::time::Duration;

// ── Instant ───────────────────────────────────────────────────────────────────

/// A point in virtual time, in microseconds since an arbitrary epoch.
///
/// Intentionally mirrors the `tokio::time::Instant` API surface used by
/// [`crate::queue`] so that `cfg`-gated type aliases keep the queue code clean.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
pub struct Instant(u64);

impl Instant {
    /// The current time on the clock in scope (see [`Clock::enter`]).
    pub fn now() -> Self {
        current().now()
    }

    pub fn from_micros(us: u64) -> Self {
        Instant(us)
    }

    pub fn as_micros(self) -> u64 {
        self.0
    }

    pub fn elapsed(self) -> Duration {
        Instant::now().duration_since(self)
    }

    pub fn duration_since(self, earlier: Instant) -> Duration {
        Duration::from_micros(self.0.saturating_sub(earlier.0))
    }
}

impl Add<Duration> for Instant {
    type Output = Instant;
    fn add(self, d: Duration) -> Instant {
        Instant(self.0.saturating_add(d.as_micros() as u64))
    }
}

impl AddAssign<Duration> for Instant {
    fn add_assign(&mut self, d: Duration) {
        *self = *self + d;
    }
}

// ── Clock ─────────────────────────────────────────────────────────────────────

/// A discrete-event virtual clock. Cheap to clone; clones share the same time.
///
/// The owner drives it: run the tasks until idle, hop to [`Clock::next_deadline`]
/// with [`Clock::advance_to`], [`Clock::fire_due`], repeat — O(events), not
/// O(steps × sleepers). This is the shape of tokio's paused test time. Parked
/// sleepers cannot observe time on their own, so whoever advances the clock
/// must fire after each advance.
#[derive(Clone, Default)]
pub struct Clock(Arc<Core>);

#[derive(Default)]
struct Core {
    now_us: AtomicU64,
    timers: Mutex<Timers>,
    /// Registration order breaks deadline ties, so equal deadlines fire FIFO —
    /// deterministic across runs.
    seq: AtomicU64,
}

/// Heap entries are lazily deleted: `wakers` is the truth, the heap is a hint.
#[derive(Default)]
struct Timers {
    heap: std::collections::BinaryHeap<std::cmp::Reverse<(u64, u64)>>,
    wakers: std::collections::HashMap<u64, std::task::Waker>,
}

impl Clock {
    pub fn new() -> Self {
        Clock::default()
    }

    pub fn now(&self) -> Instant {
        Instant::from_micros(self.0.now_us.load(Ordering::Relaxed))
    }

    /// Advance to `t` — monotonic: a target at or behind now leaves the clock
    /// where it stands, so a deadline that already passed fires in place.
    pub fn advance_to(&self, t: Instant) {
        self.0.now_us.fetch_max(t.as_micros(), Ordering::Relaxed);
    }

    /// The earliest registered deadline still armed, if any.
    pub fn next_deadline(&self) -> Option<Instant> {
        let mut t = self.timers();
        while let Some(&std::cmp::Reverse((deadline, seq))) = t.heap.peek() {
            if t.wakers.contains_key(&seq) {
                return Some(Instant::from_micros(deadline));
            }
            t.heap.pop(); // cancelled sleep — purge the stale hint
        }
        None
    }

    /// Wake every sleeper whose deadline has arrived. Call after each advance;
    /// wakes happen outside the lock.
    pub fn fire_due(&self) {
        let now = self.now();
        let mut due = Vec::new();
        {
            let mut t = self.timers();
            while let Some(&std::cmp::Reverse((deadline, seq))) = t.heap.peek() {
                if Instant::from_micros(deadline) > now {
                    break;
                }
                t.heap.pop();
                if let Some(waker) = t.wakers.remove(&seq) {
                    due.push(waker);
                }
            }
        }
        for waker in due {
            waker.wake();
        }
    }

    /// Put this clock in scope for the current thread until the guard drops:
    /// within it, [`sleep`] and [`Instant::now`] resolve to this clock. Wrap
    /// every pump of the simulation's runtime in one of these.
    pub fn enter(&self) -> Entered {
        CURRENT.with(|c| c.borrow_mut().push(self.clone()));
        Entered { _single_thread: PhantomData }
    }

    pub fn sleep(&self, duration: Duration) -> Sleep {
        self.sleep_until(self.now() + duration)
    }

    pub fn sleep_until(&self, deadline: Instant) -> Sleep {
        Sleep { clock: self.clone(), deadline, seq: None }
    }

    fn timers(&self) -> MutexGuard<'_, Timers> {
        self.0.timers.lock().expect("taipei clock timers poisoned")
    }
}

thread_local! {
    /// The clocks in scope on this thread, innermost last (see [`Clock::enter`]).
    static CURRENT: RefCell<Vec<Clock>> = const { RefCell::new(Vec::new()) };
}

/// Scope guard from [`Clock::enter`]; leaving the scope restores the outer clock.
pub struct Entered {
    _single_thread: PhantomData<*const ()>,
}

impl Drop for Entered {
    fn drop(&mut self) {
        CURRENT.with(|c| c.borrow_mut().pop());
    }
}

fn current() -> Clock {
    CURRENT
        .with(|c| c.borrow().last().cloned())
        .expect("taipei: no clock in scope — drive virtual-time futures from within Clock::enter")
}

// ── Sleep ─────────────────────────────────────────────────────────────────────

/// A future that completes once its clock reaches `deadline`.
///
/// Not due ⇒ it registers with its clock's timer queue and parks; it is woken
/// by the driver's [`Clock::fire_due`], never by polling the clock itself.
/// The clock is bound at creation, so the sleeper needs no scope to poll.
pub struct Sleep {
    clock: Clock,
    deadline: Instant,
    seq: Option<u64>,
}

impl Future for Sleep {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.clock.now() >= self.deadline {
            if let Some(seq) = self.seq.take() {
                self.clock.timers().wakers.remove(&seq);
            }
            return Poll::Ready(());
        }
        let mut t = self.clock.timers();
        match self.seq {
            // Re-poll before the deadline (spurious wake, task shuffled): refresh
            // the waker in place.
            Some(seq) => {
                t.wakers.insert(seq, cx.waker().clone());
            }
            None => {
                let seq = self.clock.0.seq.fetch_add(1, Ordering::Relaxed);
                t.heap.push(std::cmp::Reverse((self.deadline.0, seq)));
                t.wakers.insert(seq, cx.waker().clone());
                drop(t);
                self.seq = Some(seq);
            }
        }
        Poll::Pending
    }
}

impl Drop for Sleep {
    fn drop(&mut self) {
        // Disarm; the heap entry is purged lazily by next_deadline/fire_due.
        if let Some(seq) = self.seq.take() {
            self.clock.timers().wakers.remove(&seq);
        }
    }
}

/// Sleep on the clock in scope (see [`Clock::enter`]).
pub fn sleep(duration: Duration) -> Sleep {
    current().sleep(duration)
}

/// Sleep until `deadline` on the clock in scope (see [`Clock::enter`]).
pub fn sleep_until(deadline: Instant) -> Sleep {
    current().sleep_until(deadline)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::task::{Wake, Waker};

    struct Flag(AtomicBool);
    impl Wake for Flag {
        fn wake(self: Arc<Self>) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    fn waker() -> (Arc<Flag>, Waker) {
        let flag = Arc::new(Flag(AtomicBool::new(false)));
        (flag.clone(), Waker::from(flag))
    }

    #[test]
    fn sleepers_park_and_fire_on_advance() {
        let clock = Clock::new();
        let (flag, waker) = waker();
        let mut cx = Context::from_waker(&waker);

        // Park: pending, registered, and — the DES property — NOT self-woken.
        let mut s = clock.sleep(Duration::from_micros(100));
        assert!(Pin::new(&mut s).poll(&mut cx).is_pending());
        assert!(!flag.0.load(Ordering::SeqCst), "a parked sleeper must not spin");
        assert_eq!(clock.next_deadline(), Some(Instant::from_micros(100)));

        // A later sleeper doesn't change the earliest deadline.
        let mut s2 = clock.sleep(Duration::from_micros(300));
        assert!(Pin::new(&mut s2).poll(&mut cx).is_pending());
        assert_eq!(clock.next_deadline(), Some(Instant::from_micros(100)));

        // Advance past the deadline and fire: exactly the due sleeper wakes.
        clock.advance_to(Instant::from_micros(150));
        clock.fire_due();
        assert!(flag.0.load(Ordering::SeqCst), "due sleeper must be woken");
        assert!(Pin::new(&mut s).poll(&mut cx).is_ready());
        assert_eq!(clock.next_deadline(), Some(Instant::from_micros(300)));

        // Cancellation: dropping the remaining sleeper disarms its deadline.
        drop(s2);
        assert_eq!(clock.next_deadline(), None);
    }

    #[test]
    fn clocks_are_independent() {
        let a = Clock::new();
        let b = Clock::new();
        let (flag_a, waker_a) = waker();
        let mut cx = Context::from_waker(&waker_a);

        let mut s = a.sleep(Duration::from_micros(100));
        assert!(Pin::new(&mut s).poll(&mut cx).is_pending());

        // Another clock advancing moves neither a's time nor a's sleepers.
        b.advance_to(Instant::from_micros(10_000));
        b.fire_due();
        assert_eq!(a.now(), Instant::from_micros(0));
        assert_eq!(b.next_deadline(), None);
        assert!(!flag_a.0.load(Ordering::SeqCst), "b's advance must not wake a's sleeper");
        assert!(Pin::new(&mut s).poll(&mut cx).is_pending());

        a.advance_to(Instant::from_micros(100));
        a.fire_due();
        assert!(Pin::new(&mut s).poll(&mut cx).is_ready());
    }

    #[test]
    fn ambient_scope_resolves_the_entered_clock() {
        let a = Clock::new();
        a.advance_to(Instant::from_micros(42));
        {
            let _in_a = a.enter();
            assert_eq!(Instant::now(), Instant::from_micros(42));
            // A sleep created in scope binds a — it parks on a's timer queue.
            let (_, waker) = waker();
            let mut cx = Context::from_waker(&waker);
            let mut s = sleep(Duration::from_micros(8));
            assert!(Pin::new(&mut s).poll(&mut cx).is_pending());
            assert_eq!(a.next_deadline(), Some(Instant::from_micros(50)));
        }
    }
}
