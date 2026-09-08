//! LatencyTable — the experience boxplot: one row per phase, its p25–p95 as a box, its
//! p50 as a line, its share of the total as a percent, and a total row underneath. The
//! multi-server sims read the same partition of the round trip the single-server sim
//! teaches; here it is a distribution across a whole batch rather than one request.
//!
//! Presentational (`Never`): the caller passes already-computed [`Bar`]s and the axis
//! max, so the atom is just geometry — it does no percentile maths and knows no model
//! types, which keeps it reusable by any sim with a phase breakdown to show.

use idyll::{live_view, Ctx, Never, Setup, Signal};
use idyll_styles::styles;

use crate::atoms::stage::Paint;

/// One phase's bar. Positions are in milliseconds; the atom scales them against `axis`.
#[derive(Clone, PartialEq)]
pub struct Bar {
    pub label: String,
    pub p25: f64,
    pub p50: f64,
    pub p95: f64,
    /// Share of the total mean, already rounded to a percent — `None` where the rows do not
    /// partition a total, and a share of one would answer a question that does not apply.
    pub pct: Option<u32>,
    /// The row's colour. A value rather than a class: the bar and its median line are placed
    /// by inline geometry anyway, so the hue rides with them.
    pub hue: Paint,
    /// The total row draws heavier and sits under a divider.
    pub total: bool,
}

/// A row and the axis it is drawn against travel together, so a bar can never be scaled by an
/// axis from a different reading. Both arrive per row, which is what lets one live sim redraw
/// the table each sample without remounting it.
#[derive(Clone, PartialEq)]
struct Scaled {
    bar: Bar,
    axis: f64,
}

#[idyll::component]
pub async fn LatencyTable(
    ctx: Ctx<Setup, Never>,
    bars: Signal<Vec<Bar>>,
    axis: Signal<f64>,
) -> idyll::Result {
    let rows = {
        let (bars, axis) = (bars.clone(), axis.clone());
        ctx.computed(move |cx| {
            let axis = axis.get(cx).max(1.0);
            bars.get(cx).into_iter().map(|bar| Scaled { bar, axis }).collect::<Vec<_>>()
        })
        .read()
    };
    // Whether the shares column applies at all. It is a property of the cut, not of a row — rows
    // that partition a total all carry a share and rows that don't carry none — so the column is
    // there or it isn't, rather than blank on some rows.
    let shares = {
        let bars = bars.clone();
        ctx.computed(move |cx| bars.get(cx).iter().any(|b| b.pct.is_some())).read()
    };
    Ok(ctx.render(live_view! {
        div css=[styles::TT] {
            @for r in $rows [key = r.bar.label.clone()] {
                div css=[styles::TROW, total(&$r) => styles::TOTAL] {
                    div css=[styles::TL] { (label(&$r)) }
                    div css=[styles::TRACK] {
                        div css=[styles::RAIL] {}
                        div css=[styles::BX] style=(box_at(&$r)) {}
                        div css=[styles::WHISK] style=(whisker(&$r, Edge::P25)) {}
                        div css=[styles::WHISK] style=(whisker(&$r, Edge::P95)) {}
                        div css=[styles::MED] style=(median(&$r)) {}
                    }
                    div css=[styles::TP50] { (p50(&$r)) }
                    @if ($shares) {
                        div css=[styles::TPC] { (pct(&$r)) }
                    }
                }
            }
        }
    }).await?)
}

/// Which end of the box a whisker marks.
enum Edge {
    P25,
    P95,
}

fn total(r: &Scaled) -> bool {
    r.bar.total
}

fn label(r: &Scaled) -> String {
    r.bar.label.clone()
}

fn p50(r: &Scaled) -> String {
    format!("{} ms", r.bar.p50.round() as i64)
}

fn pct(r: &Scaled) -> String {
    r.bar.pct.map(|share| format!("{share}%")).unwrap_or_default()
}

/// The row's hue rides the inline style with its geometry: a live table's rows are keyed by
/// label, so the hue travels with the row rather than being a class the diff has to swap.
fn box_at(r: &Scaled) -> String {
    let (start, width) = (r.bar.p25, (r.bar.p95 - r.bar.p25).max(0.0));
    format!(
        "background:{};left:{:.2}%;width:{:.2}%",
        r.bar.hue,
        start / r.axis * 100.0,
        width / r.axis * 100.0,
    )
}

fn whisker(r: &Scaled, edge: Edge) -> String {
    let x = match edge {
        Edge::P25 => r.bar.p25,
        Edge::P95 => r.bar.p95,
    };
    format!("left:{:.2}%", x / r.axis * 100.0)
}

fn median(r: &Scaled) -> String {
    format!("background:{};left:{:.2}%", r.bar.hue, r.bar.p50 / r.axis * 100.0)
}

#[styles]
pub mod styles {
    use idyll_styles::Style;
    use crate::styles::Palette;

    pub const TT: Style = css! {{ margin_top: "16px" }};
    /// A phase row: label · track · p50 · percent. Fixed columns so the tracks align.
    pub const TROW: Style = css! {{
        display: "flex",
        align_items: "center",
        gap: "9px",
        height: "29px",
        child(div): { flex_shrink: 0 },
    }};
    pub const TL: Style = css! {{ font_size: "12px", color: Palette::control_ink, width: "70px" }};
    pub const TRACK: Style = css! {{
        position: "relative",
        height: "16px",
        flex_grow: 1,
    }};
    pub const RAIL: Style = css! {{
        position: "absolute",
        top: "7px",
        left: "0",
        right: "0",
        height: "2px",
        background: "#dee2ce",
        border_radius: "2px",
    }};
    /// The p25–p95 box, washed so the median line reads over it.
    pub const BX: Style = css! {{
        position: "absolute",
        top: "3px",
        height: "10px",
        border_radius: "3px",
        opacity: 0.32,
        transition: "left .4s, width .4s",
    }};
    pub const WHISK: Style = css! {{
        position: "absolute",
        top: "1px",
        height: "14px",
        width: "1.5px",
        background: "#aab09c",
        transition: "left .4s",
    }};
    pub const MED: Style = css! {{
        position: "absolute",
        top: "0",
        height: "16px",
        width: "2.5px",
        border_radius: "2px",
        transition: "left .4s",
    }};
    pub const TP50: Style = css! {{
        font_size: "11px",
        color: Palette::ink_muted,
        text_align: "right",
        width: "44px",
        font_variant_numeric: "tabular-nums",
    }};
    pub const TPC: Style = css! {{
        font_size: "12px",
        font_weight: 600,
        text_align: "right",
        width: "38px",
        color: Palette::control_ink,
        font_variant_numeric: "tabular-nums",
    }};
    pub const TOTAL: Style = css! {{
        height: "34px",
        box_shadow: "0 -1.5px 0 #e0e3d3",
        margin_top: "3px",
    }};

}
