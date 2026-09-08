//! The clients-to-servers stage: a column of client dots, a column of server boxes, and the wire
//! overlay between them.
//!
//! The overlay is aligned to the HTML boxes by runtime measurement (`measure=>`): the stage,
//! every client dot and every server box report their laid-out rects, and each wire is a pure
//! function of three of them — so the wires re-bow whenever the layout reflows, and nothing has
//! to be told a box moved.
//!
//! Every card that wires a crowd to machines draws from here. What they share is the geometry
//! and the surfaces; what each keeps is what its own wires *mean* — a sticky assignment, a
//! tenant's connections, warm pools, a balancer tier.
//!
//! The crowd cards draw theirs as [`Shape`]s on the canvas their traffic rides ([`standing`]),
//! so a wire and the pulses on it come off the one [`Bow`] into the one display list. The
//! panels that mark positions with dashes instead ([`marks`]) are still SVG paths, drawn from
//! the same bows through [`wires_d`].

use idyll::{Curve, Rect, Shape};

use crate::atoms::stage::{Paint, Stage};

/// The length every wire is declared to be, so a position along one is a fraction of this
/// whatever length the layout actually bowed it into. Set as the path's `pathLength`, which is
/// what `stroke-dasharray` is then measured in.
pub const WIRE_LEN: f64 = 1000.0;

/// How much of a wire one request covers.
const MARK: f64 = 20.0;

/// A wire's dash pattern: one dash per request on it, centred on the fraction of the way along
/// it that request has actually travelled.
///
/// The pattern is written to sum to the whole wire, so it neither repeats nor leaves a dash
/// anywhere a request did not put one — an empty wire draws nothing at all. Two requests close
/// enough for their dashes to overlap merge into one longer dash rather than being nudged apart:
/// a crowded wire thickens where the crowd is.
pub fn marks(mut at: Vec<f64>) -> String {
    at.sort_by(f64::total_cmp);
    let mut spans: Vec<(f64, f64)> = Vec::with_capacity(at.len());
    for p in at {
        let a = (p.clamp(0.0, 1.0) * WIRE_LEN - MARK / 2.0).clamp(0.0, WIRE_LEN - MARK);
        match spans.last_mut() {
            Some(last) if a <= last.1 => last.1 = last.1.max(a + MARK),
            _ => spans.push((a, a + MARK)),
        }
    }
    // A dash pattern begins with a dash, so a wire whose first request is not at its very start
    // opens with an empty one.
    let mut d = String::from("0");
    let mut end = 0.0;
    for (a, b) in spans {
        d.push_str(&format!(" {:.1} {:.1}", a - end, b - a));
        end = b;
    }
    d.push_str(&format!(" {:.1}", WIRE_LEN - end));
    d
}

/// Record a measured rect at `i`, ignoring an out-of-range index (a measurement for an element
/// the current batch does not have).
pub fn set_rect(slots: &mut [Option<Rect>], i: usize, rect: Rect) {
    if let Some(slot) = slots.get_mut(i) {
        *slot = Some(rect);
    }
}

/// One bow as its four cubic points, in stage-local pixels — the single geometry both the
/// `d` strings and the display list are drawn from, so a pulse and the wire under it can
/// never disagree about where the wire runs.
#[derive(Clone, Copy, PartialEq)]
pub struct Bow {
    pub from: (f64, f64),
    pub c1: (f64, f64),
    pub c2: (f64, f64),
    pub to: (f64, f64),
}

impl From<Bow> for Curve {
    fn from(bow: Bow) -> Curve {
        Curve { from: bow.from, c1: bow.c1, c2: bow.c2, to: bow.to }
    }
}

impl Bow {
    /// The bow as SVG path data.
    pub fn d(&self) -> String {
        format!(
            "M{:.1} {:.1}C{:.1} {:.1} {:.1} {:.1} {:.1} {:.1}",
            self.from.0, self.from.1, self.c1.0, self.c1.1, self.c2.0, self.c2.1, self.to.0,
            self.to.1,
        )
    }

