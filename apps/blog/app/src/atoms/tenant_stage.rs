//! The tenants-and-servers stage: a column of tenant dots on the left, server boxes in the
//! middle, and the live wires between them.
//!
//! Where [`wires`](super::wires) is the geometry, this is the picture built out of it — the
//! measured stage, the dots, the boxes, and the three strokes each wire carries: the bare
//! connection under everything, the verdicts on their way home in their own colours, and the
//! requests travelling out in their tenant's.
//!
//! Two sims draw it: the blame panel, where three tenants share one server, and the rate
//! limiting chapter, where the same three talk to a fleet of two. The difference is only how
//! many boxes and therefore how many wires — a tenant on a fleet is several [`Wire`]s sharing
//! a dot, which is why a wire names both of its ends rather than being found by position.
//!
//! The rects come in as signals and the measurements go out as one callback, because
//! measurement is the caller's message loop's: a component that owns no messages cannot
//! receive the layout it caused.

use idyll::{live_view, Callback, Ctx, Event, Never, Rect, Setup, Signal};
use idyll_styles::styles;

use super::server_box::{ServerBox, TenantLine};
use super::stage::Paint;
use super::wires::{marks, styles as wstyles, wires_d, WIRE_LEN};
use crate::engine::{ServerCounts, HOMEWARD};
use crate::machine_view::outcome_col;

/// A laid-out element of the stage reporting where it ended up. The wires are a pure
/// function of these, so a reflow re-bows them without anything being told a box moved.
#[derive(Debug)]
pub enum Measured {
    Stage(Rect),
    Tenant(usize, Rect),
    Server(usize, Rect),
}

/// One tenant's connection to one server, and what is on it this frame.
///
/// Both ends are named. A tenant talking to a fleet has one of these per server, and finding
/// the far end by the wire's position in a list would pair a dot with whichever box happened
/// to be at the same index.
#[derive(Clone, PartialEq)]
pub struct Wire {
    pub tenant: usize,
    pub server: usize,
    /// How this tenant's traffic has been going as a whole — the wire's colour. Not a reading
    /// of what is on the wire right now: a stream can be getting shed while this instant's
    /// requests are still outbound.
    pub shed: bool,
    /// The requests travelling out, each at the fraction of the wire it has covered.
    pub sent: Vec<f64>,
    /// The verdicts travelling home, one lane per [`HOMEWARD`] outcome in that order, each
    /// drawn in that outcome's colour.
    pub home: Vec<Vec<f64>>,
}

/// One server box on the stage: its name, and the live readings it draws.
#[derive(Clone)]
pub struct ServerRow {
    pub name: String,
    pub counts: Signal<ServerCounts>,
    pub idle: Signal<bool>,
    /// Who holds each busy core, in slot order — empty leaves the pips the machine's own blue.
    pub ink: Signal<Vec<Paint>>,
    /// Who is queueing on this server. Empty leaves the box its three queue depths.
    pub tenants: Signal<Vec<TenantLine>>,
}

/// One tenant's dot: what it is called and the colour it wears everywhere else.
#[derive(Clone)]
pub struct TenantDot {
    pub name: String,
    pub tint: Paint,
}

