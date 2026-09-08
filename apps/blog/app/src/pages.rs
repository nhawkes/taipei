//! The blog's pages — the root component and its head. Every view lives HERE, in the
//! app crate; the server resolves data. Which page renders is the seed's `route`
//! sum — the server parsed the path exactly once, the `match` binds each variant's
//! own payload, and it is a **mount-time value**: transitions (and the dev content
//! watcher's reload) re-mount the page, so nothing here tracks.

use idyll::{view, Ctx, Never, View, Setup};
use idyll_data::Store;
use idyll_route::Route as _;

use crate::atoms::typography::{heading, standfirst, HeadingLevel};
use crate::{styles, PageFrag, PageFragRoute, PageSeed};

/// The route view — the root component the server mounts per request and the browser
/// re-mounts per transition. Pure content: a value match, zero live bindings.
pub async fn page(ctx: Ctx<Setup, Never>, seed: PageSeed) -> idyll::Result {
    let store = Store::of(&ctx, &seed)?;
    let live = PageFrag::read(&store.cache, store.page).await;
    let content = match live.at_mount(&ctx).route {
        PageFragRoute::Article { body } => article(body),
        PageFragRoute::Index {} => index(),
        PageFragRoute::NotFound {} => not_found(),
    };
    Ok(ctx.render_content(view! {
        div css=[styles::PAGE] {
            @content(shell())
            main {
                @content(content)
            }
        }
    }).await?)
}

/// The document-head content. The `<title>` is a contract field on the page node —
/// the host writes it into the envelope, not the view.
pub async fn head(ctx: Ctx<Setup, Never>, _seed: PageSeed) -> idyll::Result {
    Ok(ctx.render_content(view! {
        meta charset=("utf-8")
        meta name=("viewport") content=("width=device-width, initial-scale=1")
        link rel=("icon") href=("/static/favicon.svg")
        // The faces the styles name. Without these the whole type scale falls back and every
        // size, weight and measure chosen for them is applied to something else. Served locally
        // from `blog-fonts` (`display:swap` in each `@font-face`), not a font CDN.
        link rel=("stylesheet") href=("/static/fonts.css")
    }).await?)
}

/// The shared page chrome, beside the styles it wears.
fn shell() -> View {
    view! {
        header css=[styles::HEADER] {
            a css=[styles::NAV_LINK] href=("/") { "taipei" }
        }
    }
}

/// An article's body arrives as **data**: the content mapping ran with the route
/// query (markdown to View IR, sim fences resolved against this crate's live
/// table), so the read yields the view and this component only wraps it.
fn article(body: View) -> View {
    view! {
        article css=[styles::CONTENT] {
            @content(body)
        }
    }
}

fn not_found() -> View {
    heading(HeadingLevel::Title, "This page does not exist")
}

/// The front page's body: the book's table of contents, static — `blog_core::BOOK`
/// compiles into this component, so the index route carries no data at all.
fn index() -> View {
    #[derive(Clone)]
    struct TocEntry {
        href: String,
        title: String,
        num: String,
    }
    let book: Vec<TocEntry> = blog_core::BOOK
        .iter()
        .enumerate()
        .map(|(i, chapter)| TocEntry {
            href: blog_core::BlogRoute::doc(chapter.path).url().into_string(),
            title: chapter.title.to_string(),
            num: (i + 1).to_string(),
        })
        .collect();

    view! {
        @content(heading(HeadingLevel::Title, "taipei"))
        @content(standfirst("A queue engine, and the framework built to write about it."))
        ul css=[styles::TOC_LIST] {
            @for chapter in (book) {
                li css=[styles::TOC_ITEM] {
                    span css=[styles::TOC_NUM] { (chapter.num.clone()) }
                    a css=[styles::LINK] href=(chapter.href.clone()) { (chapter.title.clone()) }
                }
            }
        }
    }
}
