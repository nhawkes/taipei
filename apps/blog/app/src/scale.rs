//! The autoscaler — the half of the loop that is not on the machine.
//!
//! `taipei` decides what a server does when it is full: queue, refuse, blame. Nothing in it
//! decides how many servers there should be, and nothing in it could — a machine cannot buy
//! another machine. So this is the other side of that seam: something that watches the fleet
//! on its own beat and moves the rotation, and the two readings it can take of the same fleet.
//!
//! # What it reads
//!
//! [`Signal::Queue`] mirrors the load balancing: the clients already hold a reading of every
//! machine — it is what they route on — so the scaler steers by the half of that reading that
//! hurts, the requests waiting. Nothing is measured for its benefit; the crowd has already
//! paid for the answer, which is why it is prompt.
//!
//! [`Signal::Cpu`] is what a fleet outside this framework scales on, in the shape Kubernetes'
//! horizontal autoscaler uses: `desired = ceil(machines × util ÷ target)`. It is not wrong.
//! It is late, because a CPU number is an average over a window — [`WINDOW_MS`] here, the
//! same few seconds an OS load average covers — and a machine that filled up half a second
//! ago is still, to this reading, half full.
//!
//! # The queue law
//!
//! A right-sized fleet does not have an empty queue. It has a queue that keeps emptying: work
//! arrives in bursts, and a machine with *never* anything waiting was bought for a burst and
//! idles between them. So the reading is not the queue's depth but how much of the time it is
//! clear, and the fleet is right when that is [`CLEAR_TARGET`] — the median machine empty
//! half the looks, working through a queue the other half.
//!
//! - Clear [`CLEAR_SPARE`] of the time or more: bought for bursts that are not coming. Give a
//!   machine back, one a cycle, and see how it gets on.
//! - Clear less than [`CLEAR_SHORT`] of the time: requests are waiting more often than not,
//!   and one machine at a time is the wrong shape of answer — buy the machines the waiting
//!   work would fill, at once.
//! - Between the two: leave it alone. The band is the hysteresis, which is why this arm needs
//!   no settling count on top of it.
//!
//! [`Signal::Cpu`] gets no such band — a ratio has no notion of enough — so its scale-downs
//! wait for [`SETTLE`] cycles that agree, which is the shape of the stabilisation window a
//! horizontal autoscaler ships with.
//!
//! It is deliberately not [`taipei::cpu_concurrency`]. That controller steers a
//! [`ConcurrencyLimit`](taipei::limit::ConcurrencyLimit), and only raises its ceiling while
//! the permits it hands out are nearly all taken — the test for whether the ceiling is what
//! is holding the server back. A fleet size hands out no permits, so the same code would shed
//! machines and never add one.

use std::collections::VecDeque;

use crate::engine::{ADMISSION_FRACTION, ADMISSION_LIMIT, CORES};
use crate::multi::{Believed, BLIND_AT_MS};

/// How often the scaler acts, in virtual ms.
///
/// The chapter's own timescale: propagation is sub-second, and "if the service remains
/// overloaded for 1-10 seconds then we truely have more requests than capacity". Buying
/// machines is the answer to the second of those, not the first, so the loop waits out a
/// whole propagation and then some before it believes what it is looking at.
pub const CYCLE_MS: f64 = 5000.0;

/// How long a reading is averaged over, in virtual ms — the CPU arm's window and the queue
/// arm's. As long as the gate's own ([`OS_CPU_WINDOW_MS`](crate::engine::OS_CPU_WINDOW_MS)),
/// because a fleet's answer to "how much of the time" needs the bursts *and* the gaps between
/// them inside it to mean anything.
pub const WINDOW_MS: f64 = 3000.0;

/// How often the queue arm looks at the fleet, in virtual ms. A queue that comes and goes
/// inside one cycle has to be counted coming *and* going, rather than as whichever of the two
/// the cycle happened to land on — so the looks are taken between the acts.
const LOOK_MS: f64 = 1000.0;