    /// How long the bow runs, in px: the mean of its chord and its control polygon, which for
    /// a bow this shallow is within a percent of the arc — and it is only ever asked how many
    /// wavelengths of carrier wave fit on it.
    pub fn length(&self) -> f64 {
        let span = |a: (f64, f64), b: (f64, f64)| (b.0 - a.0).hypot(b.1 - a.1);
        (span(self.from, self.to)
            + span(self.from, self.c1)
            + span(self.c1, self.c2)
            + span(self.c2, self.to))
            / 2.0
    }

    /// The point `t` of the way along the cubic.
    pub fn at(&self, t: f64) -> (f64, f64) {
        let u = 1.0 - t;
        let blend = |a: f64, b: f64, c: f64, d: f64| {
            u * u * u * a + 3.0 * u * u * t * b + 3.0 * u * t * t * c + t * t * t * d
        };
        (
            blend(self.from.0, self.c1.0, self.c2.0, self.to.0),
            blend(self.from.1, self.c1.1, self.c2.1, self.to.1),
        )
    }
}

/// The bow from a client dot's centre to the left edge of a server box, control points on the
/// horizontal midline — flat end tangents, so wires that share an endpoint bow apart instead
/// of overlapping as straight chords. Stage-local, because the measured rects are
/// root-relative.
pub fn bow_across(stage: &Rect, client: &Rect, server: &Rect) -> Bow {
    let from = (client.x - stage.x + client.width / 2.0, client.y - stage.y + client.height / 2.0);
    let to = (server.x - stage.x, server.y - stage.y + server.height / 2.0);
    let mx = (from.0 + to.0) / 2.0;
    Bow { from, c1: (mx, from.1), c2: (mx, to.1), to }
}

/// [`bow_across`] turned on its side, for a crowd standing *above* its machines: bottom centre
/// of the upper rect to top centre of the lower one, control points on the vertical midline.
pub fn bow_down(stage: &Rect, above: &Rect, below: &Rect) -> Bow {
    let from = (above.x - stage.x + above.width / 2.0, above.y - stage.y + above.height);
    let to = (below.x - stage.x + below.width / 2.0, below.y - stage.y);
    let my = (from.1 + to.1) / 2.0;
    Bow { from, c1: (from.0, my), c2: (to.0, my), to }
}

/// One wire's `path` data — see [`bow_across`].
pub fn wire_d(stage: &Rect, client: &Rect, server: &Rect) -> String {
    bow_across(stage, client, server).d()
}

/// How a standing wire is drawn — thin and faint, always, because a wire is drawn because the
/// connection exists and what rides it is what the reader is meant to see.
#[derive(Clone, Copy)]
pub struct Line {
    pub ink: Paint,
    pub width: f64,
    pub alpha: f64,
}

/// A wire with nothing on it: neutral, and never given traffic of its own — the connection is
/// drawn because it exists, and the absence of anything travelling it is what says nothing is
/// being sent.
pub const IDLE: Line = Line { ink: Stage::wall.value(), width: 1.0, alpha: 0.45 };
/// A wire whose requests are getting through, and one whose server sheds them — a shade
/// heavier, because the pileups are what the fan-out card is about.
pub const REQUEST: Line = Line { ink: Stage::teal.value(), width: 1.3, alpha: 0.4 };
pub const SHED: Line = Line { ink: Stage::amber.value(), width: 1.5, alpha: 0.6 };
/// The way home, under both of them: thin and faint, so it reads as a return current rather
/// than competing with the outbound colour.
pub const RESPONSE: Line = Line { ink: Stage::green.value(), width: 1.0, alpha: 0.3 };

/// One whole wire, end to end at one opacity — a bow as the display list draws it.
pub fn standing(bow: Bow, line: Line) -> Shape {
    Shape {
        curve: bow.into(),
        span: (0.0, 1.0),
        ink: line.ink.to_string(),
        width: line.width,
        alpha: (line.alpha, line.alpha),
    }
}

