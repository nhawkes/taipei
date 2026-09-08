//! A number that animates keeps its width: the slots it is not using are filled with the
//! figure space, a character one digit wide, so a reading is as wide at 0 as at 9999 and
//! nothing beside it moves as it counts.

const FIGURE_SPACE: char = '\u{2007}';

/// `value` right-aligned in `width` digit-wide slots.
pub fn slots(value: impl std::fmt::Display, width: usize) -> String {
    let text = value.to_string();
    let pad = width.saturating_sub(text.chars().count());
    let mut out = String::with_capacity(pad + text.len());
    out.extend(std::iter::repeat(FIGURE_SPACE).take(pad));
    out.push_str(&text);
    out
}

/// A running count as the cards and boxes show it: thousands grouped, five slots wide.
pub fn count(v: usize) -> String {
    slots(crate::atoms::server_box::group(v), 5)
}
