//! Waterfall — one request's round trip, phase by phase, each drawn where it *happened*.
//!
//! The [`LatencyTable`](crate::atoms::latency_table) draws the same cut as a distribution: four
//! boxplots over a whole batch, each starting at zero. This draws one client's experience, and
//! each phase sits at its own offset — so the row reads as a life rather than as four bars
//! sharing a baseline, and the gaps between them are the waiting.
//!
//! Presentational (`Never`): the caller passes the legs in the order the request met them and
//! the axis to scale against. The atom does the running sum, because a cumulative offset is
//! geometry; it does no percentile maths and knows no model types.

use idyll::{live_view, Ctx, Never, Setup, Signal};
use idyll_styles::styles;

use crate::atoms::stage::{Paint, Stage};

/// One phase of the trip. Milliseconds — the atom scales them against `axis`.
#[derive(Clone, PartialEq)]
pub struct Leg {
    pub label: String,
    /// How long the request spent here, or `None` where there is no request yet. A row with
    /// nothing in it is not a row that took no time, and a still that reads `0 ms` claims a
    /// measurement nobody made.
    pub ms: Option<f64>,
    /// How far either side of the reading the truth could plausibly sit — the whisker's ± in
    /// ms. `None` where the reading is exact (one request's own stamps) or where nothing
    /// measured its noise, and the row draws as it always did.
    pub give: Option<f64>,
    /// How this leg stands against the same leg of whatever it is being read against, where the
    /// waterfall is one of several — `None` where there is nothing beside it, which is most of
    /// them. It rides under the reading rather than beside it: a column of waterfalls is as
    /// narrow as the card gives it, and a row that took the width for a comparison would spend
    /// it out of the bar the comparison is about.
    pub against: Option<Against>,
    /// The colour this phase wears everywhere else. A value rather than a class: the segment is
    /// placed by inline geometry anyway, so the hue rides with it.
    pub ink: Paint,
}

/// What a leg is read against. A number rather than the sentence it renders as: how it reads —
/// and whether it is worth the reader's eye — is the atom's to say.
#[derive(Clone, Copy, PartialEq)]
pub enum Against {
    /// The reading the others are compared with. It has nothing to be nearer to.
    Baseline,
    /// Percent more time spent here than the baseline spends, negative where it is less.
    By(f64),
}

/// A leg, where it starts, and what it is measured against. All three arrive together so a
/// segment can never be placed by an offset or scaled by an axis from a different reading.
#[derive(Clone, PartialEq)]
struct Placed {
    leg: Leg,
    /// Milliseconds before this leg began — the sum of the ones ahead of it.
    at: f64,
    /// The whole trip, for the total row's share arithmetic.
    trip: f64,
    axis: f64,
}

#[idyll::component]
pub async fn Waterfall(
    ctx: Ctx<Setup, Never>,
    legs: Signal<Vec<Leg>>,
    axis: Signal<f64>,
) -> idyll::Result {
    let rows = {
        let (legs, axis) = (legs.clone(), axis.clone());
        ctx.computed(move |cx| {
            let axis = axis.get(cx).max(1.0);
            let legs = legs.get(cx);
            let trip: f64 = legs.iter().filter_map(|l| l.ms).sum();
            let mut at = 0.0;
            legs.into_iter()
                .map(|leg| {
                    let placed = Placed { at, trip, axis, leg };
                    at += placed.leg.ms.unwrap_or(0.0);
                    placed
                })
                .collect::<Vec<_>>()
        })
        .read()
    };
    // The trip's own length, which the total row is: the legs laid end to end rather than the
    // reading's `total_ms`, so the bar under them is exactly the sum of the bars above it.
    let trip = {
        let rows = rows.clone();
        ctx.computed(move |cx| rows.get(cx).first().map(|r| r.trip).unwrap_or(0.0)).read()
    };
    let trip_bar = {
        let rows = rows.clone();
        ctx.computed(move |cx| match rows.get(cx).first() {
            Some(r) => format!("width:{:.2}%", r.trip / r.axis * 100.0),
            None => "width:0".to_string(),
        })
        .read()
    };
    Ok(ctx.render(live_view! {
        div css=[styles::WF] {
            @for r in $rows [key = r.leg.label.clone()] {
                div css=[styles::WROW] {
                    span css=[styles::WL] { (label(&$r)) }
                    div css=[styles::WTRACK] {
                        div css=[styles::WRAIL] {}
                        div css=[styles::WSEG] style=(segment(&$r)) {}
                        div css=[styles::WGIVE] style=(whisker(&$r)) {}
                    }
                    span css=[styles::WV] {
                        span { (reading(&$r)) }
                        span css=[styles::WD] style=(gain(&$r)) { (against(&$r)) }
                    }
                }
            }
            div css=[styles::WROW, styles::TOTAL] {
                span css=[styles::WL] { "total" }
                div css=[styles::WTRACK] {
                    div css=[styles::WRAIL] {}
                    div css=[styles::WSTACK] style=($trip_bar) {
                        @for r in $rows [key = r.leg.label.clone()] {
                            div css=[styles::WPART] style=(share(&$r)) {}
                        }
                    }
                }
                span css=[styles::WV] { (total_reading(&$trip)) }
            }
        }
    }).await?)
}

fn label(r: &Placed) -> String {
    r.leg.label.clone()
}

fn reading(r: &Placed) -> String {
    match r.leg.ms {
        Some(ms) => format!("{} ms", ms.round() as i64),
        None => "– ms".to_string(),
    }
}

