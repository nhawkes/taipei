//! Framed — a diagram and the control that steers it, as one box.
//!
//! The control belongs *to* the picture, not to a band above it: a switch that recolours the
//! diagram reads as part of it, and a reader looking at the picture finds it without leaving.
//! So the frame owns the corner well and the placement, and the caller owns both the diagram
//! and the control — neither knows about the other.
//!
//! The frame draws no surface of its own. Whatever is placed inside brings its own (the
//! machine picture is already a card; a table sits on one), so a surface here would be a
//! second border around the first.

use idyll::{live_view, Ctx, Never, Setup, Slot};
use idyll_styles::styles;

#[idyll::component]
pub async fn Framed(ctx: Ctx<Setup, Never>, corner: Slot, children: Slot) -> idyll::Result {
    Ok(ctx.render(live_view! {
        div css=[styles::FRAME] {
            div css=[styles::CORNER] { (corner) }
            (children)
        }
    }).await?)
}

#[styles]
pub mod styles {
    use idyll_styles::Style;

    /// The positioning context the corner hangs from — and, where the picture has no corner
    /// to spare, the column the two of them stack in.
    pub const FRAME: Style = css! {{
        position: "relative",
        max_width(680px): { display: "flex", flex_direction: "column" },
    }};

    /// The corner well. Content-sized, so a control that grows to the room it is given (an
    /// [`Invite`](crate::atoms::invite::Invite)'s ring does) hugs the control instead of
    /// spanning the picture. Inset far enough that the ring's bleed, and the tag that hangs
    /// above it, both land inside the box rather than over its edge.
    ///
    /// A well over the picture needs picture to spare. Narrow enough and the diagram reaches
    /// every corner, so the control stops floating and takes its own line above — still at the
    /// trailing edge, still reading as the picture's, no longer sitting on top of it.
    pub const CORNER: Style = css! {{
        position: "absolute",
        top: "26px",
        right: "14px",
        z_index: 2,
        display: "inline-flex",
        // Full width rather than content width, so what it holds is measured against the
        // picture's width instead of against itself; the control still sits at the trailing
        // edge, placed rather than shrink-wrapped.
        max_width(680px): {
            position: "static",
            align_self: "stretch",
            display: "flex",
            justify_content: "flex-end",
            margin_bottom: "10px",
        },
    }};
}
