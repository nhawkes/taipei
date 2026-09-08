//! **Markdown → Template IR** — content becomes typed idyll nodes, never an HTML string.
//!
//! Nathan's direction for the blog: no raw-HTML splice exists in idyll (that would be an
//! XSS hole and a second parser); instead markdown maps *structurally* into the same
//! [`TplNode`] IR the `live_view!` macro emits. The server fold serializes it (escaping
//! exactly once at the edge, like everything else), and a static blog page ships as pure
//! HTML with zero client bytes.
//!
//! Deliberate scope: inline raw HTML in markdown (`Event::Html`) is **dropped** — content
//! is markdown, not HTML-with-extra-steps. Footnotes and metadata-bearing extensions are
//! out until a post needs them.
//!
//! **Sims are a first-class fence** that maps to a first-class **live**: a fenced
//! code block tagged `sim` whose **body is a JSON spec** — e.g. a `sim` fence whose
//! body is `{ "sim": "queue-viz", "width": 960, "height": 520, "stage": "queue" }` —
//! becomes a live marker named after the sim, and the parsed spec rides the marker's
//! **key** ([`SimKey`]): the fence is the live's identity and its configuration; no
//! page data, no scanner script. The framework mounts sims exactly like any live,
//! wherever the content lands (first paint or a spliced transition body).
//! pulldown-cmark does the block parsing and `serde_json` the spec — no HTML is ever
//! parsed. Content errors (bad JSON, unknown key, sim missing from the manifest) render
//! as a **visible** error placeholder.
//!
//! **Tabbed code is content plus a control.** Adjacent fences labeled
//! ` ```rust tab=Reject ` / ` ```rust tab=Queue ` (quote a label to include spaces)
//! group into one tab set. The code is **page data** ([`crate::code_tabs`]) and the
//! fence leaves a live marker keyed by the group's id, so the island reads its own
//! group from the store and owns the switching — the tab bar is a real control rather
//! than radios impersonating one. Any non-tab block ends the group; a lone labeled
//! fence renders as the plain code block it is.
//!
//! **The mapping never invents a class name.** Styling is the app's, so the app hands
//! its styles in ([`ContentMapping`]) and their rules ride out in the template like any
//! other. Only two surfaces need naming — the tab set and the error placeholder —
//! because everything within them is reached by relational conditions from those two.

use std::borrow::Cow;
use std::collections::HashSet;
use idyll::template::{StyleRule, Template, TplAttr};
use idyll::View;
use idyll_styles::{merge, Style};
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Parser, Tag, TagEnd};

use crate::code_tabs::{self, CodeGroup, CodeTab};
use crate::{GateSignal, PolicyTab, ServerBehavior, SimKey, TryControl};

/// The live a tab group becomes. The app registers it under this name; the mapping
/// writes it; neither spells it twice.
pub const CODE_TABS_LIVE: &str = "code-tabs";

/// What the app supplies the content mapping: the live table's sim names, and the
/// styles for the two surfaces the mapping renders. Markdown is content, so the
/// mapping maps structure — the look of a tab set is the app's to say.
pub struct ContentMapping {
    /// The live table's sim names. A ` ```sim ` fence naming one becomes a live
    /// marker; any other name is a content error.
    pub sims: HashSet<String>,
    /// The visible placeholder a content error renders as.
    pub error: Style,
}

/// One markdown source mapped: the content it becomes, and the errors it raised on the
/// way. Both come out of the one parse — a caller asking whether a chapter is fit to
/// publish reads [`errors`](Mapped::errors) rather than searching the view for the
/// placeholder they were rendered as, which would be re-deriving a fact the mapping
/// already held.
pub struct Mapped {
    pub view: View,
    /// Every content error, in document order: a fence spec that would not parse, a sim
    /// that is not in the live table, a stage the sim does not have. Empty is
    /// publishable. Each is also rendered into `view`, loud, where it occurred.
    pub errors: Vec<String>,
}

