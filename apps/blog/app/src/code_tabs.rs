//! The code-tabs live — the reader's choice between two spellings of the same thing.
//!
//! A ` ```rust tab=… ` run in the markdown becomes a marker keyed by its group's id
//! (`blog_core::code_tabs`); this reads that group back out of the store and renders
//! the pill plus the selected panel. The code was never in the marker, so switching
//! costs nothing and the same bar the sims use does the switching.

use idyll::{live_view, Ctx, Setup};
use blog_core::CodeKey;
use idyll_data::Store;

use crate::atoms::toggle::{self, ToggleGroup};
use crate::{CodeGroupFrag, PageFrag, PageSeed};

#[derive(Debug)]
pub enum TabsMsg {
    /// Show the tab at this index.
    Pick(usize),
}

/// The group this marker names. A code group rides inside its page, so the page is what
/// gets read; `id` is page-scoped, which makes finding it here a whole identity.
fn group_of(page: PageFrag, key: &CodeKey) -> Option<CodeGroupFrag> {
    page.code_tabs.into_iter().find(|group| group.id == key.0)
}

pub async fn run(ctx: Ctx<Setup, TabsMsg>, seed: PageSeed, key: CodeKey) -> idyll::Result {
    let store = Store::of(&ctx, &seed)?;
    let page = PageFrag::read(&store.cache, store.page.clone()).await;
    let at = ctx.mutable_signal(0usize);

    // Labels are fixed for the group's life; only which one is picked moves.
    let labels: Vec<String> = group_of(page.at_mount(&ctx), &key)
        .map(|group| group.tabs.into_iter().map(|tab| tab.label).collect())
        .unwrap_or_default();
    let items: Vec<_> = labels
        .iter()
        .enumerate()
        .map(|(i, label)| {
            let picked = at.read();
            let on = ctx.computed(move |cx| picked.get(cx) == i).read();
            (i, label.as_str().into(), on, toggle::Tone::Normal)
        })
        .collect();
    let knob = toggle::knob(&ctx, &items);

    let shown = {
        let picked = at.read();
        let live = page.clone();
        ctx.computed(move |cx| {
            let tabs = group_of(live.get(cx), &key).map(|group| group.tabs).unwrap_or_default();
            let i = picked.get(cx).min(tabs.len().saturating_sub(1));
            tabs.get(i).map(|tab| tab.code.clone()).unwrap_or_default()
        })
        .read()
    };

    let mut ctx = ctx.render(live_view! {
        div css=[styles::SET] {
            ToggleGroup items=(items) knob=(knob) picked=>(TabsMsg::Pick)
            pre css=[styles::PANEL] { code { $shown } }
        }
    }).await?;

    loop {
        let (msg, reducer) = ctx.recv().await?;
        let turn: idyll::Turn = (&reducer).into();
        match msg {
            TabsMsg::Pick(i) => at.set(&turn, i),
        }
    }
}

use idyll_styles::styles;

#[styles]
pub mod styles {
    use idyll_styles::Style;

    use crate::atoms::tokens::{Face, Radius};
    use crate::styles::Palette;

    pub const SET: Style = css! {{
        margin: "1.5rem 0",
    }};

    /// The panel below the bar — the same dark surface every other code block wears,
    /// so a tabbed block and a plain one read as the same kind of thing.
    pub const PANEL: Style = css! {{
        margin: "14px 0 0",
        padding: "20px 22px",
        background: Palette::panel,
        color: Palette::panel_ink,
        border_radius: Radius::card,
        overflow_x: "auto",
        font_family: Face::mono,
        font_size: "12.5px",
        line_height: 1.85,
    }};
}