/// How many looks the clear share is counted over: exactly one cycle's worth, so every act
/// judges the interval the last act created and nothing older.
const LOOKS: usize = (CYCLE_MS / LOOK_MS) as usize;

/// Cycles of agreement before the CPU arm gives a machine back. Two, because a cycle is now
/// five seconds: the queue arm has its band to keep it from thrashing, and this one has only
/// a ratio, which would hand a machine back on any reading that rounded down.
pub const SETTLE: u32 = 2;

/// The load the CPU arm steers toward.
///
/// Not the textbook nine-tenths, and the difference is the point of setting it at all: these
/// machines admit on CPU backpressure, which holds [`ADMISSION_FRACTION`] of every machine's
/// cores in reserve, so a machine working as hard as it is allowed to work still reads
/// half-idle. A target set where a fleet with no such gate would put it is a target this fleet
/// can never reach, and a scaler chasing it buys too few machines forever.
pub const TARGET: f64 = ADMISSION_FRACTION;

/// The share of the time a right-sized fleet's queue is clear. Half: it absorbs the bursts and
/// empties between them, which is what paying for exactly enough machines looks like from
/// outside.
pub const CLEAR_TARGET: f64 = 0.50;

/// Clear this much of the time and the fleet is paying for a burst that is not coming.
const CLEAR_SPARE: f64 = 0.75;

/// Clear less than this and requests are waiting more often than they are not.
const CLEAR_SHORT: f64 = 0.25;

/// What is waiting on the median machine in rotation, as the crowd last heard it — `None`
/// when it has heard from none of them recently, which is a scaler with nothing to go on
/// rather than a fleet with nothing waiting.
///
/// The median, because one machine caught mid-burst is not the fleet: half of them being
/// clear is a fleet keeping up, whatever the unluckiest one is holding. Nearest-rank, like
/// every other percentile in the chapter.
///
/// The queue is the whole reading. What a machine has in progress is what a machine is for; a
/// queue is the delay the chapter opens with — "picking incorrectly will lead the request
/// being delayed by up to 100ms (in the queue)".
pub fn queued(believed: &[Option<Believed>], machines: usize) -> Option<usize> {
    let mut fresh: Vec<usize> = believed
        .iter()
        .take(machines)
        .filter_map(|held| match held {
            Some(Believed { load, age_ms }) if *age_ms < BLIND_AT_MS => Some(load.queued),
            _ => None,
        })
        .collect();
    fresh.sort_unstable();
    let middle = ((fresh.len().max(1) - 1) as f64 * 0.5).round() as usize;
    fresh.get(middle).copied()
}

/// Which reading of the fleet the scaler steers by.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// What the routing knows: how much of the time the machines it picks between have
    /// nobody waiting.
    Queue,
    /// What the OS knows: how busy the cores have been, averaged over [`WINDOW_MS`].
    Cpu,
}

/// One frame's reading of the fleet, as much as a scaler is allowed to see.
pub struct Reading<'a> {
    /// What the crowd last heard about each machine — the routing's own knowledge, staleness
    /// and all.
    pub believed: &'a [Option<Believed>],
    /// Cores busy across the whole fleet, this instant.
    pub busy: usize,
}

/// One autoscaler: a rotation size, and the beat it moves it on.
pub struct Scaler {
    signal: Signal,
    machines: usize,
    ceiling: usize,
    /// Virtual ms since the last cycle.
    since: f64,
    /// The trailing CPU average, in `0.0..=1.0`. Exponential rather than a kept history: one
    /// number a frame is all a fleet reading is, and a window of samples would only be this
    /// number with more arithmetic.
    util: f64,
    /// Virtual ms since the last look at the queue.
    since_look: f64,
    /// What the median machine had waiting, one entry per look, kept [`LOOKS`] deep. How
    /// often it was nothing is what the arm steers by; the mean of them is what it buys on.
    looks: VecDeque<usize>,
    /// Consecutive cycles that wanted fewer machines — the CPU arm's only brake.
    calm: u32,
}

