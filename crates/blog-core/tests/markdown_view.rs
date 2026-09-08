//! Pins the markdown → Template IR → HTML pipeline: content becomes typed idyll nodes
//! (no raw-HTML splice anywhere), and the server fold serializes it with exactly one
//! entity-encoding pass at the edge.

use std::collections::HashSet;

use blog_core::ContentMapping;
use idyll::View;
use idyll_styles::{styles, Style};

/// The mapping wears the caller's styles, so the pins are written against these —
/// what the class *is* belongs to whoever declares it.
#[styles]
mod styles {
    use idyll_styles::Style;

    pub const ERROR: Style = css! {{ color: "#ff5555" }};
}

fn mapping(sims: &[&str]) -> ContentMapping {
    ContentMapping {
        sims: sims.iter().map(|name| name.to_string()).collect::<HashSet<_>>(),
        error: styles::ERROR,
    }
}

fn class_of(style: Style) -> String {
    idyll_styles::merge(&[style.atoms()]).class_attr
}

fn html(markdown: &str) -> String {
    fold_of(mapping(&[]).content(markdown))
}

#[test]
fn prose_structures_map_to_elements() {
    let out = html("# Title\n\nHello *world*, **bold** and `code`.\n\n> quoted\n\n---\n");
    assert_eq!(
        out,
        "<h1>Title</h1><p>Hello <em>world</em>, <strong>bold</strong> and <code>code</code>.</p>\
         <blockquote><p>quoted</p></blockquote><hr>"
    );
}

#[test]
fn lists_links_and_images() {
    let out = html("3. first\n4. second\n\n- [taipei](https://example.com \"t\")\n- ![alt text](/img.png)\n");
    assert_eq!(
        out,
        "<ol start=\"3\"><li>first</li><li>second</li></ol>\
         <ul><li><a href=\"https://example.com\" title=\"t\">taipei</a></li>\
         <li><img src=\"/img.png\" alt=\"alt text\"></li></ul>"
    );
}

#[test]
fn fenced_code_keeps_language_and_content_verbatim() {
    let out = html("```rust\nlet x = a < b && c > d;\n```\n");
    assert_eq!(
        out,
        "<pre><code class=\"language-rust\">let x = a &lt; b &amp;&amp; c &gt; d;\n</code></pre>"
    );
}

#[test]
fn adjacent_tab_fences_become_one_keyed_live() {
    let out = html(
        "before\n\n```rust tab=Reject\nreject();\n```\n\n```rust tab=\"Queue it\"\nqueue();\n```\n\nafter\n",
    );
    // The code is page data now, so the marker carries only the group's reference; the
    // island reads the group back and renders the bar and the panel. Nothing mounted
    // into this wrapper, so it carries the marker's fallback — the same rule the server
    // applies to a mount that refuses.
    assert_eq!(
        out,
        "<p>before</p>\
         <idyll-live data-i=\"code-tabs\" data-k=\"code-0\" style=\"display:contents\">\
         <div class=\"idbf2ca32-error-color\">Error occured while loading</div>\
         </idyll-live>\
         <p>after</p>"
    );
}

#[test]
fn the_mapping_and_the_resolver_number_groups_alike() {
    // THE INVARIANT the design rests on: a marker's key always names a group the
    // resolver actually put in the store. Both sides walk the same source with the same
    // fence rule, so ids agree — a lone labeled fence consumes no id, and a group a sim
    // claims consumes one exactly like a standalone set does.
    let source = "```rust tab=A\nlone();\n```\n\ntext\n\n\
                  ```rust tab=X\nx();\n```\n\n```rust tab=Y\ny();\n```\n\n\
                  more\n\n```rust tab=P\np();\n```\n\n```rust tab=Q\nq();\n```\n\n\
                  ```sim\n{ \"sim\": \"queue-viz\", \"width\": 9, \"height\": 9, \"tabs\": \
                  [{ \"display\": \"P\", \"stage\": \"reject\" }, \
                   { \"display\": \"Q\", \"stage\": \"wait\" }] }\n```\n";

    let groups = blog_core::code_groups(source);
    assert_eq!(
        groups.iter().map(|g| g.id.as_str()).collect::<Vec<_>>(),
        ["code-0", "code-1"],
        "two runs of two; the lone labeled fence is not a group"
    );

    let out = fold_of(mapping(&["queue-viz"]).content(source));
    assert!(out.contains("data-k=\"code-0\""), "the standalone set keeps its marker: {out}");
    assert!(out.contains(";code-1\""), "the sim's key ends with the group it claimed: {out}");
    assert!(
        !out.contains("data-k=\"code-1\""),
        "a claimed group gets no marker of its own: {out}"
    );
}

#[test]
fn a_lone_tab_fence_is_a_plain_code_block() {
    // One labeled fence is no group — no radio chrome, just the code block. Prose
    // between fences ends a group, so these two labeled fences render separately.
    let out = html("```rust tab=A\none();\n```\n\nbetween\n\n```rust tab=B\ntwo();\n```\n");
    assert_eq!(
        out,
        "<pre><code class=\"language-rust\">one();\n</code></pre><p>between</p>\
         <pre><code class=\"language-rust\">two();\n</code></pre>"
    );
}

