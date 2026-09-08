//! Tuning a concurrency limit from a CPU-load signal.
//!
//! [`CpuBackpressureLayer`](crate::backpressure) is the better gate when the runtime
//! owns every thread: it reports availability instantly, and nothing needs tuning.
//! That report only covers work the runtime schedules, so a process that also burns
//! CPU on threads tokio never sees — a C++ dependency, a blocking pool, a sidecar
//! sharing the cgroup — has load the layer is blind to. A CPU-load number covers all
//! of it, at the cost of being an average over a window: the delay this trades for
//! the coverage is the whole reason it is a stepping stone and not the destination.
//!
//! So the load is not used as a gate. Admission stays with
//! [`DynamicConcurrencyLimitLayer`](crate::limit), which is instantaneous, and the
//! load only *tunes its ceiling* — slowly, in the background, on the timescale the
//! signal is actually good for. Overload multiplicatively decreases the ceiling;
//! a calm and saturated server additively increases it. The shape is
//! fbthrift's `CPUConcurrencyController`.
//!
//! **The controller reads nothing.** It has no clock, no OS access, and no timer:
//! [`observe`](CpuConcurrencyController::observe) takes one load sample and the
//! caller decides where that comes from and how often to call. A server passes a
//! cgroup reading on a background interval; a simulation passes its own utilisation
//! on its own clock. Same code, no seam to mock.
//!
//! Because the caller owns the period, durations here are counted in **cycles**
//! rather than milliseconds — `refractory` is a number of `observe` calls, not a
//! wall-clock window. That is the one place this departs from the reference, which
//! configures `refreshPeriodMs` and `refractoryPeriodMs` separately and only ever
//! uses their ratio.

use crate::limit::ConcurrencyLimit;

/// How the controller reacts. The defaults track fbthrift's, with the reference's
/// separate `refreshPeriodMs`/`refractoryPeriodMs` collapsed into a cycle count.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// The load the controller steers toward, in `0.0..=1.0`. At or above this the
    /// ceiling comes down.
    pub cpu_target: f64,
    /// Fraction of the ceiling shed per overloaded cycle (at least 1).
    pub decrease: f64,
    /// Fraction of the ceiling added per calm, saturated cycle (at least 1).
    pub increase: f64,
    /// How close in-flight work must come to the ceiling to count as saturated:
    /// usage at or above `(1 - increase_distance) * limit`. Below it, the ceiling is
    /// not what is holding the server back, so raising it would prove nothing.
    pub increase_distance: f64,
    /// Cycles after an overload during which the ceiling is not raised again. The
    /// load signal lags, so an immediate raise would be reacting to stale calm.
    pub refractory: u32,
    /// EMA weight on each new sample, in `0.0..=1.0`. Lower is smoother.
    pub smoothing: f64,
    /// The ceiling never goes below this.
    pub min_limit: usize,
    /// The ceiling never goes above this.
    pub max_limit: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            cpu_target: 0.9,
            decrease: 0.05,
            increase: 0.02,
            increase_distance: 0.2,
            refractory: 3,
            smoothing: 0.3,
            min_limit: 1,
            max_limit: 1 << 16,
        }
    }
}

/// Steers a [`ConcurrencyLimit`] toward [`Config::cpu_target`].
pub struct CpuConcurrencyController {
    limit: ConcurrencyLimit,
    config: Config,
    /// EMA of the samples so far. Seeded at zero so a spike in the first sample
    /// cannot slam the ceiling shut before there is anything to average against.
    load: f64,
    /// Cycles since the last overloaded one, saturating at `refractory`.
    calm_cycles: u32,
}

impl CpuConcurrencyController {
    pub fn new(limit: ConcurrencyLimit, config: Config) -> Self {
        Self { limit, config, load: 0.0, calm_cycles: config.refractory }
    }

    /// The smoothed load the last [`observe`](Self::observe) left behind.
    pub fn load(&self) -> f64 {
        self.load
    }

    /// Feed one CPU-load sample, in `0.0..=1.0`, and move the ceiling accordingly.
    ///
    /// One call is one cycle: the caller's calling rate *is* the refresh period, and
    /// [`Config::refractory`] counts these calls.
    pub fn observe(&mut self, load: f64) {
        let alpha = self.config.smoothing.clamp(0.0, 1.0);
        self.load += alpha * (load.clamp(0.0, 1.0) - self.load);

        let limit = self.limit.get();
        if self.load >= self.config.cpu_target {
            self.calm_cycles = 0;
            self.limit.set(step(limit, -self.config.decrease, self.config));
            return;
        }

        // Saturated means the ceiling is the constraint. `available` already accounts
        // for a ceiling lowered below the work in flight, which reads as fully used.
        let usage = limit.saturating_sub(self.limit.available());
        let saturated = usage as f64 >= (1.0 - self.config.increase_distance) * limit as f64;
        if self.calm_cycles >= self.config.refractory && saturated {
            self.limit.set(step(limit, self.config.increase, self.config));
        }
        self.calm_cycles = self.calm_cycles.saturating_add(1);
    }
}

