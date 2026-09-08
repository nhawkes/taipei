//! ServerBox — one server as a box of live numbers: busy-core pips, the three queue
//! depths it keeps (kernel accept, taipei admission, runtime), the work in flight, and
//! the running outcome tallies. The shared atom of the multi-server sims — the cluster
//! grid tiles it live off each engine, and the fan-out stacks it down one side.
//!
//! Controlled (`Never`): the counts arrive as a [`Signal`], so a box drawn off a running
//! engine updates its readouts in place as that engine moves, and a box drawn off a fixed
//! snapshot takes a constant. A zero reads in the muted "nothing here" colour so a glance
//! across a fleet finds the loaded boxes; a real count wears its section's hue.

use idyll::{live_view, Ctx, Never, Setup, Signal};
use idyll_styles::{styles, Style};

use crate::atoms::stage::{Paint, Stage};
use crate::engine::{ServerCounts, TenantQueues};

/// One core pip, keyed by its slot so the pip row restyles in place as cores fill. `ink` is
/// the colour it wears while it is lit — its occupant's where the caller names one, the
/// machine's own blue otherwise — and empty while the core is free, which leaves the pip the
/// unlit colour the stylesheet gives it.
#[derive(Clone, PartialEq)]
struct Pip {
    i: usize,
    ink: String,
}

fn pip_ink(p: &Pip) -> String {
    p.ink.clone()
}

/// One tenant's line in a box's queue block: who it is, what it is costing *this* server, and
/// the share of its traffic the fleet is refusing.
///
/// The blame is the box's own — a server reports what it measured — while the share is the
/// fleet's, so the same percentage appears in every box. That is the reading: one machine's
/// evidence, everyone's decision.
#[derive(Clone, PartialEq)]
pub struct TenantLine {
    pub name: String,
    pub tint: Paint,
    /// Where this tenant's requests are waiting — the box's own three queues, its share of them.
    pub queues: TenantQueues,
    /// Shut-time this tenant has drawn on this server this epoch, in milliseconds.
    pub blame_ms: f64,
    /// The share of its traffic being refused, `0.0..=1.0`.
    pub drop_pct: f64,
}

fn line_swatch(t: &TenantLine) -> String {
    format!("background:{}", t.tint)
}

/// The three queue locations, as the box has always named them — this tenant's share of each.
fn line_queues(t: &TenantLine) -> String {
    let TenantQueues { tcp, app, run } = t.queues;
    format!("{tcp} tcp  {app} app  {run} run")
}

fn line_blame(t: &TenantLine) -> String {
    format!("{:.0} ms", t.blame_ms)
}

fn line_drop(t: &TenantLine) -> String {
    format!("{:.0}%", t.drop_pct * 100.0)
}

/// Derive one count off the box's live counts as its own signal, so each readout tracks
/// its field alone.
fn field(
    ctx: &Ctx<Setup, Never>,
    counts: &Signal<ServerCounts>,
    get: fn(&ServerCounts) -> usize,
) -> Signal<usize> {
    let counts = counts.clone();
    ctx.computed(move |cx| get(&counts.get(cx))).read()
}

/// One labelled number: its label, its section hue, and the count — muted to the "nothing
/// here" colour at zero. A plain leaf, reused seven times per box; cheap enough (no
/// boundary observes it) that factoring it beats inlining seven near-identical rows.
#[idyll::component]
async fn Metric(ctx: Ctx<Setup, Never>, label: &'static str, hue: Style, v: Signal<usize>) -> idyll::Result {
    let count = v.clone();
    let text = ctx.computed(move |cx| group(count.get(cx))).read();
    let zero = ctx.computed(move |cx| v.get(cx) == 0).read();
    Ok(ctx.render(live_view! {
        div css=[styles::M] {
            span css=[styles::ML] { (label) }
            span css=[styles::MV, hue, $zero => styles::ZERO] { $text }
        }
    }).await?)
}

