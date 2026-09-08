//! The key to a picture's colours: a swatch and what wearing it means, in a row beneath the
//! thing it explains.
//!
//! Presentational (`Never`). Two of the multi-server sims draw one — the fan-out's three lanes
//! and the percentile stage's six — and what they share is all of it: the row, the swatch, and
//! the rule that an entry is a colour beside the one thing it stands for.

use idyll::{live_view, Ctx, Never, Setup};
use idyll_styles::styles;

use crate::atoms::stage::Paint;

/// One entry: a colour, and what a thing wearing it is.
#[derive(Clone)]
pub struct Key {
    pub ink: Paint,
    pub means: &'static str,
}

#[idyll::component]
pub async fn Legend(ctx: Ctx<Setup, Never>, keys: Vec<Key>) -> idyll::Result {
    Ok(ctx.render(live_view! {
        div css=[styles::ROW] {
            @for key in (keys) {
                span css=[styles::ITEM] {
                    span css=[styles::SWATCH] style=(swatch(&key)) {} (key.means)
                }
            }
        }
    })
    .await?)
}

fn swatch(key: &Key) -> String {
    format!("background:{}", key.ink)
}

#[styles]
pub mod styles {
    use idyll_styles::Style;

    use crate::styles::Palette;

    /// The row, under the picture. It wraps, because a key that pushed the card wider would be
    /// the caption deciding the layout of the thing it is a caption for.
    pub const ROW: Style = css! {{
        display: "flex",
        gap: "14px",
        flex_wrap: "wrap",
        margin: "12px 2px 0",
        font_size: "10.5px",
        color: Palette::ink_muted,
    }};

    pub const ITEM: Style = css! {{
        display: "inline-flex",
        align_items: "center",
        gap: "5px",
    }};

    /// The colour itself, at the size a colour is legible at and no larger — a swatch big
    /// enough to read as a shape would be competing with the picture it is explaining.
    pub const SWATCH: Style = css! {{
        width: "11px",
        height: "11px",
        border_radius: "3px",
    }};
}
