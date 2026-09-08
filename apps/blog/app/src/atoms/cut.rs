//! A percentile reading and the dial that moves it — one column of a comparison.
//!
//! Two cards read a distribution this way: the percentile card cuts one batch at two places,
//! and the policy card cuts three fleets at the same place. What they share is not a layout but
//! a claim — that a reading's *place on the scale* has a colour, and that the dial, its readout
//! and the bars under it all wear it, so a column reads as one subject rather than as a control
//! next to a chart.
//!
//! The colour is published as a custom property by the column and inherited by everything
//! inside it, which is why it cannot drift: there is one declaration, not three arguments that
//! could disagree.

use idyll::{live_view, Callback, Ctx, Never, Setup, Signal};
use idyll_styles::styles;

use crate::atoms::slider::{Name, Scale, Slider};
use crate::atoms::waterfall::{Leg, Waterfall};
use crate::multi::PHASES;

/// Where a reading may stand. The ends are not readings: p0 is the one client who got lucky
/// and p100 the one who did not.
pub const CUT_SCALE: Scale = Scale::new(1, 99, 1);

/// One reading: where the reader has put it, and the trip it names.
pub struct Cut {
    pub at: idyll::MutableSignal<f64>,
    pub legs: idyll::MutableSignal<Vec<Leg>>,
}

/// The four phases with nothing in them yet — what a column reads before anything has come
/// home. The rows are there before the numbers are, so filling them fills a shape the reader
/// has already taken in rather than conjuring one.
pub fn at_rest() -> Vec<Leg> {
    PHASES
        .iter()
        .map(|p| Leg { label: p.label.to_string(), ms: None, give: None, against: None, ink: p.ink })
        .collect()
}

pub fn cut_label(pc: f64) -> String {
    format!("p{}", pc.round() as i64)
}

/// A reading's colour by where it stands: calm to the median, warming through the late
/// percentiles, and the worst of them wearing the colour a refusal wears.
pub fn ramp(pc: f64) -> String {
    const CALM: [f64; 3] = [122.0, 154.0, 94.0];
    const LATE: [f64; 3] = [218.0, 102.0, 38.0];
    const WORST: [f64; 3] = [220.0, 55.0, 66.0];
    let (from, to, t) = match pc {
        pc if pc <= 50.0 => (CALM, CALM, 0.0),
        pc if pc <= 90.0 => (CALM, LATE, (pc - 50.0) / 40.0),
        pc => (LATE, WORST, ((pc - 90.0) / 10.0).min(1.0)),
    };
    let mix = |i: usize| (from[i] + (to[i] - from[i]) * t).round();
    format!("rgb({} {} {})", mix(0), mix(1), mix(2))
}

/// The declaration a column publishes so everything inside it takes the reading's colour.
pub fn ink_of(pc: f64) -> String {
    format!("{}:{}", styles::Cut::ink.name, ramp(pc))
}

/// A dial over a waterfall, both wearing the colour of where the dial stands.
#[idyll::component]
pub async fn CutColumn(
    ctx: Ctx<Setup, Never>,
    at: Signal<f64>,
    legs: Signal<Vec<Leg>>,
    axis: Signal<f64>,
    moved: Callback<f64>,
) -> idyll::Result {
    let ink = {
        let at = at.clone();
        ctx.computed(move |cx| ink_of(at.get(cx))).read()
    };
    Ok(ctx.render(live_view! {
        div css=[styles::COL] style=($ink) {
            Slider name=(Name::new("", 0)) scale=(CUT_SCALE) at=(at) fmt=(cut_label)
                moved=(moved) ?tint=(styles::Cut::ink.value())
                ?readout_ink=(styles::Cut::ink.value())
            Waterfall legs=(legs) axis=(axis)
        }
    }).await?)
}

#[styles]
pub mod styles {
    use idyll_styles::Style;

    vars! {
        /// The colour of the reading this column stands for.
        pub Cut {
            ink: "#7a9a5e",
        }
    }

    pub const CHARTS: Style = css! {{
        display: "flex",
        gap: "28px",
        margin_top: "16px",
        max_width(680px): { flex_direction: "column", gap: "20px" },
    }};

    pub const COL: Style = css! {{
        flex_grow: 1,
        flex_basis: "0",
        min_width: "0",
    }};
}