/// What the row reads against, under its own reading — nothing at all where there is nothing
/// to compare it with, which draws as the row always did.
fn against(r: &Placed) -> String {
    match r.leg.against {
        Some(Against::Baseline) => "baseline".to_string(),
        Some(Against::By(pc)) => format!("{pc:+.0}%"),
        None => String::new(),
    }
}

/// Time the row *saved*, and only that: a leg shorter than the baseline's takes the colour a
/// served request wears everywhere else, and everything else stays the faint ink the line is
/// set in. The reading the card is for is which choice helps — a longer leg is a number to
/// read, not an alarm to raise, and colouring it would put two claims in one row.
fn gain(r: &Placed) -> String {
    match r.leg.against {
        Some(Against::By(pc)) if pc < 0.0 => format!("color:{}", Stage::green.value()),
        _ => String::new(),
    }
}

fn total_reading(trip: &f64) -> String {
    match *trip > 0.0 {
        true => format!("{} ms", trip.round() as i64),
        false => "– ms".to_string(),
    }
}

/// A phase's segment: where it began and how long it ran, both against the shared axis.
fn segment(r: &Placed) -> String {
    format!(
        "left:{:.2}%;width:{:.2}%;background:{}",
        r.at / r.axis * 100.0,
        r.leg.ms.unwrap_or(0.0) / r.axis * 100.0,
        r.leg.ink,
    )
}

/// The reading's give, centred where the phase ends — the instant its `ms` states. Clamped to
/// the track: a reading still too noisy to mean anything can carry a give wider than the axis,
/// and the whisker's job is to say so, not to leave the picture.
fn whisker(r: &Placed) -> String {
    match (r.leg.give, r.leg.ms) {
        (Some(give), Some(ms)) if give > 0.0 => {
            let lo = (r.at + ms - give).max(0.0);
            let hi = (r.at + ms + give).min(r.axis);
            format!(
                "left:{:.2}%;width:{:.2}%;background:{}",
                lo / r.axis * 100.0,
                (hi - lo).max(0.0) / r.axis * 100.0,
                r.leg.ink,
            )
        }
        _ => "display:none".to_string(),
    }
}

/// A phase's share of the trip, inside the total bar — a proportion of the stack, not of the
/// axis, so the stacked pieces fill the bar they are drawn in exactly.
fn share(r: &Placed) -> String {
    format!(
        "width:{:.2}%;background:{}",
        r.leg.ms.unwrap_or(0.0) / r.trip.max(1e-9) * 100.0,
        r.leg.ink,
    )
}

#[styles]
pub mod styles {
    use idyll_styles::Style;

    use crate::atoms::tokens::Face;
    use crate::styles::Palette;

    pub const WF: Style = css! {{ margin_top: "2px" }};

    /// A phase row: its name, the track it is placed on, and its reading.
    pub const WROW: Style = css! {{
        display: "grid",
        grid_template_columns: "64px 1fr 50px",
        align_items: "center",
        gap: "8px",
        height: "26px",
    }};

    /// The trip laid end to end, under a rule — the sum of the rows above rather than another
    /// one of them.
    pub const TOTAL: Style = css! {{
        height: "31px",
        margin_top: "3px",
        border_top: "1px solid transparent",
        border_color: Palette::line,
    }};

    pub const WL: Style = css! {{
        font_size: "11.5px",
        color: Palette::control_ink,
    }};

    pub const WV: Style = css! {{
        display: "flex",
        flex_direction: "column",
        align_items: "flex-end",
        font_family: Face::mono,
        font_size: "11px",
        line_height: 1.15,
        text_align: "right",
        color: Palette::control_ink,
        font_variant_numeric: "tabular-nums",
    }};

    /// The comparison under a reading. Quieter and smaller than the number it qualifies, and in
    /// the same column — the row is no wider for having one.
    pub const WD: Style = css! {{
        font_size: "9.5px",
        color: Palette::ink_faint,
        white_space: "nowrap",
    }};

    pub const WTRACK: Style = css! {{
        position: "relative",
        height: "13px",
    }};

    /// The axis the phases are placed along, drawn the whole width so a short phase reads as
    /// short rather than as the only thing there.
    pub const WRAIL: Style = css! {{
        position: "absolute",
        top: "5.5px",
        left: "0",
        right: "0",
        height: "2px",
        border_radius: "2px",
        background: Palette::line,
    }};

    /// One phase, washed: what is behind a segment is the axis it is being read against, and a
    /// solid block hides it.
    pub const WSEG: Style = css! {{
        position: "absolute",
        top: "1px",
        height: "11px",
        border_radius: "2px",
        opacity: 0.5,
        transition: "left 420ms cubic-bezier(.4,0,.2,1), width 420ms cubic-bezier(.4,0,.2,1)",
    }};

    /// The give: a hairline over the rail where the reading's end could sit. Solid where the
    /// segment is washed — the mark of how sure the estimate is should not be fainter than
    /// the estimate.
    pub const WGIVE: Style = css! {{
        position: "absolute",
        top: "5px",
        height: "3px",
        border_radius: "2px",
        opacity: 0.85,
        transition: "left 420ms cubic-bezier(.4,0,.2,1), width 420ms cubic-bezier(.4,0,.2,1)",
    }};

    /// The total bar: the same phases again, end to end, so the reader sees the trip as one
    /// length and as its parts at once.
    pub const WSTACK: Style = css! {{
        position: "absolute",
        top: "1px",
        left: "0",
        height: "11px",
        border_radius: "2px",
        overflow: "hidden",
        display: "flex",
        transition: "width 420ms cubic-bezier(.4,0,.2,1)",
    }};

    pub const WPART: Style = css! {{
        height: "100%",
        opacity: 0.42,
        transition: "width 420ms cubic-bezier(.4,0,.2,1)",
    }};
}