impl ContentMapping {
    /// Markdown → content, ready to hand to `AppRoot::content`.
    pub fn content(&self, markdown: &str) -> View {
        self.map(markdown).view
    }

    /// Markdown → the content and the errors raising it produced.
    pub fn map(&self, markdown: &str) -> Mapped {
        let (template, errors) = self.parse(markdown);
        Mapped {
            view: View::from_ir(template.nodes.into_owned(), template.styles.into_owned()),
            errors,
        }
    }

    /// Markdown → the [`Template`] IR (the flat node list plus the style rules its
    /// elements reference) and the content errors raised building it.
    fn parse(&self, markdown: &str) -> (Template, Vec<String>) {
        let mut builder = IrBuilder::new(self);
        let options = code_tabs::options();

        // Image alt text is the *text content* of the image span — collected, not nested.
        let mut image: Option<(String, String, String)> = None; // (dest, title, alt)
        // Inside a ` ```sim ` fence: accumulate the JSON body, parsed at the fence's end.
        let mut sim: Option<String> = None;
        // The pending tab group (closed fences) and the tab fence currently collecting.
        let mut tabs: Vec<CodeTab> = Vec::new();
        let mut tab: Option<CodeTab> = None;

        for event in Parser::new_ext(markdown, options) {
            if let Some(current) = &mut tab {
                match event {
                    Event::Text(text) => current.code.push_str(&text),
                    Event::End(TagEnd::CodeBlock) => {
                        tabs.push(tab.take().expect("collecting state present"));
                    }
                    _ => {} // only the fence's text body carries the code
                }
                continue;
            }
            if let Some(body) = &mut sim {
                match event {
                    Event::Text(text) => body.push_str(&text),
                    Event::End(TagEnd::CodeBlock) => {
                        let claimed = std::mem::take(&mut tabs);
                        match SimSpec::parse(body) {
                            Ok(spec) => builder.sim_embed(&spec, claimed),
                            Err(message) => {
                                builder.tabs_embed(claimed);
                                builder.sim_error(&message);
                            }
                        }
                        sim = None;
                    }
                    _ => {} // only the fence's text body carries the spec
                }
                continue;
            }
            if let Some((_, _, alt)) = image.as_mut() {
                match event {
                    Event::Text(text) | Event::Code(text) => {
                        alt.push_str(&text);
                        continue;
                    }
                    Event::End(TagEnd::Image) => {
                        let (dest, title, alt) = image.take().expect("image state present");
                        let mut attrs = vec![attr("src", dest), attr("alt", alt)];
                        if !title.is_empty() {
                            attrs.push(attr("title", title));
                        }
                        builder.leaf_element("img", attrs);
                        continue;
                    }
                    _ => continue, // markup inside alt text flattens to its text
                }
            }

            // A tab-labeled fence joins the pending group; any other event ends it —
            // except a `sim` fence, which may *claim* the group as its policy tabs
            // (`"tabs": [ … ]`). The group is held until the spec is parsed and says.
            let tab_start = match &event {
                Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(info))) => code_tabs::tab_fence(info),
                _ => None,
            };
            let next_is_sim = matches!(
                &event,
                Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(info)))
                    if info.split_whitespace().next() == Some("sim")
            );
            match tab_start {
                Some(started) => {
                    tab = Some(started);
                    continue;
                }
                None if !tabs.is_empty() && !next_is_sim => {
                    builder.tabs_embed(std::mem::take(&mut tabs))
                }
                None => {}
            }

            match event {
                Event::Start(tag) => match tag {
                    Tag::Paragraph => builder.open("p", Vec::new()),
                    Tag::Heading { level, .. } => builder.open(heading_tag(level), Vec::new()),
                    Tag::BlockQuote(_) => builder.open("blockquote", Vec::new()),
                    Tag::CodeBlock(kind) => {
                        if let CodeBlockKind::Fenced(info) = &kind {
                            if info.split_whitespace().next() == Some("sim") {
                                sim = Some(String::new()); // collect the JSON body
                                continue;
                            }
                        }
                        builder.open("pre", Vec::new());
                        let attrs = match kind {
                            CodeBlockKind::Fenced(lang) if !lang.is_empty() => {
                                vec![attr("class", format!("language-{lang}"))]
                            }
                            _ => Vec::new(),
                        };
                        builder.open("code", attrs);
                    }
                    Tag::List(Some(start)) => {
                        let attrs = if start != 1 {
                            vec![attr("start", start.to_string())]
                        } else {
                            Vec::new()
                        };
                        builder.open("ol", attrs);
                    }
                    Tag::List(None) => builder.open("ul", Vec::new()),
                    Tag::Item => builder.open("li", Vec::new()),
                    Tag::Emphasis => builder.open("em", Vec::new()),
                    Tag::Strong => builder.open("strong", Vec::new()),
                    Tag::Strikethrough => builder.open("del", Vec::new()),
                    Tag::Link { dest_url, title, .. } => {
                        let mut attrs = vec![attr("href", dest_url.into_string())];
                        if !title.is_empty() {
                            attrs.push(attr("title", title.into_string()));
                        }
                        builder.open("a", attrs);
                    }
                    Tag::Image { dest_url, title, .. } => {
                        image = Some((dest_url.into_string(), title.into_string(), String::new()));
                    }
                    // Tables emitted parser-fixed-point correct: table > thead/tbody > tr.
                    Tag::Table(_) => {
                        builder.open("table", Vec::new());
                        builder.in_table_head = false;
                    }
                    Tag::TableHead => {
                        builder.open("thead", Vec::new());
                        builder.open("tr", Vec::new());
                        builder.in_table_head = true;
                    }
                    Tag::TableRow => {
                        if !builder.in_table_body {
                            builder.open("tbody", Vec::new());
                            builder.in_table_body = true;
                        }
                        builder.open("tr", Vec::new());
                    }
                    Tag::TableCell => {
                        builder.open(if builder.in_table_head { "th" } else { "td" }, Vec::new())
                    }
                    _ => builder.open("span", Vec::new()), // unmapped block: neutral wrapper
                },
                Event::End(end) => match end {
                    TagEnd::CodeBlock => {
                        builder.close(); // code
                        builder.close(); // pre
                    }
                    TagEnd::TableHead => {
                        builder.close(); // tr
                        builder.close(); // thead
                        builder.in_table_head = false;
                    }
                    TagEnd::Table => {
                        if builder.in_table_body {
                            builder.close(); // tbody
                            builder.in_table_body = false;
                        }
                        builder.close(); // table
                    }
                    TagEnd::Image => {} // handled by the alt collector
                    _ => builder.close(),
                },
                Event::Text(text) => builder.text(&text),
                Event::Code(text) => {
                    builder.open("code", Vec::new());
                    builder.text(&text);
                    builder.close();
                }
                Event::SoftBreak => builder.text("\n"),
                Event::HardBreak => builder.leaf_element("br", Vec::new()),
                Event::Rule => builder.leaf_element("hr", Vec::new()),
                Event::TaskListMarker(checked) => {
                    let mut attrs = vec![attr("type", "checkbox"), attr("disabled", "")];
                    if checked {
                        attrs.push(attr("checked", ""));
                    }
                    builder.leaf_element("input", attrs);
                }
                // No raw-HTML hole: inline HTML is dropped, deliberately.
                Event::Html(_) | Event::InlineHtml(_) => {}
                Event::FootnoteReference(_) | Event::InlineMath(_) | Event::DisplayMath(_) => {}
            }
        }
        if !tabs.is_empty() {
            builder.tabs_embed(tabs);
        }

        builder.finish()
    }
}

