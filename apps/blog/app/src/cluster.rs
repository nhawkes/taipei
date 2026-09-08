//! The cluster — ten real single-server engines, running at once.
//!
//! Where [`MultiEngine`](crate::multi) fires one batch and settles, the cluster is
//! **continuous**: ten independent [`SimEngine`]s, each its own world and clock, all
//! advanced by one frame together. Every server is the same taipei stack the
//! single-server chapters drive, so a box's numbers and that server's full picture are
//! two readings of one running machine — the point this sim makes.

use taipei::compose::Stage;

use crate::engine::{Behavior, ServerCounts, SimEngine};

/// Servers in the cluster — the ten `web-01…web-10` the grid tiles.
pub const SERVERS: usize = 10;

/// The healthy arrival rate every server opens on — comfortably inside a mixed server's
/// ~100 req/s capacity, so the fleet rests busy-but-fine and the reader is the one who
/// pushes any single server past it.
pub const HEALTHY_QPS: f64 = 75.0;

/// The pace the cluster advances virtual time at. The single-server sims crawl at 0.01 to
/// follow one request end to end; the fleet is about the aggregate, so it runs quicker —
/// fast enough that a box carries a live steady state, slow enough that the focused
/// machine still reads as motion.
const CLUSTER_SPEED: f64 = 0.08;

/// The base seed the servers fan out from — `base ^ index`, so ten servers at one arrival
/// rate land on different draws. A real fleet is alike box to box, never identical.
const CLUSTER_SEED: u64 = 0x0c10_5eed;

/// Ten healthy taipei servers, ticked together.
pub struct Cluster {
    servers: Vec<SimEngine>,
}

impl Cluster {
    /// Ten healthy servers, cold at `t=0` — the reader watches them fill to steady state.
    pub fn new() -> Cluster {
        let servers = (0..SERVERS)
            .map(|k| {
                let mut engine = SimEngine::seeded(
                    CLUSTER_SEED ^ k as u64,
                    HEALTHY_QPS,
                    Stage::Queue,
                    None,
                    false,
                    Behavior::Good,
                );
                engine.set_speed(CLUSTER_SPEED);
                engine
            })
            .collect();
        Cluster { servers }
    }

    /// Advance every server by one frame of real time — one clock each, all stepped together.
    pub fn tick(&mut self, real_dt_ms: f64) {
        for server in &mut self.servers {
            server.tick(real_dt_ms);
        }
    }

    /// Every server's live summary counts, in `web-01…` order — what the fleet grid draws.
    pub fn fleet_counts(&self) -> Vec<ServerCounts> {
        self.servers.iter().map(|server| server.counts()).collect()
    }

    /// Move one server's arrival rate. The drill-down's knob calls this, and that server's
    /// box follows because it is the same engine — the reader who plays makes the fleet
    /// honestly uneven.
    pub fn set_qps(&mut self, server: usize, qps: f64) {
        if let Some(engine) = self.servers.get_mut(server) {
            engine.set_lambda(qps);
        }
    }

    /// One server, for the full picture the drill-down draws over it.
    pub fn server(&mut self, index: usize) -> &mut SimEngine {
        &mut self.servers[index]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ten servers, ticked together from cold, all reach a healthy steady state: each
    /// serves real requests and sheds little, and — because each runs its own seed — they
    /// are not carbon copies of one another.
    #[test]
    fn ten_servers_run_concurrently_to_a_healthy_steady_state() {
        let mut cluster = Cluster::new();
        // ~5 s of virtual time (2000 frames × 33 ms × 0.08) — well past fill, into steady state.
        for _ in 0..2000 {
            cluster.tick(33.0);
        }

        let counts = cluster.fleet_counts();
        assert_eq!(counts.len(), SERVERS);
        assert!(counts.iter().all(|c| c.success > 0), "every server serves its load");
        for c in &counts {
            // Healthy: what it shed is a small fraction of what it served.
            assert!(c.retried * 5 <= c.success, "a healthy server sheds little");
        }
        let successes: Vec<usize> = counts.iter().map(|c| c.success).collect();
        assert!(
            successes.iter().any(|&s| s != successes[0]),
            "independent seeds make the fleet differ in the small",
        );
    }
}