#[idyll::component]
pub async fn TenantStage(
    ctx: Ctx<Setup, Never>,
    tenants: Vec<TenantDot>,
    servers: Vec<ServerRow>,
    wires: Signal<Vec<Wire>>,
    stage: Signal<Option<Rect>>,
    tenant_rects: Signal<Vec<Option<Rect>>>,
    server_rects: Signal<Vec<Option<Rect>>>,
    measured: Callback<Measured>,
) -> idyll::Result {
    // Every wire's `d` off one computation: the bow depends only on the three measurements, so
    // a reflow rebuilds the whole set rather than each path watching its own endpoints.
    let bows = {
        let (stage, tenant_rects, server_rects, wires) =
            (stage.clone(), tenant_rects.clone(), server_rects.clone(), wires.clone());
        ctx.computed(move |cx| {
            let (stage, dots, boxes) =
                (stage.get(cx), tenant_rects.get(cx), server_rects.get(cx));
            wires
                .get(cx)
                .iter()
                .map(|w| wires_d(&stage, &dots, &boxes, [(w.tenant, w.server)]))
                .collect::<Vec<_>>()
        })
        .read()
    };
    let at = |i: usize| {
        let bows = bows.clone();
        ctx.computed(move |cx| bows.get(cx).get(i).cloned().unwrap_or_default()).read()
    };
    let wire_count = wires.at_mount(&ctx).len();

    // The connection itself, under everything: drawn because it exists, and left bare so that a
    // wire carrying nothing says so by having nothing on it.
    let bare: Vec<_> = (0..wire_count).map(at).collect();

    // The outbound stroke: this tenant's colour, or the shed one where its stream is coming
    // back refused.
    let outbound: Vec<_> = (0..wire_count)
        .map(|i| {
            let (layer, on_wire) = (wires.clone(), wires.clone());
            let shed = ctx.computed(move |cx| layer.get(cx).get(i).is_some_and(|w| w.shed)).read();
            let dashes = ctx
                .computed(move |cx| {
                    let sent = on_wire.get(cx).get(i).map(|w| w.sent.clone()).unwrap_or_default();
                    format!("stroke-dasharray:{}", marks(sent))
                })
                .read();
            (at(i), shed, dashes)
        })
        .collect();

    // One reply lane per wire per outcome: the same bow, that outcome's colour, and whatever of
    // that tenant's verdicts are on their way home wearing it.
    let mut replies: Vec<(Signal<String>, Signal<String>)> = Vec::new();
    for i in 0..wire_count {
        for (lane, outcome) in HOMEWARD.into_iter().enumerate() {
            let col = outcome_col(outcome);
            let wires = wires.clone();
            let ink = ctx
                .computed(move |cx| {
                    let on_wire = wires
                        .get(cx)
                        .get(i)
                        .and_then(|w| w.home.get(lane).cloned())
                        .unwrap_or_default();
                    format!("stroke:{col};stroke-dasharray:{}", marks(on_wire))
                })
                .read();
            replies.push((at(i), ink));
        }
    }

    let dots: Vec<_> = tenants
        .into_iter()
        .enumerate()
        .map(|(k, t)| {
            let dot = format!("border-color:{}", t.tint);
            (t.name, dot, measured.try_contra_map(move |e: Event| e.rect().map(|r| Measured::Tenant(k, r))))
        })
        .collect();
    let boxes: Vec<_> = servers
        .into_iter()
        .enumerate()
        .map(|(k, s)| {
            let placed = measured.try_contra_map(move |e: Event| e.rect().map(|r| Measured::Server(k, r)));
            (s.name, s.counts, s.idle, s.ink, s.tenants, placed)
        })
        .collect();
    let placed_stage = measured.try_contra_map(|e: Event| e.rect().map(Measured::Stage));

    Ok(ctx.render(live_view! {
        div css=[wstyles::STAGE] measure=(placed_stage) {
            div css=[wstyles::CLIENTS] {
                @for (name, dot, placed) in (dots) {
                    div css=[wstyles::CNODE] {
                        span css=[wstyles::DOT] style=(dot) measure=(placed) {}
                        span css=[wstyles::CL] { (name) }
                    }
                }
            }
            div css=[wstyles::SERVERS, styles::STACK] {
                @for (name, counts, idle, ink, tenants, placed) in (boxes) {
                    div measure=(placed) {
                        ServerBox name=(name) counts=(counts) idle=(idle) ?ink=(ink)
                            ?tenants=(tenants)
                    }
                }
            }
            svg css=[wstyles::WIRES] {
                @for d in (bare) {
                    path css=[wstyles::WIRE_IDLE] d=($d) {}
                }
                @for (d, ink) in (replies) {
                    path css=[wstyles::RESPONSES] d=($d)
                        pathLength=(WIRE_LEN.to_string()) style=($ink) {}
                }
                @for (d, shed, dashes) in (outbound) {
                    path css=[wstyles::REQUESTS, $shed => wstyles::REQUESTS_SHED]
                        d=($d) pathLength=(WIRE_LEN.to_string()) style=($dashes) {}
                }
            }
        }
    }).await?)
}

#[styles]
pub mod styles {
    use idyll_styles::Style;

    /// [`SERVERS`](crate::atoms::wires::styles::SERVERS) is a column sized to hold a box, which
    /// left where it falls sits against the tenant dots with the rest of the stage empty beside
    /// it. It takes the room instead and centres in it, both ways: the wires then cross the
    /// stage rather than huddling in its left third, and a fleet hangs level with the traffic
    /// reaching it rather than above it.
    pub const STACK: Style = css! {{
        flex_grow: 1,
        align_items: "center",
        justify_content: "center",
    }};
}