#[idyll::component]
pub async fn ServerBox(
    ctx: Ctx<Setup, Never>,
    name: String,
    counts: Signal<ServerCounts>,
    idle: Signal<bool>,
    /// The lit pips' colours, in the order the pips light — one per busy core, from a caller
    /// that colours cores by occupant (the blame panel's tenant mode). Left off, every busy
    /// pip wears the machine's own blue: a fleet box has no occupants to name. How *many*
    /// pips light is always the box's own `busy` count, so a key that runs short colours what
    /// it reaches and no pip can claim a core the counts do not.
    #[opt] ink: Signal<Vec<Paint>>,
    /// Who is queueing, where the box's server knows. Supplied, the queue block becomes one
    /// line per tenant — what each is costing this machine and what is being refused — because
    /// on a box shared between tenants *whose* queue it is outranks how deep it is. Left off
    /// (the fleet and fan-out boxes, which model no tenants) the block keeps the three depths.
    #[opt] tenants: Signal<Vec<TenantLine>>,
) -> idyll::Result {
    // The colour key a caller supplied, if any. Empty is the ordinary case — the machine's own
    // blue is the box's own colour for a busy core.
    let ink = ink.unwrap_or_else(|| ctx.constant(Vec::new()));
    let pips = {
        let counts = counts.clone();
        ctx.computed(move |cx| {
            let busy = counts.get(cx).busy;
            let ink = ink.get(cx);
            (0..8usize)
                .map(|i| Pip {
                    i,
                    ink: match i < busy {
                        true => format!("background:{}", ink.get(i).copied().unwrap_or(Stage::blue.value())),
                        false => String::new(),
                    },
                })
                .collect::<Vec<_>>()
        })
        .read()
    };
    let (tcp, req, run) = (
        field(&ctx, &counts, |c| c.tcp),
        field(&ctx, &counts, |c| c.req),
        field(&ctx, &counts, |c| c.run),
    );
    // Whether this box is shared between named tenants. A caller that models none supplies no
    // lines, and the block keeps the queue depths it always showed.
    let tenants = tenants.unwrap_or_else(|| ctx.constant(Vec::new()));
    let shared = {
        let tenants = tenants.clone();
        ctx.computed(move |cx| !tenants.get(cx).is_empty()).read()
    };
    let (inflight, retried, success, failed, refused) = (
        field(&ctx, &counts, |c| c.inflight),
        field(&ctx, &counts, |c| c.retried),
        field(&ctx, &counts, |c| c.success),
        field(&ctx, &counts, |c| c.failed),
        field(&ctx, &counts, |c| c.rate_limited),
    );
    ctx.render(live_view! {
        div css=[styles::BOX, $idle => styles::IDLE] {
            div css=[styles::SH] {
                span css=[styles::SN] { (name) }
                div css=[styles::PIPS] {
                    @for pip in $pips [key = pip.i] {
                        span css=[styles::PIP] style=(pip_ink(&$pip)) {}
                    }
                }
            }
            @if ($shared) {
                div css=[styles::QCOL] {
                    div css=[styles::QHEAD] {
                        span css=[styles::QCAP] { "queue" }
                        span css=[styles::QCAP] { "rate limiting" }
                    }
                    @for t in $tenants [key = t.name.clone()] {
                        div css=[styles::TLINE] {
                            span css=[styles::TDOT] style=(line_swatch(&$t)) {}
                            span css=[styles::TNAME] { ($t.name) }
                            span css=[styles::TQ] { (line_queues(&$t)) }
                            span css=[styles::TNUM] { (line_blame(&$t)) }
                            span css=[styles::TPCT] { (line_drop(&$t)) }
                        }
                    }
                }
            } else {
                div css=[styles::QROW] {
                    span css=[styles::QCAP] { "queue" }
                    Metric label=("tcp") hue=(styles::TEAL) v=(tcp)
                    Metric label=("req") hue=(styles::GREY) v=(req)
                    Metric label=("run") hue=(styles::AMBER) v=(run)
                }
            }
            div css=[styles::DIV] {}
            div css=[styles::RGRID] {
                div css=[styles::RCOL] {
                    Metric label=("inflight") hue=(styles::PURPLE) v=(inflight)
                    Metric label=("success") hue=(styles::GREEN) v=(success)
                }
                div css=[styles::VDIV] {}
                div css=[styles::RCOL] {
                    Metric label=("retried queue timeout") hue=(styles::ORANGE) v=(retried)
                    Metric label=("failed") hue=(styles::RED) v=(failed)
                    // A box that names its tenants is a box that can turn one away for being
                    // over its share; one that does not has no such number to report.
                    @if ($shared) {
                        Metric label=("tenant rate limited") hue=(styles::AMBER) v=(refused)
                    }
                }
            }
        }
    }).await?
    .finish()
    .await
}

/// The zero-padded display name for the server at index `i` (`web-01`…) — the one form
/// the multi-server sims label their boxes with.
pub fn server_name(i: usize) -> String {
    format!("web-{:02}", i + 1)
}

/// Thousands as a thin space — 1 234, not 1,234 — matching the design's numerals.
pub fn group(v: usize) -> String {
    let s = v.to_string();
    let mut out = String::new();
    let n = s.len();
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (n - i) % 3 == 0 {
            out.push('\u{2009}');
        }
        out.push(ch);
    }
    out
}

#[styles]
pub mod styles {
    use idyll_styles::Style;
    use crate::atoms::stage::Stage;
    use crate::atoms::tokens::Face;

