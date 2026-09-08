//! ToggleButtonGroup — a single-select pill (the sim's server-behavior and
//! admission-signal picks). The selected item is shown by a **knob**: one raised
//! surface that slides between the choices, so the change of selection is a movement
//! the eye follows rather than two simultaneous recolourings.

use std::rc::Rc;

use idyll::{live_view, Callback, Ctx, Event, Never, Setup, Signal};
use idyll_styles::{styles, Style};

use super::tokens::FOCUS;

/// What a choice *means*, where that is not neutral. A group comparing a good server
/// against two broken ones should say so in the bar, not only in the dots the broken
/// ones go on to produce.
#[derive(Clone, Copy, PartialEq)]
pub enum Tone {
    Normal,
    /// This choice is a failure mode — it carries the same red the outcome will.
    Alarm,
    /// This choice turns requests away.
    Shed,
    /// This choice holds them until it can serve them.
    Serve,
}

impl Tone {
    /// The surface the knob wears while this choice is the selected one.
    fn wash(self) -> &'static str {
        match self {
            Tone::Shed => crate::styles::Palette::wash_shed.name,
            Tone::Serve => crate::styles::Palette::wash_serve.name,
            _ => crate::styles::Palette::wash.name,
        }
    }

    /// The style the selected label wears, so the word and the surface under it agree.
    fn selected(self) -> Style {
        match self {
            Tone::Shed => styles::ON_SHED,
            Tone::Serve => styles::ON_SERVE,
            Tone::Alarm => styles::ALARM,
            Tone::Normal => styles::ON,
        }
    }
}

/// One choice: its pick index, label, selected signal, and tone (the group is
/// single-select, but selection state lives with the caller's model — switching
/// retunes a running machine, so the signal is the truth, not internal state).
///
/// The label is shared rather than borrowed: a policy's name comes off the page's
/// own data, so it outlives nothing in particular.
pub type ToggleItem = (usize, Rc<str>, Signal<bool>, Tone);

/// The knob's travel, as the style the group's knob wears. Built from the same items
/// the group renders, by the caller's `Ctx` — the geometry is the atom's, the owner
/// the computed roots in is the component's.
///
/// **A group may have nothing selected.** A set of presets is the case: any move away from
/// one leaves the reader between them, and that is a state to show rather than a state to
/// round to the nearest choice. The knob fades out where it last stood, so picking a preset
/// again slides from somewhere the eye can follow rather than reappearing from item zero.
pub fn knob<M: 'static>(ctx: &Ctx<Setup, M>, items: &[ToggleItem]) -> Signal<String> {
    let count = items.len().max(1);
    let picked: Vec<Signal<bool>> = items.iter().map(|(_, _, on, _)| on.clone()).collect();
    let washes: Vec<&'static str> = items.iter().map(|(_, _, _, tone)| tone.wash()).collect();
    // Where the knob last stood, so a deselected group fades out in place rather than
    // snapping home. A computed reruns on every read of its dependencies, so this is written
    // through a cell rather than captured by value.
    let stood_at = std::cell::Cell::new(0usize);
    ctx.computed(move |cx| {
        let selected = picked.iter().position(|on| on.get(cx));
        let at = selected.unwrap_or_else(|| stood_at.get());
        stood_at.set(at);
        // Which place the knob is in and how many places there are — the whole of what a
        // render knows. The travel those two produce is [`styles::KNOB`]'s, because which
        // axis it is along is a question about the width of the page, and a render cannot
        // see one.
        //
        // The colour rides here too: the knob takes on the chosen policy's own wash, so the
        // switch is one movement rather than a movement plus an unrelated recolouring.
        format!(
            "{}:{at};{}:{count};background-color:var({});opacity:{}",
            styles::Knob::at.name,
            styles::Knob::n.name,
            washes.get(at).copied().unwrap_or(crate::styles::Palette::wash.name),
            selected.is_some() as u8,
        )
    })
    .read()
}

