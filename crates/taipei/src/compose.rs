//! The book's progression as data: a [`Stage`] names a point in it, and
//! [`Stage::manifest`] reports the set of protection layers that point composes as a
//! [`Layers`] value — the single source of truth the visualiser reads to decide what to
//! draw, so the picture can never drift from the composition.
//!
//! The compositions themselves live where they run — the `queue-viz` app builds each
//! stage's real tower stack and captures its source for the "show code" panel, so the
//! listing is the code that ran. This module is only the vocabulary they share.

use std::time::Duration;

/// The ceiling [`Stage::Reject`] admits before it sheds — one slot per simulated
/// core, held for the whole request, so a saturated CPU is a full limit.
pub const REJECT_LIMIT: usize = 8;

/// The ceiling [`Stage::Queue`] admits behind its queue. A slot is held across the
/// request's IO as well as its CPU, so a per-core ceiling would leave cores idle
/// waiting on the database; this is tuned to the in-flight population those cores can
/// actually sustain — and being tuned by hand is exactly what makes it naive.
pub const QUEUE_LIMIT: usize = 24;

// ── Stage / manifest ──────────────────────────────────────────────────────────

/// A point in the book's progression. Selects which composition the caller builds
/// and, via [`Stage::manifest`], which layers the visualiser draws.
///
/// The kebab-case string form (`"app"`, `"backpressure"`, `"reject"`, `"queue"`)
/// is what the `data-stage` attribute on a `<canvas>` carries.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Stage {
    /// The bare service: CPU/IO work with no protection.
    App,
    /// CPU backpressure only — readiness is withheld while every core is busy.
    Backpressure,
    /// A concurrency limit plus immediate rejection, no queue: shed once every slot
    /// is taken.
    Reject,
    /// A concurrency limit and nothing else. The limit withholds readiness once every
    /// slot is taken, and with no rejection layer above it and no queue to time them
    /// out, callers park on `poll_ready` for as long as it takes. The same ceiling as
    /// [`Stage::Reject`], differing only in what reaching it does — which is the whole
    /// comparison. Nothing here can reject, so nothing draws a rejection exit.
    Wait,
    /// A hand-tuned concurrency limit behind a bounded queue that sheds on its
    /// deadline. The gate that replaces the tuning is composed on top of this
    /// manifest, not carried by the stage.
    #[default]
    Queue,
}

impl Stage {
    /// The kebab-case name used in `data-stage` and the URL.
    pub fn as_str(&self) -> &'static str {
        match self {
            Stage::App => "app",
            Stage::Backpressure => "backpressure",
            Stage::Reject => "reject",
            Stage::Wait => "wait",
            Stage::Queue => "queue",
        }
    }
}

impl std::fmt::Display for Stage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Stage {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "app" => Ok(Stage::App),
            "backpressure" => Ok(Stage::Backpressure),
            "reject" => Ok(Stage::Reject),
            "wait" => Ok(Stage::Wait),
            "queue" => Ok(Stage::Queue),
            _ => Err(()),
        }
    }
}

/// Which protection layers a [`Stage`] composes. Present ⟺ drawn; this is what
/// the visualiser inspects so the animation matches the running stack exactly.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Layers {
    /// CPU backpressure gates admission (withholds readiness, never rejects).
    pub backpressure: bool,
    /// A concurrency limit caps in-flight requests; `Some(n)` is the ceiling.
    pub limit: Option<usize>,
    /// A rejection layer sheds immediately when the inner service isn't ready.
    pub reject: bool,
    /// A **request queue**: accepted requests, parked awaiting admission. Distinct from
    /// the kernel's SYN backlog (those are not accepted yet) and from the runtime's run
    /// queue (those are runnable now, not waiting for permission). Any composition that
    /// parks a caller has one — a bare concurrency limit's semaphore waiters are a
    /// request queue too, just an implicit one nobody chose the shape of.
    pub queue: bool,
    /// The queue's shed deadline. `None` is the unbounded wait you get when nothing
    /// puts a clock on it — which is what [`QueueLayer`](crate::queue) exists to fix.
    pub queue_timeout: Option<Duration>,
}

