//! The protection compositions the queue visualiser runs — **and shows**.
//!
//! Each function is the exact tower stack for one stage, its body captured verbatim by
//! `#[shown]` into a `*_SRC` constant. The engine calls the function to build the
//! service; the panel displays the constant. The code the reader sees is the code that
//! ran, because it is the same function — there is no second, hand-copied listing to
//! drift from it.
//!
//! `app` is the sim's modelled server. `handle` is the runtime the queue workers spawn
//! on. `limit` is the concurrency ceiling the slider sets — moving it rebuilds the stack
//! and hot-swaps it in (see [`crate::hotswap`]), so a plain composition needs no live
//! handle and its body stays pristine. The one exception is the OS-CPU gate, whose whole
//! point is a handle a controller tunes; there the handle *is* the mechanism, and the
//! listing shows it.

use std::sync::Arc;
use std::time::Duration;

use taipei::backpressure::{CpuBackpressureLayer, CpuBackpressureService, RuntimeInstrumentation};
use taipei::limit::{ConcurrencyLimit, DynamicConcurrencyLimitLayer, DynamicConcurrencyLimitService};
use taipei::queue::{QueueLayer, QueueService, QueueWorker};
use taipei::rate_limit::{Limits, RateLimitLayer, RateLimitService};
use taipei::reject::{RejectionLayer, RejectionService};
use taipei::tenant::{TenantReportService, TenantReporter};
use tokio::runtime::Handle;
use tower::{Layer, ServiceBuilder};

use crate::engine::{App, SimReq};
use crate::unbounded_queue::{UnboundedQueue, UnboundedQueueService, UnboundedQueueWorker};

/// CPU backpressure only: readiness is withheld while every core is busy, so a caller
/// waits for one. Nothing sheds — there is no escape valve yet.
#[taipei_macros::shown(BACKPRESSURE_SRC)]
pub(crate) fn backpressure(
    app: App,
    instr: &RuntimeInstrumentation,
) -> CpuBackpressureService<App> {
    ServiceBuilder::new()
        .layer(CpuBackpressureLayer::new(instr))
        .service(app)
}

/// A concurrency limit plus immediate rejection, no queue.
#[taipei_macros::shown(REJECT_SRC)]
pub(crate) fn reject(
    app: App,
    limit: usize,
) -> RejectionService<DynamicConcurrencyLimitService<App>> {
    // Shed immediately while every slot is taken — HTTP 529, safe to retry.
    ServiceBuilder::new()
        .layer(RejectionLayer::new())
        .layer(DynamicConcurrencyLimitLayer::new(limit))
        .service(app)
}

/// The same limit with nothing above it: callers wait for a slot, unboundedly.
#[taipei_macros::shown(WAIT_SRC)]
pub(crate) fn wait(
    app: App,
    limit: usize,
    handle: Handle,
) -> (
    UnboundedQueueService<SimReq, ()>,
    UnboundedQueueWorker<DynamicConcurrencyLimitService<App>, SimReq, ()>,
) {
    // No rejection layer: callers wait, unboundedly, for a free slot.
    let inner = ServiceBuilder::new()
        .layer(DynamicConcurrencyLimitLayer::new(limit))
        .service(app);
    UnboundedQueue::build(inner, handle)
}

/// A hand-tuned concurrency limit behind the queue — the naive gate.
#[taipei_macros::shown(QUEUE_NAIVE_SRC)]
pub(crate) fn queue_naive(
    app: App,
    limit: usize,
    handle: Handle,
    queue_timeout: Duration,
) -> (
    QueueService<SimReq, ()>,
    QueueWorker<DynamicConcurrencyLimitService<App>, SimReq, ()>,
) {
    // Tuned by hand on an average workload; goes stale when the workload moves.
    let inner = ServiceBuilder::new()
        .layer(DynamicConcurrencyLimitLayer::new(limit))
        .service(app);
    QueueLayer::new(queue_timeout).build(inner, handle)
}

/// CPU backpressure behind the queue — the gate taipei recommends.
#[taipei_macros::shown(QUEUE_SRC)]
pub(crate) fn queue(
    app: App,
    instr: &RuntimeInstrumentation,
    handle: Handle,
    queue_timeout: Duration,
) -> (
    QueueService<SimReq, ()>,
    QueueWorker<CpuBackpressureService<App>, SimReq, ()>,
) {
    // Tokio reports thread start/stop as it happens: instantaneous availability.
    // Admit while under half the cores are active.
    let inner = ServiceBuilder::new()
        .layer(CpuBackpressureLayer::new(instr))
        .service(app);
    QueueLayer::new(queue_timeout).build(inner, handle)
}

