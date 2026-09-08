//! The blog's design tokens — crate vocabulary, referenced from any `css!` in the crate
//! (var identity is the crate). Color tokens stay in [`crate::styles`]'s `Palette`; these
//! are the geometry and typography scales, Astryx-neutral: a compact 4px-grid space scale,
//! quiet radii.

use idyll_styles::styles;

#[styles]
pub mod styles {
    use idyll_styles::Style;

    use crate::styles::Palette;

    vars! {
        pub Space {
            s1: "4px",
            s2: "8px",
            s3: "12px",
            s4: "16px",
            s6: "24px",
            s8: "32px",
        }
    }

    vars! {
        pub Radius {
            control: "6px",
            surface: "10px",
            // A card the page reads as one object — the sim's stage.
            card: "20px",
            // A listing: softer than a card, because it sits inside the reading rather
            // than beside it.
            panel: "12px",
            // Fully round: what every control the reader can grab or press wears.
            pill: "999px",
        }
    }

    vars! {
        // The design's faces, each ahead of the stack that stands in for it. Naming
        // them here is the whole declaration: a reader who has them gets them, and
        // when the files are vendored nothing has to be renamed to pick them up.
        pub Face {
            sans: "'IBM Plex Sans', system-ui, sans-serif",
            mono: "'IBM Plex Mono', ui-monospace, Menlo, monospace",
            serif: "'Newsreader', ui-serif, Georgia, 'Times New Roman', serif",
        }
    }

    /// The focus ring. Every focusable surface wears the same one, so it is a style
    /// composed in rather than a block each component repeats.
    pub const FOCUS: Style = css! {{
        ":focus-visible": {
            outline_width: "2px",
            outline_style: "solid",
            outline_color: Palette::accent,
            outline_offset: "1px",
        },
    }};
}

pub use styles::*;
