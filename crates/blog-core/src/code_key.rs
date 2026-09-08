//! The code-tabs live key. It sits beside [`SimKey`](crate::SimKey) rather than with the
//! fence parsing because a key crosses to the guest, and the parsing does not.

/// A code group's live key: the group's page-scoped id, and nothing else.
///
/// Not a record reference. A group is a run of fences in one page's markdown, numbered
/// within that page — it has no identity away from the page it was parsed out of, and
/// nothing could fetch one on its own. So the island finds its group in the page.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Deserialize)]
#[serde(transparent)]
pub struct CodeKey(pub String);

impl idyll::live::IslandKey for CodeKey {
    fn to_wire(&self) -> String {
        self.0.clone()
    }
}

impl idyll::live::FromLiveKey for CodeKey {
    fn from_wire(wire: &str) -> Self {
        CodeKey(wire.to_string())
    }
}
