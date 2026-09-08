//! Slider — a labeled range control with a live value readout.
//!
//! A **controlled** component: the value belongs to whoever is driving the machine, so
//! it arrives as a signal and every move is reported back through a callback. What the
//! slider owns is its own presentation — the filled length of its rail and the text in
//! its readout — derived from that one value, so the fill and the number can never say
//! different things.

use idyll::{live_view, Callback, Ctx, Event, Never, Setup, Signal};
use idyll_styles::styles;

use crate::atoms::stage::Paint;
use crate::styles::Palette;

/// The scale a slider spans and the resolution it moves in. `step` is the quantity's
/// own: a response deadline moves in hundreds of milliseconds, an IO factor in
/// twentieths.
#[derive(Clone, Copy)]
pub struct Scale {
    pub min: u32,
    pub max: u32,
    pub step: u32,
}

impl Scale {
    pub const fn new(min: u32, max: u32, step: u32) -> Scale {
        Scale { min, max, step }
    }

    /// Where a value stands on this scale, 0..1.
    fn along(&self, v: f64) -> f64 {
        let (lo, span) = (self.min as f64, (self.max as f64 - self.min as f64).max(1.0));
        ((v - lo) / span).clamp(0.0, 1.0)
    }
}

/// How a slider names itself: the word beside it and the width that word is set in, so
/// the rails line up down a row of controls instead of starting wherever each word
/// happens to end.
#[derive(Clone, Copy)]
pub struct Name {
    pub text: &'static str,
    pub width: u32,
}

impl Name {
    pub const fn new(text: &'static str, width: u32) -> Name {
        Name { text, width }
    }
}

/// An arrival rate, as every sim that shows one writes it.
pub fn fmt_qps(v: f64) -> String {
    format!("{v:.0}/s")
}

/// The speed sliders' shared scale: raw 0–100 to a clock dilation, one decade per 25
/// (0→0.0001, 50→0.01, 100→×1) — a reader who has learned one sim's `1×` reads the same
/// pace on every other.
pub fn speed_from_raw(raw: f64) -> f64 {
    10f64.powf((raw - 100.0) / 25.0)
}

/// The inverse — the slider position that shows a given speed, for the initial value.
pub fn raw_from_speed(speed: f64) -> f64 {
    100.0 + 25.0 * speed.log10()
}

pub fn fmt_speed(raw: f64) -> String {
    let s = format!("{:.4}", speed_from_raw(raw));
    format!("{}×", s.trim_end_matches('0').trim_end_matches('.'))
}

/// `at` is the value, `fmt` writes it for the readout, `moved` is where a move goes. To
/// mark this as the control the prose asks the reader to take, wrap it in an
/// [`Invite`](crate::atoms::invite::Invite) — the glow and its tag are the wrapper's, not
/// the slider's.
#[idyll::component]
pub async fn Slider(
    ctx: Ctx<Setup, Never>,
    name: Name,
    scale: Scale,
    at: Signal<f64>,
    fmt: fn(f64) -> String,
    moved: Callback<f64>,
    /// Something other than the reader is turning this dial: the rail and readout
    /// still follow `at`, but the control does not accept input.
    #[opt] driven: Signal<bool>,
    /// The colour the fill and the grab wear. A slider that stands for nothing in particular
    /// keeps the control palette's own; one that drives a single named thing takes that thing's
    /// hue, so the knob and the row it moves read as the same subject. The rail behind it stays
    /// neutral either way — it is the track, not the quantity.
    #[opt] tint: Paint,
    /// The colour the readout wears. Left alone the pill keeps the control palette's, which is
    /// what a number standing for a setting should look like. A dial whose *value* has a meaning
    /// of its own — a place on a scale that is itself coloured — hands that colour in here, so the
    /// number and the length agree about where on the scale they are.
    #[opt] readout_ink: Paint,
) -> idyll::Result {
    let value = at.clone();
    let readout = ctx.computed(move |cx| fmt(value.get(cx))).read();
    let pill = readout_ink
        .map(|ink| format!("color:{ink};background:color-mix(in srgb, {ink} 16%, transparent)"))
        .unwrap_or_default();
    let along = at.clone();
    // Untinted, the fill and the grab keep the two colours the control palette gives them; a tint
    // replaces both, because a knob and the length behind it are one quantity.
    let (fill_col, grab_col) = match tint {
        Some(tint) => (tint, tint),
        None => (Palette::rail_fill.value(), Palette::grab.value()),
    };
    let fill = ctx
        .computed(move |cx| {
            format!("width:{:.2}%;background:{fill_col}", scale.along(along.get(cx)) * 100.0)
        })
        .read();
    // The grab is a pseudo-element, which no inline declaration can reach — but the custom
    // property it reads is one, so the colour rides the input and the rule picks it up.
    let knob = format!("{}:{grab_col}", styles::Knob::grab.name);
    // The grab follows `at` like everything else here. Writing the value only at mount leaves
    // the browser owning the grab, which agrees with the rail exactly as long as the reader is
    // the only one turning the dial — and a preset that puts every knob somewhere is precisely
    // something else turning it, leaving the grab behind at a number the readout has stopped
    // showing. Mid-drag this writes back the value the drag just produced, which moves nothing.
    let shown = at.clone();
    let position = ctx.computed(move |cx| format!("{:.0}", shown.get(cx))).read();
    let Name { text, width } = name;
    let Scale { min, max, step } = scale;
    let driven = driven.unwrap_or_else(|| ctx.constant(false));
    let locked = driven.clone();
    // A field mid-edit is not yet a number, and a move that isn't one is not a move.
    let moved = moved.try_contra_map(|e: Event| e.target_value?.parse().ok());

    Ok(ctx.render(live_view! {
        label css=[styles::RANGE] {
            span css=[styles::NAME] style=(format!("min-width:{width}px")) { (text) }
            span css=[styles::TRACK] {
                span css=[styles::RAIL] {}
                span css=[styles::FILL] style=($fill) {}
                input css=[styles::INPUT, $driven => styles::DRIVEN] type=("range")
                    min=(min.to_string()) max=(max.to_string()) step=(step.to_string())
                    aria_label=(text) aria_valuetext=($readout)
                    value=($position) disabled[$locked] style=(knob.clone()) oninput=(moved) {}
            }
            span css=[styles::VALUE] style=(pill) { $readout }
        }
    }).await?)
}