#[test]
fn tables_emit_parser_fixed_point_structure() {
    let out = html("| a | b |\n|---|---|\n| 1 | 2 |\n");
    assert_eq!(
        out,
        "<table><thead><tr><th>a</th><th>b</th></tr></thead>\
         <tbody><tr><td>1</td><td>2</td></tr></tbody></table>"
    );
}

#[test]
fn raw_html_is_dropped_not_smuggled() {
    // The deliberate design: markdown is markdown. Inline HTML never reaches the output,
    // so there is no raw-HTML injection surface anywhere in the pipeline.
    let out = html("safe <script>alert(1)</script> text\n");
    assert!(!out.contains("<script"), "raw HTML leaked: {out}");
    assert!(out.contains("safe"), "surrounding text lost: {out}");
    assert!(out.contains("text"), "surrounding text lost: {out}");
}

#[test]
fn text_is_escaped_exactly_once() {
    let out = html("AT&T says 1 < 2\n");
    assert_eq!(out, "<p>AT&amp;T says 1 &lt; 2</p>");
}

#[test]
fn sim_fences_map_to_the_typed_embed() {
    let content = mapping(&["queue-viz"]).content(
        "before

```sim
{ \"sim\": \"queue-viz\", \"width\": 960, \"height\": 520 }
```

after
",
    );

    // The fence is a first-class live marker in the IR, named after the sim — no
    // canvas markup, no scanner attributes; the live mount's client half renders the canvas.
    // Its spec rides the marker's key ([`SimKey`]), the fence's whole payload.
    use idyll::live::FromLiveKey;
    use idyll::template::TplNode;
    let key = content
        .template()
        .nodes
        .iter()
        .find_map(|node| match node {
            TplNode::Live { name, key: Some(key), .. } if name == "queue-viz" => {
                Some(key.to_string())
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("sim fence did not become a `queue-viz` live: {:?}", content.template().nodes));
    let spec = blog_core::SimKey::from_wire(&key);
    assert_eq!((spec.ordinal, spec.width, spec.height), (0, 960, 520));

    let out = fold_of(content);
    assert!(out.contains("<p>before</p>"), "prose around the sim lost: {out}");
    assert!(out.contains("<idyll-live data-i=\"queue-viz\""), "live wrapper missing: {out}");
    assert!(out.contains("<p>after</p>"), "prose around the sim lost: {out}");
}

#[test]
fn sim_content_errors_are_loud_not_silent() {
    // Unknown sim (not in the manifest) and a malformed spec both render a visible
    // error placeholder — content bugs surface on the page, never as a silent drop.
    let out = html("```sim\n{ \"sim\": \"ghost\", \"width\": 10, \"height\": 10 }\n```\n");
    assert!(out.contains(&format!("class=\"{}\"", class_of(styles::ERROR))), "no visible error: {out}");
    assert!(out.contains("ghost"), "error doesn't name the sim: {out}");

    // Malformed spec (width is not a number): loud error, no live.
    let out = html("```sim\n{ \"sim\": \"queue-viz\", \"width\": \"abc\", \"height\": 10 }\n```\n");
    assert!(out.contains(&format!("class=\"{}\"", class_of(styles::ERROR))), "no visible error: {out}");
    assert!(!out.contains("<idyll-live"), "malformed spec still emitted a live: {out}");

    // Unknown key is caught by the parse (`deny_unknown_fields`) — not a separate check.
    let out = html("```sim\n{ \"sim\": \"queue-viz\", \"width\": 10, \"height\": 10, \"bogus\": 1 }\n```\n");
    assert!(out.contains("unknown field"), "unknown key not surfaced: {out}");

    // An unknown server behavior is an unknown-variant parse error.
    let out = html("```sim\n{ \"sim\": \"queue-viz\", \"width\": 10, \"height\": 10, \"servers\": [\"nope\"] }\n```\n");
    assert!(out.contains(&format!("class=\"{}\"", class_of(styles::ERROR))), "bad behavior not surfaced: {out}");

    // A misspelled stage is as loud as a misspelled sim. It used to be neither: the key
    // carried the token as a string and the app resolved it with `unwrap_or_default`, so
    // `quue` silently ran the queue — a fence saying one thing and the picture another.
    let known = |src: &str| fold_of(mapping(&["queue-viz"]).content(src));
    let out = known("```sim\n{ \"sim\": \"queue-viz\", \"width\": 10, \"height\": 10, \"stage\": \"quue\" }\n```\n");
    assert!(out.contains(&format!("class=\"{}\"", class_of(styles::ERROR))), "bad stage not surfaced: {out}");
    assert!(out.contains("quue"), "error doesn't name the stage: {out}");
    assert!(!out.contains("<idyll-live"), "bad stage still emitted a live: {out}");

    // The stages that do exist still resolve, and a fence naming none is the sim's default.
    for stage in ["app", "backpressure", "reject", "wait", "queue"] {
        let src = format!("```sim\n{{ \"sim\": \"queue-viz\", \"width\": 10, \"height\": 10, \"stage\": \"{stage}\" }}\n```\n");
        assert!(known(&src).contains("<idyll-live"), "`{stage}` should be a stage");
    }
}

/// The one serialization path: content IR straight through the HTML fold.
fn fold_of(content: View) -> String {
    idyll::view_html(&content, []).into_string()
}
