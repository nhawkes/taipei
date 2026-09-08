//! The collapsible code block — the sim's "taipei setup" panel. Live-plane and
//! message-generic: it binds no handlers, so it splices into any component.

use idyll_styles::styles;

/// One run of code that shares a colour. The panel renders these in order, so the
/// highlighter's whole contract is "the concatenated text is the code".
#[derive(Clone, PartialEq)]
pub struct Token {
    pub i: usize,
    pub text: String,
    pub style: String,
}

pub fn token_text(t: &Token) -> String {
    t.text.clone()
}
pub fn token_style(t: &Token) -> String {
    format!("color:var({})", t.style)
}

/// What a word is, where that is worth a colour. Everything a listing does not name
/// — punctuation, keywords, the shape of the call — is left as plain text, so the
/// colour marks the two things a reader scans for: the types and the constants.
fn classify(word: &str) -> Option<&'static str> {
    let mut chars = word.chars();
    let first = chars.next()?;
    if word.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        && word.chars().any(|c| c.is_ascii_uppercase())
    {
        return Some(Code::literal.name);
    }
    first.is_ascii_uppercase().then_some(Code::name.name)
}

/// A deliberately small Rust colouriser. It is a reading aid for the listings on this
/// site — eight lines, ours, and already known to compile — not a parser: a `//`
/// inside a string would be read as a comment, and no listing here has one.
pub fn highlight(src: &str) -> Vec<Token> {
    let mut out: Vec<Token> = Vec::new();
    {
        let mut push = |text: String, style: &str| {
            if !text.is_empty() {
                out.push(Token { i: out.len(), text, style: style.to_string() });
            }
        };
        // A word ends. If it is worth a colour, the plain run before it closes and the
        // word goes out on its own; otherwise it is left for the caller to fold back
        // into that run. `push` is a parameter rather than a capture so that this and
        // the closure above do not both hold `out`.
        let flush = |word: &mut String, plain: &mut String, push: &mut dyn FnMut(String, &str)| {
            let Some(style) = classify(word) else { return };
            push(std::mem::take(plain), Code::text.name);
            push(std::mem::take(word), style);
        };

        for (n, line) in src.lines().enumerate() {
            if n > 0 {
                push("\n".to_string(), Code::text.name);
            }
            let (line, comment) = match line.find("//") {
                Some(at) => (&line[..at], Some(&line[at..])),
                None => (line, None),
            };

            let mut word = String::new();
            let mut plain = String::new();
            for ch in line.chars() {
                if ch.is_alphanumeric() || ch == '_' {
                    word.push(ch);
                } else {
                    flush(&mut word, &mut plain, &mut push);
                    plain.push_str(&std::mem::take(&mut word));
                    plain.push(ch);
                }
            }
            flush(&mut word, &mut plain, &mut push);
            plain.push_str(&word);
            push(plain, Code::text.name);

            if let Some(comment) = comment {
                push(comment.to_string(), Code::comment.name);
            }
        }
    }
    out
}

pub use styles::Code;

#[styles]
pub mod styles {
    use idyll_styles::Style;

    use crate::atoms::tokens::{Face, Radius};
    use crate::styles::Palette;

    vars! {
        /// The four distinctions a composition's listing makes. Values are the panel's,
        /// not the page's — the code surface is dark in every scheme.
        pub Code {
            text:    "#e6edf3",
            comment: "#6e7681",
            name:    "#d2a8ff",
            literal: "#79c0ff",
        }
    }

    /// The listing, folded away behind its own summary. The panel inside wears the
    /// same dark surface every other code block on the page does, so an open
    /// `details` and a plain fence read as the same kind of thing.
    pub const DETAILS: Style = css! {{
        margin: "0 0 14px",
        summary: {
            font_family: Face::sans,
            font_size: "13px",
            color: Palette::ink_muted,
            cursor: "pointer",
            user_select: "none",
        },
        pre: {
            margin: "10px 0 0",
            padding: "20px 22px",
            background: Palette::panel,
            color: Palette::panel_ink,
            border: "none",
            border_radius: Radius::card,
            overflow_x: "auto",
            font_family: Face::mono,
            font_size: "12.5px",
            line_height: 1.85,
        },
    }};
}
