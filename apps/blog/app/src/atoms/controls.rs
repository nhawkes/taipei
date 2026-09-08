//! The control bar layout — a stacked set of labeled rows (the sim's knobs). Just
//! the container geometry; the controls inside are [`super::button`],
//! [`super::slider`], [`super::toggle`].

use idyll_styles::styles;

#[styles]
pub mod styles {
    use idyll_styles::Style;

    use crate::atoms::stage::Stage;
    use crate::atoms::tokens::{Face, Radius};
    use crate::styles::Palette;

    vars! {
        /// The deadline ring's live geometry. Registered, so the browser interpolates
        /// them between the 30 Hz writes on the compositor — which is the whole reason
        /// the ring's sweep is smooth without a per-frame repaint.
        pub Motion {
            a: angle("0deg"),
            b: angle("0deg"),
            c: color("transparent"),
            gap: length("0px"),
            gcol: color("transparent"),
        }
    }

    keyframes! {
        /// The in-flight spinner's rotation.
        pub Spin {
            to { transform: "rotate(360deg)" }
        }

        /// Materialize on mount: `from`-only, so each dot fades to its own inline
        /// opacity. Stays first in every animation list so it is matched by name
        /// across writes and never replays.
        pub In {
            from { opacity: 0 }
        }

        /// The walk: every write restarts a 0→100% sweep of `offset-distance` along
        /// that frame's path. Two names alternate so consecutive writes re-trigger.
        pub WalkA {
            from { offset_distance: "0%" }
        }
        pub WalkB {
            from { offset_distance: "0%" }
        }

        /// The composition listing arriving after a policy change: the listing and the
        /// machine change together, and a cut would read as two unrelated events. Two
        /// names alternate so consecutive changes re-trigger.
        pub FadeA {
            from { opacity: 0 }
        }
        pub FadeB {
            from { opacity: 0 }
        }

        /// At rest, a sheen sweeps the field: a brief brightness bump on each dot, clipped to
        /// the element, its phase offset by the dot's position (an inline negative delay) so
        /// the highlight travels across — the whole still glimmers, so a paused stage looks
        /// eager to move. Plays only while paused (its play-state is toggled against the walk),
        /// pure CSS, so no work runs while the stage is held. Flat at rest, spikes at 9%.
        pub DotSheen {
            "0%" { filter: "brightness(1)" }
            "9%" { filter: "brightness(1.85) saturate(1.05)" }
            "18%" { filter: "brightness(1)" }
            "100%" { filter: "brightness(1)" }
        }

        /// A core whose burst turned over within the frame blinks its ring.
        pub TurnA {
            "40%" { opacity: 0.55 }
        }
        pub TurnB {
            "40%" { opacity: 0.55 }
        }

        /// CPU occupancy dipping and refilling within a frame dims the readout.
        pub CpuDipA {
            "50%" { opacity: 0.45 }
        }
        pub CpuDipB {
            "50%" { opacity: 0.45 }
        }
    }

    /// The sim's outer block: the page's measure, exactly — a stage is read in the same
    /// column as the prose around it, and the prose resumes a paragraph's space below
    /// it. This is also the box `ctx.resizes` measures.
    pub const QV: Style = css! {{
        font_family: Face::sans,
        font_size: "13px",
        color: Palette::ink_muted,
        width: "100%",
        margin: "0 0 28px",
    }};

    /// The stage: every moving part is a child positioned in stage coordinates, which
    /// is why the children are styled from here and carry no class of their own.
    ///
    /// It is a card the page reads as one object, and it **reflows** — the diagram is
    /// laid out to the width the stage was actually given
    /// ([`crate::simview::Layout`]), so nothing scales and no label ever shrinks.
    pub const STAGE: Style = css! {{
        position: "relative",
        width: "100%",
        background: Stage::ground,
        border_width: "1px",
        border_style: "solid",
        border_color: Palette::line,
        border_radius: Radius::card,
        box_shadow: "0 6px 22px #3c462812",
        overflow: "hidden",
        user_select: "none",

        child(div): {
            position: "absolute",
            box_sizing: "border-box",
        },
        child(svg): {
            position: "absolute",
            pointer_events: "none",
        },
    }};

    /// The machine — the CPU box, its cores, the chamber. One positioned box the
    /// moving parts are placed inside, so their coordinates are the machine's rather
    /// than the stage's and the whole thing slides as the plumbing compresses.
    pub const MACHINE: Style = css! {{
        top: "0",
        child(div): {
            position: "absolute",
            box_sizing: "border-box",
        },
        child(svg): {
            position: "absolute",
            pointer_events: "none",
            overflow: "visible",
        },
    }};

    /// The policy pill and the composition it names, above the machine they describe.
    pub const POLICY: Style = css! {{
        margin: "0 0 18px",
    }};

