//! The invite affordance — a pulsing ring and a bobbing tag that mark the one control the
//! prose asks the reader to take. A **wrapper**: it takes the control as its `children` slot
//! and, while its invitation stands, draws the [`INVITE`](styles::INVITE) ring around it and
//! an [`ASK`](styles::ASK) tag of what taking it reveals. Worn by a
//! [`Slider`](crate::atoms::slider) the text invites, and by the cluster sim's Up button
//! before the reader first climbs out of a single server.

use idyll::{live_view, Ctx, Never, Setup, Signal, Slot};

/// Wrap a control in its invitation: `when` is whether the invitation still stands, `hint`
/// what taking it reveals. While `when` holds, the wrapped control wears the pulsing ring and
/// the bobbing tag; once the reader takes it (`when` goes false), both fall away and the
/// wrapper is a bare box around the control. The two travel together — a pulse with nothing
/// to say, or a hint that never shows, are not states a caller can build.
#[idyll::component]
pub async fn Invite(
    ctx: Ctx<Setup, Never>,
    when: Signal<bool>,
    hint: &'static str,
    children: Slot,
) -> idyll::Result {
    let asking = when.clone();
    Ok(ctx.render(live_view! {
        div css=[styles::WRAP, $when => styles::INVITE] {
            @if ($asking) {
                span css=[styles::ASK] { (hint) }
            }
            (children)
        }
    }).await?)
}

#[idyll_styles::styles]
pub mod styles {
    use idyll_styles::Style;

    use crate::atoms::tokens::Radius;
    use crate::styles::Palette;

    keyframes! {
        /// The invited control's pulse — a ring that expands and fades, the shape of a tap
        /// radiating outward. It repeats until the reader takes the hint, and the hint is
        /// removed the moment they do.
        pub Nudge {
            from { box_shadow: "0 0 0 0 #77935a4d" },
            "70%" { box_shadow: "0 0 0 12px #77935a00" },
            to { box_shadow: "0 0 0 0 #77935a00" }
        }

        /// The tag above the invited control, on the same breath as the ring below it.
        pub Bob {
            from { transform: "translateY(0)" },
            "50%" { transform: "translateY(-4px)" },
            to { transform: "translateY(0)" }
        }
    }

    /// The wrapper the affordance is drawn on: a flex box that fills the room its control
    /// had, and the positioning context the [`ASK`](ASK) tag hangs from. Layout-neutral at
    /// rest — [`INVITE`](INVITE)'s padding is offset by a negative margin, so turning the
    /// glow on and off never shifts the control.
    /// Where there is room above the control, the tag hangs over it and this is only the box
    /// it hangs from. Where there is not — a control with something directly above and below
    /// it — the tag takes its own line instead, which costs the reflow when the invitation is
    /// taken and buys never covering a control to ask about one.
    pub const WRAP: Style = css! {{
        position: "relative",
        display: "flex",
        align_items: "center",
        flex_grow: 1,
        flex_shrink: 1,
        flex_basis: "auto",
        max_width(680px): { flex_direction: "column", align_items: "flex-end", min_width: "0" },
    }};

    /// The invited control — a soft ground and the pulsing ring around it.
    pub const INVITE: Style = css! {{
        border_radius: "14px",
        padding: "8px 12px",
        margin: "-8px -4px",
        background: "#edf3e2",
        animation: Nudge "2.2s ease-out infinite",
    }};

    /// What the invitation is *for* — a tag over the control, bobbing so the eye finds it
    /// before it finds the control. It says what taking the hint will show; a glow alone
    /// asks without telling.
    /// Anchored by its trailing edge, so the sentence grows back across the picture — which is
    /// where the room is — instead of off the page, which is where a control near the trailing
    /// edge has none. One line either way, so it still sits in the well above the control
    /// rather than needing a well of its own.
    pub const ASK: Style = css! {{
        position: "absolute",
        right: "0",
        width: "max-content",
        top: "-15px",
        // Narrow enough and there is no room across the picture either: the tag takes its own
        // line above the control, where the width is the line's to give rather than the
        // sentence's to take, and nothing it could cover is under it.
        max_width(680px): { position: "static", width: "auto", margin_bottom: "6px" },
        padding: "4px 11px",
        border_radius: Radius::pill,
        background: Palette::wash,
        color: Palette::readout_ink,
        font_size: "11px",
        font_weight: 600,
        // It says what the control does; it is not a second way to do it. A tag hanging over a
        // dense stack of rails reaches the hit area of the one above, and a label that takes a
        // press meant for a slider is worse than one that is merely in the way.
        pointer_events: "none",
        box_shadow: "0 2px 6px #77935a33",
        animation: Bob "1.6s ease-in-out infinite",
    }};
}
