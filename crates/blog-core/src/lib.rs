#[cfg(feature = "markdown")]
use std::{
    collections::BTreeMap,
    fs,
    path::Path,
};

#[cfg(feature = "markdown")]
pub struct Doc {
    pub title: String,
    pub markdown: String,
}

// ── Book structure ──────────────────────────────────────────────────────────
// The ordered table of contents. Single source of truth for the index and for
// chapter ordering; each `path` resolves to `docs/<path>.md` and `/<path>`.

pub struct BookChapter {
    pub title: &'static str,
    pub path: &'static str,
}

pub const BOOK: &[BookChapter] = &[
    BookChapter { title: "Introduction", path: "intro" },
    BookChapter { title: "Modelling a server", path: "modelling_a_server" },
    BookChapter { title: "Rejecting requests", path: "rejecting_requests" },
    BookChapter { title: "Load balancing", path: "load_balancing" },
    BookChapter { title: "Rate limiting", path: "rate_limiting_and_auto_scaling" },
    BookChapter { title: "Other workloads", path: "other_workloads" },
];

// ── Routes ──────────────────────────────────────────────────────────────────
// The blog's route identity, as a typed value. A `BlogRoute` you can hold is a page that
// exists; its URL is a projection (`route.url()`), and `parse` recovers it from a request
// path — both from the one `#[route]` spec, so links and parsing cannot drift. A chapter
// is one segment, so the route space is exactly the flat set of files under `docs/` —
// nothing nested is addressable.

/// The blog's routes. Available everywhere (no content dependency) — the enumeration of
/// *which* routes exist is [`BOOK`], in book order.
#[derive(idyll_route::Route, Debug, Clone, PartialEq, Eq)]
pub enum BlogRoute {
    #[route("/")]
    Index,
    #[route("/{slug}")]
    Doc { slug: String },
}

impl BlogRoute {
    /// A doc route from a book path (`"modelling_a_server"`).
    pub fn doc(path: &str) -> BlogRoute {
        BlogRoute::Doc { slug: path.to_owned() }
    }

    /// Every book chapter as a `Doc` route, in book order.
    pub fn chapters() -> impl Iterator<Item = BlogRoute> {
        BOOK.iter().map(|chapter| BlogRoute::doc(chapter.path))
    }
}

// ── Content index ─────────────────────────────────────────────────────────────
// `docs/` is scanned once at startup (and again on each reload) into a lookup map. A
// request's path is then a **key**, never a filesystem path: an unknown key is a plain map
// miss (→ 404), and nothing user-supplied is ever joined onto a path — so traversal is
// impossible by construction, and a request does no disk IO.

#[cfg(feature = "markdown")]
pub struct Content {
    docs: BTreeMap<String, Doc>,
}

#[cfg(feature = "markdown")]
impl Content {
    /// Scan `docs/*.md` into memory. Call at startup and on each reload; hold the result
    /// behind an `Arc` and serve requests from it.
    pub fn load(docs_dir: &Path) -> anyhow::Result<Self> {
        Ok(Self { docs: scan_flat(docs_dir)? })
    }

    pub fn doc(&self, path: &str) -> Option<&Doc> {
        self.docs.get(path)
    }
}

/// Scan a flat directory of `<slug>.md` files into a `slug → Doc` map.
#[cfg(feature = "markdown")]
fn scan_flat(dir: &Path) -> anyhow::Result<BTreeMap<String, Doc>> {
    let mut map = BTreeMap::new();
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) == Some("md") {
            let slug = path.file_stem().unwrap().to_string_lossy().into_owned();
            map.insert(slug.clone(), read_doc(slug, &path)?);
        }
    }
    Ok(map)
}

#[cfg(feature = "markdown")]
fn read_doc(slug: String, path: &Path) -> anyhow::Result<Doc> {
    let markdown = fs::read_to_string(path)?;
    let title = extract_title(&markdown).unwrap_or(slug);
    Ok(Doc { title, markdown })
}

#[cfg(feature = "markdown")]
fn extract_title(markdown: &str) -> Option<String> {
    markdown
        .lines()
        .find(|l| l.starts_with("# "))
        .map(|l| l.trim_start_matches("# ").to_string())
}

#[cfg(all(test, feature = "markdown"))]
mod tests {
    use super::*;

    #[test]
    fn missing_content_directory_is_an_error() {
        let missing = std::env::temp_dir().join(format!("missing-blog-content-{}", std::process::id()));
        assert!(!missing.exists());
        assert!(Content::load(&missing).is_err());
    }

    #[test]
    fn content_indexes_files_and_lookups_cannot_traverse() {
        let root = std::env::temp_dir().join(format!("blogcore-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let docs = root.join("docs");
        fs::create_dir_all(docs.join("notes")).unwrap();
        fs::write(docs.join("intro.md"), "# Intro\ndoc").unwrap();
        fs::write(docs.join("notes/private.md"), "# Private\nnot a chapter").unwrap();

        let content = Content::load(&docs).unwrap();

        // Requests are pure map lookups.
        assert_eq!(content.doc("intro").unwrap().title, "Intro");

        // A file in a subdirectory is not a chapter: the scan is one level, so nothing
        // nested is ever indexed — and `/{slug}` cannot name it either.
        assert!(content.doc("notes/private").is_none());
        assert!(content.doc("private").is_none());

        // A traversal-shaped key is just a miss — no path is ever built from it, so there
        // is nothing to escape (this is why the index design is safe by construction).
        assert!(content.doc("../secret").is_none());

        let _ = fs::remove_dir_all(&root);
    }
}

pub mod sim_key;
pub use sim_key::{GateSignal, PolicyStage, PolicyTab, ServerBehavior, SimKey, TryControl};

pub mod code_key;
pub use code_key::CodeKey;

#[cfg(feature = "markdown")]
pub mod code_tabs;
#[cfg(feature = "markdown")]
pub use code_tabs::{code_groups, CodeGroup, CodeTab};

#[cfg(feature = "markdown")]
pub mod markdown_view;
#[cfg(feature = "markdown")]
pub use markdown_view::{ContentMapping, CODE_TABS_LIVE};
