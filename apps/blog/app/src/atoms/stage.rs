//! The sim stage's own palette. The stage is content, not chrome — it carries the
//! vocabulary the diagram teaches (teal is the kernel's accept queue, amber the
//! runtime's, blue a core, purple IO) and that meaning is the same whatever the page
//! around it does.
//!
//! The values live in the stylesheet like every other colour: the stage draws in
//! **inline** styles (a dot's position is not a class), so a [`Paint`] writes the
//! `var(…)` reference the declaration needs. Retinting the whole diagram is a value
//! edit here, and the fast track swaps the sheet under a running sim.

use idyll_styles::styles;

pub use styles::Stage;

/// A colour as an inline value: the `var(…)` reference for a stage token. This is the
/// framework's [`VarRef`](idyll_styles::VarRef) — write `Stage::teal.value()` — so the
/// stage's inline draws and any per-item computed colour share one source of truth with
/// the class surface.
pub type Paint = idyll_styles::VarRef<idyll_styles::kind::Color>;

#[styles]
pub mod styles {
    vars! {
        pub Stage {
            // The machine the diagram is drawn on: the ground it sits on, the
            // channel a request travels down, and the wall between them.
            ground:  "#eff1e6",
            channel: "#fcfdfa",
            wall:    "#cdd3c0",
            core:    "#dce0cf",

            // Where a request is. One hue per station, and the exits inherit the
            // hue of whatever ended the journey.
            teal:   "#149c90",
            amber:  "#db8e13",
            blue:   "#2e80d0",
            grey:   "#798598",
            purple: "#8a57c2",
            green:  "#2e9e52",
            orange: "#da6626",
            salmon: "#c55f5f",
            red:    "#dc3742",

            ink:   "#2b2a23",
            muted: "#8a9080",

            // The gate's two thirds: each exit's hue, washed out. A third is a route
            // that is *available*, not one anything has taken.
            wash_admit:   "#dbe8c8",
            wash_timeout: "#f0c58c",

            // The absent colour — a ring with nothing to show still has a colour to
            // write, so this is a value like any other rather than a special case.
            none: "transparent",
        }
    }
}