fn heading_tag(level: HeadingLevel) -> &'static str {
    match level {
        HeadingLevel::H1 => "h1",
        HeadingLevel::H2 => "h2",
        HeadingLevel::H3 => "h3",
        HeadingLevel::H4 => "h4",
        HeadingLevel::H5 => "h5",
        HeadingLevel::H6 => "h6",
    }
}

fn attr(name: &'static str, value: impl Into<String>) -> TplAttr {
    TplAttr {
        name: Cow::Borrowed(name),
        value: Cow::Owned(value.into()),
    }
}

/// A ` ```sim ` fence's JSON body, **parsed straight into typed data** by serde: an
/// unknown key, a missing `sim`/`width`/`height`, a bad type, or an unknown server
/// behavior are all *deserialization* errors — there is no separate validate-then-reread
/// pass. `deny_unknown_fields` is the loud-content-error contract, expressed once.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SimSpec {
    #[serde(rename = "sim")]
    name: String,
    width: u32,
    height: u32,
    #[serde(default)]
    stage: Option<String>,
    #[serde(default)]
    workload: Option<String>,
    #[serde(default = "yes")]
    charts: bool,
    #[serde(default)]
    manual: bool,
    #[serde(default)]
    servers: Vec<ServerBehavior>,
    #[serde(default)]
    gates: Vec<GateSignal>,
    #[serde(rename = "try", default)]
    try_control: Option<TryControl>,
    /// Policy tabs: the compositions this sim lets the reader switch between, each
    /// written `{ "display": …, "stage": … }`. A tab group immediately above the fence
    /// supplies each policy's code, matched by `display`.
    #[serde(default)]
    tabs: Vec<PolicyTab>,
}

