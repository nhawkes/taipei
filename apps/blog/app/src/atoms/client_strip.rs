//! ClientStrip — the crowd as one row: a circle per client, orange while that client is owed
//! an answer.
//!
//! The [`ClientGrid`](super::client_grid) reads as *how many*; this reads as *how alive*. It
//! sits above a fleet, so a card's picture is the crowd, then the machines, in the order a
//! request meets them. The row never wraps: at twenty clients the circles are individuals, at
//! four hundred a texture, and both are the truth about that crowd. The circles carry no
//! traffic of their own — a client's line is the card's wire from its circle to its machines,
//! and what travels it is drawn where it truly is.
//!
//! Presentational (`Never`), on the grid's conventions: every place built once, `count`
//! gates, restyle-not-rebuild.

use idyll::{live_view, Callback, Ctx, Event, Never, Rect, Setup, Signal};
use idyll_styles::styles;

use crate::atoms::stage::Stage;

/// The circle's style for a client's state: orange while an answer is owed — the waiting
/// colour everything else in the chapter waits in — and the wall tone at rest. One place, so
/// every card's crowd speaks the same two words.
pub fn ink(outstanding: bool) -> String {
    let paint = match outstanding {
        true => Stage::orange,
        false => Stage::wall,
    };
    format!("background:{}", paint.value())
}

/// Fold one frame of engine truth into the circles: ink from the open-trip counts, touching
/// only the signals whose face changed.
pub fn ink_frame(
    turn: impl idyll::InTurn + Copy,
    faces: &mut [String],
    signals: &[idyll::MutableSignal<String>],
    outstanding: &[usize],
) {
    for (c, face) in faces.iter_mut().enumerate() {
        let now = ink(outstanding.get(c).copied().unwrap_or(0) > 0);
        if *face != now {
            *face = now.clone();
            signals[c].set(turn, now);
        }
    }
}

#[idyll::component]
pub async fn ClientStrip(
    ctx: Ctx<Setup, Never>,
    /// Every place a client can hold, each with its own ink — mounted once, restyled.
    dots: Vec<(usize, Signal<String>)>,
    /// How many of those places this crowd fills; the rest draw nothing.
    count: Signal<usize>,
    /// Where a circle's laid-out rect goes — the client end of the card's wires. The strip
    /// owns no messages, so the caller supplies the carrier.
    measured: Callback<(usize, Rect)>,
) -> idyll::Result {
    Ok(ctx.render(live_view! {
        div css=[styles::STRIP] {
            @for (at, ink) in (dots) {
                @if (at < $count) {
                    span css=[styles::DOT] style=($ink)
                        measure=(measured.try_contra_map(move |e: Event| e.rect().map(|r| (at, r)))) {}
                }
            }
        }
    }).await?)
}

#[styles]
pub mod styles {
    use idyll_styles::Style;

    /// The row. Auto-flow columns share the width equally however many circles are drawn, so
    /// the crowd's size decides how individual a client gets to be — and the row never wraps,
    /// because a second row would put circles above circles instead of above machines.
    pub const STRIP: Style = css! {{
        display: "grid",
        grid_auto_flow: "column",
        grid_auto_columns: "1fr",
        justify_items: "center",
        gap: "2px",
        margin: "8px 0 2px",
        min_width: "0",
    }};

    /// The circle. Filled — at texture scale a hollow ring is invisible — and never wider
    /// than its cell, so a crowd of hundreds shrinks the circles rather than the row.
    pub const DOT: Style = css! {{
        width: "100%",
        max_width: "12px",
        aspect_ratio: "1",
        border_radius: "50%",
        flex_shrink: 0,
        transition: "background-color 300ms ease",
    }};
}
