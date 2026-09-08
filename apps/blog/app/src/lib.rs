#![cfg_attr(target_arch = "wasm32", feature(type_alias_impl_trait))]

//! The blog's **live table** — the sims, and the code-tab set, all **normal DOM lives**.
//!
//! A markdown ` ```sim ` fence becomes a live marker in the page body
//! (`blog_core::markdown_view`); this crate is that live's live half. Each instance
//! reads **its own** resolved spec off its live key ([`SimKey`]) and runs an ordinary
//! message loop shaped like Racket's `big-bang` and Elm's TEA: state in signals,
//! `on-tick` as a frame **message** (`ctx.frames(&running.read(), …)` — the Elm-style
//! signal-gated subscription), `to-draw` as ordinary reactive DOM bindings. No canvas, no escape hatch, no second wasm: the
//! moving parts are positioned elements the existing command stream updates, so a sim
//! is inspectable in devtools and **replayable from its message log** like everything
//! else.

use blog_core::SimKey;
use idyll::{Ctx, Setup};
use idyll_data::{fragment, query};

pub mod atoms;
mod autoscale;
mod blame;
mod cluster;
mod code_tabs;
mod engine;
mod fan;
mod fleet;
mod limits;
mod machine_view;
mod multi;
#[cfg(test)]
mod perf_probe;
mod flow;
mod lb;
mod percentile;
#[path = "queue_viz.rs"]
mod qviz;
mod rate_limit;
mod scale;
mod simview;
mod compose;
mod hotswap;
mod unbounded_queue;
pub mod styles;

// ── Data needs (validated against the published schema.json) ─────────────────────────

// The page fragment: the contract fields plus the `route` sum, matched exhaustively —
// an article's markdown, and an index that needs nothing. A sim's configuration rides its
// live **key** ([`SimKey`]), so nothing sim-specific rides the page node.
// A code group is fetched with the page, so the tab island has its data at first paint —
// the marker in the prose carries the group's id, and the island reads it back.
fragment! { CodeTabFrag on CodeTab { label, lang, code } }
fragment! { CodeGroupFrag on CodeGroup { id, tabs: [CodeTabFrag] } }
fragment! { PageFrag on Page { id, title, code_tabs: [CodeGroupFrag], route { Article { body }, Index {}, NotFound {} } } }

// **The route query** — the one persisted operation; the server imports
// `RouteQuery::query_file()`.
query! { RouteQuery($request: idyll_data::Request) { route(request: $request): PageFrag } }

/// The executed route query — every mount's one argument, the page included.
pub type PageSeed = idyll_data::Preloaded<RouteQueryRoots>;

pub mod pages;
pub use pages::{head, page};

// ── The sim live ────────────────────────────────────────────────────────────────────

/// The fence names this table can run — what the markdown mapper validates ` ```sim `
/// fences against (an unknown name is a visible content error, never a dead marker).
///
/// Each name is read off the live's own `LiveDef::NAME` — the same kebab-cased wire name
/// `guest!` derives from the table ident (`queue_viz` → `"queue-viz"`) and the marker
/// mounts under — so the fence allow-list cannot drift from the registration: renaming a
/// sim moves its def, and a stale entry here is a *compile* error, not a dead fence. The
/// set is still authored (the sim defs, not `code_tabs`), since "which lives are sims"
/// is app knowledge the membrane does not carry.
pub const SIM_NAMES: &[&str] = &[
    <live::QueueViz as idyll::live::LiveDef>::NAME,
    <live::Fleet as idyll::live::LiveDef>::NAME,
    <live::Fan as idyll::live::LiveDef>::NAME,
    <live::Blame as idyll::live::LiveDef>::NAME,
    <live::RateLimit as idyll::live::LiveDef>::NAME,
    <live::Percentile as idyll::live::LiveDef>::NAME,
    <live::Flow as idyll::live::LiveDef>::NAME,
    <live::Policies as idyll::live::LiveDef>::NAME,
    <live::Pool as idyll::live::LiveDef>::NAME,
    <live::Lb as idyll::live::LiveDef>::NAME,
    <live::Autoscale as idyll::live::LiveDef>::NAME,
];

/// The sim loop's messages: `big-bang`'s `on-tick`, the scrim toggle, and the
/// queue visualiser's knobs (each sim handles the subset it renders controls for).
#[derive(Debug)]
pub enum SimMsg {
    /// One animation frame: the delta since the previous, in milliseconds.
    Tick(f64),
    /// The run/pause control.
    Toggle,
    /// Release the still: the stage is armed until the first click, and clicking it
    /// only ever *starts* — pausing is the run control's job, so a stray click on a
    /// running machine can't stop it.
    Start,
    /// Arrival rate, in requests/second.
    Qps(f64),
    /// Simulation speed slider, raw 5–100 (÷100 ⇒ 0.05–1.00×).
    Speed(f64),
    /// Client-side response deadline, in ms.
    RespTimeoutMs(f64),
    /// The reject stage's live concurrency ceiling.
    Concurrency(f64),
    /// IO latency multiplier slider, raw 10–200 (÷100 ⇒ 0.1–2.0×).
    IoSpeed(f64),
    ToggleBackpressure,
    ToggleQueueTimeout,
    ToggleProcessingTimeout,
    /// Inject a single request right now (the blog's "send a request" control).
    Inject,
    /// Send one client `GET /ping` and watch for its reply (the manual sim's
    /// request/response affordance).
    Ping,
    /// Switch the leaf-server behavior tab (index into the fence's `servers` list).
    SetBehavior(usize),
    /// Switch the admission-gate tab (index into the fence's `gates` list).
    SetGate(usize),
    /// Show the policy at this index. Each policy keeps its own machine, so this
    /// changes what is shown and ticked — never what is running.
    SetPolicy(usize),
    /// Rebuild the engine with the current knob positions.
    Reset,
    /// The stage's width changed (`ctx.resizes`): the picture is laid out to the room
    /// it actually has, so a new width is a new layout.
    Resize(f64),
}