#[idyll::component]
pub async fn ToggleGroup(
    ctx: Ctx<Setup, Never>,
    items: Vec<ToggleItem>,
    knob: Signal<String>,
    picked: Callback<usize>,
) -> idyll::Result {
    let rows: Vec<_> = items
        .into_iter()
        .map(|(index, label, on, tone)| {
            (label, on, tone.selected(), picked.contra_map(move |_: Event| index))
        })
        .collect();
    Ok(ctx.render(live_view! {
        span css=[styles::GROUP] {
            span css=[styles::KNOB] style=($knob) {}
            @for (label, on, selected, choose) in (rows) {
                button css=[styles::ITEM, FOCUS, $on => selected] aria_pressed=($on) onclick=(choose) { (label) }
            }
        }
    }).await?)
}

#[styles]
pub mod styles {
    use idyll_styles::Style;

    use crate::atoms::stage::Stage;
    use crate::atoms::tokens::{Face, Radius};
    use crate::styles::Palette;

    vars! {
        /// Where the knob stands and how many places it can stand in — written per render by
        /// [`super::knob`]. Unregistered: what makes the slide smooth is the transition on
        /// `transform`, not an interpolation of these, and a registered pair would animate
        /// the *place* as a number and land the knob between two items.
        pub Knob {
            at: 0,
            n: 1,
        }
    }

    /// The trough the knob travels in. Items sit above it, so the knob reads as the
    /// surface the selected label is printed on rather than a highlight behind it.
    ///
    /// Its choices are nowrap, so past the width that holds them on one line there is no
    /// line to have: the trough turns and the choices stack, one per row.
    ///
    /// Equal columns, each at least as wide as the widest choice. A row of flex items dividing
    /// the trough evenly divides whatever width the trough already has, so the longest label
    /// spills out of its share; `1fr` tracks take their floor from the content and then match
    /// each other, which is the same "one knob is one item" the knob is sized against — arrived
    /// at from the labels rather than in spite of them.
    pub const GROUP: Style = css! {{
        position: "relative",
        display: "inline-grid",
        grid_auto_flow: "column",
        grid_auto_columns: "1fr",
        padding: "4px",
        background: Palette::ground,
        border_radius: Radius::pill,
        max_width(680px): { grid_auto_flow: "row", grid_auto_rows: "1fr", width: "100%" },
    }};

    /// The moving surface. It is sized from the box the items divide — the trough less its
    /// padding — so one knob *is* one item and travel is that per step; sizing it against
    /// the padding box instead drifts by the padding on every step.
    ///
    /// The axis is the trough's: across while the choices sit in a row, down once they
    /// stack. Both are the same two numbers, so a render says where the knob is and this
    /// says what that means at this width.
    pub const KNOB: Style = css! {{
        position: "absolute",
        top: "4px",
        bottom: "4px",
        left: "4px",
        width: calc((100% - 8px) / Knob::n),
        transform: translate_x(calc(Knob::at * 100%)),
        background: Palette::wash,
        border_radius: Radius::pill,
        box_shadow: "0 2px 6px #28301a29",
        transition: "transform 320ms cubic-bezier(.34,1.4,.5,1), background-color 320ms ease, opacity 200ms ease",
        max_width(680px): {
            right: "4px",
            bottom: "auto",
            width: "auto",
            height: calc((100% - 8px) / Knob::n),
            transform: translate_y(calc(Knob::at * 100%)),
        },
    }};

    /// One choice. It sets its own column's floor and the columns then match, which is what
    /// makes one knob width exactly one item — and what keeps a long label inside the surface
    /// that is supposed to be printing it.
    pub const ITEM: Style = css! {{
        position: "relative",
        z_index: 1,
        padding: "10px 22px",
        pointer_coarse: { padding: "11px 20px" },
        font_family: Face::sans,
        font_size: "14px",
        font_weight: 600,
        white_space: "nowrap",
        color: Palette::ink_muted,
        background: "transparent",
        border: "none",
        border_radius: Radius::pill,
        cursor: "pointer",
        transition: "color 240ms",
    }};

    /// The selected label, in each tone. Only the colour changes — the surface under
    /// it is the knob's, which is how the two stay in step while it slides.
    pub const ON: Style = css! {{
        color: Palette::wash_ink,
    }};

    pub const ON_SHED: Style = css! {{
        color: Palette::wash_shed_ink,
    }};

    pub const ON_SERVE: Style = css! {{
        color: Palette::wash_serve_ink,
    }};

    pub const ALARM: Style = css! {{
        color: Stage::red,
    }};
}
