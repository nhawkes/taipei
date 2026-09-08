//! A number's box: as wide at its widest as at 0, so nothing beside a reading moves as it
//! counts. `1ch` is a digit's width and the page sets tabular figures, so `Nch` holds N digits
//! exactly; the number sits against the right edge and grows leftwards into the room it has.

use idyll_styles::styles;

#[styles]
pub mod styles {
    use idyll_styles::Style;

    pub const FIG3: Style = css! {{ display: "inline-block", min_width: "3ch", text_align: "right" }};
    pub const FIG4: Style = css! {{ display: "inline-block", min_width: "4ch", text_align: "right" }};
    pub const FIG5: Style = css! {{ display: "inline-block", min_width: "5ch", text_align: "right" }};
    pub const FIG9: Style = css! {{ display: "inline-block", min_width: "9ch", text_align: "right" }};
}

pub use styles::{FIG3, FIG4, FIG5, FIG9};