fn yes() -> bool {
    true
}

impl SimSpec {
    fn parse(body: &str) -> Result<SimSpec, String> {
        let body = body.trim();
        if body.is_empty() {
            return Err("sim fence body is empty: expected a JSON object like \
                { \"sim\": \"queue-viz\", \"width\": 960, \"height\": 520 }".to_string());
        }
        let spec: SimSpec = serde_json::from_str(body).map_err(|e| format!("sim fence: {e}"))?;
        // Tab labels ride the live key, which is `;`-delimited, `,`-joined, and pairs a
        // label to its stage with `:`. Rejecting those three here is what lets the key
        // stay unescaped — the invariant is enforced where the label enters, not where
        // it is written.
        let breaks_key = |c: char| c == ';' || c == ',' || c == ':';
        if let Some(bad) = spec.tabs.iter().find(|t| t.display.contains(breaks_key)) {
            return Err(format!(
                "sim fence: tab label `{}` may not contain `;`, `,` or `:`",
                bad.display
            ));
        }
        Ok(spec)
    }
}

/// A nested-tree builder that flattens to the pre-order IR at the end — the runtime twin
/// of the `live_view!` macro's compile-time emitter (adjacent text merges here too).
struct IrBuilder<'a> {
    /// One children-list per open element; index 0 is the root level.
    stack: Vec<Vec<Nested>>,
    /// Tags of the open elements (paired with `stack[1..]`).
    open: Vec<(String, Vec<TplAttr>)>,
    /// Sim fences seen so far — the next fence's ordinal.
    sim_count: u32,
    /// Tab groups seen so far — the next group's id. Shared by the standalone tab set
    /// and a sim's claimed policies, so ids follow document order and match
    /// [`code_tabs::code_groups`], which numbers the same runs the same way.
    tabs_count: u32,
    in_table_head: bool,
    in_table_body: bool,
    /// The app's half of the mapping: what a sim name resolves against, and the
    /// styles the rendered surfaces wear.
    mapping: &'a ContentMapping,
    /// Content errors raised so far, in document order. Recorded where they are
    /// rendered, so the two cannot disagree about what this source got wrong.
    errors: Vec<String>,
    /// The style rules the emitted classes reference, riding out in the template.
    rules: Vec<StyleRule>,
}

enum Nested {
    Element {
        tag: String,
        attrs: Vec<TplAttr>,
        children: Vec<Nested>,
    },
    Text(String),
    /// A sim live marker: the sim's name, its [`SimKey`] wire string, and what stands
    /// in the hole until a mount paints over it. A mount that refuses leaves the
    /// fallback, so a sim that breaks reads like a sim that was authored wrong — to
    /// the reader they are the same event.
    Live { name: String, key: String, fallback: Vec<Nested> },
}