/// Move `limit` by `fraction` of itself — at least one, so a small ceiling still
/// moves — and hold it inside the configured bounds.
fn step(limit: usize, fraction: f64, config: Config) -> usize {
    let delta = ((limit as f64 * fraction.abs()) as usize).max(1);
    let moved = match fraction < 0.0 {
        true => limit.saturating_sub(delta),
        false => limit.saturating_add(delta),
    };
    moved.clamp(config.min_limit, config.max_limit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limit::DynamicConcurrencyLimitLayer;
    use std::convert::Infallible;
    use std::future::Future;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tower::{Layer, Service, ServiceExt};

    #[derive(Clone)]
    struct HoldForever;
    impl Service<()> for HoldForever {
        type Response = ();
        type Error = Infallible;
        type Future = Pin<Box<dyn Future<Output = Result<(), Infallible>> + Send>>;
        fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
        fn call(&mut self, _: ()) -> Self::Future {
            Box::pin(std::future::pending())
        }
    }

    /// A limit with `held` of its slots taken by requests that never finish — the
    /// only honest way to make the ceiling the binding constraint, since a slot is
    /// something the layer takes, not something a handle can fake.
    async fn occupied(limit: usize, held: usize) -> (ConcurrencyLimit, Vec<impl Service<()>>) {
        let layer = DynamicConcurrencyLimitLayer::new(limit);
        let mut services = Vec::new();
        for _ in 0..held {
            let mut svc = layer.layer(HoldForever);
            svc.ready().await.unwrap();
            services.push(svc);
        }
        (layer.handle(), services)
    }

    #[tokio::test]
    async fn a_single_spike_does_not_shed_the_ceiling() {
        let (limit, _held) = occupied(100, 100).await;
        let mut cc = CpuConcurrencyController::new(limit.clone(), Config::default());
        cc.observe(1.0);
        assert!(cc.load() < Config::default().cpu_target, "one hot sample is smoothed, not obeyed");
        assert!(limit.get() >= 100, "so nothing is shed on the strength of it");
    }

    #[tokio::test]
    async fn sustained_overload_walks_the_ceiling_down() {
        let (limit, _held) = occupied(100, 100).await;
        let mut cc = CpuConcurrencyController::new(limit.clone(), Config::default());
        for _ in 0..20 {
            cc.observe(1.0);
        }
        let after_20 = limit.get();
        assert!(after_20 < 100, "sustained load sheds the ceiling: {after_20}");
        for _ in 0..20 {
            cc.observe(1.0);
        }
        assert!(limit.get() < after_20, "and keeps shedding while it stays hot");
    }

    #[tokio::test]
    async fn a_calm_saturated_server_reclaims_the_ceiling() {
        let (limit, _held) = occupied(100, 100).await;
        let config = Config { refractory: 0, ..Config::default() };
        let mut cc = CpuConcurrencyController::new(limit.clone(), config);
        for _ in 0..10 {
            cc.observe(0.1);
        }
        assert!(limit.get() > 100, "calm and saturated reclaims: {}", limit.get());
    }

    #[tokio::test]
    async fn an_idle_server_is_left_alone() {
        let (limit, _held) = occupied(100, 0).await;
        let mut cc = CpuConcurrencyController::new(limit.clone(), Config::default());
        for _ in 0..50 {
            cc.observe(0.1);
        }
        assert_eq!(limit.get(), 100, "nothing waits on the ceiling, so it does not move");
    }

    #[tokio::test]
    async fn the_refractory_period_outlasts_the_lagging_signal() {
        let config = Config { refractory: 5, ..Config::default() };
        let (limit, _held) = occupied(100, 100).await;
        let mut cc = CpuConcurrencyController::new(limit.clone(), config);
        for _ in 0..20 {
            cc.observe(1.0);
        }
        let shed = limit.get();
        // The load reads calm the moment the spike passes, but the average that
        // produced it is stale — so the ceiling holds until the refractory expires.
        for _ in 0..config.refractory {
            cc.observe(0.0);
            assert_eq!(limit.get(), shed, "still cooling off after an overload");
        }
        cc.observe(0.0);
        assert!(limit.get() > shed, "then it reclaims");
    }

    #[tokio::test]
    async fn the_decrease_stops_at_the_floor() {
        let config = Config { min_limit: 10, ..Config::default() };
        let (limit, _held) = occupied(12, 12).await;
        let mut cc = CpuConcurrencyController::new(limit.clone(), config);
        for _ in 0..500 {
            cc.observe(1.0);
        }
        assert_eq!(limit.get(), 10);
    }

    #[tokio::test]
    async fn the_increase_stops_at_the_ceiling() {
        let config = Config { max_limit: 14, refractory: 0, ..Config::default() };
        let (limit, _held) = occupied(12, 12).await;
        let mut cc = CpuConcurrencyController::new(limit.clone(), config);
        for _ in 0..500 {
            cc.observe(0.0);
        }
        assert_eq!(limit.get(), 14);
    }

    /// The ceiling stops rising once the work no longer reaches it, well before any
    /// configured bound: raising it further would buy capacity nothing is asking for.
    #[tokio::test]
    async fn the_increase_stops_when_the_ceiling_is_no_longer_the_constraint() {
        let config = Config { refractory: 0, ..Config::default() };
        let (limit, _held) = occupied(12, 12).await;
        let mut cc = CpuConcurrencyController::new(limit.clone(), config);
        for _ in 0..500 {
            cc.observe(0.0);
        }
        let settled = limit.get();
        let saturates = |l: usize| 12.0 >= (1.0 - config.increase_distance) * l as f64;
        assert!(
            saturates(settled - 1) && !saturates(settled),
            "settles one step past the widest ceiling 12 in-flight still saturates: {settled}"
        );
    }
}