impl Stage {
    /// The layer set this stage composes — the source of truth for the viz.
    pub fn manifest(&self, queue_timeout: Duration) -> Layers {
        match self {
            Stage::App => Layers::default(),
            Stage::Backpressure => Layers { backpressure: true, ..Layers::default() },
            Stage::Reject => Layers {
                limit: Some(REJECT_LIMIT),
                reject: true,
                ..Layers::default()
            },
            // The same ceiling as Reject and no rejection, so reaching it is a wait —
            // and a wait is a request queue, here the implicit one the limit's own
            // semaphore keeps. No deadline is on it, which is the whole difference.
            Stage::Wait => Layers {
                limit: Some(REJECT_LIMIT),
                queue: true,
                ..Layers::default()
            },
            Stage::Queue => Layers {
                limit: Some(QUEUE_LIMIT),
                queue: true,
                queue_timeout: Some(queue_timeout),
                ..Layers::default()
            },
        }
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    const T: Duration = Duration::from_millis(100);

    fn stages() -> [Stage; 5] {
        let all = [Stage::App, Stage::Backpressure, Stage::Reject, Stage::Wait, Stage::Queue];
        // A new variant makes this match non-exhaustive, so the list cannot fall behind
        // the enum and quietly shrink what the tests below range over.
        all.map(|stage| match stage {
            Stage::App | Stage::Backpressure | Stage::Reject | Stage::Wait | Stage::Queue => stage,
        })
    }

    /// `as_str` and `FromStr` are two hand-written tables over the same names, and
    /// `data-stage` crosses between them — so only a round trip catches one drifting.
    #[test]
    fn every_stage_round_trips_through_its_string() {
        for stage in stages() {
            assert_eq!(Stage::from_str(stage.as_str()), Ok(stage));
            assert_eq!(stage.to_string(), stage.as_str());
        }
        assert_eq!(Stage::from_str("Queue"), Err(()));
    }

    /// The gate is its own chapter, and the stage the book reaches first must not have
    /// arrived there already: `Queue` is the naive ceiling, tuned by hand. Backpressure
    /// is composed onto a manifest afterwards, so no stage may bring it along.
    #[test]
    fn backpressure_belongs_to_exactly_one_stage() {
        let carried: Vec<Stage> =
            stages().into_iter().filter(|s| s.manifest(T).backpressure).collect();
        assert_eq!(carried, [Stage::Backpressure]);
    }

    /// The comparison those two pages are built on: the same ceiling, opposite answers
    /// at it. Let the ceilings diverge and the pages stop comparing like with like.
    #[test]
    fn wait_and_reject_differ_only_in_what_reaching_the_ceiling_does() {
        let (wait, reject) = (Stage::Wait.manifest(T), Stage::Reject.manifest(T));
        assert_eq!(wait.limit, reject.limit);
        assert!(reject.reject && !reject.queue, "reaching it sheds");
        assert!(wait.queue && !wait.reject, "reaching it waits");
    }

    /// A deadline is what separates the two waits, it is meaningless without something
    /// to wait in, and it is the caller's — a constant here would draw one clock while
    /// the tower ran another.
    #[test]
    fn the_deadline_is_the_callers_and_belongs_to_a_queue() {
        for stage in stages() {
            let layers = stage.manifest(T);
            assert!(layers.queue_timeout.is_none() || layers.queue, "{stage} times out nothing");
        }
        assert!(Stage::Wait.manifest(T).queue_timeout.is_none(), "the unbounded wait");
        assert!(Stage::Queue.manifest(T).queue_timeout.is_some(), "the deadline that ends it");
        assert_ne!(Stage::Queue.manifest(T).queue_timeout, Stage::Queue.manifest(T * 2).queue_timeout);
    }

    /// Present ⟺ drawn. Two stages sharing a manifest are one picture, and the reader
    /// would be looking at a composition other than the one the page names.
    #[test]
    fn each_stage_draws_a_distinct_picture() {
        for (i, a) in stages().into_iter().enumerate() {
            for b in stages().into_iter().skip(i + 1) {
                assert_ne!(a.manifest(T), b.manifest(T), "{a} and {b} draw the same picture");
            }
        }
    }
}