/// One layer of wires as shapes — [`wires_d`]'s picture on a canvas, with the same patience:
/// nothing is drawn until the stage and both of a wire's own endpoints have been measured.
pub fn wires(
    stage: &Option<Rect>,
    clients: &[Option<Rect>],
    servers: &[Option<Rect>],
    pairs: impl IntoIterator<Item = (usize, usize)>,
    line: Line,
    out: &mut Vec<Shape>,
) {
    let Some(stage) = stage else { return };
    for (client, server) in pairs {
        let (Some(Some(client)), Some(Some(server))) = (clients.get(client), servers.get(server))
        else {
            continue;
        };
        out.push(standing(bow_across(stage, client, server), line));
    }
}

/// [`wires`] on its side, for a crowd standing above the machines its `pairs` reach.
pub fn wires_down(
    stage: &Option<Rect>,
    aboves: &[Option<Rect>],
    belows: &[Option<Rect>],
    pairs: impl IntoIterator<Item = (usize, usize)>,
    line: Line,
    out: &mut Vec<Shape>,
) {
    let Some(stage) = stage else { return };
    for (above, below) in pairs {
        let (Some(Some(above)), Some(Some(below))) = (aboves.get(above), belows.get(below)) else {
            continue;
        };
        out.push(standing(bow_down(stage, above, below), line));
    }
}

/// One layer's `path` data: every wire in the layer, concatenated.
///
/// A layer is a set of client-to-server `pairs`, and which pairs belong together is entirely the
/// caller's: the fan-out sorts its wires by whether the server a client stuck to shed, the blame
/// panel by what its tenant's traffic is doing, and a tenant talking to a whole fleet is several
/// pairs sharing a client. What travels with the geometry is only this — how a set of endpoints
/// becomes one `d`.
///
/// Empty until the stage has been measured, and each wire waits on both of its own endpoints, so a
/// layer draws exactly the wires it can place.
pub fn wires_d(
    stage: &Option<Rect>,
    clients: &[Option<Rect>],
    servers: &[Option<Rect>],
    pairs: impl IntoIterator<Item = (usize, usize)>,
) -> String {
    let Some(stage) = stage else { return String::new() };
    let mut d = String::new();
    for (client, server) in pairs {
        let (Some(Some(client)), Some(Some(server))) = (clients.get(client), servers.get(server))
        else {
            continue;
        };
        d.push_str(&wire_d(stage, client, server));
    }
    d
}

#[idyll_styles::styles]
pub mod styles {
    use idyll_styles::Style;

    use crate::atoms::stage::Stage;

