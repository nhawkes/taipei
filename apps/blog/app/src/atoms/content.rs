//! The one surface the content mapping renders: the error placeholder. The mapping is
//! structure only, so it travels there as a value (`blog_core::ContentMapping`) rather
//! than as a class name it would have to know.

use idyll_styles::styles;

#[styles]
pub mod styles {
    use idyll_styles::Style;

    use crate::atoms::tokens::{Face, Radius};

    /// A content error, rendered loud in the page.
    pub const SIM_ERROR: Style = css! {{
        background: "#fbe9e7",
        color: "#a3352c",
        font_family: Face::mono,
        font_size: "13px",
        padding: "12px 16px",
        border_radius: Radius::control,
    }};
}

pub use styles::SIM_ERROR;