impl<'a> IrBuilder<'a> {
    fn new(mapping: &'a ContentMapping) -> Self {
        IrBuilder {
            stack: vec![Vec::new()],
            open: Vec::new(),
            sim_count: 0,
            tabs_count: 0,
            in_table_head: false,
            in_table_body: false,
            mapping,
            rules: Vec::new(),
            errors: Vec::new(),
        }
    }

    /// A style becomes a class attribute, its rules joining the template's — the same
    /// journey `css=[…]` gives an element in a view.
    fn styled(&mut self, style: &Style) -> Vec<TplAttr> {
        let merged = merge(&[style.atoms()]);
        if merged.rules.is_empty() {
            return Vec::new();
        }
        self.rules.extend(merged.rules);
        vec![attr("class", merged.class_attr)]
    }

    fn open(&mut self, tag: &str, attrs: Vec<TplAttr>) {
        self.open.push((tag.to_string(), attrs));
        self.stack.push(Vec::new());
    }

    fn close(&mut self) {
        let children = self.stack.pop().expect("balanced markdown events");
        let (tag, attrs) = self.open.pop().expect("balanced markdown events");
        self.push(Nested::Element { tag, attrs, children });
    }

    fn leaf_element(&mut self, tag: &str, attrs: Vec<TplAttr>) {
        self.push(Nested::Element {
            tag: tag.to_string(),
            attrs,
            children: Vec::new(),
        });
    }

    fn text(&mut self, text: &str) {
        let level = self.stack.last_mut().expect("a level is always open");
        if let Some(Nested::Text(previous)) = level.last_mut() {
            previous.push_str(text);
            return;
        }
        level.push(Nested::Text(text.to_string()));
    }

    /// A resolvable sim fence becomes a **`sim` live marker** — a first-class
    /// [`TplNode::Live`] hole the framework mounts like any other live (SSR paint
    /// through the membrane, browser claim, spliced-content builds). The resolved spec
    /// is recorded in fence order; `Page.sims[n]` is instance `n`'s data. A sim
    /// missing from the manifest renders as a visible error instead — content errors
    /// are loud, never a dead marker.
    /// `claimed` is the tab group that sat immediately above the fence, if any. A sim
    /// with `"tabs"` takes it — the policies and their code become the sim's, so one
    /// island owns the choice, the code it shows, and the machine it runs. A sim
    /// without takes nothing, and the group renders as its own tab set.
    fn sim_embed(&mut self, spec: &SimSpec, claimed: Vec<CodeTab>) {
        if !self.mapping.sims.contains(&spec.name) {
            self.tabs_embed(claimed);
            return self.sim_error(&format!("sim `{}` is not in the live table", spec.name));
        }
        let stage = match spec.stage.as_deref().map(str::parse).transpose() {
            Ok(stage) => stage,
            Err(_) => {
                self.tabs_embed(claimed);
                let named = spec.stage.as_deref().unwrap_or_default();
                return self.sim_error(&format!("sim `{}`: no stage named `{named}`", spec.name));
            }
        };
        let code = match spec.tabs.is_empty() {
            true => {
                self.tabs_embed(claimed);
                String::new()
            }
            false => self.claim_group(claimed).unwrap_or_default(),
        };
        let key = SimKey {
            ordinal: self.sim_count,
            width: spec.width,
            height: spec.height,
            stage,
            workload: spec.workload.clone().unwrap_or_default(),
            charts: spec.charts,
            manual: spec.manual,
            servers: spec.servers.clone(),
            gates: spec.gates.clone(),
            try_control: spec.try_control,
            tabs: spec.tabs.clone(),
            code,
        };
        self.sim_count += 1;
        let fallback = self.live_fallback();
        self.push(Nested::Live { name: spec.name.clone(), key: key.wire(), fallback });
    }