#[styles]
pub mod styles {
    use idyll_styles::Style;

    use crate::atoms::tokens::{Face, Radius};
    use crate::styles::Palette;

    // The grab's colour, written per instance by the component: a pseudo-element is out of an
    // inline declaration's reach, but the custom property it reads is not. There is no resting
    // value to state — the component always writes it, and a grab that went unwritten should be
    // visibly missing rather than quietly the wrong colour.
    vars! {
        pub Knob {
            grab: "transparent",
        }
    }

    /// A slider takes the room its row has left. Travel is precision: a rail squeezed
    /// to its label's width can't be aimed, on a finger or a mouse.
    ///
    /// The basis below is a **row**'s: `flex-basis` sizes the main axis, so a slider placed in a
    /// `flex-direction: column` reads it as a 220px *height* and stands ten times too tall. Put
    /// a group of these in a wrapping row ([`ROW_BODY`](crate::atoms::controls::styles::ROW_BODY)).
    pub const RANGE: Style = css! {{
        display: "flex",
        align_items: "center",
        gap: "10px",
        flex_grow: 1,
        flex_shrink: 1,
        flex_basis: "220px",
        font_family: Face::sans,
        font_size: "14px",
        color: Palette::control_ink,
    }};

    /// The control's name, set to a fixed width so every rail in a row starts at the
    /// same x — a column of controls whose rails begin wherever their words end reads
    /// as five unrelated widgets.
    pub const NAME: Style = css! {{
        flex_shrink: 0,
    }};

    /// The rail the grab travels, and the two bands drawn in it. The rail and the fill
    /// are drawn here rather than through the vendor pseudo-elements because the grab
    /// stands taller than the rail it rides: any fill clipped to the rail's own box
    /// clips the grab to it too, and a slider whose grab is a 7px sliver is not the
    /// control this draws.
    pub const TRACK: Style = css! {{
        position: "relative",
        display: "flex",
        align_items: "center",
        flex_grow: 1,
        height: "22px",
    }};

    pub const RAIL: Style = css! {{
        width: "100%",
        height: "7px",
        border_radius: Radius::pill,
        background: Palette::rail,
    }};

    /// How far the value has come, as the length of the rail behind the grab.
    pub const FILL: Style = css! {{
        position: "absolute",
        left: "0",
        height: "7px",
        border_radius: Radius::pill,
        pointer_events: "none",
    }};

    /// The custom range control: the browser chrome is turned off (`appearance` twins
    /// the `-webkit-` prefix) and the grab is drawn through the vendor pseudo-elements.
    /// The native input is kept — it owns the keyboard and the assistive behaviour —
    /// and lies transparent over the rail, so what the reader grabs is the real
    /// control and what they see is the drawn one.
    pub const INPUT: Style = css! {{
        appearance: "none",
        position: "absolute",
        left: "0",
        width: "100%",
        height: "22px",
        margin: "0",
        background: "transparent",
        cursor: "pointer",
        pointer_coarse: { height: "40px" },
        slider_track: {
            height: "22px",
            background: "transparent",
        },
        // The grab swells under the finger: a control being dragged should feel held.
        ":active": { cursor: "grabbing" },
        slider_thumb: {
            appearance: "none",
            width: "18px",
            height: "18px",
            background: Knob::grab,
            border_radius: "50%",
            // Longhands, not `border: 3px solid transparent` plus a colour: a shorthand
            // and a longhand for the same property are resolved by declaration order,
            // and atoms are equal-specificity single classes — so the shorthand would
            // win and the ring would be invisible.
            border_width: "3px",
            border_style: "solid",
            border_color: Palette::ground,
            box_shadow: "0 1px 3px #28301a4d",
            transition: "transform 120ms",
        },
        webkit_slider_thumb: {
            // WebKit lays the grab against the top of the track box; Firefox centres it
            // in the track itself. This is the half-difference that centres it here.
            margin_top: "2px",
        },
        active_slider_thumb: { transform: "scale(1.18)" },
    }};

    /// The live value, as a pill beside the rail — tabular so the digits stay put
    /// while it counts.
    pub const VALUE: Style = css! {{
        min_width: "8ch",
        text_align: "right",
        font_size: "13px",
        font_variant_numeric: "tabular-nums",
        color: Palette::readout_ink,
        background: Palette::readout,
        border_radius: Radius::pill,
        padding: "3px 8px",
    }};

    /// A dial something else is turning. The rail and the readout keep their colour —
    /// the number is live and worth watching — but the grab reads as not-yours.
    pub const DRIVEN: Style = css! {{
        cursor: "default",
        slider_thumb: {
            background: Palette::ground,
            border_color: Palette::rail,
        },
        active_slider_thumb: { transform: "scale(1)" },
    }};
}