/// The recommended gate with tenant accounting under it. The reporter sits **below the queue and
/// above the gate**, which is the only place it can measure: `poll_ready` is where a caller is told
/// to wait, so the layer's own `poll_ready` — delegating to the backpressure gate's — spans exactly
/// the shut-time, and the queue worker racing its deadline on `service.ready()` is the one caller
/// the reporter is specified for.
#[taipei_macros::shown(QUEUE_TENANT_SRC)]
pub(crate) fn queue_tenant<R: Fn(&str, Duration) + Clone + Send + 'static>(
    app: App,
    instr: &RuntimeInstrumentation,
    handle: Handle,
    queue_timeout: Duration,
    reporter: &TenantReporter,
    report: R,
) -> (QueueService<SimReq, ()>, QueueWorker<TenantReportService<CpuBackpressureService<App>, R>, SimReq, ()>) {
    // Below the queue, so only admitted requests are occupiers — a waiter never pays. Above the
    // gate, so the shut-time it bills is the gate's own `Pending`.
    let inner = ServiceBuilder::new()
        .layer(reporter.layer(report))
        .layer(CpuBackpressureLayer::new(instr))
        .service(app);
    QueueLayer::new(queue_timeout).build(inner, handle)
}

/// The tenanted gate, with the fleet's rate limit in front of it. The one composition where a
/// server is not deciding alone: `limits` is the shared store, and both directions of it are
/// here — blame goes out through the reporter's completion callback, and the share to refuse
/// comes back through the layer on top.
///
/// The limit is the **outermost** layer, above the queue. A request that is going to be refused
/// must not first take a queue slot from one that is not.
#[taipei_macros::shown(QUEUE_RATE_LIMITED_SRC)]
pub(crate) fn queue_rate_limited<L, R>(
    app: App,
    instr: &RuntimeInstrumentation,
    handle: Handle,
    queue_timeout: Duration,
    reporter: &TenantReporter,
    write_blame: R,
    limits: Arc<L>,
) -> (
    RateLimitService<QueueService<SimReq, ()>, L>,
    QueueWorker<TenantReportService<CpuBackpressureService<App>, R>, SimReq, ()>,
)
where
    L: Limits,
    R: Fn(&str, Duration) + Clone + Send + 'static,
{
    // Below the queue, so only admitted requests are occupiers — a waiter never pays. Above the
    // gate, so the shut-time it bills is the gate's own `Pending`.
    let inner = ServiceBuilder::new()
        .layer(reporter.layer(write_blame))
        .layer(CpuBackpressureLayer::new(instr))
        .service(app);
    let (queue, worker) = QueueLayer::new(queue_timeout).build(inner, handle);
    // The share this tenant is over is the fleet's answer, three epochs stale, and refusing by
    // it costs the server nothing it was going to spend on anyone else.
    (RateLimitLayer::new(limits).layer(queue), worker)
}

/// An OS-CPU controller tuning the limit behind the queue. The controller drives the
/// shared [`ConcurrencyLimit`] from a load signal (the sim feeds it utilisation per
/// frame where a server would read the cgroup); admission stays instant, only the
/// ceiling moves — slowly, which is the lesson.
#[taipei_macros::shown(QUEUE_OS_CPU_SRC)]
pub(crate) fn queue_os_cpu(
    app: App,
    limit: ConcurrencyLimit,
    handle: Handle,
    queue_timeout: Duration,
) -> (
    QueueService<SimReq, ()>,
    QueueWorker<DynamicConcurrencyLimitService<App>, SimReq, ()>,
) {
    // The ceiling is the controller's; the limit layer only enforces it.
    let inner = ServiceBuilder::new()
        .layer(DynamicConcurrencyLimitLayer::from_handle(limit))
        .service(app);
    QueueLayer::new(queue_timeout).build(inner, handle)
}

/// The captured source of the composition [`crate::engine`] runs for a stage — the exact
/// text of the function above, so the panel can never show code the machine isn't running.
pub(crate) fn src_for(stage: taipei::compose::Stage, gate: Option<crate::engine::Gate>) -> &'static str {
    use crate::engine::Gate;
    use taipei::compose::Stage;
    match (stage, gate) {
        (Stage::App, _) => "// The bare service — no protection.\napp",
        (Stage::Backpressure, _) => BACKPRESSURE_SRC,
        (Stage::Reject, _) => REJECT_SRC,
        (Stage::Wait, _) => WAIT_SRC,
        (Stage::Queue, Some(Gate::RuntimeCpu)) => QUEUE_SRC,
        (Stage::Queue, Some(Gate::OsCpu)) => QUEUE_OS_CPU_SRC,
        (Stage::Queue, _) => QUEUE_NAIVE_SRC,
    }
}