    pub const BOX: Style = css! {{
        background: Stage::channel,
        border: "1px solid #e2e5d5",
        border_radius: "11px",
        padding: "8px 10px 9px",
        position: "relative",
    }};
    pub const IDLE: Style = css! {{ opacity: 0.58 }};
    pub const SH: Style = css! {{
        display: "flex",
        align_items: "center",
        justify_content: "space-between",
        margin_bottom: "5px",
    }};
    pub const SN: Style = css! {{
        font_family: Face::mono,
        font_size: "11.5px",
        font_weight: 600,
    }};
    pub const PIPS: Style = css! {{ display: "flex", gap: "2px" }};
    /// A core pip at rest. A lit one writes its occupant's colour inline over this, so one
    /// row of pips can carry a whole colour scheme without a class per hue.
    pub const PIP: Style = css! {{
        width: "4px",
        height: "11px",
        border_radius: "1.5px",
        background: "#e3e7d6",
        transition: "background .2s",
    }};

    pub const QROW: Style = css! {{
        display: "flex",
        gap: "10px",
        font_size: "10.5px",
        font_variant_numeric: "tabular-nums",
        align_items: "center",
    }};
    pub const QCAP: Style = css! {{
        font_size: "8px",
        letter_spacing: ".08em",
        text_transform: "uppercase",
        color: "#aab09c",
        font_weight: 600,
        flex_shrink: 0,
    }};
    /// The two halves of a tenant line, named over the columns they head: where that tenant is
    /// waiting on the left, what the scheme is doing about it on the right.
    pub const QHEAD: Style = css! {{
        display: "flex",
        align_items: "baseline",
        justify_content: "space-between",
        gap: "10px",
    }};

    /// The queue block when the box is shared: a stack of tenant lines rather than a row of
    /// depths.
    pub const QCOL: Style = css! {{
        display: "flex",
        flex_direction: "column",
        gap: "2px",
        font_size: "10.5px",
        font_variant_numeric: "tabular-nums",
    }};

    /// One tenant's line. The name takes the slack so both numbers sit against the right edge,
    /// where a column of them can be read down.
    pub const TLINE: Style = css! {{
        display: "flex",
        align_items: "center",
        gap: "5px",
    }};
    pub const TDOT: Style = css! {{
        width: "5px",
        height: "5px",
        border_radius: "50%",
        flex_shrink: 0,
    }};
    pub const TNAME: Style = css! {{
        color: "#9aa08d",
        width: "48px",
        flex_shrink: 0,
    }};

    /// Where this tenant is waiting, left aligned under the name — the queue locations read
    /// across, so a glance down the block compares the same place on every tenant. It takes
    /// the slack, which is what pushes the two readings against the right edge.
    pub const TQ: Style = css! {{
        color: "#9aa08d",
        flex_grow: 1,
        min_width: "0",
        white_space: "nowrap",
    }};
    /// Fixed widths, so the millisecond and the percentage each form a column whatever their
    /// digits — the point of the block is comparing tenants down it.
    pub const TNUM: Style = css! {{
        width: "44px",
        text_align: "right",
        font_weight: 600,
        color: Stage::amber,
    }};
    pub const TPCT: Style = css! {{
        width: "30px",
        text_align: "right",
        font_weight: 600,
        color: Stage::orange,
    }};

    pub const DIV: Style = css! {{
        height: "1px",
        background: "#edefe1",
        margin: "6px -3px",
    }};
    /// What became of the traffic: what the server still holds on the left, what left it
    /// without being served on the right. The right column is the wider of the two because its
    /// readings need naming — "failed" and "turned away for being over a share" are not the
    /// same event, and a box that ran them together would be saying the server dropped both.
    pub const RGRID: Style = css! {{
        display: "grid",
        grid_template_columns: "auto 1px 1fr",
        column_gap: "10px",
        font_size: "10.5px",
        font_variant_numeric: "tabular-nums",
    }};
    pub const RCOL: Style = css! {{
        display: "flex",
        flex_direction: "column",
        gap: "3px",
    }};
    pub const VDIV: Style = css! {{
        background: "#edefe1",
    }};
    pub const M: Style = css! {{
        display: "flex",
        justify_content: "space-between",
        gap: "6px",
    }};
    pub const ML: Style = css! {{ color: "#9aa08d" }};
    pub const MV: Style = css! {{ display: "inline-block", min_width: "5ch", text_align: "right", font_weight: 600 }};

    // The station hues are the diagram's shared vocabulary — one source in `stage.rs`.
    pub const TEAL: Style = css! {{ color: Stage::teal }};
    pub const GREY: Style = css! {{ color: Stage::grey }};
    pub const AMBER: Style = css! {{ color: Stage::amber }};
    pub const PURPLE: Style = css! {{ color: Stage::purple }};
    pub const ORANGE: Style = css! {{ color: Stage::orange }};
    pub const GREEN: Style = css! {{ color: Stage::green }};
    pub const RED: Style = css! {{ color: Stage::red }};
    pub const ZERO: Style = css! {{ color: "#c3c8b8", font_weight: 500 }};
}