/// The queue visualiser. With `"tabs"` the fence names the policies the reader can
/// switch between; each is a real composition and each keeps its own machine, so
/// switching back finds the sim as it was left rather than at zero.
pub async fn queue_viz(ctx: Ctx<Setup, SimMsg>, seed: PageSeed, key: SimKey) -> idyll::Result {
    qviz::run(ctx, seed, key).await
}

/// The code-tabs live — a `` ```rust tab=… `` run in the markdown. Its key is the
/// group's page-scoped id, and the group itself rides inside the page record.
pub async fn code_tabs(
    ctx: Ctx<Setup, code_tabs::TabsMsg>,
    seed: PageSeed,
    key: blog_core::CodeKey,
) -> idyll::Result {
    code_tabs::run(ctx, seed, key).await
}

/// The cluster (Sim 1 of the load-balancing chapter): ten real taipei servers running
/// concurrently from t=0, each drawn as a live [`ServerBox`](atoms::server_box::ServerBox)
/// summary off its own engine.
pub async fn fleet(ctx: Ctx<Setup, fleet::FleetMsg>, seed: PageSeed, key: SimKey) -> idyll::Result {
    fleet::run(ctx, seed, key).await
}

/// The fan-out (Sim 2): ten clients to ten servers, one batch, and the latency it paid.
pub async fn fan(ctx: Ctx<Setup, fan::FanMsg>, seed: PageSeed, key: SimKey) -> idyll::Result {
    fan::run(ctx, seed, key).await
}

/// The percentile spread (Sim 3): one batch read at two percentiles, so the median client's
/// round trip and an unlucky one's are two lengths on one axis. The batch settles in the
/// browser once released — a frame of virtual time per animation frame.
pub async fn percentile(
    ctx: Ctx<Setup, percentile::PercentileMsg>,
    seed: PageSeed,
    key: SimKey,
) -> idyll::Result {
    percentile::run(ctx, seed, key).await
}

/// Continuous demand (Sim 4): the same ten machines under a rate that does not stop, filling
/// unevenly — the feedback loop a policy needs before it can have an opinion.
pub async fn flow(ctx: Ctx<Setup, flow::FlowMsg>, seed: PageSeed, key: SimKey) -> idyll::Result {
    flow::run(ctx, seed, key, flow::Shows::Loop).await
}

/// Picking a server (Sim 5): the same fleet with the three policies on a pill, switchable
/// while it runs, and the experience two percentiles deep.
pub async fn policies(
    ctx: Ctx<Setup, flow::FlowMsg>,
    seed: PageSeed,
    key: SimKey,
) -> idyll::Result {
    flow::run(ctx, seed, key, flow::Shows::Policies).await
}

/// Picking with what you paid for (Sim 6): the pill again, but pick-2's counters are what the
/// machines answered — aged, pooled, and only as fresh as the crowd's own request rate buys.
pub async fn pool(ctx: Ctx<Setup, flow::FlowMsg>, seed: PageSeed, key: SimKey) -> idyll::Result {
    flow::run(ctx, seed, key, flow::Shows::Pool).await
}

/// The balancer tier (Sim 7): a churning crowd through balancers that hold the warm lines and
/// the heard counters, read as five percentile lines the reader's own knob-turns move.
pub async fn lb(ctx: Ctx<Setup, lb::LbMsg>, seed: PageSeed, key: SimKey) -> idyll::Result {
    lb::run(ctx, seed, key).await
}

/// The blame panel (rate limiting): one server, three tenants, and the shut-time the real
/// `TenantReporter` mints and splits between them — drawn over the machine that earns it.
pub async fn blame(ctx: Ctx<Setup, blame::BlameMsg>, seed: PageSeed, key: SimKey) -> idyll::Result {
    blame::run(ctx, seed, key).await
}

/// Autoscale: a fleet that buys itself a machine, steered by what the routing already knows
/// or by CPU-utilisation — the same demand, and the delay between the two readings.
pub async fn autoscale(
    ctx: Ctx<Setup, autoscale::AutoscaleMsg>,
    seed: PageSeed,
    key: SimKey,
) -> idyll::Result {
    autoscale::run(ctx, seed, key).await
}

/// Rate limiting: three tenants and two servers dividing one budget, and the three seconds a
/// blame number takes to become a refusal.
pub async fn rate_limit(
    ctx: Ctx<Setup, rate_limit::RateLimitMsg>,
    seed: PageSeed,
    key: SimKey,
) -> idyll::Result {
    rate_limit::run(ctx, seed, key).await
}

idyll::guest! {
    seed: PageSeed,
    page: page(seed),
    head: head(seed),
    live: {
        queue_viz(seed, key: SimKey),
        fleet(seed, key: SimKey),
        fan(seed, key: SimKey),
        blame(seed, key: SimKey),
        rate_limit(seed, key: SimKey),
        percentile(seed, key: SimKey),
        flow(seed, key: SimKey),
        policies(seed, key: SimKey),
        pool(seed, key: SimKey),
        lb(seed, key: SimKey),
        autoscale(seed, key: SimKey),
        code_tabs(seed, key: blog_core::CodeKey),
    }
}
