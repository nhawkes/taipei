//! Typography atoms — the type ramp as components (content plane: pure `View`
//! builders, embedded with `@content(…)`). The ramp's geometry lives here; page
//! layout (what sits where) stays with the page.

use idyll::{view, View};
use idyll_styles::styles;

pub enum HeadingLevel {
    /// The page's own title (`h1`).
    Title,
    /// A section heading within the page (`h2`).
    Section,
}

pub fn heading(level: HeadingLevel, text: &str) -> View {
    let text = text.to_string();
    match level {
        HeadingLevel::Title => view! { h1 css=[styles::TITLE] { (text) } },
        HeadingLevel::Section => view! { h2 css=[styles::SECTION] { (text) } },
    }
}

/// The muted one-liner under a title.
pub fn standfirst(text: &str) -> View {
    let text = text.to_string();
    view! { p css=[styles::STANDFIRST] { (text) } }
}

#[styles]
pub mod styles {
    use idyll_styles::Style;

    use crate::styles::Palette;

    pub const TITLE: Style = css! {{
        font_size: "2.25rem",
        line_height: 1.15,
        margin: "0 0 0.5rem",
        mobile: { font_size: "1.75rem" },
    }};

    pub const SECTION: Style = css! {{
        font_size: "1.375rem",
        line_height: 1.3,
        margin: "2.5rem 0 0.75rem",
    }};

    pub const STANDFIRST: Style = css! {{
        color: Palette::ink_muted,
        margin: "0 0 2rem",
    }};
}
