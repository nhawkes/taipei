//! Tabbed code as **data**. A run of adjacent fences labeled ` ```rust tab=Reject ` /
//! ` ```rust tab=Queue ` is one group; the group's code is page data (the resolver puts
//! it in the store), and the markdown mapping leaves behind a live marker keyed by the
//! group's id. The island reads its own group back out of the store.
//!
//! The code therefore crosses once, as data, rather than being duplicated into a marker
//! attribute — and the tab bar can be a real control instead of a stack of radios.
//!
//! **The two sides agree because they are the same walk.** [`code_groups`] and the
//! mapping's `tabs_embed` share [`tab_fence`] and this module's grouping rule, and both
//! number groups in document order; `markdown_view`'s tests pin that the marker count
//! and the group count match on the same source.

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

/// One tab within a group: what the bar shows, the fence's language, and the code.
#[derive(Clone, Debug, PartialEq)]
pub struct CodeTab {
    pub label: String,
    pub lang: String,
    pub code: String,
}

/// A run of adjacent labeled fences, and the id the marker references it by.
#[derive(Clone, Debug, PartialEq)]
pub struct CodeGroup {
    /// Page-scoped and positional (`code-0`, `code-1`, …). The store is the page's, so
    /// a page-local id is a whole identity; nothing here is addressable across pages.
    pub id: String,
    pub tabs: Vec<CodeTab>,
}

impl CodeGroup {
    /// The id, as a live marker's key — the wire spelling [`CodeKey`](crate::CodeKey)
    /// decodes on the guest side.
    pub fn key(&self) -> String {
        self.id.clone()
    }

    /// The id group `n` gets. The one place the numbering is written.
    pub fn id_at(n: usize) -> String {
        format!("code-{n}")
    }
}

/// Every tab group in `markdown`, in document order.
///
/// A group is a run of **adjacent** labeled fences: any other event ends it, and a run
/// of one is not a group — a lone labeled fence is just a code block, which is what the
/// mapping renders it as.
pub fn code_groups(markdown: &str) -> Vec<CodeGroup> {
    let mut out: Vec<CodeGroup> = Vec::new();
    let mut run: Vec<CodeTab> = Vec::new();
    let mut collecting: Option<CodeTab> = None;

    let close = |run: &mut Vec<CodeTab>, out: &mut Vec<CodeGroup>| {
        if run.len() > 1 {
            out.push(CodeGroup { id: CodeGroup::id_at(out.len()), tabs: std::mem::take(run) });
        } else {
            run.clear();
        }
    };

    for event in Parser::new_ext(markdown, options()) {
        if let Some(tab) = &mut collecting {
            match event {
                Event::Text(text) => tab.code.push_str(&text),
                Event::End(TagEnd::CodeBlock) => {
                    run.push(collecting.take().expect("collecting state present"));
                }
                _ => {}
            }
            continue;
        }
        match &event {
            Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(info))) => match tab_fence(info) {
                Some(started) => collecting = Some(started),
                None => close(&mut run, &mut out),
            },
            _ => close(&mut run, &mut out),
        }
    }
    close(&mut run, &mut out);
    out
}

/// The parser options the mapping walks with — shared so both walks see the same events.
pub(crate) fn options() -> Options {
    Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES
}

/// Parse a fence info string as a tab fence: ` ```rust tab=Reject ` or
/// ` ```rust tab="OS CPU %" `. `None` = an ordinary code fence.
pub(crate) fn tab_fence(info: &str) -> Option<CodeTab> {
    let info = info.trim();
    let (lang, rest) = info.split_once(char::is_whitespace)?;
    let value = rest.trim_start().strip_prefix("tab=")?;
    let label = match value.strip_prefix('"') {
        Some(quoted) => quoted.split('"').next().unwrap_or_default(),
        None => value.split_whitespace().next().unwrap_or_default(),
    };
    (!label.is_empty()).then(|| CodeTab {
        label: label.to_string(),
        lang: lang.to_string(),
        code: String::new(),
    })
}