    /// Take the next group id for `tabs`, or `None` if this is not a group (fewer than
    /// two fences). The one place a group is numbered — both the standalone tab set and
    /// a sim's claimed policies come through here, so ids stay in document order and
    /// agree with [`code_tabs::code_groups`].
    fn claim_group(&mut self, tabs: Vec<CodeTab>) -> Option<String> {
        if tabs.len() < 2 {
            self.tabs_embed(tabs);
            return None;
        }
        let id = CodeGroup::id_at(self.tabs_count as usize);
        self.tabs_count += 1;
        Some(id)
    }

    /// A closed tab group becomes a **live marker keyed by the group's id**. The code
    /// itself is page data ([`code_tabs::code_groups`], resolved into the store), so the
    /// island reads its own group and owns the switching — the tab bar is the same
    /// control the sims use, not a stack of radios pretending to be one.
    ///
    /// One fence alone is no group: it renders as the plain code block it is, and does
    /// not consume an id — which is why both walks count only runs of two or more.
    fn tabs_embed(&mut self, tabs: Vec<CodeTab>) {
        match <[CodeTab; 1]>::try_from(tabs) {
            Ok([only]) => self.code_block(&only.lang, &only.code),
            Err(tabs) if tabs.is_empty() => {}
            Err(tabs) => {
                let id = CodeGroup::id_at(self.tabs_count as usize);
                self.tabs_count += 1;
                let key = CodeGroup { id, tabs }.key();
                let fallback = self.live_fallback();
                self.push(Nested::Live { name: CODE_TABS_LIVE.to_string(), key, fallback });
            }
        }
    }

    fn code_block(&mut self, lang: &str, body: &str) {
        self.open("pre", Vec::new());
        let attrs = if lang.is_empty() {
            Vec::new()
        } else {
            vec![attr("class", format!("language-{lang}"))]
        };
        self.open("code", attrs);
        self.text(body);
        self.close();
        self.close();
    }

    /// A content error, rendered loud in the page — never a silent drop, never a panic —
    /// and recorded, so a caller deciding whether this source is publishable is told
    /// rather than left to recognise the placeholder.
    fn sim_error(&mut self, message: &str) {
        self.errors.push(message.to_string());
        let error_attrs = self.styled(&self.mapping.error);
        self.open("div", error_attrs);
        self.text(message);
        self.close();
    }

    /// What a live marker carries until its mount paints over it. The same surface
    /// [`sim_error`](Self::sim_error) gives a malformed fence: a sim the reader cannot
    /// see is one event to them, whichever side it failed on.
    fn live_fallback(&mut self) -> Vec<Nested> {
        vec![Nested::Element {
            tag: "div".to_string(),
            attrs: self.styled(&self.mapping.error),
            children: vec![Nested::Text("Error occurred while loading".to_string())],
        }]
    }

    fn push(&mut self, node: Nested) {
        self.stack.last_mut().expect("a level is always open").push(node);
    }

    fn finish(mut self) -> (Template, Vec<String>) {
        // Unbalanced input degrades gracefully: close anything left open.
        while !self.open.is_empty() {
            self.close();
        }
        let root = self.stack.pop().expect("root level");
        let template = root
            .into_iter()
            .fold(View::EMPTY, |acc, node| acc.append(build(node)))
            .into_template();
        let styles = self.rules;
        (template.with_styles(styles), self.errors)
    }
}

/// One nested node folded onto the public content constructors — this mapper is the
/// first hand-written `View` builder, no raw IR anywhere.
fn build(node: Nested) -> View {
    match node {
        Nested::Text(text) => View::text(text),
        Nested::Live { name, key, fallback } => View::live_mount(
            name,
            Some(key),
            fallback.into_iter().fold(View::EMPTY, |acc, node| acc.append(build(node))),
        ),
        Nested::Element { tag, attrs, children } => View::element(
            tag,
            attrs,
            children.into_iter().fold(View::EMPTY, |acc, child| acc.append(build(child))),
        ),
    }
}