impl Scaler {
    pub fn new(signal: Signal, machines: usize, ceiling: usize) -> Scaler {
        Scaler {
            signal,
            machines,
            ceiling,
            since: 0.0,
            util: 0.0,
            since_look: 0.0,
            looks: VecDeque::new(),
            calm: 0,
        }
    }

    /// The smoothed load the CPU arm is acting on — nothing to do with the other arm, which
    /// never asks.
    pub fn util(&self) -> f64 {
        self.util
    }

    /// The share of the last [`LOOKS`] looks that found the median machine clear — what the
    /// queue arm steers by. A scaler that has looked at nothing yet reads the target: no
    /// evidence is no reason to buy or to sell.
    pub fn clear(&self) -> f64 {
        match self.looks.len() {
            0 => CLEAR_TARGET,
            n => self.looks.iter().filter(|&&queued| queued == 0).count() as f64 / n as f64,
        }
    }

    /// What the median machine has been holding across those looks.
    ///
    /// The mean of them rather than the newest: the arm acts on the window it has just judged,
    /// and a fleet that was behind for four looks out of five is behind whether or not the
    /// fifth caught it between bursts.
    pub fn queued(&self) -> f64 {
        match self.looks.len() {
            0 => 0.0,
            n => self.looks.iter().sum::<usize>() as f64 / n as f64,
        }
    }

    /// One frame: age both averages, and cycle if the beat has come round. Returns the
    /// rotation, moved or not.
    ///
    /// `dt_ms` is virtual, so the fleet's clock and the scaler's are the same clock — a sim
    /// run at a tenth speed does not get an autoscaler ten times as quick.
    pub fn frame(&mut self, dt_ms: f64, reading: Reading) -> usize {
        let busy = reading.busy as f64 / (self.machines * CORES) as f64;
        self.util += (dt_ms / WINDOW_MS).min(1.0) * (busy.clamp(0.0, 1.0) - self.util);

        self.since_look += dt_ms;
        if self.since_look >= LOOK_MS {
            self.since_look = 0.0;
            if let Some(queued) = queued(reading.believed, self.machines) {
                self.looks.push_back(queued);
                if self.looks.len() > LOOKS {
                    self.looks.pop_front();
                }
            }
        }

        self.since += dt_ms;
        if self.since < CYCLE_MS {
            return self.machines;
        }
        self.since = 0.0;

        let want = match self.signal {
            Signal::Queue => self.by_queue(),
            Signal::Cpu => self.by_cpu(),
        }
        .clamp(1, self.ceiling);
        self.machines = match (self.signal, want) {
            (_, want) if want > self.machines => {
                self.calm = 0;
                want
            }
            // The CPU arm's ratio rounds down as readily as it rounds up, so it hands a machine
            // back only when the cycles agree. The queue arm's band already means it is not
            // asked twice about the same fleet.
            (Signal::Cpu, want) if want < self.machines => {
                self.calm += 1;
                match self.calm >= SETTLE {
                    true => {
                        self.calm = 0;
                        self.machines - 1
                    }
                    false => self.machines,
                }
            }
            (_, want) => {
                self.calm = 0;
                want
            }
        };
        self.machines
    }

    /// The queue law: clear too much of the time is a machine spare, clear too little is a
    /// fleet short by the work that is waiting on it.
    fn by_queue(&self) -> usize {
        match self.clear() {
            clear if clear >= CLEAR_SPARE => self.machines - 1,
            clear if clear < CLEAR_SHORT => {
                let waiting = self.queued() * self.machines as f64;
                self.machines + (waiting / ADMISSION_LIMIT as f64).ceil().max(1.0) as usize
            }
            _ => self.machines,
        }
    }

    /// The horizontal-autoscaler ratio over the trailing average: the fleet it would take to
    /// put this much load at the target.
    fn by_cpu(&self) -> usize {
        (self.machines as f64 * self.util / TARGET).ceil() as usize
    }
}
