//! Button — the action atom: one component, a `kind` variant enum. The label is a
//! signal because a control's word can change under it (`run`/`pause`); a caller whose
//! word is fixed passes `ctx.constant(…)`.

use idyll::{live_view, Callback, Ctx, Event, Never, Setup, Signal};
use idyll_styles::styles;

use super::tokens::FOCUS;

pub enum ButtonKind {
    /// The one control the reader has to find for anything to happen — the sim's
    /// `run`. Solid-filled, and the only button on the page set that strongly.
    Cta,
    /// The filled call-to-action (the sim's `GET /ping`, "send a request").
    Primary,
    /// A control that is one of several equals in its row (reset) — filled, but in
    /// the surface's own tint rather than the affordance's.
    Solid,
    /// The quiet outlined control (the timeout toggles).
    Ghost,
}

#[idyll::component]
pub async fn Button(
    ctx: Ctx<Setup, Never>,
    kind: ButtonKind,
    label: Signal<String>,
    pressed: Callback<Event>,
) -> idyll::Result {
    let variant = match kind {
        ButtonKind::Cta => styles::CTA,
        ButtonKind::Primary => styles::PRIMARY,
        ButtonKind::Solid => styles::SOLID,
        ButtonKind::Ghost => styles::GHOST,
    };
    Ok(ctx.render(live_view! {
        button css=[styles::BASE, FOCUS, variant] onclick=(pressed) { $label }
    }).await?)
}

#[styles]
pub mod styles {
    use idyll_styles::Style;

    use crate::atoms::tokens::{Face, Radius};
    use crate::styles::Palette;

    /// Every button is the same pill: one shape, four weights of emphasis. The
    /// variants below change only colour, so a row of mixed kinds still reads as one
    /// family of things to press.
    ///
    /// A button whose label changes (`run`/`pause`) keeps its box: a control that
    /// resizes under the cursor is a control that moves away from the next click.
    pub const BASE: Style = css! {{
        font_family: Face::sans,
        font_size: "14px",
        font_weight: 500,
        min_width: "96px",
        padding: "9px 18px",
        // A finger needs a real target.
        pointer_coarse: { padding: "11px 18px" },
        text_align: "center",
        white_space: "nowrap",
        border_radius: Radius::pill,
        cursor: "pointer",
        transition: "background 120ms",
    }};

    /// The sim's `run`. The strongest thing on the page, because nothing else the
    /// reader can press matters until this one has been.
    pub const CTA: Style = css! {{
        font_weight: 600,
        color: Palette::cta_ink,
        background: Palette::cta,
        border: "none",
        ":hover": { background: Palette::action_hover },
        ":active": { background: Palette::action_press },
    }};

    pub const PRIMARY: Style = css! {{
        font_weight: 600,
        color: Palette::wash_ink,
        background: Palette::wash,
        border: "none",
        ":hover": { background: Palette::action_hover },
        ":active": { background: Palette::action_press },
    }};

    pub const SOLID: Style = css! {{
        color: Palette::solid_ink,
        background: Palette::solid,
        border: "none",
        ":hover": { background: Palette::wash },
    }};

    pub const GHOST: Style = css! {{
        color: Palette::control_ink,
        background: "transparent",
        border_width: "1px",
        border_style: "solid",
        border_color: Palette::control_line,
        ":hover": { background: Palette::ground },
    }};
}