    /// The composition the chosen policy produces. It cross-fades rather than
    /// hard-swapping: the listing and the machine change together, and a cut would
    /// read as two unrelated events.
    pub const POLICY_CODE: Style = css! {{
        margin: "20px 0 2px",
        padding: "20px 22px",
        background: Palette::panel,
        color: Palette::panel_ink,
        border_radius: Radius::panel,
        overflow_x: "auto",
        font_family: Face::mono,
        font_size: "12.5px",
        line_height: 1.85,
    }};

    pub const CONTROLS: Style = css! {{
        display: "flex",
        flex_direction: "column",
        gap: "20px",
        margin: "26px 2px 0",
        user_select: "none",
    }};

    pub const ROW: Style = css! {{
        display: "flex",
        align_items: "center",
        gap: "16px",
        flex_wrap: "wrap",
        pointer_coarse: { row_gap: "10px" },
    }};

    /// The row's controls, as one group beside the gutter label. Without it the label
    /// is just another flex item and the row wraps between the controls rather than
    /// after the name of what they control.
    pub const ROW_BODY: Style = css! {{
        display: "flex",
        align_items: "center",
        gap: "16px",
        flex_wrap: "wrap",
        flex_grow: 1,
        flex_basis: "0",
    }};

    /// The row's leading group label (`simulation`, `client`, `server`) — a gutter
    /// the controls hang off, set small and tracked so it reads as the row's name
    /// rather than as another control.
    pub const GRP: Style = css! {{
        width: "80px",
        font_family: Face::sans,
        font_size: "12px",
        font_weight: 600,
        letter_spacing: "0.06em",
        text_transform: "uppercase",
        color: Palette::gutter,
        // Too narrow for a gutter: the label takes its own line and the controls
        // wrap underneath it.
        max_width(680px): { width: "100%" },
    }};

    /// The manual sim's request/response line — the `GET /ping` button, an arrow,
    /// the in-flight spinner, and the pong reply, above the stage they flow through.
    pub const PING: Style = css! {{
        display: "flex",
        align_items: "center",
        gap: "12px",
        margin: "0 0 10px",
        font_family: "ui-monospace, Menlo, monospace",
        font_size: "13px",
        min_height: "34px",
    }};

    pub const PING_ARROW: Style = css! {{
        color: "#445566",
    }};

    pub const PING_REPLY: Style = css! {{
        font_weight: 600,
    }};

    /// The chart polylines (SVG). The base sets the stroke geometry; a variant paints
    /// each series. The chart stretches to whatever width its column has, so the stroke
    /// is held out of that stretch — a trace drawn 2 px thick uphill and 5 px thick
    /// along the flat is not one line.
    pub const LINE: Style = css! {{
        fill: "none",
        stroke_width: "2px",
        stroke_linejoin: "round",
        vector_effect: "non-scaling-stroke",
    }};
    /// The dot: position is `offset-path` (set inline — the exact polyline it walked
    /// this frame), so only the geometry that never changes lives here. Offset
    /// anchoring centres the box on the path point.
    ///
    /// The halo is what keeps a dot legible against the channel it rides: without it
    /// a queue of them reads as one smear at the width they stack to.
    pub const DOT: Style = css! {{
        width: "11px",
        height: "11px",
        border_radius: "50%",
        top: "0",
        left: "0",
        offset_rotate: "0deg",
        box_shadow: "0 0 0 2px #ffffffd9",
        will_change: "offset-distance, transform",
    }};

    /// The deadline ring: a conic sweep from `-b` to `-a`, feathered ~1° at each edge
    /// so the gradient boundary is never aliased, and masked to an annulus. The
    /// gradient reads the registered vars, so the compositor interpolates the sweep.
    pub const RING: Style = css! {{
        top: "0",
        left: "0",
        width: "20px",
        height: "20px",
        border_radius: "50%",
        offset_rotate: "0deg",
        will_change: "offset-distance",
        background: conic_gradient(
            from: calc(0deg - Motion::b),
            stops: [
                ("transparent", 0deg),
                (Motion::c, 5deg),
                (Motion::c, calc(Motion::b - Motion::a - 5deg)),
                ("transparent", calc(Motion::b - Motion::a)),
            ],
        ),
        // px feathers: %-based stops on an 8px radius are subpixel, i.e. hard edges.
        // The annulus is the 2px stroke at r=8 the rest of the stage's rings wear.
        mask: radial_gradient("closest-side", stops: [
            ("transparent", 6.4px),
            ("#000000", 7.2px),
            ("#000000", 8.8px),
            ("transparent", 9.6px),
        ]),
    }};

    /// The core's slot ring — the same sweep at core scale.
    /// A core's burst ring: the box its arc is drawn in. The arc itself is a path, so
    /// the sweep is the same round-capped stroke as every other ring on the stage.
    pub const CORE_RING: Style = css! {{
        width: "38px",
        height: "38px",
    }};

