//! Strips — stacked time-series rows, one reading each, scrolling as the sim runs.
//!
//! The panel a practitioner already reads: lines moving left while the machine runs, so an
//! edited setting shows up as the lines *changing* rather than as a number the reader has to
//! remember to compare. Two cards draw them — the balancer tier's five percentiles of one
//! distribution, sharing one eased ms axis because they are readings of each other, and the
//! autoscaler's demand, fleet and refusals, each on an axis of its own because they are not.
//!
//! Presentational (`Never`): the caller decides what a sample is, what it is worth against,
//! and what the row says it is. What lives here is the geometry the two share — [`slot_x`]
//! and [`y_of`] for a point, [`note`] and [`mark_path`] for the rules under it, and the
//! cadence and depth ([`SAMPLE_MS`], [`POINTS`]) that make a slot mean the same width in
//! both.

use std::collections::VecDeque;

use idyll::{live_view, Ctx, Never, Setup, Signal};
use idyll_styles::styles;

/// The drawing surface every row's polyline is scaled to. Fixed and private to the pair of
/// scaler and drawer: `preserveAspectRatio="none"` stretches it to the track, and
/// `non-scaling-stroke` keeps the line's weight out of the stretch.
pub const STRIP_W: f64 = 600.0;
pub const STRIP_H: f64 = 26.0;

/// Virtual ms between samples. Four times a second: fast enough that a line moves while the
/// reader is watching it, slow enough that whatever the caller computes per sample is a
/// quarter-second cost rather than a per-frame one.
pub const SAMPLE_MS: f64 = 250.0;

/// How many samples a row keeps — thirty seconds of history at [`SAMPLE_MS`], enough that a
/// setting changed half a minute ago is still on stage with its consequences.
pub const POINTS: usize = 120;

/// One row: the percentile it reads, its scrolling line, and where the line stands now.
#[derive(Clone, PartialEq)]
pub struct StripRow {
    pub label: &'static str,
    /// The polyline's `points`, already scaled to `STRIP_W`×`STRIP_H`.
    pub points: String,
    /// The latest reading, as the row states it ("412 ms" — or "– ms" before there is one).
    pub reading: String,
    /// The line's colour, as the caller's own vocabulary already paints this reading — a
    /// percentile's place on the [`ramp`](crate::atoms::cut::ramp), a refusal's amber.
    pub ink: String,
}

/// A sample slot's x: fixed by index, so a line grows rightward until the window fills and
/// scrolls only then — the same behaviour every chart in the book has.
pub fn slot_x(j: usize) -> f64 {
    j as f64 / (POINTS - 1) as f64 * STRIP_W
}

/// A value's y against the axis it is read on, clamped to the strip with a hair of margin.
pub fn y_of(v: f64, axis: f64) -> f64 {
    STRIP_H - 2.0 - (v / axis.max(1.0)).min(1.0) * (STRIP_H - 4.0)
}

/// Remember that a setting changed in the current sample slot — once, however many times the
/// control fired on the way there.
pub fn note(marked: &mut VecDeque<u64>, sampled: u64) {
    if marked.back() != Some(&sampled) {
        marked.push_back(sampled);
    }
}

/// The rules of every remembered change still on stage, as one path of vertical strokes.
pub fn mark_path(marked: &VecDeque<u64>, sampled: u64, len: usize) -> String {
    let first = sampled - len as u64;
    marked
        .iter()
        .filter_map(|&m| {
            let j = m.checked_sub(first)?;
            (j <= len as u64).then(|| format!("M {:.1} 0 V {STRIP_H}", slot_x(j as usize)))
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[idyll::component]
pub async fn Strips(
    ctx: Ctx<Setup, Never>,
    rows: Signal<Vec<StripRow>>,
    /// The width the row names are set in, so the tracks start at the same place down the
    /// whole chart — three characters for a percentile, two words for a metric.
    labels: u32,
    /// Rules at the instants a setting changed — one `path` of vertical strokes, scaled like
    /// the rows are, drawn under every row so a step in the lines has its cause marked.
    marks: Signal<String>,
) -> idyll::Result {
    let columns = format!("grid-template-columns:{labels}px 1fr 56px");
    Ok(ctx.render(live_view! {
        div css=[styles::STRIPS] {
            @for r in $rows [key = r.label] {
                div css=[styles::SROW] style=(columns.clone()) {
                    span css=[styles::SL] { (label(&$r)) }
                    svg css=[styles::STRACK] viewBox=("0 0 600 26") preserveAspectRatio=("none") {
                        path css=[styles::SMARK] d=($marks) {}
                        polyline css=[styles::SLINE] points=(points_of(&$r)) style=(stroke_of(&$r)) {}
                    }
                    span css=[styles::SV] { (reading_of(&$r)) }
                }
            }
        }
    }).await?)
}

fn label(r: &StripRow) -> &'static str {
    r.label
}

fn points_of(r: &StripRow) -> String {
    r.points.clone()
}

fn stroke_of(r: &StripRow) -> String {
    format!("stroke:{}", r.ink)
}

fn reading_of(r: &StripRow) -> String {
    r.reading.clone()
}

#[styles]
pub mod styles {
    use idyll_styles::Style;

    use crate::atoms::tokens::Face;
    use crate::styles::Palette;

    pub const STRIPS: Style = css! {{ margin_top: "2px" }};

    /// A row: its name, its track, its current reading — the same three columns a waterfall
    /// row keeps, so the two charts read as one family. The name's column is the caller's,
    /// set inline: a percentile needs three characters and a metric needs a word or two.
    pub const SROW: Style = css! {{
        display: "grid",
        grid_template_columns: "36px 1fr 56px",
        align_items: "center",
        gap: "8px",
        height: "30px",
    }};

    pub const SL: Style = css! {{
        font_size: "11.5px",
        color: Palette::control_ink,
    }};

    pub const SV: Style = css! {{
        font_family: Face::mono,
        font_size: "11px",
        text_align: "right",
        color: Palette::control_ink,
        font_variant_numeric: "tabular-nums",
    }};

    /// The track: a bordered lane the line scrolls in, stretched to the row.
    pub const STRACK: Style = css! {{
        display: "block",
        width: "100%",
        height: "26px",
        border_bottom: "1px solid transparent",
        border_color: Palette::line,
    }};

    pub const SLINE: Style = css! {{
        fill: "none",
        stroke_width: "1.5px",
        vector_effect: "non-scaling-stroke",
        opacity: 0.85,
    }};

    /// A changed setting's rule: faint, so the lines' motion stays the subject and the mark
    /// is only its date.
    pub const SMARK: Style = css! {{
        stroke: Palette::line,
        stroke_width: "1px",
        vector_effect: "non-scaling-stroke",
    }};
}
