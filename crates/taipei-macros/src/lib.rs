//! Proc-macros for taipei. Currently just `#[shown]`, which captures a
//! function's body source into a constant so a "show code" panel can display
//! exactly the code that compiled and ran.

use proc_macro::TokenStream;
use proc_macro2::{Delimiter, TokenStream as TokenStream2, TokenTree};
use quote::quote;
use syn::{parse_macro_input, Ident};

/// Emit the annotated function unchanged, plus `pub const <NAME>: &str` holding
/// the verbatim source text of the function body (outer braces stripped, common
/// indentation removed).
///
/// ```ignore
/// #[shown(PROTECT_SRC)]
/// pub fn protect(...) -> ... {
///     let inner = ServiceBuilder::new()
///         .layer(CpuBackpressureLayer::new(instr))
///         .service(inner);
///     QueueLayer::new(queue_timeout).build(inner, handle)
/// }
/// // ⇒ PROTECT_SRC == "let inner = ServiceBuilder::new()\n    .layer(…)…"
/// ```
#[proc_macro_attribute]
pub fn shown(attr: TokenStream, item: TokenStream) -> TokenStream {
    let const_name = parse_macro_input!(attr as Ident);
    let item2: TokenStream2 = item.into();

    // The function body is the final top-level brace group.
    let body = item2
        .clone()
        .into_iter()
        .filter_map(|tt| match tt {
            TokenTree::Group(g) if g.delimiter() == Delimiter::Brace => Some(g),
            _ => None,
        })
        .last();

    // Prefer the real source text (preserves formatting + comments); fall back
    // to the reformatted token stream when source text is unavailable.
    let raw = body
        .as_ref()
        .and_then(|g| g.span().source_text())
        .or_else(|| body.as_ref().map(|g| g.to_string()))
        .unwrap_or_default();

    let src = clean_body(&raw);

    quote! {
        pub const #const_name: &str = #src;
        #item2
    }
    .into()
}

/// Strip the outer braces and common leading indentation from a block's source.
fn clean_body(raw: &str) -> String {
    let inner = raw.trim();
    let inner = inner.strip_prefix('{').unwrap_or(inner);
    let inner = inner.strip_suffix('}').unwrap_or(inner);

    let lines: Vec<&str> = inner.lines().collect();
    let start = lines.iter().position(|l| !l.trim().is_empty()).unwrap_or(0);
    let end = lines
        .iter()
        .rposition(|l| !l.trim().is_empty())
        .map(|i| i + 1)
        .unwrap_or(0);
    if start >= end {
        return String::new();
    }
    let lines = &lines[start..end];

    let indent = lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);

    lines
        .iter()
        .map(|l| {
            if l.len() >= indent {
                &l[indent..]
            } else {
                l.trim_start()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}
