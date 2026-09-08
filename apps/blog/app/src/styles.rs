//! The blog's typed styles (idyll-styles). Content typography rides [`PAGE`] as
//! descendant element blocks — markdown output can't carry `css=[…]`, its ancestor
//! can. Colors read from the `Palette` var group: every surface names its ink, and a value
//! edit is a stylesheet-only change everywhere.

use idyll_styles::styles;

#[styles]
pub mod styles {
    use idyll_styles::Style;

    use crate::atoms::tokens::{Face, Radius};

    vars! {
        pub Palette {
            page:      "#f8faf3",
            ink:       "#2b2a23",
            ink_muted: "#8a9080",
            // Quieter than `ink_muted`: a caption under a reading, a lane with nothing in
            // it — present, but never competing with the number it sits beside.
            ink_faint: "#aab09c",
            line:      "#e0e3d3",
            accent:    "#5f7d45",
            // The reading surface a component sits *on* rather than in — the sim's
            // card, a control's rail. One step down from the page, never a border's job.
            ground:    "#eff1e6",
            // The code surface. Still dark, for the reason it always was: a code block
            // is content, not chrome, and it does not follow the page.
            panel:     "#0b0d10",
            panel_ink: "#e6edf3",
            // The affordance — what the reader can act on. `wash` is the filled state,
            // `wash_ink` the text that must stay legible on it.
            action:       "#7d9b63",
            action_hover: "#8fae74",
            action_press: "#6b8853",
            wash:         "#dbe8c8",
            wash_ink:     "#445a2e",
            // The one control a reader has to find before anything happens — a step
            // stronger than `wash`, because it is asking rather than offering.
            cta:          "#b6cc9a",
            cta_ink:      "#2f4420",
            // A choice that sheds and a choice that serves wear their own outcome, so
            // the switch says which world you are in before you read the label.
            wash_shed:     "#f4e1db",
            wash_shed_ink: "#a25c4f",
            wash_serve:     "#e6efd8",
            wash_serve_ink: "#546b3b",

            // The controls' own set. A knob, a rail and a button's outline are not the
            // page's ink and line at lower opacity — they are their own decisions, and
            // naming them is what lets the whole control surface retune together.
            control_ink:  "#57614a",
            control_line: "#d8d9c8",
            solid:        "#ecf0e2",
            solid_ink:    "#4c5a37",
            readout:      "#e6efd8",
            readout_ink:  "#546b3b",
            gutter:       "#9a9a8c",
            rail:         "#dee2ce",
            rail_fill:    "#aecb8d",
            grab:         "#88a76a",
        }
    }

    theme! {
        /// The code surface — the one region that is dark rather than dark chrome.
        /// `page`/`ink` are remapped onto the panel pair rather than restated, so the
        /// colours have one definition; descendants then name the ordinary tokens and
        /// get panel-appropriate values.
        pub Panel: Palette {
            page: Palette::panel,
            ink: Palette::panel_ink,
            ink_muted: "#7d8896",
        }
    }

    /// The document itself — the two elements no component owns.
    document! {
        root { color_scheme: "light" },
        body {
            margin: "0",
            background: Palette::page,
        },
    }

    /// The page shell: the one styled wrapper every route renders into. Content
    /// typography lives on [`CONTENT`] (the `<article>`), so a descendant rule here
    /// never outweighs a chrome element's own class.
    pub const PAGE: Style = css! {{
        max_width: "900px",
        margin: "0 auto",
        padding: "2rem 20px",
        font_family: Face::sans,
        font_size: "1.0625rem",
        line_height: 1.65,
        color: Palette::ink,
        background: Palette::page,
        mobile: {
            padding: "1.25rem 1rem",
            font_size: "1rem",
            line_height: 1.6,
        },
    }};

    /// The article's typography, as descendant blocks on the `<article>` wrapper.
    /// The shell **is** the measure — one width for the page, with the sim breaking
    /// out of it by a deliberate margin rather than the two unrelated widths a
    /// separate prose measure produced.
    pub const CONTENT: Style = css! {{
        margin: "0 auto",
        mobile: {
            h1: { font_size: "1.75rem" },
            h2: { font_size: "1.25rem" },
        },

        // Prose is set in the serif, larger and looser than the chrome around it — the
        // reading voice is a different voice from the controls'.
        p: {
            font_family: Face::serif,
            font_size: "21px",
            line_height: 1.5,
            margin: "0 0 22px",
            text_wrap: "pretty",
        },
        li: { font_family: Face::serif, font_size: "21px", line_height: 1.5 },

        h1: { font_size: "2.25rem", line_height: 1.15, margin: "0 0 1rem" },
        h2: { font_size: "1.375rem", line_height: 1.3, margin: "2.5rem 0 0.75rem" },
        h3: { font_size: "1.125rem", line_height: 1.35, margin: "2rem 0 0.5rem" },
        a: { color: Palette::accent },
        blockquote: {
            margin: "1.5rem 0",
            padding: "0.125rem 0 0.125rem 1rem",
            border_left: "3px solid transparent",
            border_left_color: Palette::accent,
            color: Palette::ink_muted,
            font_family: Face::serif,
        },
        hr: { border: "none", border_bottom: "1px solid transparent", border_color: Palette::line, margin: "2.5rem 0" },
        table: { display: "block", overflow_x: "auto" },
        img: { max_width: "100%" },
        pre: {
            background: Palette::panel,
            color: Palette::panel_ink,
            padding: "1.25rem 1.375rem",
            border_radius: Radius::card,
            overflow_x: "auto",
            line_height: 1.85,
            font_size: "0.8125rem",
        },
        code: {
            font_family: Face::mono,
            font_size: "0.875em",
        },
    }};

    pub const HEADER: Style = css! {{
        display: "flex",
        align_items: "baseline",
        justify_content: "space-between",
        margin: "0 0 2.5rem",
        padding: "0 0 0.875rem",
        border_bottom: "1px solid transparent",
        border_color: Palette::line,
        mobile: { margin: "0 0 1.75rem" },
    }};

    pub const NAV_LINK: Style = css! {{
        color: Palette::ink,
        font_weight: 650,
        font_size: "1.125rem",
        text_decoration: "none",
        ":hover": { color: Palette::accent },
    }};

    pub const TOC_LIST: Style = css! {{
        list_style: "none",
        padding: "0",
        margin: "0.25rem 0",
    }};

    pub const TOC_ITEM: Style = css! {{
        display: "flex",
        align_items: "baseline",
        gap: "0.625rem",
        padding: "0.3rem 0",
    }};

    /// A chapter's number in the contents — muted, tabular by the monospace stack.
    pub const TOC_NUM: Style = css! {{
        color: Palette::ink_muted,
        font_family: Face::mono,
        font_size: "0.875rem",
    }};

    pub const LINK: Style = css! {{
        color: Palette::accent,
        text_decoration: "none",
        ":hover": { text_decoration: "underline" },
    }};
}

pub use styles::*;
