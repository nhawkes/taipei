//! ClientGrid — a batch's clients as a field of rings, one per client, each measurable so the
//! wires reaching it can be drawn.
//!
//! The [`wires`](super::wires) column is the other picture of clients: a dot with its name
//! beside it, which reads as *who is talking*. This one reads as *how many*. At a thousand
//! clients there is no room beside a ring for anything, so a ring's colour is the whole of what
//! most of them say and the few that carry a reading carry it inside themselves.
//!
//! Presentational (`Never`): a client arrives as its own signal and the laid-out rects leave
//! through a callback, because a component that owns no messages cannot receive the layout it
//! caused.

use idyll::{live_view, Callback, Ctx, Event, Never, Rect, Setup, Signal};
use idyll_styles::styles;

/// One client as the grid draws it.
#[derive(Clone, PartialEq)]
pub struct Client {
    /// The ring's colour. A string rather than a [`Paint`](super::stage::Paint): it is a
    /// position on a continuous ramp, and a ramp has no tokens to name.
    pub ink: String,
    /// What is written inside the ring — empty for most of them.
    pub label: String,
}

#[idyll::component]
pub async fn ClientGrid(
    ctx: Ctx<Setup, Never>,
    /// Every place a client can hold, each with its own signal. A row is mounted once and
    /// restyled: a client's colour changes on every completion, and a ring that was rebuilt
    /// rather than restyled would lose the transition that is the change being seen.
    clients: Vec<(usize, Signal<Client>)>,
    /// How many of those places this batch fills. The rest draw no ring at all, so a small
    /// batch is a small grid rather than a large one with holes in it.
    count: Signal<usize>,
    /// Where a node's laid-out rect goes. The grid owns no messages, so the caller
    /// supplies the carrier.
    measured: Callback<(usize, Rect)>,
) -> idyll::Result {
    Ok(ctx.render(live_view! {
        div css=[styles::GRID] style=(spread($count)) {
            @for (at, client) in (clients) {
                @if (at < $count) {
                    div css=[styles::NODE] {
                        span css=[styles::RING] style=(ring_ink(&$client))
                            measure=(measured.try_contra_map(move |e: Event| e.rect().map(|r| (at, r)))) {
                            span css=[styles::LABEL] style=(label_ink(&$client)) { ($client.label) }
                        }
                    }
                }
            }
        }
    }).await?)
}

fn spread(clients: usize) -> String {
    format!("{}:{}", styles::Grid::clients, clients)
}

fn ring_ink(client: &Client) -> String {
    format!("border-color:{}", client.ink)
}

/// A label wears its own ring's colour: two claims about one client that could disagree would
/// be one claim too many.
fn label_ink(client: &Client) -> String {
    format!("color:{}", client.ink)
}

#[styles]
pub mod styles {
    use idyll_styles::Style;

    use crate::atoms::tokens::Face;

    vars! {
        /// How many clients there are — the whole of what a render knows about the shape of
        /// the grid. What that count *means* for the columns is [`GRID`]'s, because how many
        /// rings fit across is a question about the width of the page, and a render cannot
        /// see one.
        ///
        /// Unregistered: registration is what lets the browser interpolate a custom property,
        /// and a count is not a value to interpolate — a run from ten to a thousand would
        /// reflow the grid through every width on the way.
        pub Grid {
            clients: 0,
        }
    }

    /// The field. Rings tile across whatever room there is, which past a handful of clients is
    /// the picture: a block whose colour is read as a mass rather than ring by ring.
    ///
    /// At ten or fewer it does not spread. The basis is two columns' worth — two rings and the
    /// gap between them — and the growth is one unit per client past the tenth, so a handful
    /// has none to spend and stays a group. Strung across the full width, a handful reads as
    /// noise instead.
    ///
    /// Rows are a ring tall, which is also what keeps them at the top of a grid given more
    /// height than its clients need.
    pub const GRID: Style = css! {{
        display: "grid",
        grid_template_columns: "repeat(auto-fill, minmax(24px, 1fr))",
        grid_auto_rows: "24px",
        gap: "6px 4px",
        flex_basis: "52px",
        flex_grow: calc(Grid::clients - 10),
        min_width: "0",
    }};

    /// One cell. The ring is centred in it rather than filling it, so a track wider than a ring
    /// spaces the field out instead of stretching what is in it.
    pub const NODE: Style = css! {{
        display: "flex",
        align_items: "center",
        justify_content: "center",
    }};

    /// The ring. Border-box, so 24px is the size it draws at and two of them plus the column
    /// gap are the 52px the grid holds itself to.
    ///
    /// Hollow: what a client has to say is said by the ring it wears, and a filled disc at this
    /// size reads as a dot on the page rather than as a client waiting.
    pub const RING: Style = css! {{
        width: "24px",
        height: "24px",
        box_sizing: "border-box",
        border_radius: "50%",
        background: "transparent",
        border_width: "2.5px",
        border_style: "solid",
        display: "flex",
        align_items: "center",
        justify_content: "center",
        flex_shrink: 0,
        transition: "border-color 300ms ease",
    }};

    /// The reading, inside the ring it belongs to. Beside it there is no room at a thousand
    /// clients, and no way to tell which ring a label is for.
    pub const LABEL: Style = css! {{
        font_family: Face::mono,
        font_size: "8px",
        font_weight: 600,
        line_height: 1,
    }};
}