    /// Pinned as a stacking context (like [`TIER`]) so an [`UNDER`] overlay sits behind the
    /// columns without leaving the stage.
    pub const STAGE: Style = css! {{
        position: "relative",
        display: "flex",
        gap: "16px",
        align_items: "flex-start",
        z_index: 0,
        mobile: { gap: "10px" },
    }};
    /// A stage whose crowd stands on top: the wires run in an [`UNDER`] overlay, and the
    /// `z-index` here pins a stacking context so that overlay's `-1` sits behind the boxes
    /// without ever leaving the stage.
    pub const TIER: Style = css! {{
        position: "relative",
        z_index: 0,
    }};
    pub const UNDER: Style = css! {{
        z_index: -1,
    }};
    /// The wire overlay: covers the stage, draws over the columns, and never eats a click.
    pub const WIRES: Style = css! {{
        position: "absolute",
        inset: "0",
        width: "100%",
        height: "100%",
        overflow: "visible",
        pointer_events: "none",
    }};
    /// A request whose server shed it, on a panel that marks its traffic with dashes — a shade
    /// heavier than an idle wire, so the pileups stand out. ([`super::SHED`] is the same wire on
    /// a canvas.)
    pub const WIRE_SHED: Style = css! {{
        fill: "none",
        stroke: Stage::amber,
        stroke_width: "1.5px",
        opacity: 0.6,
    }};
    /// A wire with nothing on it. Neutral and faint, and — because it is its own layer — never
    /// given a dash pattern: the connection is drawn because it exists, and the absence of any
    /// travel along it is what says nothing is being sent. ([`super::IDLE`] on a canvas.)
    pub const WIRE_IDLE: Style = css! {{
        fill: "none",
        stroke: Stage::wall,
        stroke_width: "1px",
        opacity: 0.45,
    }};
    /// The requests riding a wire: one dash each, at the point along it the engine says that
    /// request has reached. Heavier than the wire beneath them, because these are the traffic and
    /// the wire is only where it runs. Teal while the tenant's requests are getting through, amber
    /// once some of them come back shed.
    pub const REQUESTS: Style = css! {{
        fill: "none",
        stroke: Stage::teal,
        stroke_width: "2.6px",
        opacity: 0.9,
    }};
    pub const REQUESTS_SHED: Style = css! {{
        fill: "none",
        stroke: Stage::amber,
        stroke_width: "2.6px",
        opacity: 0.9,
    }};
    /// The answers riding home: the served colour, as heavy as the requests they answer.
    pub const ANSWERS: Style = css! {{
        fill: "none",
        stroke: Stage::green,
        stroke_width: "2.6px",
        opacity: 0.9,
    }};
    /// The verdicts riding home. Thinner than the requests they answer, and given no colour here:
    /// a reply wears its outcome's, so a dash on the wire and a dot leaving down an exit pipe
    /// cannot disagree about what happened.
    pub const RESPONSES: Style = css! {{
        fill: "none",
        stroke_width: "2px",
        opacity: 0.85,
    }};
    pub const CLIENTS: Style = css! {{
        width: "52px",
        display: "flex",
        flex_direction: "column",
        justify_content: "space-around",
        flex_shrink: 0,
    }};
    pub const CNODE: Style = css! {{
        display: "flex",
        align_items: "center",
        gap: "4px",
    }};
    pub const DOT: Style = css! {{
        width: "11px",
        height: "11px",
        border_radius: "50%",
        background: Stage::channel,
        border_width: "2px",
        border_style: "solid",
        flex_shrink: 0,
        transition: "background-color 300ms ease",
    }};
    pub const CL: Style = css! {{
        font_family: "'IBM Plex Mono', monospace",
        font_size: "8px",
        color: "#9aa08d",
    }};
    /// A column wide enough for a server box. Past the width that holds one beside the traffic
    /// reaching it, the column gives way instead — a box narrower than its design width still
    /// reads, and a stage the box hangs out of does not.
    pub const SERVERS: Style = css! {{
        flex_grow: 0,
        flex_shrink: 0,
        flex_basis: "320px",
        display: "flex",
        flex_direction: "column",
        gap: "7px",
        max_width(680px): { flex_shrink: 1, min_width: "0" },
        // A phone has no room for a fixed column beside a field of clients: shrunk in
        // proportion to its basis, a 320px column leaves the field a single ring wide. A share
        // of the stage instead, so what the clients get is a share too.
        mobile: { flex_basis: "56%" },
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A dash pattern spans the wire exactly once, so it never repeats and never leaves a mark
    /// where no request is — and a wire with nothing on it draws nothing at all.
    #[test]
    fn marks_cover_the_wire_once() {
        for at in [
            vec![],
            vec![0.0],
            vec![1.0],
            vec![0.5],
            vec![0.2, 0.9],
            vec![0.5, 0.505],
            vec![0.9, 0.1],
        ] {
            let requests = at.len();
            let d = marks(at);
            let run: Vec<f64> =
                d.split(' ').map(|v| v.parse().expect("a pattern is lengths")).collect();
            assert!(run.len().is_multiple_of(2), "whole dash/gap pairs: {d}");
            let span: f64 = run.iter().sum();
            assert!((span - WIRE_LEN).abs() < 0.05, "{d} spans the wire once");
            let ink: f64 = run.iter().step_by(2).sum();
            assert_eq!(ink > 0.0, requests > 0, "ink iff a request put it there: {d}");
            assert!(ink <= requests as f64 * MARK + 0.05, "none draws past its own mark: {d}");
        }
    }
}