    /// The in-flight spinner beside the ping reply.
    pub const SPINNER: Style = css! {{
        display: "inline-block",
        width: "13px",
        height: "13px",
        border_width: "2px",
        border_style: "solid",
        border_color: Palette::line,
        border_top_color: Palette::action,
        border_radius: "50%",
        animation: Spin "0.7s linear infinite",
    }};

    /// A stage label — the labels are the picture, so they never wrap or catch clicks.
    pub const LABEL: Style = css! {{
        white_space: "nowrap",
        line_height: 1.2,
        pointer_events: "none",
    }};

    /// The surface a reading wears once it has been pulled in off its own pipe and sits over
    /// the machine. The stage's own ground, so against the stage it is nothing and against
    /// the machine it is the reading's own chip — a background exactly where it is covering
    /// something and nowhere else.
    pub const STAT_SCRIM: Style = css! {{
        padding: "1px 5px",
        border_radius: "4px",
        background: Stage::ground,
    }};

    /// An exit's reading, once its pipe's plumbing is too narrow to hold it on one
    /// line: the middle dot goes, and what followed it takes the line below. Two
    /// deliberate lines rather than a wrap, so the figure that matters stays on top.
    pub const STAT_TAIL: Style = css! {{
        display: "block",
    }};

    pub const HIDDEN: Style = css! {{
        display: "none",
    }};

    /// A plotted figure: the number the chart above is drawing, underlined in its own
    /// series colour. Thicker and further off the baseline than a default underline,
    /// which at 11px reads as a smudge under the digits.
    pub const PLOTTED: Style = css! {{
        text_decoration: "underline",
        text_decoration_thickness: "1.5px",
        text_underline_offset: "2px",
    }};

    /// The sparkline column. Only the column is placed; everything in it flows, so a
    /// chart and its key stay together whichever side of the machine the column is on.
    pub const CHARTS: Style = css! {{
        child(div): { position: "static" },
        child(svg): { position: "static" },
    }};

    /// What a chart's series are counted in, at the end of its key — the one thing a
    /// key of names does not already say, so it rides the key rather than a title.
    pub const UNITS: Style = css! {{
        font_style: "italic",
    }};

    /// The sparkline itself: drawn at its viewBox's aspect, stretched to the column.
    pub const CHART: Style = css! {{
        display: "block",
        width: "100%",
    }};

    /// One chart's key, directly beneath it — which is what makes it that chart's key
    /// rather than the picture's.
    pub const LEGROW: Style = css! {{
        display: "flex",
        gap: "16px",
        margin: "4px 0 8px",
        font_size: "11px",
        color: Palette::ink_muted,
        child(span): {
            display: "inline-flex",
            align_items: "center",
        },
    }};

    /// A series' key: the line's own colour as a short bar, which is what the reader
    /// is matching against — a dot would be the wrong shape for a line.
    pub const CHIP: Style = css! {{
        display: "inline-block",
        width: "18px",
        height: "3px",
        border_radius: "2px",
        margin_right: "6px",
    }};

    /// The key to the dot colours — a caption under the picture, beside the controls
    /// it isn't one of. Keeping it out of the card leaves the stage's whole band to
    /// the machine, which is what the picture is for.
    pub const LEGEND: Style = css! {{
        display: "flex",
        flex_wrap: "wrap",
        align_items: "center",
        gap: "22px",
        row_gap: "6px",
        margin: "12px 2px 0",
        font_size: "11px",
        line_height: 1.3,
        color: Palette::ink_muted,
    }};

    /// A stage that has not been released yet. The still is a picture worth reading —
    /// a machine already full of requests, frozen — so nothing is laid over it; the
    /// only affordance it needs is the cursor saying the whole card is the button.
    pub const ARMED: Style = css! {{
        cursor: "pointer",
    }};

    /// The stage when it is drawn *inside* another card. Elevation says "this is a thing in
    /// its own right", which is true of a sim that is the page's own object and false of a
    /// picture that is one section of a panel — so the nested one keeps its shape and its
    /// clipping and gives the surface back to the card around it. Listed after
    /// [`STAGE`](STAGE), whose declarations it overrides.
    pub const STAGE_NESTED: Style = css! {{
        background: "transparent",
        border_color: "transparent",
        box_shadow: "none",
    }};

    pub const LINE_OFFERED: Style = css! {{ stroke: Stage::orange }};
    pub const LINE_GOODPUT: Style = css! {{ stroke: Stage::green }};
    pub const LINE_INFLIGHT: Style = css! {{ stroke: Stage::blue }};
    pub const LINE_QUEUE: Style = css! {{ stroke: Stage::amber }};
}
