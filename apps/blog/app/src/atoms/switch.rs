//! Switch — a two-position control that names both of its positions.
//!
//! Distinct from [`ToggleGroup`](crate::atoms::toggle): a group is a *set* of choices and
//! grows one more the day a fourth policy exists; a switch is a single quantity with two
//! settings, and the pair is the whole of it. Both labels are drawn — an unlabelled knob
//! says a state is on without saying what the other state would be.

use idyll::{live_view, Callback, Ctx, Event, Never, Setup, Signal};
use idyll_styles::styles;

use super::tokens::FOCUS;

/// `off`/`on` name the two positions, `at` is which one holds (`false` = `off`), and
/// `flipped` is where a press goes. Controlled, like every other control here: the state
/// belongs to whoever is driving the machine.
#[idyll::component]
pub async fn Switch(
    ctx: Ctx<Setup, Never>,
    off: &'static str,
    on: &'static str,
    at: Signal<bool>,
    flipped: Callback<Event>,
) -> idyll::Result {
    let (is_on, is_off) = (at.clone(), {
        let at = at.clone();
        ctx.computed(move |cx| !at.get(cx)).read()
    });
    Ok(ctx.render(live_view! {
        button css=[styles::SWITCH, FOCUS] role=("switch") aria_checked=($is_on) onclick=(flipped) {
            span css=[styles::LABEL, $is_off => styles::PICKED] { (off) }
            span css=[styles::TRACK] {
                span css=[styles::KNOB, $is_on => styles::KNOB_ON] {}
            }
            span css=[styles::LABEL, $is_on => styles::PICKED] { (on) }
        }
    }).await?)
}

#[styles]
pub mod styles {
    use idyll_styles::Style;

    use crate::atoms::tokens::{Face, Radius};
    use crate::styles::Palette;

    pub const SWITCH: Style = css! {{
        display: "inline-flex",
        align_items: "center",
        gap: "10px",
        padding: "8px 14px",
        pointer_coarse: { padding: "10px 16px" },
        background: "transparent",
        border: "none",
        border_radius: Radius::pill,
        cursor: "pointer",
    }};

    /// A position's name. Both are always drawn; the one in force takes the ink.
    pub const LABEL: Style = css! {{
        font_family: Face::sans,
        font_size: "12px",
        font_weight: 600,
        color: Palette::ink_muted,
        transition: "color 240ms",
    }};
    pub const PICKED: Style = css! {{ color: Palette::wash_ink }};

    pub const TRACK: Style = css! {{
        position: "relative",
        width: "34px",
        height: "18px",
        border_radius: Radius::pill,
        background: Palette::rail,
        flex_shrink: 0,
    }};

    /// The knob's travel is the track less its own width and both insets.
    pub const KNOB: Style = css! {{
        position: "absolute",
        top: "2px",
        left: "2px",
        width: "14px",
        height: "14px",
        border_radius: Radius::pill,
        background: Palette::grab,
        box_shadow: "0 1px 3px #28301a3d",
        transition: "transform 240ms cubic-bezier(.34,1.4,.5,1)",
    }};
    pub const KNOB_ON: Style = css! {{ transform: "translateX(16px)" }};
}
