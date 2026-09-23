use crate::limit::ConcurrencyLimit;

#[derive(Clone, Copy, Debug)]
pub struct Config {
    pub cpu_target: f64,
    pub decrease: f64,
    pub increase: f64,
    pub increase_distance: f64,
    pub refractory: u32,
    pub smoothing: f64,
    pub min_limit: usize,
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

pub struct CpuConcurrencyController {
    limit: ConcurrencyLimit,
    config: Config,
    load: f64,
    calm_cycles: u32,
}

impl CpuConcurrencyController {
    pub fn new(limit: ConcurrencyLimit, config: Config) -> Self {
        Self {
            limit,
            config,
            load: 0.0,
            calm_cycles: config.refractory,
        }
    }

    pub fn load(&self) -> f64 {
        self.load
    }

    pub fn observe(&mut self, load: f64) {
        let alpha = self.config.smoothing.clamp(0.0, 1.0);
        self.load += alpha * (load.clamp(0.0, 1.0) - self.load);

        let limit = self.limit.get();
        if self.load >= self.config.cpu_target {
            self.calm_cycles = 0;
            self.limit
                .set(step(limit, -self.config.decrease, self.config));
            return;
        }

        let usage = limit.saturating_sub(self.limit.available());
        let saturated = usage as f64 >= (1.0 - self.config.increase_distance) * limit as f64;
        if self.calm_cycles >= self.config.refractory && saturated {
            self.limit
                .set(step(limit, self.config.increase, self.config));
        }
        self.calm_cycles = self.calm_cycles.saturating_add(1);
    }
}

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
        assert!(
            cc.load() < Config::default().cpu_target,
            "one hot sample is smoothed, not obeyed"
        );
        assert!(
            limit.get() >= 100,
            "so nothing is shed on the strength of it"
        );
    }

    #[tokio::test]
    async fn sustained_overload_walks_the_ceiling_down() {
        let (limit, _held) = occupied(100, 100).await;
        let mut cc = CpuConcurrencyController::new(limit.clone(), Config::default());
        for _ in 0..20 {
            cc.observe(1.0);
        }
        let after_20 = limit.get();
        assert!(
            after_20 < 100,
            "sustained load sheds the ceiling: {after_20}"
        );
        for _ in 0..20 {
            cc.observe(1.0);
        }
        assert!(
            limit.get() < after_20,
            "and keeps shedding while it stays hot"
        );
    }

    #[tokio::test]
    async fn a_calm_saturated_server_reclaims_the_ceiling() {
        let (limit, _held) = occupied(100, 100).await;
        let config = Config {
            refractory: 0,
            ..Config::default()
        };
        let mut cc = CpuConcurrencyController::new(limit.clone(), config);
        for _ in 0..10 {
            cc.observe(0.1);
        }
        assert!(
            limit.get() > 100,
            "calm and saturated reclaims: {}",
            limit.get()
        );
    }

    #[tokio::test]
    async fn an_idle_server_is_left_alone() {
        let (limit, _held) = occupied(100, 0).await;
        let mut cc = CpuConcurrencyController::new(limit.clone(), Config::default());
        for _ in 0..50 {
            cc.observe(0.1);
        }
        assert_eq!(
            limit.get(),
            100,
            "nothing waits on the ceiling, so it does not move"
        );
    }

    #[tokio::test]
    async fn the_refractory_period_outlasts_the_lagging_signal() {
        let config = Config {
            refractory: 5,
            ..Config::default()
        };
        let (limit, _held) = occupied(100, 100).await;
        let mut cc = CpuConcurrencyController::new(limit.clone(), config);
        for _ in 0..20 {
            cc.observe(1.0);
        }
        let shed = limit.get();
        for _ in 0..config.refractory {
            cc.observe(0.0);
            assert_eq!(limit.get(), shed, "still cooling off after an overload");
        }
        cc.observe(0.0);
        assert!(limit.get() > shed, "then it reclaims");
    }

    #[tokio::test]
    async fn the_decrease_stops_at_the_floor() {
        let config = Config {
            min_limit: 10,
            ..Config::default()
        };
        let (limit, _held) = occupied(12, 12).await;
        let mut cc = CpuConcurrencyController::new(limit.clone(), config);
        for _ in 0..500 {
            cc.observe(1.0);
        }
        assert_eq!(limit.get(), 10);
    }

    #[tokio::test]
    async fn the_increase_stops_at_the_ceiling() {
        let config = Config {
            max_limit: 14,
            refractory: 0,
            ..Config::default()
        };
        let (limit, _held) = occupied(12, 12).await;
        let mut cc = CpuConcurrencyController::new(limit.clone(), config);
        for _ in 0..500 {
            cc.observe(0.0);
        }
        assert_eq!(limit.get(), 14);
    }

    #[tokio::test]
    async fn the_increase_stops_when_the_ceiling_is_no_longer_the_constraint() {
        let config = Config {
            refractory: 0,
            ..Config::default()
        };
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
